//! Discrete refinement — the only place a trade size is decided (§14.3,
//! INV-18).
//!
//! Takes the continuous warm start, evaluates **integer** sizes against the
//! exact AMM arithmetic, and returns the one that will be executed. It is the
//! only function in the workspace that can mint an `apex_types::DiscreteSize`,
//! because it is the only one that can produce the `DiscreteRefined` witness
//! the constructor demands.
//!
//! # The guarantee, and why it holds by construction
//!
//! **The refined size is never worse than the nearest integer to the continuous
//! optimum.** Not because the search is clever, but because it *starts* there
//! and only ever moves to a strictly better point. A hill climb cannot regress
//! below its own starting point, so the property is structural rather than
//! something the search has to get right.
//!
//! That framing matters because the alternative — "search the range and trust
//! it beats the warm start" — is a claim about the optimiser, and optimisers on
//! a lattice with truncating integer arithmetic are exactly where quiet
//! regressions live. Net profit is concave, so the climb also finds the global
//! integer optimum; but the guarantee does not depend on that being true.
//!
//! # `None` means no size, not zero
//!
//! A route with no profitable integer size returns `None`. Returning
//! `Some(zero)` would be a size — one that encodes to a real transaction,
//! pays gas, and trades nothing.

use super::continuous::ContinuousOptimum;
use apex_math::finite_size::{SizedRoute, Surplus};
use apex_types::candidate::{DiscreteRefined, DiscreteSize};
use apex_types::compat::u256_to_alloy;
use ethers_core::types::U256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefineBudget {
    /// Ceiling on exact route evaluations (§29). The climb is O(distance to
    /// the optimum), which is small from a good warm start and bounded here
    /// when the warm start is bad.
    pub max_evaluations: u32,
    /// The largest step the climb will take before halving. Sizes span many
    /// orders of magnitude, so a fixed step either crawls or overshoots.
    pub max_step: U256,
}

impl Default for RefineBudget {
    fn default() -> Self {
        Self {
            max_evaluations: 256,
            max_step: U256::from(1_000_000_000_000_000_000u128), // 1 token
        }
    }
}

/// What the refinement found, including the losing cases.
///
/// The search result is reported even when it is not profitable, because
/// "the best size loses 4 wei" and "the route could not be priced" are
/// different facts and the missed-opportunity accounting (§27) needs to tell
/// them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refinement {
    pub size: Option<DiscreteSize>,
    pub amount: U256,
    pub output: U256,
    pub net: Surplus,
    pub evaluations: u32,
}

/// Refine a continuous optimum into an executable integer size.
///
/// `None` when the route cannot be priced at the starting point at all.
pub fn refine_detailed<R: SizedRoute + ?Sized>(
    route: &R,
    continuous: ContinuousOptimum,
    budget: RefineBudget,
) -> Option<Refinement> {
    let lo = U256::one();
    let hi = route.max_input();
    if hi < lo {
        return None;
    }

    let start = nearest_integer(continuous.value(), lo, hi);
    let mut evaluations = 0u32;
    let mut evaluate = |x: U256| -> Option<(U256, Surplus)> {
        if evaluations >= budget.max_evaluations {
            return None;
        }
        evaluations += 1;
        let out = route.output(x)?;
        let cost = x.saturating_add(route.fixed_cost());
        Some((
            out,
            if out >= cost {
                Surplus::Gain(out - cost)
            } else {
                Surplus::Loss(cost - out)
            },
        ))
    };

    // The starting point. Everything after this can only improve on it.
    let (mut best_out, mut best_net) = evaluate(start)?;
    let mut best_amount = start;

    let mut step = budget.max_step.min(hi);
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
                let Some((out, net)) = evaluate(candidate) else {
                    // Budget exhausted or the route refused this size. Either
                    // way the climb stops where it is, which is still at least
                    // as good as the warm start.
                    step = U256::zero();
                    moved = false;
                    break;
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

    Some(Refinement {
        // The single minting site in the workspace. `no_unearned_discrete_size.sh`
        // fails the build if `DiscreteRefined::new()` appears anywhere else.
        size: best_net
            .is_gain()
            .then(|| DiscreteSize::from_refinement(u256_to_alloy(best_amount), DiscreteRefined::new())),
        amount: best_amount,
        output: best_out,
        net: best_net,
        evaluations,
    })
}

/// The §14.3 entry point: a size, or nothing.
pub fn refine<R: SizedRoute + ?Sized>(
    route: &R,
    continuous: ContinuousOptimum,
    budget: RefineBudget,
) -> Option<DiscreteSize> {
    refine_detailed(route, continuous, budget)?.size
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_is_clamped_into_range() {
        let lo = U256::from(10u64);
        let hi = U256::from(100u64);
        assert_eq!(nearest_integer(-5.0, lo, hi), lo);
        assert_eq!(nearest_integer(f64::NAN, lo, hi), lo);
        assert_eq!(nearest_integer(1e30, lo, hi), hi);
        assert_eq!(nearest_integer(42.4, lo, hi), U256::from(42u64));
        assert_eq!(nearest_integer(42.6, lo, hi), U256::from(43u64));
    }
}
