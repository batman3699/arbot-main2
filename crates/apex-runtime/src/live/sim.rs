//! Tier 2 simulation (Task 8.5 R7): **the call**, as the lane that will sign
//! it, against the chain's latest state, by `eth_simulateV1`.
//!
//! # A success that is not a `startV2` is a failure
//!
//! Until the Phase 5 executor is deployed at the committed address, a call to
//! that address is a call to an account with no code — and the EVM runs that
//! successfully and returns nothing. Recorded on a fork: status 1, empty return
//! data. `startV2` returns its gross profit, one word, so a success that returns
//! anything else is refused here as a failure, rather than reported as the one
//! thing it is not. Until the deploy lands, every simulation is filed this way —
//! as a failure with a reason, never hidden.
//!
//! # What a revert is
//!
//! A step's revert bubbles up through the executor, the flash loan and the
//! router, so the data is the innermost cause. Classified by what it is, and
//! nothing a class does not establish:
//!
//! | Revert data | Class |
//! |---|---|
//! | `DebtNotRepaid` | `FlashRepaymentShortfall` |
//! | `UnaccountedResidue`, `ProfitTokenNotBorrowed` | `ProfitInvariantViolated` |
//! | `RouteExpired`, a router's `Transaction too old` | `Expired` |
//! | a router's `Too little received` | `MinOutNotMet` |
//! | `NotExecutor`, `SelectorNotAllowed`, `UnknownAdapter` | `Unauthorized` — a deployment that does not know the lane or the adapter |
//! | Balancer's `BAL#528` | `InsufficientLiquidity` — the vault cannot lend it |
//! | `STF`, `ST`, `SA`, `TF` (Uniswap's transfer helpers) | `TokenTransferFailed` |
//! | an `out of gas` failure | `OutOfGas` |
//! | anything else, an empty revert included | `Unknown` |
//!
//! Every class above was either recorded on the fork
//! (`tests/fixtures/simulate_v1_fork.json`) or is a string or selector checked
//! against its signature; an empty revert is `Unknown` because on this path it
//! could be the profit floor or a bare `require` in any token.
//!
//! # What the result says, and from what
//!
//! Balance deltas are the executor's, from the transfers the simulation traced
//! (`traceTransfers`); the residues are what it is left holding. The executor
//! reverts unless the loan is repaid and the settlement invariant holds, so a
//! success is both, and a failure neither.

use crate::plane::{Decline, Simulator};
use alloy_primitives::{b256, hex, keccak256, Address, B256};
use apex_chain::rpc::RpcTransport;
use apex_exec::call::ExecutorCall;
use apex_types::candidate::Candidate;
use apex_types::ids::{ChainId, TokenId};
use apex_types::sim::{RevertClass, SimulationResult, SimulationTier};
use apex_types::state::StateFingerprint;
use apex_types::time::DurationNanos;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// `Transfer(address,address,uint256)`. `traceTransfers` reports native value
/// under the same topic, from the pseudo-address `0xEeee…EEeE`, which the deltas
/// count as a token like any other.
pub const TRANSFER: B256 = b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// The executor's errors, and the ABI's own, each `keccak256(signature)[..4]`.
pub mod revert {
    /// `DebtNotRepaid(address,uint256,uint256)`
    pub const DEBT_NOT_REPAID: [u8; 4] = [0xda, 0x63, 0x42, 0xc6];
    /// `UnaccountedResidue(address,uint256,uint256)`
    pub const UNACCOUNTED_RESIDUE: [u8; 4] = [0x67, 0xa0, 0x73, 0x26];
    /// `ProfitTokenNotBorrowed(address)`
    pub const PROFIT_TOKEN_NOT_BORROWED: [u8; 4] = [0x94, 0xed, 0x3b, 0xa3];
    /// `RouteExpired(uint64,uint256)`
    pub const ROUTE_EXPIRED: [u8; 4] = [0xae, 0x8d, 0x15, 0x76];
    /// `NotExecutor()`
    pub const NOT_EXECUTOR: [u8; 4] = [0xc3, 0x2d, 0x1d, 0x76];
    /// `SelectorNotAllowed(uint16,bytes4)`
    pub const SELECTOR_NOT_ALLOWED: [u8; 4] = [0x15, 0x3a, 0x89, 0xc1];
    /// `UnknownAdapter(uint16)`
    pub const UNKNOWN_ADAPTER: [u8; 4] = [0x07, 0x69, 0xba, 0xab];
    /// `Error(string)`
    pub const ERROR_STRING: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
}

