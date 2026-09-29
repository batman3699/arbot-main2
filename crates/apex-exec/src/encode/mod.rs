//! Executor calldata (§25, §26.2).
//!
//! # One venue, built so the rest are additions
//!
//! The executor decodes a different payload per `Op`, and each needs its own
//! encoder and its own differential against the contract's decoder. This builds
//! **UniV3** — the one `util::encode_univ3_path` already existed for, and the one
//! the measured tradeable set on Base actually uses. Balancer and the generic
//! adapter path are listed as not delivered rather than stubbed: a stub that
//! produced plausible bytes would be decoded by the contract into a trade nobody
//! described, which is precisely the failure the commitment exists to catch and
//! a worse way to discover it.
//!
//! # Min-out is derived here and is never zero
//!
//! `_execUniswap` reverts on `minOut == 0` (`InvalidGenericAction`), and that is
//! the right reflex: a zero minimum is a swap that accepts any output, which on a
//! public mempool is a donation. [`apply_slippage`] therefore refuses to return
//! zero — it saturates to 1 — and [`EncodeError::ZeroMinOut`] is what a caller
//! gets when the arithmetic would have produced one.

use alloy_primitives::{Address, U256};

/// A packed UniV3 path element: the token, and the fee of the pool reached from
/// it. The last hop carries no fee, which is what makes the path length
/// `20 + 23n` rather than `43n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathHop {
    pub token_in: Address,
    /// Pool fee in hundredths of a bip (UniV3's own unit): 500 = 0.05%.
    pub fee: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// A path needs at least one hop and a final token.
    EmptyPath,
    /// UniV3 packs the fee into three bytes, so anything above 2^24 − 1 cannot
    /// be expressed. Refused rather than truncated: a truncated fee is a path
    /// that points at a different pool.
    FeeTooLarge { fee: u32 },
    /// The slippage arithmetic produced zero, which the executor reverts on.
    ZeroMinOut,
    /// A basis-point figure above 10,000 is not a slippage bound; it is a sign
    /// the caller meant something else.
    SlippageOutOfRange { bps: u32 },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPath => f.write_str("a UniV3 path needs at least one hop"),
            Self::FeeTooLarge { fee } => write!(f, "fee {fee} does not fit in three bytes"),
            Self::ZeroMinOut => f.write_str("min-out of zero accepts any output"),
            Self::SlippageOutOfRange { bps } => write!(f, "{bps} bps is not a slippage bound"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// UniV3's packed path: `token || fee(3) || token || fee(3) || … || token`.
///
/// Big-endian three-byte fees, which is what `_tokenAt` and the router's own
/// path decoder read. Packed rather than ABI-encoded — this is a `bytes` blob
/// whose internal layout the router defines, and the ABI encoding of that blob
/// happens one level up in [`univ3_step`].
pub fn univ3_path(hops: &[PathHop], token_out: Address) -> Result<Vec<u8>, EncodeError> {
    if hops.is_empty() {
        return Err(EncodeError::EmptyPath);
    }
    let mut out = Vec::with_capacity(20 + hops.len() * 23);
    for hop in hops {
        if hop.fee > 0x00ff_ffff {
            return Err(EncodeError::FeeTooLarge { fee: hop.fee });
        }
        out.extend_from_slice(hop.token_in.as_slice());
        out.extend_from_slice(&hop.fee.to_be_bytes()[1..]);
    }
    out.extend_from_slice(token_out.as_slice());
    Ok(out)
}

/// `abi.encode(bytes path, uint256 amountIn, uint256 minOut)` — the payload
/// `_execUniswap` decodes.
///
/// `bytes` is a dynamic type, so the head is three words — an **offset** to the
/// path data, then the two amounts — followed by the path's length and its bytes
/// right-padded to a word boundary. The offset is `0x60` and not `0x20`: it
/// counts from the start of this encoding's own head, past all three head words.
/// Getting that wrong is the classic ABI mistake and it decodes as garbage rather
/// than reverting, which is why `test/PlanCommitmentFixture.t.sol` decodes these
/// bytes rather than hashing them.
pub fn univ3_step(path: &[u8], amount_in: U256, min_out: U256) -> Result<Vec<u8>, EncodeError> {
    if min_out.is_zero() {
        return Err(EncodeError::ZeroMinOut);
    }
    let mut out = Vec::with_capacity(96 + 32 + path.len().div_ceil(32) * 32);
    out.extend_from_slice(&U256::from(0x60u64).to_be_bytes::<32>());
    out.extend_from_slice(&amount_in.to_be_bytes::<32>());
    out.extend_from_slice(&min_out.to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(path.len()).to_be_bytes::<32>());
    out.extend_from_slice(path);
    // Right-pad to a word boundary. A `bytes` whose tail is not padded is not a
    // valid ABI encoding, and a decoder reading past it gets the next field.
    let remainder = path.len() % 32;
    if remainder != 0 {
        out.extend(std::iter::repeat_n(0u8, 32 - remainder));
    }
    Ok(out)
}

/// The minimum output a swap will accept, from an expected output and a slippage
/// bound in basis points.
///
/// **Never zero.** `_execUniswap` reverts on `minOut == 0`, and it is right to:
/// a zero minimum accepts any output at all. Saturating to 1 rather than
/// returning zero means an expected output so small that the arithmetic rounds
/// away still produces a bound the executor will honour — and a trade that small
/// is refused by the economics long before it reaches here.
pub fn apply_slippage(expected_out: U256, slippage_bps: u32) -> Result<U256, EncodeError> {
    if slippage_bps > 10_000 {
        return Err(EncodeError::SlippageOutOfRange { bps: slippage_bps });
    }
    if expected_out.is_zero() {
        return Err(EncodeError::ZeroMinOut);
    }
    let kept = U256::from(10_000u64 - u64::from(slippage_bps));
    let min = expected_out.saturating_mul(kept) / U256::from(10_000u64);
    Ok(if min.is_zero() { U256::from(1u64) } else { min })
}

/// Assembles the executor's `PlanV2` from encoded steps.
///
/// Deliberately thin: it is the place where the pieces meet, and every piece is
/// tested where it is built. A fatter encoder here would be the one place in the
/// system that knows both venue calldata and plan structure, which is how a
/// 2,472-line `plan.rs` happened the first time.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanEncoder;

impl PlanEncoder {
    /// A single-loan, N-step plan — the shape every route this repository has
    /// measured actually takes.
    pub fn build(
        loan: crate::commitment::Loan,
        steps: Vec<crate::commitment::Step>,
        cycle_slippage_bps: u16,
        min_profit: U256,
        chain_id: u64,
        deadline: u64,
    ) -> crate::commitment::PlanV2 {
        crate::commitment::PlanV2 {
            loans: vec![loan],
            cycle_slippage_bps,
            steps,
            min_profit,
            // Zero: every route the planner builds ends flat. The contract
            // reverts on a surplus above this, which is what makes a mispriced
            // or overtaken hop visible rather than a quiet balance drift.
            declared_residue: U256::ZERO,
            chain_id,
            deadline,
        }
    }
}
