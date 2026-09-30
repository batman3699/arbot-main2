//! Executor calldata (§25, §26.2).
//!
//! # One venue, built so the rest are additions
//!
//! The executor decodes a different payload per `Op`, and each needs its own
//! encoder and its own differential against the contract's decoder. This builds
//! **UniV3** — the one `util::encode_univ3_path` already existed for, and the one
//! the measured tradeable set on Base actually uses — and the **generic adapter
//! step**, with the one call the live cycles route through it: Aerodrome
//! Slipstream's `exactInputSingle` (Task 8.5 R6). Balancer swaps are listed as
//! not delivered rather than stubbed: a stub that produced plausible bytes
//! would be decoded by the contract into a trade nobody described, which is
//! precisely the failure the commitment exists to catch and a worse way to
//! discover it.
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
    /// Slipstream's `tickSpacing` is an `int24`. Refused rather than truncated:
    /// a truncated spacing names a different pool.
    TickSpacingOutOfRange { spacing: i32 },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPath => f.write_str("a UniV3 path needs at least one hop"),
            Self::FeeTooLarge { fee } => write!(f, "fee {fee} does not fit in three bytes"),
            Self::ZeroMinOut => f.write_str("min-out of zero accepts any output"),
            Self::SlippageOutOfRange { bps } => write!(f, "{bps} bps is not a slippage bound"),
            Self::TickSpacingOutOfRange { spacing } => {
                write!(f, "tick spacing {spacing} does not fit an int24")
            }
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

/// `abi.encode(uint16 adapterId, address token, uint256 approveAmount, bytes
/// callData)` — the payload `_execAdapter` decodes.
///
/// The contract resolves `adapterId` to the address the owner registered and
/// refuses a selector not allowlisted for it (INV-26), approves `token` to that
/// adapter for `approveAmount`, then calls it with `callData`. Nothing here can
/// name a target: that is the point of the registry (B-1).
///
/// `bytes` is the one dynamic member, so the head is four words with an offset
/// of `0x80` — past all four — and the tail is its length and its bytes,
/// right-padded to a word.
pub fn generic_step(adapter_id: u16, token: Address, approve_amount: U256, call: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 * WORD + call.len() + padding(call.len()));
    push_uint(&mut out, u64::from(adapter_id));
    push_address(&mut out, token);
    push_u256(&mut out, approve_amount);
    push_uint(&mut out, (4 * WORD) as u64);
    push_uint(&mut out, call.len() as u64);
    out.extend_from_slice(call);
    out.extend(std::iter::repeat_n(0u8, padding(call.len())));
    out
}

/// `exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))`
/// on Aerodrome Slipstream's `SwapRouter` — `0xa026383e`, which the router's
/// deployed code contains (checked on Base 2026-09-30) and which the deploy
/// script allowlists for adapter 1.
pub const SLIPSTREAM_EXACT_INPUT_SINGLE: [u8; 4] = [0xa0, 0x26, 0x38, 0x3e];

/// One Slipstream swap, exact input. A pool is its two tokens and its **tick
/// spacing** — Slipstream keys pools by spacing, not fee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlipstreamSwap {
    pub token_in: Address,
    pub token_out: Address,
    pub tick_spacing: i32,
    pub recipient: Address,
    /// Unix seconds; the router refuses a later block.
    pub deadline: u64,
    pub amount_in: U256,
    pub min_out: U256,
}

