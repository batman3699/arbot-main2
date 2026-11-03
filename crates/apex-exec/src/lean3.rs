//! The third lean executor's plan (Task 8.5 R40.1): `run(bytes plan)` against
//! `contracts/lean/LeanExecutor3.sol`, which sizes the trade itself at
//! execution. So the plan names no size and no hop minimums: it carries a cap,
//! and what the executor needs to price its own floor.
//!
//! # The bytes
//!
//! Big-endian, packed, two hops, always 162 bytes.
//! `contracts/lean/LeanPlan3Codec.sol` reads them, and
//! `tests/fixtures/lean3_plans.json` holds the two to each other.
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 1    | lender: 0 Balancer V2, 1 Morpho Blue, 2 Uniswap v4's PoolManager |
//! | 1      | 1    | start: 0 WETH, 1 USDC |
//! | 2      | 16   | cap, in the start token |
//! | 18     | 5    | deadline: unix seconds |
//! | 23     | 16   | rate: start units per 1e18 wei, rounded up; 0 for WETH |
//! | 39     | 4    | gasOverhead: intrinsic, calldata and the work after the floor |
//! | 43     | 4    | swapGas: both hops and the loan, as the gas model expects |
//! | 47     | 8    | l1Bound, wei |
//! | 55     | 8    | minNet, in the start token |
//! | 63     | 1    | maxSteps: 1 to 16 |
//! | 64 + 49i | 1  | hop kind: 0 Uniswap v3, 1 PancakeSwap v3, 2 Aerodrome v2, 3 Uniswap v4, 4 Slipstream |
//! | +1     | 20   | hop pool; for kind 3 the pool key's hooks |
//! | +21    | 20   | hop token out (in: the start for hop 0, else hop 0's out) |
//! | +41    | 3    | fee, ppm: the pool's for kinds 0 and 1, the key's for 3; read at execution for 2 and 4 |
//! | +44    | 3    | tick spacing, two's complement; unused for kind 2 |
//! | +47    | 2    | crossGas: one tick crossed on this hop |

use crate::lean::RUN_SELECTOR;
use crate::lean2::{Lean2Lender, StartToken};
use alloy_primitives::{keccak256, Address, B256};

/// Committed to off-chain, beside the plan's bytes. Plan 2's is 4.
pub const LEAN3_PLAN_VERSION: u32 = 5;
pub const LEAN3_PLAN_LEN: usize = 162;
/// The most boundaries a plan may let the executor walk.
pub const MAX_STEPS_LIMIT: u8 = 16;

/// How the executor reads and swaps on a pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hop3Kind {
    /// `uniswapV3SwapCallback`, an immutable fee.
    UniswapV3 = 0,
    /// `pancakeV3SwapCallback`, an immutable fee.
    PancakeV3 = 1,
    /// Paid first, then asked for exactly its own `getAmountOut`.
    AerodromeV2 = 2,
    /// `PoolManager.swap` inside the trade's unlock.
    UniswapV4 = 3,
    /// Both Slipstream factories: `uniswapV3SwapCallback`, its fee read with
    /// `fee()` at execution.
    Slipstream = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lean3Hop {
    pub kind: Hop3Kind,
    /// For kind 3, the pool key's hooks.
    pub pool: Address,
    pub token_out: Address,
    /// 24 bits.
    pub fee: u32,
    /// 24 bits signed.
    pub tick_spacing: i32,
    pub cross_gas: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lean3Plan {
    pub lender: Lean2Lender,
    pub start: StartToken,
    pub cap: u128,
    pub deadline: u64,
    pub rate: u128,
    pub gas_overhead: u32,
    pub swap_gas: u32,
    pub l1_bound: u64,
    pub min_net: u64,
    pub max_steps: u8,
    pub hops: [Lean3Hop; 2],
}

/// A plan the contract would refuse, refused here first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lean3Error {
    /// Five bytes hold 2^40 - 1 at most.
    DeadlineTooLarge(u64),
    /// A cycle borrows its start token and must end in it.
    LastHopNotStart,
    /// A v4 hop runs only inside PoolManager's unlock.
    V4NeedsPoolManager,
    /// A fee past 24 bits, or a tick spacing past `int24`.
    HopOutOfRange { hop: usize },
    /// Between 1 and [`MAX_STEPS_LIMIT`].
    MaxSteps(u8),
    CommitmentMismatch { declared: B256, recomputed: B256 },
}

impl std::fmt::Display for Lean3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadlineTooLarge(d) => write!(f, "deadline {d} does not fit five bytes"),
            Self::LastHopNotStart => f.write_str("the last hop does not end in the start token"),
            Self::V4NeedsPoolManager => f.write_str("a v4 hop needs PoolManager as the lender"),
            Self::HopOutOfRange { hop } => write!(f, "hop {hop}'s fee or tick spacing does not fit its bytes"),
            Self::MaxSteps(n) => write!(f, "maxSteps {n} is not between 1 and {MAX_STEPS_LIMIT}"),
            Self::CommitmentMismatch { declared, recomputed } => {
                write!(f, "plan declares {declared} but recomputes to {recomputed}")
            }
        }
    }
}

