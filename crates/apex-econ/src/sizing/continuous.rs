//! The continuous warm start (§14.3).
//!
//! A real-valued maximiser of net profit, used to tell [`super::discrete`]
//! where to begin. It is deliberately **not** trusted for anything else, and
//! nothing here returns a type that can reach a candidate.
//!
//! # Why an f64 optimum is not a trade size
//!
//! Three reasons, in increasing order of how quietly they bite.
//!
//! 1. No AMM accepts a fractional wei, so the answer has to be rounded
//!    regardless.
//! 2. `f64` has 53 bits of mantissa. A wei-denominated size on an 18-decimal
//!    token routinely exceeds 2^53, so the continuous optimum cannot even
//!    *represent* every integer in its own search range — it is quantised, and
//!    quantised more coarsely the larger the trade.
//! 3. The objective is evaluated in f64 here and in exact integer arithmetic
//!    there. Those disagree, and the disagreement is not symmetric: integer
//!    truncation in the AMM always rounds against the trader.
//!
//! So this returns a `ContinuousOptimum`, which carries no path to a
//! `DiscreteSize`.

use apex_math::finite_size::SizedRoute;
use ethers_core::types::U256;

/// An approximate maximiser, in the input token's smallest unit, as a real
/// number.
///
/// A plain newtype over `f64` with no conversion to `U256` and no constructor
/// outside this module. `refine` takes one and reads [`Self::value`]; nothing
/// else can turn it into a size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContinuousOptimum {
    value: f64,
    /// How many route evaluations produced it, for the §29 budget accounting.
    evaluations: u32,
}

impl ContinuousOptimum {
    pub const fn value(&self) -> f64 {
        self.value
    }

    pub const fn evaluations(&self) -> u32 {
        self.evaluations
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuousBudget {
    /// Golden-section iterations. Each halves the bracket by ~0.618, so 60 is
    /// ample for any range a trade occupies and still bounded (§29).
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

/// Golden-section search for the net-profit maximiser.
///
/// Net profit is concave — a concave output minus a linear cost — so it is
/// unimodal and golden-section converges without needing a derivative, which
/// matters because the objective is an AMM quote rather than a formula.
///
/// `None` when the route cannot be priced anywhere in range. Note that this
/// says nothing about profitability: an unprofitable route still has a
/// maximiser, and deciding whether to trade it is [`super::discrete`]'s job,
/// not this one's. Conflating "where is the best size" with "is it worth
/// trading" is how a warm start turns into a decision.
pub fn optimize<R: SizedRoute + ?Sized>(
    route: &R,
    budget: ContinuousBudget,
) -> Option<ContinuousOptimum> {
    let lo_u = budget.min_input.max(U256::one());
    let hi_u = route.max_input();
    if hi_u < lo_u {
        return None;
    }

    let (mut lo, mut hi) = (u256_to_f64(lo_u), u256_to_f64(hi_u));
    let mut evaluations = 0u32;
    let mut net = |x: f64| -> Option<f64> {
        let amount = f64_to_u256(x)?;
        if amount < lo_u || amount > hi_u {
            return None;
        }
        evaluations += 1;
        let out = route.output(amount)?;
        Some(u256_to_f64(out) - x - u256_to_f64(route.fixed_cost()))
    };

    // 1 / phi.
    const INV_PHI: f64 = 0.618_033_988_749_894_9;
    let mut c = hi - (hi - lo) * INV_PHI;
    let mut d = lo + (hi - lo) * INV_PHI;
    let (mut fc, mut fd) = (net(c)?, net(d)?);

    for _ in 0..budget.iterations {
        if hi - lo <= 1.0 {
            break;
        }
        if fc > fd {
            hi = d;
            d = c;
            fd = fc;
            c = hi - (hi - lo) * INV_PHI;
            let Some(v) = net(c) else { break };
            fc = v;
        } else {
            lo = c;
            c = d;
            fc = fd;
            d = lo + (hi - lo) * INV_PHI;
            let Some(v) = net(d) else { break };
            fd = v;
        }
    }

    Some(ContinuousOptimum {
        value: if fc > fd { c } else { d },
        evaluations,
    })
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
        };
        assert_eq!(o.value(), 1.5);
        assert_eq!(o.evaluations(), 3);
    }

    #[test]
    fn f64_conversion_refuses_what_is_not_a_size() {
        assert_eq!(f64_to_u256(-1.0), None);
        assert_eq!(f64_to_u256(f64::NAN), None);
        assert_eq!(f64_to_u256(f64::INFINITY), None);
        assert_eq!(f64_to_u256(0.0), Some(U256::zero()));
        assert_eq!(f64_to_u256(1e18), Some(U256::from(1_000_000_000_000_000_000u128)));
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
