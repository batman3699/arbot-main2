//! The lean executor's plan (Task 8.5 R29): `run(bytes plan)` against
//! `contracts/lean/LeanExecutor.sol`.
//!
//! # The bytes
//!
//! Big-endian, packed, no padding; `contracts/lean/LeanPlanCodec.sol` reads
//! them and `tests/fixtures/lean_plans.json` holds the two to each other.
//!
//! | Offset   | Size | Field |
//! |----------|------|-------|
//! | 0        | 1    | lender: 0 Balancer V2, 1 Morpho Blue |
//! | 1        | 16   | amount: WETH borrowed |
//! | 17       | 5    | deadline: unix seconds |
//! | 22       | 16   | min profit |
//! | 38 + 57i | 1    | hop kind |
//! | +1       | 20   | hop pool |
//! | +21      | 20   | hop token out (in: the previous hop's out, WETH for the first) |
//! | +41      | 16   | hop minimum out: the exact quote; for Aerodrome v2 the amount asked for |
//!
//! # The commitment is off-chain only
//!
//! The operator dropped the on-chain plan hash (2026-10-17). INV-06's signer
//! half stays: [`SignedLeanPlan::check`] recomputes [`lean_commitment`] and
//! refuses a plan that does not hash to what the ticket declared, so a plan
//! cannot change between the planner and the signature.

use alloy_primitives::{address, keccak256, Address, B256};

/// Base's WETH: every loan's token, and every cycle's last.
pub const WETH: Address = address!("4200000000000000000000000000000000000006");

/// The plan version the configuration names for the lean executor.
pub const LEAN_PLAN_VERSION: u32 = 3;

/// `run(bytes)`.
pub const RUN_SELECTOR: [u8; 4] = [0x3a, 0x27, 0x65, 0x23];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeanLender {
    Balancer = 0,
    Morpho = 1,
}

/// How the executor swaps on a pool, by the callback the pool makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HopKind {
    /// Uniswap v3 and both Slipstream factories: `uniswapV3SwapCallback`.
    UniswapV3Style = 0,
    /// PancakeSwap v3: `pancakeV3SwapCallback`.
    PancakeV3 = 1,
    /// Aerodrome v2: paid first, then asked for exactly `min_out`.
    AerodromeV2 = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeanHop {
    pub kind: HopKind,
    pub pool: Address,
    pub token_out: Address,
    pub min_out: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeanPlan {
    pub lender: LeanLender,
    /// WETH borrowed, and the first hop's input.
    pub amount: u128,
    pub deadline: u64,
    pub min_profit: u128,
    pub hops: Vec<LeanHop>,
}

/// A plan the contract would refuse, refused here first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeanError {
    NoHops,
    /// Five bytes hold 2^40 - 1 at most.
    DeadlineTooLarge(u64),
    /// A cycle borrows WETH and must end in it.
    LastHopNotWeth,
    CommitmentMismatch { declared: B256, recomputed: B256 },
}

impl std::fmt::Display for LeanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHops => f.write_str("a plan with no hops"),
            Self::DeadlineTooLarge(d) => write!(f, "deadline {d} does not fit five bytes"),
            Self::LastHopNotWeth => f.write_str("the last hop does not end in WETH"),
            Self::CommitmentMismatch { declared, recomputed } => {
                write!(f, "plan declares {declared} but recomputes to {recomputed}")
            }
        }
    }
}

impl std::error::Error for LeanError {}

/// The plan's bytes.
pub fn encode_plan(plan: &LeanPlan) -> Result<Vec<u8>, LeanError> {
    let last = plan.hops.last().ok_or(LeanError::NoHops)?;
    if last.token_out != WETH {
        return Err(LeanError::LastHopNotWeth);
    }
    if plan.deadline >= 1 << 40 {
        return Err(LeanError::DeadlineTooLarge(plan.deadline));
    }
    let mut out = Vec::with_capacity(38 + 57 * plan.hops.len());
    out.push(plan.lender as u8);
    out.extend_from_slice(&plan.amount.to_be_bytes());
    out.extend_from_slice(&plan.deadline.to_be_bytes()[3..]);
    out.extend_from_slice(&plan.min_profit.to_be_bytes());
    for h in &plan.hops {
        out.push(h.kind as u8);
        out.extend_from_slice(h.pool.as_slice());
        out.extend_from_slice(h.token_out.as_slice());
        out.extend_from_slice(&h.min_out.to_be_bytes());
    }
    Ok(out)
}

/// `run(plan)`'s calldata.
pub fn run_calldata(plan: &LeanPlan) -> Result<Vec<u8>, LeanError> {
    let bytes = encode_plan(plan)?;
    let mut out = Vec::with_capacity(68 + bytes.len().div_ceil(32) * 32);
    out.extend_from_slice(&RUN_SELECTOR);
    out.extend_from_slice(&B256::left_padding_from(&[0x20]).0);
    out.extend_from_slice(&B256::left_padding_from(&(bytes.len() as u64).to_be_bytes()).0);
    out.extend_from_slice(&bytes);
    out.resize(68 + bytes.len().div_ceil(32) * 32, 0);
    Ok(out)
}

/// INV-06's off-chain commitment: `keccak256(abi.encode(chain_id, executor,
/// uint8(LEAN_PLAN_VERSION), keccak256(plan bytes)))`.
pub fn lean_commitment(plan: &LeanPlan, chain_id: u64, executor: Address) -> Result<B256, LeanError> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&B256::left_padding_from(&chain_id.to_be_bytes()).0);
    buf.extend_from_slice(&B256::left_padding_from(executor.as_slice()).0);
    buf.extend_from_slice(&B256::left_padding_from(&[LEAN_PLAN_VERSION as u8]).0);
    buf.extend_from_slice(&keccak256(encode_plan(plan)?).0);
    Ok(keccak256(&buf))
}

/// A lean plan whose commitment was recomputed and matched: unforgeable outside
/// this module, as `SignedPlan` is.
///
/// ```compile_fail
/// use apex_exec::lean::SignedLeanPlan;
/// let forged = SignedLeanPlan { plan: todo!(), commitment: todo!(), executor: todo!(), block_chain_id: todo!(), _sealed: () };
/// ```
#[derive(Debug)]
pub struct SignedLeanPlan {
    plan: LeanPlan,
    commitment: B256,
    executor: Address,
    block_chain_id: u64,
    _sealed: (),
}

impl SignedLeanPlan {
    /// Recompute, compare, and refuse on a mismatch.
    pub fn check(plan: LeanPlan, declared: B256, block_chain_id: u64, executor: Address) -> Result<Self, LeanError> {
        let recomputed = lean_commitment(&plan, block_chain_id, executor)?;
        if recomputed != declared {
            return Err(LeanError::CommitmentMismatch { declared, recomputed });
        }
        Ok(Self { plan, commitment: declared, executor, block_chain_id, _sealed: () })
    }

    pub const fn plan(&self) -> &LeanPlan {
        &self.plan
    }

    pub const fn commitment(&self) -> B256 {
        self.commitment
    }

    pub const fn executor(&self) -> Address {
        self.executor
    }

    pub const fn block_chain_id(&self) -> u64 {
        self.block_chain_id
    }
}
