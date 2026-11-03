//! The two-pool tick walk (Task 8.5 R40.1): the size `LeanExecutor3` computes
//! at execution, mirrored here integer for integer.
//!
//! # Size, then quote
//!
//! Inside one pair of liquidity segments each pool is a constant product on
//! virtual reserves, so a two-hop cycle returns `a·Δ / (b + c·Δ)` and the best
//! input has a closed form. The walk evaluates it, and when that input would
//! take either pool past its next boundary it moves both pools to the nearer
//! boundary, carries what it spent, and evaluates the next pair.
//!
//! Its rounding only decides how good the size is. [`quote`] then prices that
//! size step for step as the pools do, and only that must match a swap to the
//! unit. A size that is off loses some value, never money: the floor is checked
//! on what the swaps returned.
//!
//! # Boundaries
//!
//! A boundary is what `nextInitializedTickWithinOneWord` answers: the next
//! initialized tick, or the edge of the bitmap word. An edge costs a bitmap read
//! and no crossing. Each boundary counts towards `max_steps`, on either pool.
//!
//! The walk stops one unit short of a boundary it declines to pass, so the real
//! swap doesn't pay to cross a tick it gains nothing from.

use crate::cl_math::{
    compute_swap_step, get_amount0_delta, get_amount1_delta, get_sqrt_ratio_at_tick, max_sqrt_ratio,
    min_sqrt_ratio, mul_div_checked, mul_div_rounding_up, q96,
};
use crate::cl_state::ClPoolState;
use crate::cl_swap::{quote_exact_input_multi_tick, word_edge, LadderStep, TickLadder};
use ethers_core::types::U256;

/// The launch value of the plan's `maxSteps`.
pub const MAX_STEPS: u32 = 8;
/// The most any plan may ask of `LeanExecutor3`.
pub const MAX_STEPS_LIMIT: u32 = 16;

/// Parts per million: fees are ppm here, and on Aerodrome v2 its bps × 100.
const PPM: u64 = 1_000_000;

/// One hop of the cycle, as the executor loads it.
#[derive(Clone, Copy)]
pub enum WalkLeg<'a> {
    Cl { state: &'a ClPoolState, ladder: &'a TickLadder, zero_for_one: bool, cross_gas: u64 },
    Reserves { reserve_in: U256, reserve_out: U256, fee_ppm: u32 },
}

/// What one gas costs: wei per gas, and the plan's rate (start-token units per
/// 1e18 wei; none for WETH).
#[derive(Clone, Copy, Debug)]
pub struct WalkPrice {
    pub wei_per_gas: U256,
    pub rate: Option<U256>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walked {
    pub size: U256,
    /// Boundaries reached, word edges included, on both pools.
    pub steps: u32,
    pub capped: bool,
    pub truncated: bool,
    /// The ladder ended before the walk did: the mirror is short of the chain.
    pub ladder_end: bool,
    /// Crossing gas of the crossings taken.
    pub cross_gas: u64,
}

/// A wei amount in start units, rounded up: `ceil(wei × rate / 1e18)`, 1:1 with
/// no rate. The contract's `toStart`.
pub fn to_start(wei: U256, rate: Option<U256>) -> Option<U256> {
    match rate {
        Some(rate) => mul_div_rounding_up(wei, rate, U256::exp10(18)),
        None => Some(wei),
    }
}

#[derive(Clone, Copy)]
struct Boundary {
    tick: i32,
    initialized: bool,
    net: i128,
    sqrt: U256,
}

/// A leg's running position. No ladder: a reserves leg, which has no boundary.
#[derive(Clone, Copy)]
struct Run<'a> {
    ladder: Option<&'a TickLadder>,
    zero_for_one: bool,
    fee_ppm: u32,
    spacing: i32,
    cross_gas: u64,
    sqrt: U256,
    liquidity: u128,
    tick: i32,
    reserve_in: U256,
    reserve_out: U256,
    bound: Option<Boundary>,
}

