//! Tier 2 simulation (Task 8.5 R7): **the call**, as the lane that will sign
//! it, inside the block being built, by `eth_simulateV1`.
//!
//! # Inside the block being built (R16)
//!
//! The shadow prices against preconfirmed state — the in-progress block through
//! its latest flashblock — and a trade lands in that block or the next. Until
//! 2026-10-05 the simulation ran on the last sealed block instead, a different
//! state: a gap that a swap opened inside the block was not in it, and R11's
//! first candidate, priced on such a gap, reverted. So the base is `pending`,
//! whose state BlockPI keeps current — recorded: the sequencer fee vault, which
//! every transaction pays, reads there exactly as `eth_getBalance` at `pending`
//! does, ahead of `latest`. Its block context under `pending` is not current:
//! it lagged by two to four blocks — the blueprint's warning (§15.2). So the
//! block being built is named explicitly — its number, timestamp and base fee.
//! BlockPI reaches the requested number by inserting empty blocks, so the call
//! is in the answer's last block; an answer whose last block is not the one
//! asked for ran somewhere else, and is refused.
//!
//! # The block being built comes from the feed (R18)
//!
//! It was read with `eth_getBlockByNumber("pending")` before every simulation:
//! 207–311 ms on BlockPI, as long as the simulation's own round trip (215–400
//! ms, measured 2026-10-06), and the first real gap the shadow priced lasted
//! one flashblock, 200 ms. The capture feed already knows it — the newest
//! sealed head, and the newest block a preconfirmed log came from
//! ([`BlockContext::being_built`]) — so the shadow publishes it as the feed
//! learns it, and a simulation reads it and carries it on to the block being
//! built when it runs ([`BlockContext::at`]): a block is sealed when its time
//! comes, whether or not the feed has heard. Against BlockPI's own pending
//! header read at the same moment, that named the same block 84 times in 94
//! and the next one 10 times (BlockPI moves on 0.2–0.4 s after the boundary),
//! never the one before; the simulation took 167–614 ms, 234 at the median.
//! Before the feed has seen a head there is nothing to read, and nothing is
//! simulated.
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
//! could be the profit floor, a bare `require` in any token, PancakeSwap's
//! minimum check (its `SmartRouter`'s is bare too), or a swap that ran out of
//! gas inside the executor's call — whose frame then reverts empty, and the
//! node says "execution reverted", never "out of gas" (all recorded on
//! BlockPI, `tests/fixtures/simulate_v1_blockpi.json`, R12).
//!
//! The bytes are wherever the node puts them: anvil returns them as
//! `returnData`, BlockPI as `error.data` with `returnData` empty.
//!
//! # What the result says, and from what
//!
//! Balance deltas are the executor's, from the transfers the simulation traced
//! (`traceTransfers`); the residues are what it is left holding. The executor
//! reverts unless the loan is repaid and the settlement invariant holds, so a
//! success is both, and a failure neither.