/// The class of a revert, from its data and — for failures that carry none,
/// like running out of gas — the node's message.
pub fn classify(data: &[u8], message: Option<&str>) -> RevertClass {
    use revert::*;
    let Some(selector) = data.get(..4) else {
        return if message.is_some_and(|m| m.to_ascii_lowercase().contains("out of gas")) {
            RevertClass::OutOfGas
        } else {
            RevertClass::Unknown
        };
    };
    match <[u8; 4]>::try_from(selector).unwrap_or_default() {
        DEBT_NOT_REPAID => RevertClass::FlashRepaymentShortfall,
        UNACCOUNTED_RESIDUE | PROFIT_TOKEN_NOT_BORROWED => RevertClass::ProfitInvariantViolated,
        ROUTE_EXPIRED => RevertClass::Expired,
        NOT_EXECUTOR | SELECTOR_NOT_ALLOWED | UNKNOWN_ADAPTER => RevertClass::Unauthorized,
        ERROR_STRING => match reason(data).as_deref() {
            Some("Too little received") => RevertClass::MinOutNotMet,
            Some("Transaction too old") => RevertClass::Expired,
            Some("BAL#528") => RevertClass::InsufficientLiquidity,
            Some("STF" | "ST" | "SA" | "TF") => RevertClass::TokenTransferFailed,
            _ => RevertClass::Unknown,
        },
        _ => RevertClass::Unknown,
    }
}

/// The string of an `Error(string)` revert.
fn reason(data: &[u8]) -> Option<String> {
    let body = data.get(4..)?;
    let len = usize::try_from(alloy_primitives::U256::from_be_slice(body.get(32..64)?)).ok()?;
    String::from_utf8(body.get(64..64usize.checked_add(len)?)?.to_vec()).ok()
}

fn quantity(v: &Value) -> Option<u64> {
    u64::from_str_radix(v.as_str()?.strip_prefix("0x")?, 16).ok()
}

fn hash(v: &Value) -> Option<B256> {
    v.as_str()?.parse().ok()
}

/// The executor's net transfers, by token, from the traced logs. `None` for a
/// transfer log that does not decode — a delta built past one would be a
/// delta of the transfers that happened to parse.
fn deltas(chain: ChainId, executor: Address, logs: &[Value]) -> Option<BTreeMap<TokenId, i128>> {
    let mut out: BTreeMap<TokenId, i128> = BTreeMap::new();
    for l in logs {
        // ERC-20's shape exactly: the topic and two parties. ERC-721 shares the
        // topic with the token id as a fourth, and a malformed log may have
        // fewer; neither is a balance, and neither can be indexed into.
        let [topic, from, to] = &l.get("topics")?.as_array()?[..] else { continue };
        if hash(topic) != Some(TRANSFER) {
            continue;
        }
        let party = |t: &Value| hash(t).map(|h| Address::from_slice(&h[12..]));
        let (from, to) = (party(from)?, party(to)?);
        let amount = hex::decode(l.get("data")?.as_str()?).ok()?;
        let amount = i128::try_from(alloy_primitives::U256::try_from_be_slice(&amount)?).ok()?;
        let token = TokenId { chain, address: l.get("address")?.as_str()?.parse().ok()? };
        let d = out.entry(token).or_insert(0);
        if to == executor {
            *d = d.checked_add(amount)?;
        }
        if from == executor {
            *d = d.checked_sub(amount)?;
        }
    }
    out.retain(|_, d| *d != 0);
    Some(out)
}

