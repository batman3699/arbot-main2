#![allow(dead_code)]
//! The economics stage's sizing before R27.3, verbatim: a golden-section
//! search on the net in `f64`, then a hill climb from the nearest whole
//! number. Kept as a comparator for the tests and the harness. It promises
//! nothing once costs step, and nothing in the runtime calls it.

use apex_math::finite_size::{SizedRoute, Surplus};
use ethers_core::types::U256;

/// What one probe of the climb learned.
enum Probe {
    Net(U256, Surplus),
    Refused,
    Spent,
}

/// The golden section's answer, and how many quotes it took.
pub fn golden<R: SizedRoute + ?Sized>(route: &R, iterations: u32, min_input: U256) -> Option<(f64, u32)> {
    let lo_u = min_input.max(U256::one());
    let hi_u = route.max_input();
    if hi_u < lo_u {
        return None;
    }

    let (mut lo, mut hi) = (u256_to_f64(lo_u), u256_to_f64(hi_u));
    let mut evaluations = 0u32;
    // A size the route refuses scores below every size it prices. A
    // concentrated pool's quote runs off its proven ladder, or past its tick
    // limit, beyond some size, and every larger size is refused too — so the
    // section closes on what the route can price rather than stopping at the
    // first refusal, which for a pool holding far more than it can price is the
    // first probe.
    let mut net = |x: f64| -> f64 {
        let Some(amount) = f64_to_u256(x) else { return f64::NEG_INFINITY };
        if amount < lo_u || amount > hi_u {
            return f64::NEG_INFINITY;
        }
        evaluations += 1;
        // Each size at its own cost: gas grows with the ticks a size crosses.
        route
            .priced(amount)
            .map_or(f64::NEG_INFINITY, |p| u256_to_f64(p.output) - x - u256_to_f64(p.cost))
    };

    // 1 / phi.
    const INV_PHI: f64 = 0.618_033_988_749_894_9;
    let mut c = hi - (hi - lo) * INV_PHI;
    let mut d = lo + (hi - lo) * INV_PHI;
    let (mut fc, mut fd) = (net(c), net(d));
    // Both probes refused: what the route prices lies below the lower one.
    // Narrow until a probe prices, within the same iteration budget.
    let mut narrowing = iterations;
    while fc == f64::NEG_INFINITY && narrowing > 0 && hi - lo > 1.0 {
        narrowing -= 1;
        hi = c;
        c = hi - (hi - lo) * INV_PHI;
        d = lo + (hi - lo) * INV_PHI;
        fc = net(c);
        fd = net(d);
    }

    for _ in 0..iterations {
        if hi - lo <= 1.0 {
            break;
        }
        if fc > fd {
            hi = d;
            d = c;
            fd = fc;
            c = hi - (hi - lo) * INV_PHI;
            fc = net(c);
        } else {
            lo = c;
            c = d;
            fc = fd;
            d = lo + (hi - lo) * INV_PHI;
            fd = net(d);
        }
    }

    let (value, best) = if fc > fd { (c, fc) } else { (d, fd) };
    (best > f64::NEG_INFINITY).then_some((value, evaluations))
}

/// The pair as `LiveEconomics` ran it: 60 iterations, then a climb of up to
/// 256 quotes in steps from one token down. The size, its net, and the quotes
/// both took.
pub fn golden_then_climb<R: SizedRoute + ?Sized>(route: &R, min_input: U256) -> Option<(U256, Surplus, u32)> {
    let (value, spent) = golden(route, 60, min_input)?;
    let (max_evaluations, max_step) = (256u32, U256::from(1_000_000_000_000_000_000u128));
    let lo = U256::one();
    let hi = route.max_input();
    if hi < lo {
        return None;
    }

    let start = nearest_integer(value, lo, hi);
    let mut evaluations = 0u32;
    let mut evaluate = |x: U256| -> Probe {
        if evaluations >= max_evaluations {
            return Probe::Spent;
        }
        evaluations += 1;
        // Each size at its own cost: gas grows with the ticks a size crosses.
        let Some(priced) = route.priced(x) else { return Probe::Refused };
        let cost = x.saturating_add(priced.cost);
        Probe::Net(
            priced.output,
            if priced.output >= cost {
                Surplus::Gain(priced.output - cost)
            } else {
                Surplus::Loss(cost - priced.output)
            },
        )
    };

    // The starting point. Everything after this can only improve on it.
    let Probe::Net(mut best_out, mut best_net) = evaluate(start) else { return None };
    let mut best_amount = start;

    let mut step = max_step.min(hi);
    while step > U256::zero() {
        let mut moved = true;
        while moved {
            moved = false;
            for candidate in [
                best_amount.checked_add(step).filter(|x| *x <= hi),
                best_amount.checked_sub(step).filter(|x| *x >= lo),
            ]
            .into_iter()
            .flatten()
            {
                let (out, net) = match evaluate(candidate) {
                    Probe::Net(out, net) => (out, net),
                    // Past what the route can price: no better, and every
                    // larger size is refused too, so the climb goes on below
                    // it. Until 2026-10-05 a refusal ended the climb.
                    Probe::Refused => continue,
                    // The budget is gone: the climb stops where it is, which is
                    // still at least as good as the warm start.
                    Probe::Spent => {
                        step = U256::zero();
                        moved = false;
                        break;
                    }
                };
                if net > best_net {
                    best_amount = candidate;
                    best_out = out;
                    best_net = net;
                    moved = true;
                }
            }
        }
        if step == U256::zero() {
            break;
        }
        step /= U256::from(2u64);
    }

    let _ = best_out;
    Some((best_amount, best_net, spent + evaluations))
}

/// `U256` -> `f64`, losing precision above 2^53 exactly as described in the
/// module docs. Saturates rather than producing infinity.
fn u256_to_f64(value: U256) -> f64 {
    // `to_string` then parse is exact to f64's precision and cannot overflow
    // the way a limb-wise shift-and-add can.
    value.to_string().parse::<f64>().unwrap_or(f64::MAX)
}

/// `f64` -> `U256`. `None` for negative, NaN or infinite input: those are not
/// trade sizes, and rounding one to zero would look like a refusal rather than
/// the bug it is.
fn f64_to_u256(value: f64) -> Option<U256> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    U256::from_dec_str(&format!("{:.0}", value)).ok()
}


/// Round a continuous value to the nearest integer in `[lo, hi]`.
///
/// Clamping rather than refusing: a warm start outside the range is a bad
/// suggestion, not an error, and the climb recovers from it. What it must not
/// do is start outside the range and evaluate there.
fn nearest_integer(value: f64, lo: U256, hi: U256) -> U256 {
    if !value.is_finite() || value <= 0.0 {
        return lo;
    }
    let rounded = value.round();
    U256::from_dec_str(&format!("{rounded:.0}"))
        .unwrap_or(hi)
        .clamp(lo, hi)
}

