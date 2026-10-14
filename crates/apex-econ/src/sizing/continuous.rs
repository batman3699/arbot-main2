//! The warm start (§14.3).
//!
//! Where [`super::discrete`] begins: the size the route's net is highest at,
//! found by `apex_math::finite_size::best_size`. It is deliberately **not** a
//! trade size, and nothing here returns a type that can reach a candidate.
//!
//! # What it was, and why it changed (R27.3)
//!
//! Until R27.3 this was a golden-section search on the net, in `f64`, from
//! the top of the route's range. A net that steps up with gas is not
//! unimodal, so that search proved nothing, and a cost step below the gross's
//! peak could leave the best size outside wherever it converged; the climb
//! that followed is local and could not find it. It also cost 62 quotes, the
//! first of them at the largest sizes the route allowed, and the climb up to
//! 256 more. `best_size` bounds every size it has not priced, and stops when
//! none can net more than its tolerance above its best (R27.2).
//!
//! # Why the float is still not a trade size
//!
//! `f64` has 53 bits of mantissa. A wei-denominated size on an 18-decimal
//! token routinely exceeds 2^53: near 18 WETH a float holds a size only to the
//! nearest 2,048 wei. The search's size is a whole number it priced, and can
//! sit just under a cost step that such rounding would cross.
//!
//! So a [`ContinuousOptimum`] shows the outside a float, for reporting, and
//! carries the exact size to [`super::discrete`] and to nothing else. It has
//! no path to a `DiscreteSize`: `refine` still mints the only one.

use apex_math::finite_size::{best_size, NoSize, Priced, SearchBudget, SizedRoute};
use ethers_core::types::U256;
use std::cell::Cell;

/// The size a route's net is highest at, as [`optimize`] found it.
///
/// Outside this module it is a float and a count: no conversion to `U256`, no
/// constructor. `refine` takes one; nothing else can turn it into a size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContinuousOptimum {
    value: f64,
    /// How many route evaluations produced it, for the §29 budget accounting.
    evaluations: u32,
    /// The size itself, to the wei: where the refinement starts.
    at: U256,
}

impl ContinuousOptimum {
    pub const fn value(&self) -> f64 {
        self.value
    }

    pub const fn evaluations(&self) -> u32 {
        self.evaluations
    }

    /// The size the search priced, exactly. For the refinement alone.
    pub(super) const fn at(&self) -> U256 {
        self.at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuousBudget {
    /// The warm start prices at most this many sizes and two (§29): what the
    /// golden section it replaced could spend. The search usually needs far
    /// fewer.
    pub iterations: u32,
    pub min_input: U256,
}

impl Default for ContinuousBudget {
    fn default() -> Self {
        Self {
            iterations: 60,
            min_input: U256::from(1_000_000_000_000u64),
        }
    }
}

/// A route, counting what it is asked to price.
struct Counted<'a, R: SizedRoute + ?Sized> {
    route: &'a R,
    quotes: Cell<u32>,
}

impl<R: SizedRoute + ?Sized> SizedRoute for Counted<'_, R> {
    fn output(&self, amount_in: U256) -> Option<U256> {
        self.route.output(amount_in)
    }
    fn fixed_cost(&self) -> U256 {
        self.route.fixed_cost()
    }
    fn max_input(&self) -> U256 {
        self.route.max_input()
    }
    fn priced(&self, amount_in: U256) -> Option<Priced> {
        self.quotes.set(self.quotes.get() + 1);
        self.route.priced(amount_in)
    }
    fn rounding(&self) -> U256 {
        self.route.rounding()
    }
}

/// The size the route's net is highest at.
///
/// `None` when the route cannot be priced anywhere in range. Note that this
/// says nothing about profitability: an unprofitable route still has a best
/// size the search priced, and deciding whether to trade it is
/// [`super::discrete`]'s job, not this one's. Conflating "where is the best
/// size" with "is it worth trading" is how a warm start turns into a decision.
pub fn optimize<R: SizedRoute + ?Sized>(
    route: &R,
    budget: ContinuousBudget,
) -> Option<ContinuousOptimum> {
    let counted = Counted { route, quotes: Cell::new(0) };
    let search = SearchBudget {
        max_evaluations: budget.iterations.saturating_add(2),
        min_input: budget.min_input,
        ..SearchBudget::default()
    };
    let best = match best_size(&counted, search) {
        Ok(found) | Err(NoSize::NoProfitableSize { best: found }) => found,
        Err(NoSize::Unpriceable | NoSize::RangeEmpty) => return None,
    };
    Some(ContinuousOptimum { value: u256_to_f64(best.amount_in), evaluations: counted.quotes.get(), at: best.amount_in })
}

/// `U256` -> `f64`, losing precision above 2^53 exactly as described in the
/// module docs. Saturates rather than producing infinity.
fn u256_to_f64(value: U256) -> f64 {
    // `to_string` then parse is exact to f64's precision and cannot overflow
    // the way a limb-wise shift-and-add can.
    value.to_string().parse::<f64>().unwrap_or(f64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The optimum carries no route to a size. This is the whole design, and
    /// it is asserted by what the type does NOT have: no `From`, no `into`, no
    /// public field. The compile-fail fixture in `tests/` covers the other
    /// half.
    #[test]
    fn a_continuous_optimum_exposes_only_a_float() {
        let o = ContinuousOptimum {
            value: 1.5,
            evaluations: 3,
            at: U256::one(),
        };
        assert_eq!(o.value(), 1.5);
        assert_eq!(o.evaluations(), 3);
    }

    /// The precision claim in the module docs, as a test rather than a
    /// statement: above 2^53 an f64 cannot distinguish adjacent wei.
    #[test]
    fn an_f64_cannot_represent_every_wei_amount() {
        let big = U256::from(1u64) << 60;
        let one_more = big + U256::one();
        assert_ne!(big, one_more);
        assert_eq!(
            u256_to_f64(big),
            u256_to_f64(one_more),
            "f64 collapses two distinct wei amounts, which is why the continuous \
             stage cannot be the one that decides"
        );
    }
}
