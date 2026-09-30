//! JSON-RPC transport (§31, BP-167).
//!
//! # A read instrument, by type
//!
//! Everything this module builds can **read** the chain and cannot write to it.
//! [`READ_METHODS`] is an allowlist, and [`RpcTransport::call`] refuses a method
//! off it before any byte leaves the process. That is what lets a shadow run be
//! unable to broadcast rather than merely configured not to: §16.1's null
//! dispatcher is the only dispatcher it holds, and the transport it reads the
//! chain through has no method that sends.
//!
//! An allowlist and not a denylist, because the dangerous direction is the one a
//! denylist gets wrong. Providers add submission methods —
//! `eth_sendRawTransactionConditional`, `eth_sendBundle`, private-transaction
//! variants — and a denylist that did not know a new one would pass it through.
//! A read that is missing from the allowlist fails loudly on first use, which is
//! the cheap failure.
//!
//! Submission, when it arrives, is a separate type on a separate lane
//! (`base::submit`), and that separation is the point: the process that reads
//! state for a thousand candidates a minute is not the process holding a
//! transport that can spend money.
//!
//! # Moved from `arb-exec-legacy::rpc_failover`, and what changed
//!
//! §3.4 classifies `rpc_failover.rs` as **KEEP** — it ended ~40 days of
//! `rpc_error` with zero trades — and Phase 7 deferred the move to "the phase
//! that ports their consumers". This is that phase. The policy is the legacy
//! one, rebuilt without `ethers::providers` so the new crates do not inherit a
//! provider type:
//!
//! - last-known-good **affinity**, so consecutive reads do not alternate
//!   between two nodes' views of the chain;
//! - **rotate** on any transport or endpoint fault;
//! - **never rotate a revert** — it is the chain's answer, and every endpoint
//!   replaying the same call gives the same one. The legacy log measured 75% of
//!   its rotations spent on reverts before that rule existed;
//! - bounded exponential **backoff only between full passes**, so one bad
//!   endpoint costs no sleep at all.
//!
//! Four things were added, each closing a failure the legacy client could not
//! see: the read allowlist above; a **per-attempt timeout** (the legacy client
//! had none, so a hung endpoint hung the caller); **chain-id verification** of
//! every endpoint before it answers anything (`wrong_chain_submission` is a
//! hard-zero counter, and a wrong-chain *read* transport feeds everything
//! upstream of a submission); and **id matching**, because an answer carrying a
//! different id is the answer to a different question.
//!
//! The legacy module stays where it is until Phase 17 retires its consumers.
//! Two copies of a policy is a cost; moving it out from under a live binary
//! mid-phase is a bigger one.
//!
//! # INV-46: the credential is the URL
//!
//! `.env` carries provider keys **in the URL path**
//! (`https://base.blockpi.network/v1/rpc/<key>`). So the URL is held as a
//! `Secret`, every log line and error names an endpoint by [`endpoint_label`],
//! and reqwest's errors are stripped of the URL before they become strings —
//! its default `Display` embeds the full request URL, key included.

pub mod failover;

pub use failover::{ConnectError, FailoverSettings, FailoverTransport};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Every method a transport in this crate will send. **Reads only.**
///
/// Adding one is a conscious act and the test
/// `the_allowlist_holds_reads_only` checks the list for the shape of a write.
pub const READ_METHODS: &[&str] = &[
    "eth_chainId",
    "net_version",
    "eth_blockNumber",
    "eth_getBlockByNumber",
    "eth_getBlockByHash",
    "eth_getBalance",
    "eth_getTransactionCount",
    "eth_getCode",
    "eth_getStorageAt",
    "eth_call",
    "eth_simulateV1",
    "eth_estimateGas",
    "eth_gasPrice",
    "eth_maxPriorityFeePerGas",
    "eth_feeHistory",
    "eth_getTransactionByHash",
    "eth_getTransactionReceipt",
    "eth_getLogs",
    // Base's Flashblocks-aware status. `Known` means the preconfirmation node
    // received a transaction -- §21.5, and `base::submit` maps it to
    // `NodeKnown`, never to an inclusion.
    "base_transactionStatus",
];

pub fn is_read(method: &str) -> bool {
    READ_METHODS.contains(&method)
}

/// Why a call produced no answer.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcError {
    /// Not on [`READ_METHODS`]. Refused before any I/O.
    NotARead { method: String },
    /// The call reached the EVM and reverted. **The chain's answer**, carried
    /// with its revert data so a caller can decode it — `Too little received`
    /// and an access-control failure are different findings.
    Reverted { message: String, data: Option<Value> },
    /// No endpoint on the expected chain produced an answer. `last` is the most
    /// recent failure, naming its endpoint by label.
    Exhausted { method: String, endpoints: usize, last: String },
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARead { method } => {
                write!(f, "`{method}` is not a read; this transport cannot send it")
            }
            Self::Reverted { message, .. } => write!(f, "reverted: {message}"),
            Self::Exhausted { method, endpoints, last } => {
                write!(f, "all {endpoints} endpoints failed for `{method}`; last: {last}")
            }
        }
    }
}

impl std::error::Error for RpcError {}

/// One JSON-RPC call, answered or refused.
///
/// `serde_json::Value` in and out, which is `apex-sim`'s seam: request *shape*
/// is built and asserted on as a pure function, and this is the transport that
/// is somebody else's problem.
#[async_trait]
pub trait RpcTransport: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

/// A name for an endpoint that is safe to log.
///
/// Keeps the scheme and host — the part an operator needs to tell endpoints
/// apart — and replaces everything that can carry a credential (path, query,
/// fragment, userinfo) with `***` and a short fingerprint of the whole URL, so
/// two keys on one host stay distinguishable without either being shown.
/// Anything that does not parse as `scheme://…` is `***` outright.
///
/// Semantics match the legacy `util::redact_endpoint`, whose log lines
/// operators already read.
pub fn endpoint_label(endpoint: &str) -> String {
    let trimmed = endpoint.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return "***".to_string();
    };
    let scheme_ok = !scheme.is_empty()
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !scheme_ok {
        return "***".to_string();
    }

    let had_query = rest.contains('?') || rest.contains('#');
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let had_userinfo = authority.contains('@');
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.is_empty() {
        return "***".to_string();
    }

    if had_query || had_userinfo || !path.trim_matches('/').is_empty() {
        let digest = Sha256::digest(trimmed.as_bytes());
        format!("{scheme}://{host}/***#{:02x}{:02x}{:02x}", digest[0], digest[1], digest[2])
    } else {
        format!("{scheme}://{host}")
    }
}

/// `"0x2105"` → `8453`. `None` for anything that is not a hex quantity.
pub fn parse_quantity(v: &Value) -> Option<u64> {
    let s = v.as_str()?.strip_prefix("0x")?;
    if s.is_empty() {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}