/// The router call for one Slipstream swap. Every member of the params tuple is
/// static, so it is the selector and eight words, in declaration order.
///
/// `sqrtPriceLimitX96` is zero — no price limit — because the bound is
/// `amountOutMinimum`, and a limit would turn an overtaken swap into a partial
/// fill rather than a refusal. Min-out is refused at zero here as it is for
/// UniV3, although Slipstream would accept it: a zero minimum accepts any
/// output.
pub fn slipstream_exact_input_single(s: &SlipstreamSwap) -> Result<Vec<u8>, EncodeError> {
    if s.min_out.is_zero() {
        return Err(EncodeError::ZeroMinOut);
    }
    if !(-(1 << 23)..(1 << 23)).contains(&s.tick_spacing) {
        return Err(EncodeError::TickSpacingOutOfRange { spacing: s.tick_spacing });
    }
    let mut out = Vec::with_capacity(4 + 8 * WORD);
    out.extend_from_slice(&SLIPSTREAM_EXACT_INPUT_SINGLE);
    push_address(&mut out, s.token_in);
    push_address(&mut out, s.token_out);
    // Sign-extended to a word, as every signed ABI integer is.
    let fill = if s.tick_spacing < 0 { 0xff } else { 0x00 };
    out.extend_from_slice(&[fill; 28]);
    out.extend_from_slice(&s.tick_spacing.to_be_bytes());
    push_address(&mut out, s.recipient);
    push_uint(&mut out, s.deadline);
    push_u256(&mut out, s.amount_in);
    push_u256(&mut out, s.min_out);
    push_u256(&mut out, U256::ZERO);
    Ok(out)
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

/// `bytes4(keccak256("startV2(((address,uint256,uint8,address)[],uint16,(uint8,bytes)[],uint256,uint256,bytes32,uint64,uint64))"))`.
///
/// A constant rather than derived at run time, because the derivation is where a
/// typo would break silently. `tests/start_v2_calldata.rs` holds it to the
/// selector the contract itself reports, through the tracked fixture.
pub const START_V2_SELECTOR: [u8; 4] = [0x62, 0x90, 0xaa, 0x48];

const WORD: usize = 32;

/// The calldata for `MultiVenueArbImplementation.startV2(PlanV2 p)`, with
/// `p.commitment = commitment`.
///
/// # The layout, because the offsets are the whole difficulty
///
/// `PlanV2` is a dynamic tuple — it holds two dynamic arrays — so the argument
/// area is one offset word (`0x20`) followed by the tuple. Inside the tuple:
///
/// ```text
/// head, 8 words:  off(loans) | cycleSlippageBps | off(steps) | minProfit
///                 | declaredResidue | commitment | chainId | deadline
/// tail:           loans:  len | 4 static words per Loan
///                 steps:  len | one offset word per Step | each Step
/// Step:           op | 0x40 | len(data) | data, right-padded to a word
/// ```
///
/// Offsets in the tuple head count from the **start of the tuple**; offsets in
/// the steps array count from **just after its length word**. Two origins, and an
/// encoder that uses one for both produces calldata that decodes — as a
/// different plan. `tests/start_v2_calldata.rs` holds every byte to the
/// compiler's own encoding, including payloads of 0, 2, 3, 32 and 33 bytes.
///
/// Infallible: every field is fixed-width or length-prefixed, so a `PlanV2` has
/// exactly one encoding. Whether the executor *accepts* the plan is the
/// contract's question, and the simulator's.
pub fn start_v2_calldata(
    plan: &crate::commitment::PlanV2,
    commitment: alloy_primitives::B256,
) -> Vec<u8> {
    let loans_tail = WORD + plan.loans.len() * 4 * WORD;
    let head = 8 * WORD;

    let mut out = Vec::with_capacity(4 + WORD + head + loans_tail + steps_len(&plan.steps));
    out.extend_from_slice(&START_V2_SELECTOR);
    // The one argument is dynamic, so its slot holds where it starts.
    push_uint(&mut out, WORD as u64);

    // ---- the tuple's head
    push_uint(&mut out, head as u64);
    push_u256(&mut out, U256::from(plan.cycle_slippage_bps));
    push_uint(&mut out, (head + loans_tail) as u64);
    push_u256(&mut out, plan.min_profit);
    push_u256(&mut out, plan.declared_residue);
    out.extend_from_slice(commitment.as_slice());
    push_uint(&mut out, plan.chain_id);
    push_uint(&mut out, plan.deadline);

    // ---- loans: a static-element array, so no offsets
    push_uint(&mut out, plan.loans.len() as u64);
    for loan in &plan.loans {
        push_address(&mut out, loan.token);
        push_u256(&mut out, loan.amount);
        push_uint(&mut out, loan.provider as u64);
        push_address(&mut out, loan.provider_addr);
    }

    // ---- steps: dynamic elements, so an offset per element first
    push_uint(&mut out, plan.steps.len() as u64);
    let mut next = plan.steps.len() * WORD;
    for step in &plan.steps {
        push_uint(&mut out, next as u64);
        next += step_len(step);
    }
    for step in &plan.steps {
        push_uint(&mut out, step.op as u64);
        // `(uint8 op, bytes data)`: `data` is the tuple's only dynamic member and
        // starts after its two head words.
        push_uint(&mut out, (2 * WORD) as u64);
        push_uint(&mut out, step.data.len() as u64);
        out.extend_from_slice(&step.data);
        out.extend(std::iter::repeat_n(0u8, padding(step.data.len())));
    }
    out
}

/// The encoded size of one `Step`: two head words, a length word, and the
/// payload rounded up to a word.
fn step_len(step: &crate::commitment::Step) -> usize {
    3 * WORD + step.data.len() + padding(step.data.len())
}

fn steps_len(steps: &[crate::commitment::Step]) -> usize {
    WORD + steps.len() * WORD + steps.iter().map(step_len).sum::<usize>()
}

/// Zero bytes that round `len` up to a whole word. Zero for an exact multiple —
/// including zero itself, so an empty payload is its length word and nothing.
const fn padding(len: usize) -> usize {
    (WORD - len % WORD) % WORD
}

fn push_u256(out: &mut Vec<u8>, v: U256) {
    out.extend_from_slice(&v.to_be_bytes::<32>());
}

fn push_uint(out: &mut Vec<u8>, v: u64) {
    push_u256(out, U256::from(v));
}

/// Left-padded: the twelve zero bytes in front are part of the encoding.
fn push_address(out: &mut Vec<u8>, a: Address) {
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(a.as_slice());
}
