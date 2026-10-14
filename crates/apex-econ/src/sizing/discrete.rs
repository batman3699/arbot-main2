//! Discrete refinement — the only place a trade size is decided (§14.3,
//! INV-18).
//!
//! Takes the warm start, evaluates **integer** sizes against the exact AMM
//! arithmetic, and returns the one that will be executed. It is the only
//! function in the workspace that can mint an `apex_types::DiscreteSize`,
//! because it is the only one that can produce the `DiscreteRefined` witness
//! the constructor demands.
//!
//! # The guarantee, and why it holds by construction
//!
//! **The refined size is never worse than its start.** Not because the climb
//! is clever, but because it *starts* there and only ever moves to a strictly
//! better point. A hill climb cannot regress below its own starting point, so
//! the property is structural rather than something the climb has to get
//! right.
//!
//! It is a local guarantee and no more. A net that steps up with gas has more
//! than one peak, and a climb finds the one its start is on. Since R27.3 the
//! start is the size `apex_math::finite_size::best_size` certified, to the
//! wei ([`super::continuous`]): finding the best size is that search's job,
//! and what is left for this one is to price it and mint it
//! ([`RefineBudget::start_only`]).
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

impl RefineBudget {
    /// Price the start and mint it: no climb (R27.3).
    ///
    /// For a start that is already the best size a search could certify. Since
    /// R27.3 the warm start is `apex_math::finite_size::best_size`'s own size,
    /// handed over exactly, and that search stops only when no size can net
    /// more than its tolerance above it. A climb from there can gain no more
    /// than that tolerance, and on the capture path it cost 120 to 250 quotes,
    /// four times a candidate.
    pub fn start_only() -> Self {
        Self { max_evaluations: 1, ..Self::default() }
    }
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

/// What one probe of the climb learned.
enum Probe {
    Net(U256, Surplus),
    /// The route cannot be priced at this size, nor at any larger one.
    Refused,
    /// The evaluation budget is gone.
    Spent,
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

    // The warm start's own size, to the wei: a float could not carry it.
    // Clamped, not refused: a warm start made on another route, or on this
    // one before its range moved, is a bad suggestion and not an error. What
    // it must not do is start outside the range and price there.
    let start = continuous.at().clamp(lo, hi);
    let mut evaluations = 0u32;
    let mut evaluate = |x: U256| -> Probe {
        if evaluations >= budget.max_evaluations {
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