impl<'a> Run<'a> {
    fn new(leg: &WalkLeg<'a>) -> Option<Self> {
        Some(match *leg {
            WalkLeg::Cl { state, ladder, zero_for_one, cross_gas } => {
                if state.liquidity == 0 || state.sqrt_price_x96.is_zero() {
                    return None;
                }
                Self {
                    ladder: Some(ladder),
                    zero_for_one,
                    fee_ppm: state.fee_ppm,
                    spacing: state.tick_spacing,
                    cross_gas,
                    sqrt: state.sqrt_price_x96,
                    liquidity: state.liquidity,
                    tick: state.tick,
                    reserve_in: U256::zero(),
                    reserve_out: U256::zero(),
                    bound: None,
                }
            }
            WalkLeg::Reserves { reserve_in, reserve_out, fee_ppm } => {
                if reserve_in.is_zero() || reserve_out.is_zero() {
                    return None;
                }
                Self {
                    ladder: None,
                    zero_for_one: false,
                    fee_ppm,
                    spacing: 0,
                    cross_gas: 0,
                    sqrt: U256::zero(),
                    liquidity: 0,
                    tick: 0,
                    reserve_in,
                    reserve_out,
                    bound: None,
                }
            }
        })
    }

    fn keep(&self) -> Option<U256> {
        Some(U256::from(PPM.checked_sub(u64::from(self.fee_ppm)).filter(|g| *g > 0)?))
    }

    /// The constant product this leg sits on: what goes in, what comes out.
    fn virtuals(&self) -> Option<(U256, U256)> {
        if self.ladder.is_none() {
            return Some((self.reserve_in, self.reserve_out));
        }
        let l = U256::from(self.liquidity);
        let zero = mul_div_checked(l, q96(), self.sqrt)?;
        let one = mul_div_checked(l, self.sqrt, q96())?;
        Some(if self.zero_for_one { (zero, one) } else { (one, zero) })
    }

    /// `sqrt(out / in)`, Q96.
    fn root(&self) -> Option<U256> {
        if self.ladder.is_none() {
            return Some(mul_div_checked(self.reserve_out, U256::one() << 192, self.reserve_in)?.integer_sqrt());
        }
        if self.zero_for_one {
            Some(self.sqrt)
        } else {
            mul_div_checked(q96(), q96(), self.sqrt)
        }
    }

    /// Find the next boundary. False: the ladder ended before it.
    fn load(&mut self) -> Option<bool> {
        let Some(ladder) = self.ladder else { return Some(true) };
        let up = !self.zero_for_one;
        let edge = word_edge(self.tick, self.spacing, self.zero_for_one);
        let found = match ladder.next_initialized(self.tick, self.zero_for_one) {
            LadderStep::Initialized { tick, liquidity_net } if (up && tick <= edge) || (!up && tick >= edge) => {
                Some((tick, liquidity_net))
            }
            _ => None,
        };
        let (tick, net, initialized) = match found {
            Some((tick, net)) => (tick, net, true),
            None if ladder.covers(edge) => (edge, 0, false),
            None => return Some(false),
        };
        let at = get_sqrt_ratio_at_tick(tick)?;
        let sqrt = if up { at.min(max_sqrt_ratio()) } else { at.max(min_sqrt_ratio()) };
        self.bound = Some(Boundary { tick, initialized, net, sqrt });
        Some(true)
    }

    /// The gross input taking this leg to its boundary, and the output there.
    /// `U256::MAX` twice for a leg with none.
    fn boundary_amounts(&self) -> Option<(U256, U256)> {
        let Some(b) = self.bound else { return Some((U256::MAX, U256::MAX)) };
        let (net_in, out) = if self.zero_for_one {
            (get_amount0_delta(b.sqrt, self.sqrt, self.liquidity, true)?, get_amount1_delta(b.sqrt, self.sqrt, self.liquidity, false)?)
        } else {
            (get_amount1_delta(self.sqrt, b.sqrt, self.liquidity, true)?, get_amount0_delta(self.sqrt, b.sqrt, self.liquidity, false)?)
        };
        Some((mul_div_rounding_up(net_in, U256::from(PPM), self.keep()?)?, out))
    }

