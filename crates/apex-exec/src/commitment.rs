//! `PlanV2` and the commitment the executor recomputes (§25, INV-06).
//!
//! # A Rust mirror of a Solidity hash, and why it is written out by hand
//!
//! Every field in `planCommitment` is a static ABI type, so `abi.encode` is
//! plain 32-byte word concatenation with no offsets or length prefixes. Writing
//! that out is a dozen lines and it is legible against the Solidity beside it.
//! Reaching for a full ABI encoder would add a dependency whose correctness is
//! then the thing under test, and would hide the one property that matters —
//! **that the field order and padding here are the field order and padding
//! there** — inside a macro.
//!
//! The risk of a hand-written mirror is that it drifts. That is what
//! `tests/plan_commitment.rs` is for: the expected hashes are produced **by the
//! contract**, through `test/PlanCommitmentFixture.t.sol`, and both sides assert
//! against the same tracked file. A change to either half turns one of them red.

use alloy_primitives::{keccak256, Address, B256, U256};
use serde::{Deserialize, Serialize};

/// `MultiVenueArbImplementation.PLAN_VERSION_V2`.
pub const PLAN_VERSION_V2: u8 = 2;

/// The executor's `Op`, in its declared order — the discriminant is what gets
/// hashed, so the order is load-bearing.
///
/// §1.4 excludes JIT liquidity and cross-chain bridging, and the contract's
/// comment records that `BRIDGE`, `JIT_LP_ADD` and `JIT_LP_REMOVE` were the last
/// three variants and were removed: an old plan naming one now decodes as an
/// out-of-range `Op` and reverts. Mirroring the same three variants keeps that
/// true from this side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Op {
    UniV3 = 0,
    Balancer = 1,
    Generic = 2,
}

/// The executor's `LoanProvider`, in its declared order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LoanProvider {
    Balancer = 0,
    Aave = 1,
    Erc3156 = 2,
    UniV2 = 3,
    UniV3 = 4,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub op: Op,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loan {
    pub token: Address,
    pub amount: U256,
    pub provider: LoanProvider,
    pub provider_addr: Address,
}

/// The executor's `PlanV2`, minus `commitment` — which is the output rather than
/// an input, and including it would let a caller hash a plan against its own
/// declared value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanV2 {
    pub loans: Vec<Loan>,
    pub cycle_slippage_bps: u16,
    pub steps: Vec<Step>,
    pub min_profit: U256,
    pub declared_residue: U256,
    /// INV-30. Zero means unstated, which the contract accepts.
    pub chain_id: u64,
    /// INV-31, unix seconds. Zero means no expiry.
    pub deadline: u64,
}

/// A 32-byte big-endian word, the unit `abi.encode` works in for static types.
fn word(bytes: [u8; 32]) -> [u8; 32] {
    bytes
}

fn u256_word(v: U256) -> [u8; 32] {
    v.to_be_bytes()
}

fn u64_word(v: u64) -> [u8; 32] {
    u256_word(U256::from(v))
}

fn u16_word(v: u16) -> [u8; 32] {
    u256_word(U256::from(v))
}

fn u8_word(v: u8) -> [u8; 32] {
    u256_word(U256::from(v))
}

/// `address` is left-padded to 32 bytes, which is why it cannot simply be
/// appended: the twelve zero bytes in front are part of the preimage.
fn address_word(a: Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(a.as_slice());
    out
}

/// The rolling loans hash: `keccak256(abi.encode(acc, token, amount, uint8(provider), providerAddr))`.
///
/// Rolling rather than a hash over the concatenation, and the difference
/// matters: a rolling hash makes each element's position part of its own
/// preimage, so two loan lists cannot be rearranged into the same digest.
pub fn loans_hash(loans: &[Loan]) -> B256 {
    let mut acc = B256::ZERO;
    for l in loans {
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(&word(acc.0));
        buf.extend_from_slice(&address_word(l.token));
        buf.extend_from_slice(&u256_word(l.amount));
        buf.extend_from_slice(&u8_word(l.provider as u8));
        buf.extend_from_slice(&address_word(l.provider_addr));
        acc = keccak256(&buf);
    }
    acc
}

/// The rolling steps hash: `keccak256(abi.encode(acc, uint8(op), keccak256(data)))`.
///
/// The step's `data` is **hashed** rather than concatenated, which the contract
/// comments on directly: *"a long payload cannot be split across a boundary to
/// collide with a different step list."* Concatenating variable-length payloads
/// is the classic length-extension shape, and hashing each one first removes the
/// boundary entirely.
pub fn steps_hash(steps: &[Step]) -> B256 {
    let mut acc = B256::ZERO;
    for s in steps {
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(&word(acc.0));
        buf.extend_from_slice(&u8_word(s.op as u8));
        buf.extend_from_slice(&keccak256(&s.data).0);
        acc = keccak256(&buf);
    }
    acc
}

/// **INV-06.** The value the executor recomputes and reverts on.
///
/// `block_chain_id` and `executor` are the chain and address the plan will
/// execute at — `block.chainid` and `address(this)` on the other side. They are
/// arguments rather than fields of [`PlanV2`] because they are facts about the
/// *deployment*, not about the plan: a plan committed for one deployment must
/// not execute on another, which is INV-05's wrong-chain submission expressed
/// where it can be enforced.
///
/// Note that `PlanV2::chain_id` is a **separate** field and is also hashed. The
/// contract carries both deliberately: `block.chainid` is where the code is
/// running, and `chainId` is what the planner said it built for. Checking one
/// against the other is `_initiateLoanV2`'s `WrongChain`, and hashing both means
/// a plan cannot be re-aimed without the commitment noticing.
pub fn plan_commitment(plan: &PlanV2, block_chain_id: u64, executor: Address) -> B256 {
    let mut buf = Vec::with_capacity(320);
    buf.extend_from_slice(&u64_word(block_chain_id));
    buf.extend_from_slice(&address_word(executor));
    buf.extend_from_slice(&u8_word(PLAN_VERSION_V2));
    buf.extend_from_slice(&loans_hash(&plan.loans).0);
    buf.extend_from_slice(&u16_word(plan.cycle_slippage_bps));
    buf.extend_from_slice(&steps_hash(&plan.steps).0);
    buf.extend_from_slice(&u256_word(plan.min_profit));
    buf.extend_from_slice(&u256_word(plan.declared_residue));
    buf.extend_from_slice(&u64_word(plan.chain_id));
    buf.extend_from_slice(&u64_word(plan.deadline));
    keccak256(&buf)
}
