//! Tier 0 — analytic screening (§20, §15).
//!
//! # Zero RPC is a property of the signature, not of the implementation
//!
//! Task 4.1 asks for a test asserting Tier 0 makes no RPC call, checked by a
//! mock provider recording zero requests. That test exists in
//! `tests/tier0.rs`, and it is the weaker half.
//!
//! The stronger half is here: [`screen`] is a plain `fn` that takes no
//! provider, no client, no handle, and is not `async`. **There is nothing to
//! make a call with, and no point at which to await one.** A future change
//! that wanted to reach the network would have to alter the signature, which
//! is a visible act rather than an added line. `scripts/ci/tier0_is_pure.sh`
//! fails the build if this module ever names a provider type or becomes
//! `async`.
//!
//! # What Tier 0 is for
//!
//! Rejecting candidates that cannot pay for themselves, before anything
//! expensive happens. It is allowed to be wrong in one direction only: a
//! candidate it admits may still fail at a higher tier, but a candidate it
//! rejects must be one no tier could have saved. So every comparison here uses
//! the **conservative** total — p99 gas and the full failure cost — and every
//! rounding goes against the trade.

use apex_types::cost::TotalExecutionCost;
use apex_types::sim::SimulationTier;

/// Everything Tier 0 needs. Note what is absent: no provider, no state handle,
/// no block number — screening is arithmetic on a candidate that has already
/// been priced.
#[derive(Clone, Debug, PartialEq)]
pub struct Tier0Input {
    /// Gross return minus DEX fees, in wei of the input token. Signed: a
    /// candidate can arrive already negative and still be worth recording.
    pub gross_profit_wei: i128,
    pub cost: TotalExecutionCost,
    /// The gas price the conservative total is evaluated at.
    pub gas_price_wei: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier0Verdict {
    /// Worth the cost of a higher tier. Not a prediction that it will succeed.
    Escalate { margin_wei: i128 },
    /// Cannot pay for itself under the conservative total.
    Reject { shortfall_wei: u128 },
}

impl Tier0Verdict {
    pub const fn escalates(&self) -> bool {
        matches!(self, Self::Escalate { .. })
    }
}

/// Screen a candidate against the cost of executing it.
///
/// Pure, synchronous, and provider-free. See the module docs.
pub fn screen(input: &Tier0Input) -> Tier0Verdict {
    let conservative = input.cost.conservative_total(input.gas_price_wei);
    // The comparison is done in i128 so a cost above i128::MAX -- which would
    // be a malformed cost, not an expensive trade -- rejects rather than
    // wrapping into a bargain.
    let cost = i128::try_from(conservative).unwrap_or(i128::MAX);
    let margin = input.gross_profit_wei.saturating_sub(cost);
    if margin > 0 {
        Tier0Verdict::Escalate { margin_wei: margin }
    } else {
        Tier0Verdict::Reject {
            shortfall_wei: margin.unsigned_abs(),
        }
    }
}

/// The tier this module implements, for the ladder.
pub const TIER: SimulationTier = SimulationTier::Tier0Analytic;

#[cfg(test)]
mod tests {
    use super::*;
    use apex_types::cost::{GasDistribution, GasLimit, GasUsed};

    fn cost(l1: u128) -> TotalExecutionCost {
        TotalExecutionCost {
            l2_execution_fee: 0,
            l1_data_fee: l1,
            priority_fee: 0,
            builder_payment: 0,
            sequencer_payment: 0,
            flash_fee: 0,
            dex_fees: 0,
            expected_failure_cost: 0,
            calldata_bytes: 400,
            compressed_data_estimate: 300,
            gas_limit: GasLimit(250_000),
            gas_used_distribution: GasDistribution {
                p50: GasUsed(180_000),
                p90: GasUsed(210_000),
                p99: GasUsed(240_000),
                max_observed: GasUsed(249_000),
            },
        }
    }

    fn input(gross: i128, l1: u128) -> Tier0Input {
        Tier0Input {
            gross_profit_wei: gross,
            cost: cost(l1),
            gas_price_wei: 10_000_000, // 0.01 gwei
        }
    }

    /// Below the conservative total is a rejection, and the shortfall is
    /// reported rather than being an unexplained "no".
    #[test]
    fn a_candidate_below_the_conservative_total_is_rejected() {
        let i = input(1_000_000_000_000, 18_000_000_000_000);
        let total = i.cost.conservative_total(i.gas_price_wei);
        assert_eq!(
            screen(&i),
            Tier0Verdict::Reject {
                shortfall_wei: total - 1_000_000_000_000
            }
        );
    }

    #[test]
    fn a_candidate_above_it_escalates_with_its_margin() {
        let i = input(100_000_000_000_000, 18_000_000_000_000);
        let total = i.cost.conservative_total(i.gas_price_wei) as i128;
        assert_eq!(
            screen(&i),
            Tier0Verdict::Escalate {
                margin_wei: 100_000_000_000_000 - total
            }
        );
    }

    /// Exactly breaking even is a rejection. A trade that nets zero has
    /// consumed a simulation slot, a signer and a block's worth of attention
    /// for nothing.
    #[test]
    fn breaking_even_exactly_does_not_escalate() {
        let mut i = input(0, 0);
        i.gas_price_wei = 0;
        assert_eq!(i.cost.conservative_total(0), 0);
        assert_eq!(screen(&i), Tier0Verdict::Reject { shortfall_wei: 0 });
    }

    /// Tier 0 screens on the CONSERVATIVE total, so p99 gas is what a
    /// candidate must clear -- not p50.
    #[test]
    fn screening_uses_p99_gas_not_the_median() {
        let mut i = input(2_400_000_000_000, 0);
        // At p99 (240,000 gas) the cost is exactly the gross: reject.
        assert!(!screen(&i).escalates());
        // The same candidate priced at p50 would have cleared, which is the
        // error this guards.
        i.cost.gas_used_distribution.p99 = i.cost.gas_used_distribution.p50;
        assert!(screen(&i).escalates());
    }

    /// A malformed cost rejects rather than wrapping into a bargain.
    #[test]
    fn an_absurd_cost_rejects_rather_than_overflowing() {
        let mut i = input(i128::MAX, 0);
        i.cost.l1_data_fee = u128::MAX;
        assert!(!screen(&i).escalates());
    }

    /// An already-negative candidate is rejected, not screened as if the sign
    /// were a detail.
    #[test]
    fn a_negative_candidate_never_escalates() {
        assert!(!screen(&input(-1, 0)).escalates());
        assert!(!screen(&input(i128::MIN, 0)).escalates());
    }
}

/// INV-40. Tier 0 declines for exactly one economic reason: the candidate
/// cannot pay for itself. `Escalate` is not a rejection and maps to the same
/// bucket only because the trait is total -- a caller asks this of a verdict it
/// has already decided is a rejection.
impl apex_types::miss::ExplainsMiss for Tier0Verdict {
    fn miss_reason(&self) -> apex_types::miss::MissReason {
        apex_types::miss::MissReason::LowEv
    }
}