    /// Move forward by `amount` in, inside this segment. Returns the output.
    fn forward(&mut self, amount: U256) -> Option<U256> {
        if amount.is_zero() {
            return Some(U256::zero());
        }
        match self.bound {
            Some(b) => {
                let step = compute_swap_step(self.sqrt, b.sqrt, self.liquidity, amount, self.fee_ppm)?;
                self.sqrt = step.sqrt_price_next;
                Some(step.amount_out)
            }
            None => {
                let in_after = amount.checked_sub(mul_div_checked(amount, U256::from(self.fee_ppm), U256::from(PPM))?)?;
                let out = mul_div_checked(in_after, self.reserve_out, self.reserve_in.checked_add(in_after)?)?;
                self.reserve_in = self.reserve_in.checked_add(in_after)?;
                self.reserve_out = self.reserve_out.checked_sub(out)?;
                Some(out)
            }
        }
    }

    /// The gross input that returns `out` from this segment, rounded up.
    fn input_for(&self, out: U256) -> Option<U256> {
        let (v_in, v_out) = self.virtuals()?;
        let left = v_out.checked_sub(out).filter(|l| !l.is_zero())?;
        let net = mul_div_rounding_up(out, v_in, left)?;
        mul_div_rounding_up(net, U256::from(PPM), self.keep()?)
    }

    /// Step onto the boundary: the price, the tick and, when it is initialized,
    /// the liquidity. Returns the gas of the crossing, 0 at a word's edge.
    fn cross(&mut self) -> Option<u64> {
        let b = self.bound.take()?;
        self.sqrt = b.sqrt;
        self.tick = if self.zero_for_one { b.tick - 1 } else { b.tick };
        if !b.initialized {
            return Some(0);
        }
        let signed = if self.zero_for_one { b.net.checked_neg()? } else { b.net };
        self.liquidity = if signed >= 0 {
            self.liquidity.checked_add(signed.unsigned_abs())?
        } else {
            self.liquidity.checked_sub(signed.unsigned_abs())?
        };
        Some(self.cross_gas)
    }
}

/// The pair's `sqrt(a / b)`, Q96: above one in Q96, a trade here pays.
fn margin(a: &Run, b: &Run) -> Option<U256> {
    let roots = mul_div_checked(a.root()?, b.root()?, q96())?;
    // Scaled by 2^64: the fees' root in whole ppm would blur a small gap.
    let keep = (a.keep()?.checked_mul(b.keep()?)? << 128).integer_sqrt();
    mul_div_checked(roots, keep, U256::from(PPM) << 64)
}

/// `b / c`: the best input is this times the margin less one.
fn scale(a: &Run, b: &Run) -> Option<U256> {
    let ppm = U256::from(PPM);
    let (a_in, a_out) = a.virtuals()?;
    let (b_in, _) = b.virtuals()?;
    let denom = a.keep()?.checked_mul(b_in.checked_mul(ppm)?.checked_add(b.keep()?.checked_mul(a_out)?)?)?;
    mul_div_checked(a_in, b_in.checked_mul(ppm.checked_mul(ppm)?)?, denom)
}

/// The profit of the pair's closed-form best, boundaries ignored: an upper
/// bound on what its segment can add.
fn gain(a: &Run, b: &Run) -> Option<U256> {
    let m = margin(a, b)?;
    let Some(over) = m.checked_sub(q96()).filter(|o| !o.is_zero()) else { return Some(U256::zero()) };
    let step = mul_div_checked(scale(a, b)?, over, q96())?;
    mul_div_checked(step, over, q96())
}