use crate::plane::{Decline, Simulator};
use apex_capture::clock::Clock;
use alloy_primitives::{b256, hex, keccak256, Address, B256};
use apex_chain::rpc::ws::Head;
use apex_chain::rpc::RpcTransport;
use apex_exec::call::ExecutorCall;
use apex_types::candidate::Candidate;
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, TokenId};
use apex_types::sim::{RevertClass, SimulationResult, SimulationTier};
use apex_types::state::StateFingerprint;
use apex_types::time::{DurationNanos, UnixNanos};
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
    // The last block: any before it are the empty ones a node inserts to reach
    // the block number asked for.
    let block = answer.as_array()?.last()?;
    let [call] = &block.get("calls")?.as_array()?[..] else { return None };
    let number = quantity(block.get("number")?)?;
    let (hash_now, parent) = (hash(block.get("hash")?)?, hash(block.get("parentHash")?)?);
    let data = hex::decode(call.get("returnData")?.as_str()?).ok()?;
    let status = quantity(call.get("status")?)?;
    let error = call.get("error");
    // A revert's bytes: anvil returns them as `returnData`; BlockPI leaves that
    // empty and returns them as `error.data` (recorded,
    // `tests/fixtures/simulate_v1_blockpi.json`). Bytes that do not decode are
    // a malformed answer, refused whole.
    let error_data = match error.and_then(|e| e.get("data")) {
        Some(d) => hex::decode(d.as_str()?).ok()?,
        None => Vec::new(),
    };

    // One word is a `startV2`; an empty success is an account with no code.
    let success = status == 1 && data.len() == 32;
    let revert = (!success).then(|| {
        let message = error.and_then(|e| e.get("message")).and_then(Value::as_str);
        if status == 1 {
            // Not a revert, so its bytes are not read as one's.
            return (RevertClass::Unknown, data.clone());
        }
        let bytes = if data.is_empty() { error_data } else { data.clone() };
        (classify(&bytes, message), bytes)
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

/// The block a simulation runs in: the one being built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockContext {
    pub number: u64,
    pub timestamp: u64,
    pub base_fee: u128,
}

/// Base seals a block every two seconds: the pending header's timestamp was the
/// latest's plus two per block in every sample (2026-10-06).
pub const BLOCK_TIME_S: u64 = 2;

impl BlockContext {
    /// The block being built, from what the capture feed has seen: one past
    /// the newest sealed `head`, or the newest block a preconfirmed log came
    /// from when the head lags behind the flashblocks. Its timestamp is the
    /// head's plus two seconds a block.
    ///
    /// The base fee is the head's. The next block's is at most a hundredth
    /// away — Base's EIP-1559 denominator is 100, and at its 0.005 gwei floor
    /// the two are equal — and nothing the simulated call runs reads it: the
    /// executor, the routers and the pools never use `BASEFEE`, and
    /// `validation: false` charges no gas. `None` for a head without one.
    /// The block being built at `now`. A block is built in the two seconds
    /// before its timestamp, so once `now` reaches it the block is sealed and
    /// the one being built is the next — or later, two seconds a block.
    /// BlockPI's pending block moved on 0.2–0.4 s after each boundary and the
    /// feed's head later still: without this, one simulation in five named the
    /// block before, and two of 94 were refused (2026-10-06).
    pub fn at(self, now: UnixNanos) -> Self {
        let sealed_at = self.timestamp.saturating_mul(1_000_000_000);
        if now.0 < sealed_at {
            return self;
        }
        let blocks = 1 + (now.0 - sealed_at) / BLOCK_TIME_S.saturating_mul(1_000_000_000);
        Self {
            number: self.number.saturating_add(blocks),
            timestamp: self.timestamp.saturating_add(BLOCK_TIME_S.saturating_mul(blocks)),
            ..self
        }
    }

    pub fn being_built(head: &Head, newest_preconfirmed: Option<u64>) -> Option<Self> {
        let number = head.number.saturating_add(1).max(newest_preconfirmed.unwrap_or(0));
        Some(Self {
            number,
            timestamp: head.timestamp.saturating_add(BLOCK_TIME_S.saturating_mul(number - head.number)),
            base_fee: head.base_fee_per_gas?,
        })
    }

}

/// The `eth_simulateV1` request: one call, from the signing lane, at the gas
/// limit it will be signed with, in the block `at` — on `pending`'s state, with
/// the block's context set explicitly rather than taken from `pending`'s.
pub fn simulate_request(call: &ExecutorCall, from: Address, gas_limit: GasLimit, at: BlockContext) -> Value {
    json!([{
        "blockStateCalls": [{
            "blockOverrides": {
                "number": format!("{:#x}", at.number),
                "time": format!("{:#x}", at.timestamp),
                "baseFeePerGas": format!("{:#x}", at.base_fee),
            },
            "calls": [{
                "from": from,
                "to": call.to(),
                "data": format!("0x{}", hex::encode(call.data())),
                "gas": format!("{:#x}", gas_limit.0),
            }],
        }],
        "traceTransfers": true,
        "validation": false,
    }, "pending"])
}

/// `eth_simulateV1` over the read-only transport.
pub struct LiveSimulator {
    rpc: Arc<dyn RpcTransport>,
    chain: ChainId,
    /// The block being built, as the capture feed has seen it (R18).
    building: tokio::sync::watch::Receiver<Option<BlockContext>>,
    /// To carry it on to the block being built when the simulation runs.
    clock: Arc<dyn Clock>,
}

impl LiveSimulator {
    pub fn new(
        rpc: Arc<dyn RpcTransport>,
        chain: ChainId,
        building: tokio::sync::watch::Receiver<Option<BlockContext>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { rpc, chain, building, clock }
    }
}

#[async_trait::async_trait]
impl Simulator for LiveSimulator {
    /// One call, inside the block being built: from the signing lane, to the
    /// committed executor, with the gas limit it will be signed with — a call
    /// that needs more fails here as it would on chain. `validation: false`:
    /// nonce and balance are last-mile's to check; this answers what the call
    /// does. A simulation that did not run, or ran in another block, has no
    /// outcome and so no class.
    async fn simulate(
        &self,
        _c: &Candidate,
        call: &ExecutorCall,
        from: Address,
        gas_limit: GasLimit,
    ) -> Result<SimulationResult, Decline> {
        let started = std::time::Instant::now();
        let unrun = || Decline::SimulationFailed { class: None };
        let at = (*self.building.borrow()).ok_or_else(unrun)?.at(self.clock.now());
        let answer =
            self.rpc.call("eth_simulateV1", simulate_request(call, from, gas_limit, at)).await.map_err(|_| unrun())?;
        let elapsed = DurationNanos(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        let r = read_simulation(self.chain, call.to(), &answer, elapsed).ok_or_else(unrun)?;
        if r.state_after.confirmed_block_number != at.number {
            return Err(unrun());
        }
        Ok(r)
    }
}
