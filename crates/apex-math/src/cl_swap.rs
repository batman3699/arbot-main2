//! `TickLadder` + the pure multi-tick exact-input swap loop.
//!
//! Everything here is synchronous and total: given a ladder, the quote is a
//! deterministic function with no I/O. Tick acquisition lives in `cl_ticks`.

use crate::cl_math::{
    compute_swap_step, get_sqrt_ratio_at_tick, max_sqrt_ratio, min_sqrt_ratio, MAX_TICK, MIN_TICK,
};
use crate::cl_state::ClPoolState;
use ethers_core::types::U256;

/// Outcome of asking a ladder for the next initialized tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderStep {
    Initialized { tick: i32, liquidity_net: i128 },
    /// The search left the range this ladder is known-complete over. The
    /// caller must NOT assume constant liquidity beyond this point.
    Exhausted,
}

/// A materialised, sorted view of a pool's initialized ticks over a bounded
/// range, plus the bounds over which that view is known complete.
///
/// The bounds are the load-bearing part. A ladder that merely held ticks would
/// let a large swap walk off the end and silently continue at the last known
/// liquidity — exactly the constant-liquidity error in
/// `quote_exact_input_single_tick`. Recording where knowledge stops turns that
/// into a reported `Exhausted` and a caller-side fallback.
#[derive(Clone, Debug)]
pub struct TickLadder {
    /// `(tick, liquidity_net)` sorted ascending by tick, deduplicated.
    ticks: Vec<(i32, i128)>,
    lower_bound: i32,
    upper_bound: i32,
}

// main.rs compiles its own copy of this module; items used only by the
// library, tests or helper bins read as dead there.
#[allow(dead_code)]
impl TickLadder {
    pub fn new(mut ticks: Vec<(i32, i128)>, lower_bound: i32, upper_bound: i32) -> Self {
        ticks.sort_unstable_by_key(|(t, _)| *t);
        ticks.dedup_by_key(|(t, _)| *t);
        Self {
            ticks,
            lower_bound: lower_bound.max(MIN_TICK),
            upper_bound: upper_bound.min(MAX_TICK),
        }
    }

    /// The initialized ticks this ladder proves, ascending.
    ///
    /// Exposed for `ExactPricingEngine::state_dependencies`: a quote that
    /// crossed these ticks is invalidated by a mint or burn on any of them,
    /// and a caller cannot know that without being told which ones they were.
    pub fn ticks(&self) -> &[(i32, i128)] {
        &self.ticks
    }

    pub fn is_empty(&self) -> bool {
        self.ticks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    pub fn lower_bound(&self) -> i32 {
        self.lower_bound
    }

    pub fn upper_bound(&self) -> i32 {
        self.upper_bound
    }

    /// Whether `tick` falls inside the range this ladder proved complete.
    pub fn covers(&self, tick: i32) -> bool {
        tick >= self.lower_bound && tick <= self.upper_bound
    }

    /// Next initialized tick in the direction of travel.
    ///
    /// Mirrors v3-core's `nextInitializedTickWithinOneWord`: `zero_for_one`
    /// (price falling) searches for the greatest initialized tick `<= from`,
    /// the other direction for the least initialized tick `> from`.
    pub fn next_initialized(&self, from_tick: i32, zero_for_one: bool) -> LadderStep {
        if !self.covers(from_tick) {
            return LadderStep::Exhausted;
        }
        let found = if zero_for_one {
            self.ticks
                .iter()
                .rev()
                .find(|(t, _)| *t <= from_tick)
                .copied()
        } else {
            self.ticks.iter().find(|(t, _)| *t > from_tick).copied()
        };
        match found {
            Some((tick, liquidity_net)) if self.covers(tick) => {
                LadderStep::Initialized { tick, liquidity_net }
            }
            _ => LadderStep::Exhausted,
        }
    }
}

/// Result of a multi-tick exact-input quote.
// Diagnostic fields on the quote are read by tests and the cl_parity harness,
// which are separate crates from the binary that also compiles this module.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct MultiTickQuote {
    pub amount_out: U256,
    /// Input actually spent, including fee. Below the requested amount only
    /// when `exhausted` is set.
    pub amount_in_consumed: U256,
    pub sqrt_price_after: U256,
    pub ticks_crossed: u32,
    /// The swap wanted to continue past the ladder's proven coverage or past
    /// `max_ticks`. The quote is a LOWER bound on a partial fill, not a
    /// complete answer — callers must fall back rather than use it as a real
    /// output.
    pub exhausted: bool,
    /// Pool liquidity when the loop stopped, after every tick it crossed.
    ///
    /// Added in Phase 2 for `ExactPricingEngine::next_state_exact`. The loop
    /// has always tracked this; it simply never reported it, because a quote
    /// only needs the output. Composing two swaps through one pool needs the
    /// state between them.
    pub liquidity_after: u128,
    /// The swap came to rest exactly ON an initialized tick without crossing
    /// it.
    ///
    /// The quote is complete and correct. The post-swap STATE is not
    /// determined: v3-core crosses eagerly in this situation and this loop
    /// deliberately does not (see the `remaining.is_zero()` break below), so
    /// `liquidity_after` is the pre-crossing value while v3 would report the
    /// post-crossing one. `next_state_exact` refuses rather than guess.
    pub ended_on_tick_boundary: bool,
}

