#![allow(dead_code)]
//! The size search before R27.2, verbatim: a ternary search over the whole
//! range, which spends its budget every time and promises nothing once costs
//! step. Kept as a comparator for the tests and the harness; agreement with it
//! proves nothing by itself. Nothing in the runtime calls it.

use apex_math::finite_size::{NoSize, SizedOpportunity, SizedRoute, Surplus};
use ethers_core::types::U256;

enum Probe {
    Net(Surplus),
    Refused,
    Spent,
}

pub fn ternary_best_size<R: SizedRoute + ?Sized>(
    route: &R,
    max_evaluations: u32,
    min_input: U256,
) -> Result<SizedOpportunity, NoSize> {
    let lo_bound = min_input.max(U256::one());
    let hi_bound = route.max_input();
    if hi_bound < lo_bound {
        return Err(NoSize::RangeEmpty);
    }

    let mut evaluations = 0u32;
    let mut best: Option<SizedOpportunity> = None;
    let mut evaluate = |x: U256, best: &mut Option<SizedOpportunity>| -> Probe {
        if evaluations >= max_evaluations {
            return Probe::Spent;
        }
        evaluations += 1;
        let Some(priced) = route.priced(x) else { return Probe::Refused };
        let spent = x.saturating_add(priced.cost);
        let net = if priced.output >= spent { Surplus::Gain(priced.output - spent) } else { Surplus::Loss(spent - priced.output) };
        let candidate = SizedOpportunity {
            amount_in: x,
            output: priced.output,
            net,
        };
        if best.is_none_or(|b| candidate.net > b.net) {
            *best = Some(candidate);
        }
        Probe::Net(net)
    };

    let (mut lo, mut hi) = (lo_bound, hi_bound);
    let three = U256::from(3u64);
    while hi > lo && hi - lo > U256::from(2u64) {
        let span = hi - lo;
        let m1 = lo + span / three;
        let m2 = hi - span / three;
        if m1 >= m2 {
            break;
        }
        // The lower probe first: refused, the higher one would be too.
        let f1 = match evaluate(m1, &mut best) {
            Probe::Net(f) => f,
            Probe::Refused => {
                hi = m1 - U256::one();
                continue;
            }
            Probe::Spent => break,
        };
        let f2 = match evaluate(m2, &mut best) {
            Probe::Net(f) => f,
            Probe::Refused => {
                hi = m2 - U256::one();
                continue;
            }
            Probe::Spent => break,
        };
        if f1 < f2 {
            lo = m1 + U256::one();
        } else {
            hi = m2;
        }
    }

    // Exact scan of the narrowed bracket. Bounded: the loop above reduces the
    // span geometrically, so this is a handful of points. A refusal ends it:
    // every size past it is refused too.
    let mut x = lo;
    while x <= hi {
        if !matches!(evaluate(x, &mut best), Probe::Net(_)) {
            break;
        }
        x = x.saturating_add(U256::one());
        if x == U256::zero() {
            break;
        }
    }

    match best {
        None => Err(NoSize::Unpriceable),
        Some(b) if b.net.is_gain() => Ok(b),
        Some(b) => Err(NoSize::NoProfitableSize { best: b }),
    }
}
