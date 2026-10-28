//! The second lean executor's plan (Task 8.5 R37): `run(bytes plan)` against
//! `contracts/lean/LeanExecutor2.sol`. v1's plan (`crate::lean`) is unchanged:
//! the reactive lane and the deployed executor read it.
//!
//! # The bytes
//!
//! Big-endian, packed, no padding; `contracts/lean/LeanPlan2Codec.sol` reads
//! them and `tests/fixtures/lean2_plans.json` holds the two to each other.
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 1    | lender: 0 Balancer V2, 1 Morpho Blue, 2 Uniswap v4's PoolManager |
//! | 1      | 1    | start: 0 WETH, 1 USDC — borrowed, and every cycle's last token |
//! | 2      | 16   | amount borrowed |
//! | 18     | 5    | deadline: unix seconds |
//! | 23     | 16   | min profit, in the start token |
//! | 39 + … | 1    | hop kind: 0 v3-style, 1 PancakeSwap v3, 2 Aerodrome v2, 3 Uniswap v4 |
//! | +1     | 20   | hop pool; for kind 3 the pool key's hooks |
//! | +21    | 20   | hop token out (in: the previous hop's out, the start for the first) |
//! | +41    | 16   | hop minimum out |
//! | +57    | 3    | kind 3 only: the pool key's fee |
//! | +60    | 3    | kind 3 only: the pool key's tick spacing, two's complement |
//!
//! A v4 pool key's currencies are the hop's tokens, lower address first. A v4
//! hop runs inside PoolManager's unlock, so only under lender 2.

use crate::lean::{RUN_SELECTOR, WETH};
use alloy_primitives::{address, keccak256, Address, B256};

/// Base's USDC: the second start token.
pub const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

/// Committed to off-chain, beside the plan's bytes. v1's is 3.
pub const LEAN2_PLAN_VERSION: u32 = 4;

/// The token a plan borrows and must end in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartToken {
    Weth = 0,
    Usdc = 1,
}

impl StartToken {
    pub const fn address(self) -> Address {
        match self {
            Self::Weth => WETH,
            Self::Usdc => USDC,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lean2Lender {
    Balancer = 0,
    Morpho = 1,
    /// Uniswap v4's flash accounting: the whole trade inside one unlock.
    PoolManager = 2,
}

/// How the executor swaps on a pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hop2Kind {
    /// Uniswap v3 and both Slipstream factories: `uniswapV3SwapCallback`.
    UniswapV3Style = 0,
    /// PancakeSwap v3: `pancakeV3SwapCallback`.
    PancakeV3 = 1,
    /// Aerodrome v2: paid first, then asked for exactly `min_out`.
    AerodromeV2 = 2,
    /// Uniswap v4: `PoolManager.swap`, exact input, inside the trade's unlock.
    UniswapV4 = 3,
}

/// What a v4 pool key holds beyond its two currencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V4HopKey {
    /// 24 bits, as the key's `uint24`.
    pub fee: u32,
    /// 24 bits signed, as the key's `int24`.
    pub tick_spacing: i32,
    pub hooks: Address,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lean2Hop {
    pub kind: Hop2Kind,
    /// Ignored for kind 3: the key's hooks are written in its place.
    pub pool: Address,
    pub token_out: Address,
    pub min_out: u128,
    /// Kind 3's pool key, and only kind 3's.
    pub v4: Option<V4HopKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lean2Plan {
    pub lender: Lean2Lender,
    pub start: StartToken,
    /// Borrowed in the start token, and the first hop's input.
    pub amount: u128,
    pub deadline: u64,
    pub min_profit: u128,
    pub hops: Vec<Lean2Hop>,
}

/// A plan the contract would refuse, refused here first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lean2Error {
    NoHops,
    /// Five bytes hold 2^40 - 1 at most.
    DeadlineTooLarge(u64),
    /// A cycle borrows its start token and must end in it.
    LastHopNotStart,
    /// A v4 hop runs only inside PoolManager's unlock.
    V4NeedsPoolManager,
    /// A v4 hop without its key, or a key on another kind.
    V4KeyMismatch { hop: usize },
    /// A fee past 24 bits, or a tick spacing past `int24`.
    V4KeyOutOfRange { hop: usize },
    CommitmentMismatch { declared: B256, recomputed: B256 },
}

impl std::fmt::Display for Lean2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHops => f.write_str("a plan with no hops"),
            Self::DeadlineTooLarge(d) => write!(f, "deadline {d} does not fit five bytes"),
            Self::LastHopNotStart => f.write_str("the last hop does not end in the start token"),
            Self::V4NeedsPoolManager => f.write_str("a v4 hop needs PoolManager as the lender"),
            Self::V4KeyMismatch { hop } => write!(f, "hop {hop}'s v4 key does not match its kind"),
            Self::V4KeyOutOfRange { hop } => write!(f, "hop {hop}'s v4 key does not fit its bytes"),
            Self::CommitmentMismatch { declared, recomputed } => {
                write!(f, "plan declares {declared} but recomputes to {recomputed}")
            }
        }
    }
}