/// Published UniV3 price bounds. A swap that ends here has consumed every
/// unit of the output token the pool can give in that direction.
pub const MIN_SQRT_RATIO: u128 = 4_295_128_739;

impl MultiTickQuote {
    /// True when this quote is not a real, fillable answer.
    ///
    /// `exhausted` alone is not enough. It describes OUR ladder — how far we
    /// could see — so a wide ladder over a pool whose `liquidity()` overstates
    /// what it actually holds walks happily to the end and reports success.
    /// These two signals describe the POOL instead, and neither needs an extra
    /// RPC:
    ///
    /// * `amount_in_consumed < amount_in` — the swap could not spend the full
    ///   input. Whatever the reason, the pool cannot fill this size.
    /// * `sqrt_price_after` at the price bound — the swap drove the pool to
    ///   MIN/MAX sqrt ratio, i.e. drained that side outright.
    ///
    /// Measured on WETH/bsdETH 0xdea629c5587037d0925ff85f1961d95db62bedd6: the
    /// pool reports `liquidity() = 2.446e22` while holding just 0.154 bsdETH
    /// against 248.9 WETH. The model quoted 2 WETH -> 1.904 bsdETH; the on-chain
    /// quoter pays 0.0000169 and returns `sqrtPriceAfter = 4295128740`, exactly
    /// `MIN_SQRT_RATIO + 1`. Because the mispricing made it the most profitable
    /// edge on the chain (~1.9%), it outranked every genuine edge and was
    /// selected on every block — the ranking was sorting on model error.
    pub fn is_unfillable(&self, amount_in: U256) -> bool {
        if self.amount_in_consumed < amount_in {
            return true;
        }
        // Within a small margin of the bound: the quoter lands on
        // MIN_SQRT_RATIO + 1, so an exact compare would miss it.
        self.sqrt_price_after <= U256::from(MIN_SQRT_RATIO).saturating_add(U256::from(16u64))
    }
}

/// Apply a signed liquidity delta. Returns `None` (never saturates) if the
/// result would over/underflow `u128` — the caller treats that as exhaustion.
fn apply_liquidity_net(liquidity: u128, liquidity_net: i128) -> Option<u128> {
    if liquidity_net >= 0 {
        liquidity.checked_add(liquidity_net.unsigned_abs())
    } else {
        liquidity.checked_sub(liquidity_net.unsigned_abs())
    }
}