/// The size to trade. `None` when there is no gap (the pair's margin is at most
/// one), a leg is empty, or the arithmetic overflows; the contract reverts on
/// the last. `size` is zero only with `ladder_end`.
pub fn walk(legs: [WalkLeg; 2], cap: U256, price: WalkPrice, max_steps: u32) -> Option<Walked> {
    let (mut a, mut b) = (Run::new(&legs[0])?, Run::new(&legs[1])?);
    if margin(&a, &b)? <= q96() {
        return None;
    }
    let mut out =
        Walked { size: U256::zero(), steps: 0, capped: false, truncated: false, ladder_end: false, cross_gas: 0 };
    if !a.load()? || !b.load()? {
        out.ladder_end = true;
        return Some(out);
    }
    let mut total = U256::zero();
    loop {
        let over = margin(&a, &b)?.checked_sub(q96())?;
        let best = mul_div_checked(scale(&a, &b)?, over, q96())?;
        let clamped = total.checked_add(best)? >= cap;
        let dx = if clamped { cap.checked_sub(total)? } else { best };
        let (in_a, out_a) = a.boundary_amounts()?;
        let (in_b, _) = b.boundary_amounts()?;
        let hit_a = dx >= in_a;
        let reach = if hit_a { out_a } else { a.clone().forward(dx)? };
        let hit_b = reach >= in_b;
        if !hit_a && !hit_b {
            total = total.checked_add(dx)?;
            out.capped = clamped;
            break;
        }
        // The nearer boundary: pool a's, or pool b's when its input arrives first.
        let a_first = hit_a && (!hit_b || out_a <= in_b);
        if a_first {
            b.forward(out_a)?;
            total = total.checked_add(in_a)?;
        } else {
            let back = a.input_for(in_b)?;
            a.forward(back)?;
            total = total.checked_add(back)?;
        }
        out.steps += 1;
        if out.steps >= max_steps {
            out.truncated = true;
            total = total.saturating_sub(U256::one());
            break;
        }
        let crossed = if a_first { &mut a } else { &mut b };
        let gas = crossed.cross()?;
        let cost = to_start(U256::from(gas).checked_mul(price.wei_per_gas)?, price.rate)?;
        // No liquidity left in a segment has no gain either: this stops there too.
        if gain(&a, &b)? <= cost {
            total = total.saturating_sub(U256::one());
            break;
        }
        out.cross_gas = out.cross_gas.saturating_add(gas);
        let crossed = if a_first { &mut a } else { &mut b };
        if !crossed.load()? {
            out.ladder_end = true;
            break;
        }
    }
    out.size = total;
    (!out.size.is_zero() || out.ladder_end).then_some(out)
}

fn hop_quote(leg: &WalkLeg, amount: U256) -> Option<U256> {
    match *leg {
        WalkLeg::Cl { state, ladder, zero_for_one, .. } => {
            let q = quote_exact_input_multi_tick(state, ladder, amount, zero_for_one, u32::MAX)?;
            (!q.exhausted && q.amount_in_consumed == amount).then_some(q.amount_out)
        }
        WalkLeg::Reserves { .. } => Run::new(leg)?.forward(amount),
    }
}

