//! Simulation backends, and the line between building a request and sending
//! one.
//!
//! # Why request construction is separate from transport
//!
//! Every backend here is defined as a **pure request builder** plus a
//! transport that is someone else's problem. That is not tidiness: it is what
//! makes the request *shape* testable without a node, and the shape is where
//! the capture-critical decisions live. `validation: true`, an explicit block
//! number instead of `pending`, state overrides that pin what was simulated
//! against — each of those is a correctness property, and each would otherwise
//! only be checkable against a live endpoint this environment cannot reach.
//!
//! A request builder that returns `serde_json::Value` can be asserted on
//! exactly. A backend that builds and sends in one `async fn` can only be
//! tested by mocking the transport, which tests the mock.

pub mod base_simulate_v1;
pub mod eth_call;

use apex_types::ids::ChainId;

/// Which backend answered, recorded on every result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// `eth_simulateV1` — explicit block and state context, validation on.
    EthSimulateV1,
    /// `eth_call` — the fallback, and the quorum verifier.
    EthCall,
    /// Local REVM against cached state (Tier 1).
    LocalRevm,
}

impl BackendKind {
    /// Whether this backend may answer a capture-critical question.
    ///
    /// §24.6: Base documents that `eth_call` against `pending` may return a
    /// cached block context, so an `eth_call` result cannot be the thing a
    /// live dispatch rests on. It remains the fallback and the quorum
    /// verifier — a second opinion from a weaker instrument is still worth
    /// having, as long as nobody mistakes it for the instrument.
    pub const fn is_capture_critical(self) -> bool {
        matches!(self, Self::EthSimulateV1)
    }
}

/// Which backend a chain's capture-critical path uses.
///
/// Per chain because the answer genuinely differs: `eth_simulateV1` is an OP
/// Stack and post-Cancun Geth facility, and a chain without it has to fall
/// back rather than pretend.
pub const fn capture_critical_backend(chain: ChainId) -> BackendKind {
    match chain.0 {
        // Base. §24.6 makes this the capture-critical backend.
        8453 => BackendKind::EthSimulateV1,
        // Ethereum mainnet: available, and Phase 14's concern.
        1 => BackendKind::EthSimulateV1,
        // Anything else has not been checked, and the honest answer for an
        // unchecked chain is the weaker backend rather than an assumption.
        _ => BackendKind::EthCall,
    }
}

/// One call inside a simulated block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimCall {
    pub from: String,
    pub to: String,
    pub data: String,
    /// Hex-quantity. `None` omits the field, which lets the node choose.
    pub value: Option<String>,
    pub gas: Option<String>,
}

/// The state a simulation runs against, pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimContext {
    /// The block to simulate on top of, as a hex quantity. **Never a tag.**
    /// §24.6: a simulation whose block is `pending` answers a question about
    /// whichever block the node felt like, which is a different question from
    /// the one that was asked.
    pub block_number_hex: String,
    /// Account overrides applied before the calls run: balances, nonces, code.
    pub state_overrides: serde_json::Value,
    /// Block-level overrides — base fee, timestamp — for simulating the block
    /// the trade would actually land in rather than the last one.
    pub block_overrides: serde_json::Value,
}