/// Exact-input quote that crosses tick boundaries, updating liquidity at each
/// initialized tick.
///
/// Returns `None` for structurally invalid input (zero amount, zero liquidity,
/// unusable state). A quote that runs out of ladder returns `Some` with
/// `exhausted = true` — the distinction matters: `None` means "cannot answer",
/// `exhausted` means "answered partially, do not trust as a full fill".
pub fn quote_exact_input_multi_tick(
    state: &ClPoolState,
    ladder: &TickLadder,
    amount_in: U256,
    zero_for_one: bool,
    max_ticks: u32,
) -> Option<MultiTickQuote> {
    if amount_in.is_zero() || state.liquidity == 0 || state.sqrt_price_x96.is_zero() {
        return None;
    }

    let price_limit = if zero_for_one {
        min_sqrt_ratio()
    } else {
        max_sqrt_ratio()
    };

    let mut remaining = amount_in;
    let mut amount_out = U256::zero();
    let mut sqrt_price = state.sqrt_price_x96;
    let mut liquidity = state.liquidity;
    let mut tick = state.tick;
    let mut ticks_crossed: u32 = 0;
    let mut exhausted = false;
    let mut ended_on_tick_boundary = false;

    while !remaining.is_zero() {
        if zero_for_one && sqrt_price <= price_limit {
            break;
        }
        if !zero_for_one && sqrt_price >= price_limit {
            break;
        }

        let (next_tick, liquidity_net) = match ladder.next_initialized(tick, zero_for_one) {
            LadderStep::Initialized { tick, liquidity_net } => (tick, liquidity_net),
            LadderStep::Exhausted => {
                exhausted = true;
                break;
            }
        };

        let sqrt_price_next_tick = get_sqrt_ratio_at_tick(next_tick)?;

        // Clamp the step target to the global price limit.
        let target = if zero_for_one {
            sqrt_price_next_tick.max(price_limit)
        } else {
            sqrt_price_next_tick.min(price_limit)
        };

        let step = compute_swap_step(sqrt_price, target, liquidity, remaining, state.fee_ppm)?;

        remaining = remaining
            .checked_sub(step.amount_in.checked_add(step.fee_amount)?)
            .unwrap_or_else(U256::zero);
        amount_out = amount_out.checked_add(step.amount_out)?;
        sqrt_price = step.sqrt_price_next;

        if sqrt_price != sqrt_price_next_tick {
            // Stopped mid-range: the input is spent. v3-core would recompute
            // `tick` here via `getTickAtSqrtRatio`; the loop exits on the next
            // condition check and never reads it, so it is omitted.
            break;
        }
        if remaining.is_zero() {
            // Input landed exactly on the boundary. The fill is COMPLETE —
            // crossing now would only matter if there were more to swap, and
            // running the crossing guards here can mislabel a complete quote
            // as exhausted.
            //
            // It does, however, leave the post-swap state undetermined: the
            // price is at an initialized tick whose liquidity delta has not
            // been applied. Flag it so `next_state_exact` refuses instead of
            // publishing pre-crossing liquidity as if the tick were still
            // ahead of us.
            ended_on_tick_boundary = true;
            break;
        }

        // Landed exactly on an initialized tick — cross it.
        if ticks_crossed >= max_ticks {
            exhausted = true;
            break;
        }
        let signed = if zero_for_one { -liquidity_net } else { liquidity_net };
        liquidity = match apply_liquidity_net(liquidity, signed) {
            Some(l) if l > 0 => l,
            // Liquidity would go to zero or underflow: the ladder disagrees
            // with reality, so refuse rather than quote through a dead range.
            _ => {
                exhausted = true;
                break;
            }
        };
        // Downward crossings land on `tick - 1` so the next search makes
        // progress instead of re-finding the tick just crossed.
        tick = if zero_for_one { next_tick - 1 } else { next_tick };
        ticks_crossed += 1;
    }

    if amount_out.is_zero() {
        return None;
    }

    Some(MultiTickQuote {
        amount_out,
        amount_in_consumed: amount_in.checked_sub(remaining).unwrap_or(amount_in),
        sqrt_price_after: sqrt_price,
        ticks_crossed,
        exhausted,
        liquidity_after: liquidity,
        ended_on_tick_boundary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cl_state::ClPoolState;
    use ethers_core::types::U256;

    fn ladder() -> TickLadder {
        // Symmetric band around 0, 60-spaced, with liquidity added at the
        // inner ticks and removed at the outer ones.
        TickLadder::new(
            vec![
                (-180, 1_000_000_000),
                (-120, 2_000_000_000),
                (-60, 3_000_000_000),
                (60, -3_000_000_000),
                (120, -2_000_000_000),
                (180, -1_000_000_000),
            ],
            -180,
            180,
        )
    }

    #[test]
    fn next_initialized_walks_down_for_zero_for_one() {
        let l = ladder();
        // zero_for_one searches for the greatest initialized tick <= from.
        assert_eq!(
            l.next_initialized(0, true),
            LadderStep::Initialized { tick: -60, liquidity_net: 3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(-60, true),
            LadderStep::Initialized { tick: -60, liquidity_net: 3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(-61, true),
            LadderStep::Initialized { tick: -120, liquidity_net: 2_000_000_000 }
        );
    }

    #[test]
    fn next_initialized_walks_up_for_one_for_zero() {
        let l = ladder();
        // one_for_zero searches for the least initialized tick strictly > from.
        assert_eq!(
            l.next_initialized(0, false),
            LadderStep::Initialized { tick: 60, liquidity_net: -3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(60, false),
            LadderStep::Initialized { tick: 120, liquidity_net: -2_000_000_000 }
        );
    }

    /// Running off the end of proven coverage must be reported, never guessed.
    /// This is the defect the whole plan exists to remove.
    #[test]
    fn next_initialized_reports_exhaustion_past_coverage() {
        let l = ladder();
        assert_eq!(l.next_initialized(-181, true), LadderStep::Exhausted);
        assert_eq!(l.next_initialized(180, false), LadderStep::Exhausted);
    }

    #[test]
    fn empty_ladder_is_immediately_exhausted() {
        let l = TickLadder::new(Vec::new(), -60, 60);
        assert_eq!(l.next_initialized(0, true), LadderStep::Exhausted);
        assert_eq!(l.next_initialized(0, false), LadderStep::Exhausted);
    }

    fn pool_state(liquidity: u128, fee_ppm: u32) -> ClPoolState {
        ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity,
            tick: 0,
            tick_spacing: 60,
            fee_ppm,
            ..Default::default()
        }
    }

    /// A swap small enough to stay inside the current range must agree with
    /// the single-tick model, which is exactly correct in that regime.
    ///
    /// NOTE ON MAGNITUDE: `amount_in` must be large enough that integer
    /// rounding is not the dominant term. At `amount_in = 1000` the output is
    /// ~997 units, so a single unit of last-place rounding is ~10 bps and
    /// exceeds any sane relative tolerance — the two models genuinely differ by
    /// 1 there (single-tick 997, `SwapMath` 996), and the SwapMath value is the
    /// correct one. Reaching tick -60 needs ~3.0e15 of input at this liquidity,
    /// so 1e12 is comfortably in-range and the two agree exactly. Verified
    /// numerically across 1e4/1e6/1e9/1e12: difference is 0 at every one.
    #[test]
    fn small_swap_matches_single_tick_model() {
        let state = pool_state(1_000_000_000_000_000_000, 3_000);
        let l = ladder();
        let amount_in = U256::from(1_000_000_000_000u64);

        let multi = quote_exact_input_multi_tick(&state, &l, amount_in, true, 128)
            .expect("multi-tick quote");
        let single = crate::cl_state::quote_exact_input_single_tick(&state, amount_in, true, 3_000)
            .expect("single-tick call")
            .expect("single-tick quote");

        assert!(!multi.exhausted, "an in-range swap must not exhaust the ladder");
        assert_eq!(multi.ticks_crossed, 0, "an in-range swap crosses no ticks");

        // The multi-tick model rounds output DOWN, matching how the pool itself
        // rounds. It must therefore never quote ABOVE the single-tick estimate.
        assert!(
            multi.amount_out <= single,
            "multi-tick ({}) must never exceed the single-tick estimate ({})",
            multi.amount_out,
            single
        );

        let diff = single - multi.amount_out;
        assert!(
            diff * U256::from(1_000u64) <= single,
            "in-range multi ({}) and single ({}) must agree within 0.1%",
            multi.amount_out,
            single
        );
    }

    /// The headline regression: once a swap crosses out of the initial range,
    /// the constant-liquidity model overstates output. Falling liquidity on
    /// the way down means the multi-tick answer must be strictly smaller.
    #[test]
    fn crossing_swap_is_below_the_constant_liquidity_estimate() {
        // Liquidity DROPS as price falls through -60 then -120.
        // See the SIGN CONVENTION note above: ticks below the current price
        // carry POSITIVE liquidity_net, and crossing them downward negates
        // that, removing liquidity. Starting at 1000e9: -60 -> 500e9,
        // -120 -> 100e9, -180 -> 50e9, never reaching zero.
        let l = TickLadder::new(
            vec![(-180, 50_000_000_000), (-120, 400_000_000_000), (-60, 500_000_000_000)],
            -180,
            180,
        );
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(10u64).pow(U256::from(18u64));

        let multi = quote_exact_input_multi_tick(&state, &l, amount_in, true, 128)
            .expect("multi-tick quote");
        let single = crate::cl_state::quote_exact_input_single_tick(&state, amount_in, true, 3_000)
            .expect("single-tick call")
            .expect("single-tick quote");

        assert!(multi.ticks_crossed >= 1, "this size must cross at least one tick");
        assert!(
            multi.amount_out < single,
            "multi-tick {} must be below the optimistic single-tick {}",
            multi.amount_out,
            single
        );
    }

    /// The mirror of `crossing_swap_is_below_the_constant_liquidity_estimate`
    /// for the `!zero_for_one` (one_for_zero) direction: the loop must not
    /// negate `liquidity_net` above the starting price, and must advance
    /// `tick` to `next_tick` (not `next_tick - 1`) after crossing upward.
    /// Neither of those is exercised by ladder-navigation tests alone.
    #[test]
    fn crossing_swap_works_for_one_for_zero() {
        // Ticks above the current price carry NEGATIVE liquidity_net (see the
        // SIGN CONVENTION note above). The loop does not negate net when
        // `!zero_for_one`, so crossing 60 upward applies -500e9 directly,
        // dropping liquidity from 1000e9 to 500e9 — still positive, so the
        // swap continues rather than exhausting on that count. A second,
        // far-away tick (6000) keeps the ladder from running out of coverage:
        // `amount_in` is enough to fully cross 60 but far short of what a
        // ~2970-tick price move to 6000 would need, so the loop stops
        // mid-range on the way there and exits via the ordinary "stopped
        // mid-range" break rather than exhaustion.
        let l = TickLadder::new(
            vec![(60, -500_000_000_000), (6_000, -100_000_000_000)],
            -180,
            6_000,
        );
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(10_000_000_000u64);

        let quote = quote_exact_input_multi_tick(&state, &l, amount_in, false, 128)
            .expect("multi-tick quote");

        assert_eq!(quote.ticks_crossed, 1, "this size must cross tick 60 upward and stop before 6000");
        assert!(!quote.exhausted, "sufficient remaining liquidity and coverage must not exhaust");
    }

    /// The bug this regression pins: a step that lands EXACTLY on a tick
    /// boundary while consuming precisely all of `remaining` must not fall
    /// through into the crossing guards and get mislabelled `exhausted`. The
    /// fill is complete the moment `remaining` hits zero.
    ///
    /// These constants are an exact fixed-point tie between `amount_in` and
    /// `amount_in_full + fee_amount` for this liquidity/tick/fee combination.
    /// If `compute_swap_step`'s fee or delta rounding ever changes, this test
    /// will fail and must be RE-DERIVED (new exact numbers for the new
    /// rounding), not loosened — the point is exactness, not an approximate
    /// tolerance.
    #[test]
    fn exact_boundary_fill_is_not_exhausted() {
        // net exactly equals current liquidity, so crossing tick -60 downward
        // zeroes liquidity and would trip the liquidity guard on the next
        // iteration IF the loop incorrectly tried to keep going.
        let l = TickLadder::new(vec![(-60, 1_000_000_000_000)], -180, 180);
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(3_013_394_246u64);

        let quote = quote_exact_input_multi_tick(&state, &l, amount_in, true, 128)
            .expect("multi-tick quote");

        assert!(
            !quote.exhausted,
            "a fill that lands exactly on a boundary and spends all of amount_in is complete, not exhausted"
        );
        assert_eq!(
            quote.amount_in_consumed, amount_in,
            "amount_in_consumed must equal amount_in for a complete fill"
        );
    }

    /// Walking past proven coverage must set the flag rather than extrapolate.
    #[test]
    fn swap_beyond_coverage_reports_exhaustion() {
        let l = TickLadder::new(vec![(-60, 900_000_000_000)], -60, 60);
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(10u64).pow(U256::from(24u64));

        let quote = quote_exact_input_multi_tick(&state, &l, amount_in, true, 128)
            .expect("multi-tick quote");

        assert!(quote.exhausted, "a swap past coverage must report exhaustion");
        assert!(
            quote.amount_in_consumed < amount_in,
            "an exhausted quote must not claim to have spent the whole input"
        );
    }

    /// `max_ticks` bounds the work per quote; hitting it is an exhaustion, not
    /// a silently truncated answer.
    #[test]
    fn tick_budget_is_enforced_as_exhaustion() {
        let l = TickLadder::new(
            (1..=40).map(|i| (-60 * i, 10_000_000_000i128)).collect(),
            -2_400,
            2_400,
        );
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(10u64).pow(U256::from(24u64));

        let quote = quote_exact_input_multi_tick(&state, &l, amount_in, true, 3)
            .expect("multi-tick quote");

        assert!(quote.exhausted, "hitting the tick budget must report exhaustion");
        assert!(quote.ticks_crossed <= 3, "budget must be respected");
    }

    #[test]
    fn zero_input_yields_none() {
        let state = pool_state(1_000_000_000_000, 3_000);
        assert!(quote_exact_input_multi_tick(&state, &ladder(), U256::zero(), true, 128).is_none());
    }

    #[test]
    fn zero_liquidity_yields_none() {
        let state = pool_state(0, 3_000);
        assert!(
            quote_exact_input_multi_tick(&state, &ladder(), U256::from(1_000u64), true, 128)
                .is_none()
        );
    }
}