/// The exact outputs at `size`, hop 1's then hop 2's, as the pools would pay.
pub fn quote(legs: [WalkLeg; 2], size: U256) -> Option<[U256; 2]> {
    let mid = hop_quote(&legs[0], size)?;
    Some([mid, hop_quote(&legs[1], mid)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: u128 = 1_000_000_000_000_000_000_000_000; // 1e24
    const FREE: WalkPrice = WalkPrice { wei_per_gas: U256::zero(), rate: None };

    fn u(v: u128) -> U256 {
        U256::from(v)
    }

    fn cl(tick: i32, liquidity: u128, fee_ppm: u32, spacing: i32) -> ClPoolState {
        ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity,
            tick,
            tick_spacing: spacing,
            fee_ppm,
            balance0: None,
            balance1: None,
        }
    }

    fn ladder(ticks: Vec<(i32, i128)>) -> TickLadder {
        TickLadder::new(ticks, crate::cl_math::MIN_TICK, crate::cl_math::MAX_TICK)
    }

    fn sell<'a>(state: &'a ClPoolState, ladder: &'a TickLadder) -> WalkLeg<'a> {
        WalkLeg::Cl { state, ladder, zero_for_one: true, cross_gas: 30_000 }
    }

    fn buy<'a>(state: &'a ClPoolState, ladder: &'a TickLadder) -> WalkLeg<'a> {
        WalkLeg::Cl { state, ladder, zero_for_one: false, cross_gas: 30_000 }
    }

    /// What `size` nets through both pools, as a signed float.
    fn net(legs: [WalkLeg; 2], size: U256) -> f64 {
        let [_, out] = quote(legs, size).expect("a quote");
        out.as_u128() as f64 - size.as_u128() as f64
    }

    /// `w` is a best size: nudging it either way, by 0.5%, nets no more.
    fn is_best(legs: [WalkLeg; 2], w: U256) {
        let at = net(legs, w);
        let nudge = w / 200;
        assert!(at > 0.0, "the size nets {at}");
        assert!(net(legs, w + nudge) <= at, "more input nets more");
        assert!(net(legs, w - nudge) <= at, "less input nets more");
    }

    #[test]
    fn no_gap_is_none() {
        let empty = ladder(vec![]);
        let (a, b) = (cl(6_000, L, 500, 60), cl(6_000, L, 500, 60));
        assert_eq!(walk([sell(&a, &empty), buy(&b, &empty)], U256::MAX, FREE, MAX_STEPS), None);
    }

    #[test]
    fn fees_alone_close_a_small_gap() {
        let empty = ladder(vec![]);
        // 10 ticks of gap against 60 bps of fees.
        let (a, b) = (cl(6_010, L, 3_000, 60), cl(6_000, L, 3_000, 60));
        assert_eq!(walk([sell(&a, &empty), buy(&b, &empty)], U256::MAX, FREE, MAX_STEPS), None);
    }

    #[test]
    fn single_segment_matches_the_closed_form() {
        let empty = ladder(vec![]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let legs = [sell(&a, &empty), buy(&b, &empty)];
        let w = walk(legs, U256::MAX, FREE, MAX_STEPS).unwrap();
        assert_eq!(w.steps, 0);
        assert!(!w.capped && !w.truncated && !w.ladder_end);

        let root = |t: i32| 1.0001f64.powf(f64::from(t) / 2.0);
        let (ra, rb) = (root(6_100), root(6_000));
        let l = L as f64;
        let g = 1.0 - 500.0 / 1e6;
        let (x1, y1, x2, y2) = (l / ra, l * ra, l * rb, l / rb);
        let (a2, b2, c) = (g * g * y1 * y2, x1 * x2, g * (x2 + g * y1));
        let expected = ((a2 * b2).sqrt() - b2) / c;
        let got = w.size.as_u128() as f64;
        assert!((got - expected).abs() / expected < 1e-9, "{got} against {expected}");
        is_best(legs, w.size);
    }

    #[test]
    fn the_cap_holds() {
        let empty = ladder(vec![]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let legs = [sell(&a, &empty), buy(&b, &empty)];
        let free = walk(legs, U256::MAX, FREE, MAX_STEPS).unwrap();
        let cap = free.size / 3;
        let w = walk(legs, cap, FREE, MAX_STEPS).unwrap();
        assert_eq!(w.size, cap);
        assert!(w.capped);
        assert!(net(legs, cap) > 0.0);
    }

    #[test]
    fn a_crossing_is_taken_when_it_pays() {
        // Going down through 6,060 adds liquidity: a position whose top is there.
        let more = ladder(vec![(6_060, -(L as i128))]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let legs = [sell(&a, &more), buy(&b, &none)];
        let w = walk(legs, U256::MAX, FREE, MAX_STEPS).unwrap();
        assert_eq!(w.steps, 1);
        assert_eq!(w.cross_gas, 30_000);
        is_best(legs, w.size);
    }

    #[test]
    fn a_crossing_is_refused_when_it_does_not_pay() {
        let more = ladder(vec![(6_060, -(L as i128))]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let legs = [sell(&a, &more), buy(&b, &none)];
        // A gas price at which the gain past the tick can't pay for crossing it.
        let dear = WalkPrice { wei_per_gas: U256::exp10(18), rate: None };
        let w = walk(legs, U256::MAX, dear, MAX_STEPS).unwrap();
        let at = get_amount0_delta(get_sqrt_ratio_at_tick(6_060).unwrap(), a.sqrt_price_x96, L, true).unwrap();
        let gross = mul_div_rounding_up(at, U256::from(PPM), U256::from(PPM - 500)).unwrap();
        assert_eq!(w.size, gross - 1, "one short of the boundary");
        assert_eq!(w.cross_gas, 0);
        let q = quote_exact_input_multi_tick(&a, &more, w.size, true, 8).unwrap();
        assert_eq!(q.ticks_crossed, 0, "the real swap crosses nothing");
    }

    #[test]
    fn negative_net_going_down_deepens_and_positive_narrows() {
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let size = |l: &TickLadder| walk([sell(&a, l), buy(&b, &none)], U256::MAX, FREE, MAX_STEPS).unwrap().size;
        let flat = size(&none);
        let deeper = ladder(vec![(6_060, -(L as i128))]);
        let thinner = ladder(vec![(6_060, (L / 2) as i128)]);
        assert!(size(&deeper) > flat);
        assert!(size(&thinner) < flat);
        for l in [&deeper, &thinner] {
            is_best([sell(&a, l), buy(&b, &none)], size(l));
        }
    }

    #[test]
    fn going_up_the_sign_is_the_other_way() {
        // The pools meet near 6,050, so the rising pool must find its tick below that.
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 10));
        let none = ladder(vec![]);
        let size = |l: &TickLadder| walk([sell(&a, &none), buy(&b, l)], U256::MAX, FREE, MAX_STEPS).unwrap().size;
        let deeper = ladder(vec![(6_030, L as i128)]);
        assert!(size(&deeper) > size(&none));
        is_best([sell(&a, &none), buy(&b, &deeper)], size(&deeper));
    }

    #[test]
    fn a_tick_going_to_zero_liquidity_stops_the_walk() {
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let end = ladder(vec![(6_060, L as i128)]);
        let w = walk([sell(&a, &end), buy(&b, &none)], U256::MAX, FREE, MAX_STEPS).unwrap();
        let q = quote_exact_input_multi_tick(&a, &end, w.size, true, 8).unwrap();
        assert_eq!(q.ticks_crossed, 0);
        assert!(!q.exhausted);
    }

    #[test]
    fn word_edges_are_steps_and_cost_no_crossing_gas() {
        let none = ladder(vec![]);
        // Spacing 1: a word is 256 ticks, so a falling pool stops at 256 and a
        // rising one at 255.
        let (a, b) = (cl(300, L, 100, 1), cl(200, L, 100, 1));
        let legs = [sell(&a, &none), buy(&b, &none)];
        let w = walk(legs, U256::MAX, FREE, MAX_STEPS).unwrap();
        assert!(w.steps >= 1, "{w:?}");
        assert_eq!(w.cross_gas, 0);
        is_best(legs, w.size);
    }

    #[test]
    fn several_bitmap_words_and_negative_ticks() {
        let none = ladder(vec![]);
        let (a, b) = (cl(-200, L, 100, 1), cl(-1_000, L, 100, 1));
        let legs = [sell(&a, &none), buy(&b, &none)];
        let w = walk(legs, U256::MAX, FREE, MAX_STEPS_LIMIT).unwrap();
        assert!(w.steps >= 3, "{w:?}");
        assert!(!w.truncated);
        is_best(legs, w.size);
    }

    #[test]
    fn max_steps_truncates_below_the_boundary() {
        let none = ladder(vec![]);
        let (a, b) = (cl(-200, L, 100, 1), cl(-1_000, L, 100, 1));
        let legs = [sell(&a, &none), buy(&b, &none)];
        let full = walk(legs, U256::MAX, FREE, MAX_STEPS_LIMIT).unwrap();
        let short = walk(legs, U256::MAX, FREE, 1).unwrap();
        assert!(short.truncated);
        assert_eq!(short.steps, 1);
        assert!(short.size < full.size);
        assert!(net(legs, short.size) > 0.0, "still profitable");
    }

    #[test]
    fn max_steps_stops_short_of_a_crossing() {
        let more = ladder(vec![(6_060, -(L as i128))]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let w = walk([sell(&a, &more), buy(&b, &none)], U256::MAX, FREE, 1).unwrap();
        assert!(w.truncated);
        let q = quote_exact_input_multi_tick(&a, &more, w.size, true, 8).unwrap();
        assert_eq!(q.ticks_crossed, 0, "the real swap crosses nothing");
        assert!(!q.ended_on_tick_boundary);
    }

    #[test]
    fn the_cap_is_never_passed() {
        // Caps either side of where the second pool's boundary falls: its input
        // comes from a rounded-up inverse, the one path that could overshoot.
        let none = ladder(vec![]);
        for l in [L, 40_000_000_000, 7_000_000, 900_000] {
            let tick = ladder(vec![(6_030, l as i128)]);
            let (a, b) = (cl(6_100, l, 500, 60), cl(6_000, l, 500, 10));
            let legs = [sell(&a, &none), buy(&b, &tick)];
            let target = get_amount1_delta(b.sqrt_price_x96, get_sqrt_ratio_at_tick(6_030).unwrap(), l, true).unwrap();
            let near = Run::new(&legs[0]).unwrap().input_for(target).unwrap();
            for offset in 0..600u64 {
                let cap = (near + U256::from(offset)).saturating_sub(U256::from(300u64));
                if let Some(w) = walk(legs, cap, FREE, MAX_STEPS) {
                    assert!(w.size <= cap, "{} over {cap}", w.size);
                }
                assert!(cap.is_zero() || walk(legs, cap, FREE, MAX_STEPS).is_some(), "no size at a cap of {cap}, liquidity {l}");
            }
        }
    }

    #[test]
    fn both_directions_mirror() {
        let none = ladder(vec![]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let one = walk([sell(&a, &none), buy(&b, &none)], U256::MAX, FREE, MAX_STEPS).unwrap();
        // The same two curves with the tokens' order swapped, which turns each
        // price over: the size is measured in the same token.
        let (c, d) = (cl(-6_100, L, 500, 60), cl(-6_000, L, 500, 60));
        let two = walk([buy(&c, &none), sell(&d, &none)], U256::MAX, FREE, MAX_STEPS).unwrap();
        let (x, y) = (one.size.as_u128() as f64, two.size.as_u128() as f64);
        assert!((x - y).abs() / x < 1e-6, "{x} against {y}");
    }

    #[test]
    fn a_reserves_leg_has_no_boundary() {
        let none = ladder(vec![]);
        let a = cl(6_100, L, 500, 60);
        let root = 1.0001f64.powf(3_000.0);
        let reserves = WalkLeg::Reserves {
            reserve_in: u((L as f64 * root) as u128),
            reserve_out: u((L as f64 / root) as u128),
            fee_ppm: 500,
        };
        let legs = [sell(&a, &none), reserves];
        let w = walk(legs, U256::MAX, FREE, MAX_STEPS).unwrap();
        assert_eq!(w.steps, 0);
        is_best(legs, w.size);

        let first = WalkLeg::Reserves {
            reserve_in: u((L as f64 / root) as u128),
            reserve_out: u((L as f64 * root) as u128),
            fee_ppm: 500,
        };
        let c = cl(5_900, L, 500, 60);
        let legs = [first, buy(&c, &none)];
        is_best(legs, walk(legs, U256::MAX, FREE, MAX_STEPS).unwrap().size);
    }

    #[test]
    fn two_reserves_legs_walk_nothing() {
        let (x, y) = (u(1_000_000 * 10u128.pow(18)), u(1_010_000 * 10u128.pow(18)));
        let legs = [
            WalkLeg::Reserves { reserve_in: x, reserve_out: y, fee_ppm: 500 },
            WalkLeg::Reserves { reserve_in: x, reserve_out: y, fee_ppm: 500 },
        ];
        let w = walk(legs, U256::MAX, FREE, MAX_STEPS).unwrap();
        assert_eq!(w.steps, 0);
        is_best(legs, w.size);
    }

    #[test]
    fn a_reserves_quote_pays_as_get_amount_out() {
        // 30 of fee leaves 9,970: 9,970 × 2,000,000 / 1,009,970, floored.
        let leg = WalkLeg::Reserves { reserve_in: u(1_000_000), reserve_out: u(2_000_000), fee_ppm: 3_000 };
        assert_eq!(hop_quote(&leg, u(10_000)), Some(u(19_743)));
    }

    #[test]
    fn to_start_rounds_up_and_is_one_to_one_without_a_rate() {
        assert_eq!(to_start(u(123), None), Some(u(123)));
        // 3,000 USDC a WETH, six decimals: a wei is 3e-9 of a unit.
        let rate = Some(u(3_000_000_000));
        assert_eq!(to_start(u(1), rate), Some(u(1)));
        assert_eq!(to_start(u(333_333_333), rate), Some(u(1)));
        assert_eq!(to_start(u(333_333_334), rate), Some(u(2)));
    }

    #[test]
    fn a_usdc_price_charges_crossings_in_usdc() {
        let more = ladder(vec![(6_060, -(L as i128))]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let legs = [sell(&a, &more), buy(&b, &none)];
        let cheap = WalkPrice { wei_per_gas: u(10_000_000), rate: Some(u(3_000_000_000)) };
        assert_eq!(walk(legs, U256::MAX, cheap, MAX_STEPS).unwrap().cross_gas, 30_000);
    }

    #[test]
    fn the_ladders_end_is_flagged() {
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let short = TickLadder::new(vec![], 6_090, 7_000);
        let w = walk([sell(&a, &short), buy(&b, &none)], U256::MAX, FREE, MAX_STEPS).unwrap();
        assert!(w.ladder_end);
        assert!(w.size.is_zero());
    }

    #[test]
    fn a_ladder_ending_after_a_step_keeps_what_was_walked() {
        let (a, b) = (cl(300, L, 100, 1), cl(200, L, 100, 1));
        let none = ladder(vec![]);
        // Covers the first word's edge, at 256, and no further.
        let short = TickLadder::new(vec![], 256, 1_000);
        let w = walk([sell(&a, &short), buy(&b, &none)], U256::MAX, FREE, MAX_STEPS).unwrap();
        assert!(w.ladder_end);
        assert!(!w.size.is_zero());
    }

    #[test]
    fn an_input_exactly_at_a_boundary_ends_capped_and_filled() {
        let more = ladder(vec![(6_060, -(L as i128))]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 500, 60));
        let none = ladder(vec![]);
        let at = get_amount0_delta(get_sqrt_ratio_at_tick(6_060).unwrap(), a.sqrt_price_x96, L, true).unwrap();
        let gross = mul_div_rounding_up(at, U256::from(PPM), U256::from(PPM - 500)).unwrap();
        let legs = [sell(&a, &more), buy(&b, &none)];
        let w = walk(legs, gross, FREE, MAX_STEPS).unwrap();
        assert_eq!(w.size, gross);
        assert!(w.capped);
        assert!(quote(legs, w.size).is_some());
    }

    #[test]
    fn quote_equals_the_multi_tick_quote() {
        let more = ladder(vec![(6_060, -(L as i128)), (5_940, 300_000_000_000_000_000_000_000)]);
        let (a, b) = (cl(6_100, L, 500, 60), cl(6_000, L, 3_000, 60));
        let none = ladder(vec![]);
        let size = u(50_000_000_000_000_000_000_000);
        let [mid, out] = quote([sell(&a, &more), buy(&b, &none)], size).unwrap();
        let one = quote_exact_input_multi_tick(&a, &more, size, true, u32::MAX).unwrap();
        let two = quote_exact_input_multi_tick(&b, &none, one.amount_out, false, u32::MAX).unwrap();
        assert_eq!((mid, out), (one.amount_out, two.amount_out));
    }

    #[test]
    fn an_empty_leg_is_none() {
        let none = ladder(vec![]);
        let (a, b) = (cl(6_100, 0, 500, 60), cl(6_000, L, 500, 60));
        assert_eq!(walk([sell(&a, &none), buy(&b, &none)], U256::MAX, FREE, MAX_STEPS), None);
    }
}