impl std::error::Error for Lean2Error {}

/// The plan's bytes.
pub fn encode_plan2(plan: &Lean2Plan) -> Result<Vec<u8>, Lean2Error> {
    let last = plan.hops.last().ok_or(Lean2Error::NoHops)?;
    if last.token_out != plan.start.address() {
        return Err(Lean2Error::LastHopNotStart);
    }
    if plan.deadline >= 1 << 40 {
        return Err(Lean2Error::DeadlineTooLarge(plan.deadline));
    }
    let mut out = Vec::with_capacity(39 + 63 * plan.hops.len());
    out.push(plan.lender as u8);
    out.push(plan.start as u8);
    out.extend_from_slice(&plan.amount.to_be_bytes());
    out.extend_from_slice(&plan.deadline.to_be_bytes()[3..]);
    out.extend_from_slice(&plan.min_profit.to_be_bytes());
    for (i, h) in plan.hops.iter().enumerate() {
        match (h.kind, h.v4) {
            (Hop2Kind::UniswapV4, Some(k)) => {
                if plan.lender != Lean2Lender::PoolManager {
                    return Err(Lean2Error::V4NeedsPoolManager);
                }
                if k.fee >= 1 << 24 || !(-(1 << 23)..1 << 23).contains(&k.tick_spacing) {
                    return Err(Lean2Error::V4KeyOutOfRange { hop: i });
                }
                out.push(h.kind as u8);
                out.extend_from_slice(k.hooks.as_slice());
                out.extend_from_slice(h.token_out.as_slice());
                out.extend_from_slice(&h.min_out.to_be_bytes());
                out.extend_from_slice(&k.fee.to_be_bytes()[1..]);
                out.extend_from_slice(&k.tick_spacing.to_be_bytes()[1..]);
            }
            (Hop2Kind::UniswapV4, None) | (_, Some(_)) => return Err(Lean2Error::V4KeyMismatch { hop: i }),
            (_, None) => {
                out.push(h.kind as u8);
                out.extend_from_slice(h.pool.as_slice());
                out.extend_from_slice(h.token_out.as_slice());
                out.extend_from_slice(&h.min_out.to_be_bytes());
            }
        }
    }
    Ok(out)
}

/// `run(plan)`'s calldata: v1's selector, over plan 2's bytes.
pub fn run2_calldata(plan: &Lean2Plan) -> Result<Vec<u8>, Lean2Error> {
    let bytes = encode_plan2(plan)?;
    let mut out = Vec::with_capacity(68 + bytes.len().div_ceil(32) * 32);
    out.extend_from_slice(&RUN_SELECTOR);
    out.extend_from_slice(&B256::left_padding_from(&[0x20]).0);
    out.extend_from_slice(&B256::left_padding_from(&(bytes.len() as u64).to_be_bytes()).0);
    out.extend_from_slice(&bytes);
    out.resize(68 + bytes.len().div_ceil(32) * 32, 0);
    Ok(out)
}

/// INV-06's off-chain commitment, as v1's with version 4:
/// `keccak256(abi.encode(chain_id, executor, uint8(4), keccak256(plan bytes)))`.
pub fn lean2_commitment(plan: &Lean2Plan, chain_id: u64, executor: Address) -> Result<B256, Lean2Error> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&B256::left_padding_from(&chain_id.to_be_bytes()).0);
    buf.extend_from_slice(&B256::left_padding_from(executor.as_slice()).0);
    buf.extend_from_slice(&B256::left_padding_from(&[LEAN2_PLAN_VERSION as u8]).0);
    buf.extend_from_slice(&keccak256(encode_plan2(plan)?).0);
    Ok(keccak256(&buf))
}

/// A plan-2 plan whose commitment was recomputed and matched: unforgeable
/// outside this module, as `SignedLeanPlan` is.
///
/// ```compile_fail
/// use apex_exec::lean2::SignedLean2Plan;
/// let forged = SignedLean2Plan { plan: todo!(), calldata: todo!(), commitment: todo!(), executor: todo!(), block_chain_id: todo!(), _sealed: () };
/// ```
#[derive(Debug)]
pub struct SignedLean2Plan {
    plan: Lean2Plan,
    /// `run(plan)`'s calldata, encoded as the check encoded it.
    calldata: Vec<u8>,
    commitment: B256,
    executor: Address,
    block_chain_id: u64,
    _sealed: (),
}

impl SignedLean2Plan {
    /// Recompute, compare, and refuse on a mismatch.
    pub fn check(plan: Lean2Plan, declared: B256, block_chain_id: u64, executor: Address) -> Result<Self, Lean2Error> {
        let recomputed = lean2_commitment(&plan, block_chain_id, executor)?;
        if recomputed != declared {
            return Err(Lean2Error::CommitmentMismatch { declared, recomputed });
        }
        let calldata = run2_calldata(&plan)?;
        Ok(Self { plan, calldata, commitment: declared, executor, block_chain_id, _sealed: () })
    }

    pub const fn plan(&self) -> &Lean2Plan {
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