/// A `SimulationResult` from one `eth_simulateV1` answer for one call. Pure, so
/// the answers recorded on the fork test it; `None` for an answer that is not
/// one simulated block with one call.
pub fn read_simulation(
    chain: ChainId,
    executor: Address,
    answer: &Value,
    elapsed: DurationNanos,
) -> Option<SimulationResult> {
    let block = answer.get(0)?;
    let [call] = &block.get("calls")?.as_array()?[..] else { return None };
    let number = quantity(block.get("number")?)?;
    let (hash_now, parent) = (hash(block.get("hash")?)?, hash(block.get("parentHash")?)?);
    let data = hex::decode(call.get("returnData")?.as_str()?).ok()?;
    let status = quantity(call.get("status")?)?;

    // One word is a `startV2`; an empty success is an account with no code.
    let success = status == 1 && data.len() == 32;
    let revert = (!success).then(|| {
        let message = call.get("error").and_then(|e| e.get("message")).and_then(Value::as_str);
        let class = if status == 1 { RevertClass::Unknown } else { classify(&data, message) };
        (class, data.clone())
    });
    let balance_deltas = if success {
        deltas(chain, executor, call.get("logs")?.as_array()?)?
    } else {
        BTreeMap::new()
    };
    let token_residues = balance_deltas
        .iter()
        .filter_map(|(t, d)| u128::try_from(*d).ok().map(|d| (*t, d)))
        .collect();
    let fingerprint = |confirmed: u64, block_hash: B256, root: Option<B256>| StateFingerprint {
        chain_id: chain,
        parent_block_hash: parent,
        confirmed_block_number: confirmed,
        preconf_sequence: None,
        flashblock_index: None,
        state_root_or_equivalent: root,
        block_hash_if_available: Some(block_hash),
        state_delta_hash: keccak256(block_hash),
        venue_state_version: BTreeMap::new(),
        external_dependency_fingerprint: None,
    };
    let mut r = SimulationResult {
        tier: SimulationTier::Tier2FullEvm,
        success,
        revert,
        gas_used: quantity(call.get("gasUsed")?)?,
        balance_deltas,
        loan_repaid: success,
        profit_invariant_held: success,
        token_residues,
        state_after: fingerprint(number, hash_now, block.get("stateRoot").and_then(hash)),
        simulated_at_state: fingerprint(number.saturating_sub(1), parent, None),
        result_hash: B256::ZERO,
        elapsed,
    };
    r.result_hash = r.canonical_hash();
    Some(r)
}

/// `eth_simulateV1` over the read-only transport.
pub struct LiveSimulator {
    rpc: Arc<dyn RpcTransport>,
    chain: ChainId,
}

impl LiveSimulator {
    pub fn new(rpc: Arc<dyn RpcTransport>, chain: ChainId) -> Self {
        Self { rpc, chain }
    }
}

#[async_trait::async_trait]
impl Simulator for LiveSimulator {
    /// One block, one call: from the signing lane, to the committed executor,
    /// with the candidate's gas limit — a call that needs more fails here as it
    /// would on chain. `validation: false`: nonce and balance are last-mile's to
    /// check; this answers what the call does.
    async fn simulate(
        &self,
        c: &Candidate,
        call: &ExecutorCall,
        from: Address,
    ) -> Result<SimulationResult, Decline> {
        let started = std::time::Instant::now();
        let params = json!([{
            "blockStateCalls": [{ "calls": [{
                "from": from,
                "to": call.to(),
                "data": format!("0x{}", hex::encode(call.data())),
                "gas": format!("{:#x}", c.total_execution_cost.gas_limit.0),
            }]}],
            "traceTransfers": true,
            "validation": false,
        }, "latest"]);
        // No outcome, so no class: the simulation did not run.
        let answer = self
            .rpc
            .call("eth_simulateV1", params)
            .await
            .map_err(|_| Decline::SimulationFailed { class: None })?;
        let elapsed = DurationNanos(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        read_simulation(self.chain, call.to(), &answer, elapsed).ok_or(Decline::SimulationFailed { class: None })
    }
}