impl std::error::Error for Lean3Error {}

/// The plan's bytes.
pub fn encode_plan3(plan: &Lean3Plan) -> Result<Vec<u8>, Lean3Error> {
    if plan.hops[1].token_out != plan.start.address() {
        return Err(Lean3Error::LastHopNotStart);
    }
    if plan.deadline >= 1 << 40 {
        return Err(Lean3Error::DeadlineTooLarge(plan.deadline));
    }
    if plan.max_steps == 0 || plan.max_steps > MAX_STEPS_LIMIT {
        return Err(Lean3Error::MaxSteps(plan.max_steps));
    }
    let mut out = Vec::with_capacity(LEAN3_PLAN_LEN);
    out.push(plan.lender as u8);
    out.push(plan.start as u8);
    out.extend_from_slice(&plan.cap.to_be_bytes());
    out.extend_from_slice(&plan.deadline.to_be_bytes()[3..]);
    out.extend_from_slice(&plan.rate.to_be_bytes());
    out.extend_from_slice(&plan.gas_overhead.to_be_bytes());
    out.extend_from_slice(&plan.swap_gas.to_be_bytes());
    out.extend_from_slice(&plan.l1_bound.to_be_bytes());
    out.extend_from_slice(&plan.min_net.to_be_bytes());
    out.push(plan.max_steps);
    for (i, h) in plan.hops.iter().enumerate() {
        if h.kind == Hop3Kind::UniswapV4 && plan.lender != Lean2Lender::PoolManager {
            return Err(Lean3Error::V4NeedsPoolManager);
        }
        if h.fee >= 1 << 24 || !(-(1 << 23)..1 << 23).contains(&h.tick_spacing) {
            return Err(Lean3Error::HopOutOfRange { hop: i });
        }
        out.push(h.kind as u8);
        out.extend_from_slice(h.pool.as_slice());
        out.extend_from_slice(h.token_out.as_slice());
        out.extend_from_slice(&h.fee.to_be_bytes()[1..]);
        out.extend_from_slice(&h.tick_spacing.to_be_bytes()[1..]);
        out.extend_from_slice(&h.cross_gas.to_be_bytes());
    }
    Ok(out)
}

/// `run(plan)`'s calldata: the lean executors' selector, over plan 3's bytes.
pub fn run3_calldata(plan: &Lean3Plan) -> Result<Vec<u8>, Lean3Error> {
    let bytes = encode_plan3(plan)?;
    let mut out = Vec::with_capacity(68 + bytes.len().div_ceil(32) * 32);
    out.extend_from_slice(&RUN_SELECTOR);
    out.extend_from_slice(&B256::left_padding_from(&[0x20]).0);
    out.extend_from_slice(&B256::left_padding_from(&(bytes.len() as u64).to_be_bytes()).0);
    out.extend_from_slice(&bytes);
    out.resize(68 + bytes.len().div_ceil(32) * 32, 0);
    Ok(out)
}

/// INV-06's off-chain commitment, as plan 2's with version 5:
/// `keccak256(abi.encode(chain_id, executor, uint8(5), keccak256(plan bytes)))`.
pub fn lean3_commitment(plan: &Lean3Plan, chain_id: u64, executor: Address) -> Result<B256, Lean3Error> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&B256::left_padding_from(&chain_id.to_be_bytes()).0);
    buf.extend_from_slice(&B256::left_padding_from(executor.as_slice()).0);
    buf.extend_from_slice(&B256::left_padding_from(&[LEAN3_PLAN_VERSION as u8]).0);
    buf.extend_from_slice(&keccak256(encode_plan3(plan)?).0);
    Ok(keccak256(&buf))
}

/// A plan-3 plan whose commitment was recomputed and matched: unforgeable
/// outside this module, as `SignedLean2Plan` is.
///
/// ```compile_fail
/// use apex_exec::lean3::SignedLean3Plan;
/// let forged = SignedLean3Plan { plan: todo!(), calldata: todo!(), commitment: todo!(), executor: todo!(), block_chain_id: todo!(), _sealed: () };
/// ```
#[derive(Debug)]
pub struct SignedLean3Plan {
    plan: Lean3Plan,
    /// `run(plan)`'s calldata, encoded as the check encoded it.
    calldata: Vec<u8>,
    commitment: B256,
    executor: Address,
    block_chain_id: u64,
    _sealed: (),
}

impl SignedLean3Plan {
    /// Recompute, compare, and refuse on a mismatch.
    pub fn check(plan: Lean3Plan, declared: B256, block_chain_id: u64, executor: Address) -> Result<Self, Lean3Error> {
        let recomputed = lean3_commitment(&plan, block_chain_id, executor)?;
        if recomputed != declared {
            return Err(Lean3Error::CommitmentMismatch { declared, recomputed });
        }
        let calldata = run3_calldata(&plan)?;
        Ok(Self { plan, calldata, commitment: declared, executor, block_chain_id, _sealed: () })
    }

    pub const fn plan(&self) -> &Lean3Plan {
        &self.plan
    }

    pub fn calldata(&self) -> &[u8] {
        &self.calldata
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
