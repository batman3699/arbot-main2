//! What a failed transaction costs (§23.1, §25).
//!
//! # The asymmetry that gets forgotten
//!
//! A reverted transaction on an OP Stack chain **still pays the full L1 data
//! fee**. The calldata was posted to Ethereum the moment the transaction was
//! included; whether it then reverted is an L2 execution detail that the L1
//! never learns. Only the L2 execution component differs, and it differs
//! *downward* — a revert burns gas up to the revert point rather than the whole
//! limit.
//!
//! So the failure cost is not "a fraction of the success cost". Its L1 half is
//! identical and its L2 half is smaller, and on Base — where the L1 data fee is
//! usually the larger of the two — that makes a failure nearly as expensive as
//! a success. A model that scales the total by a failure fraction understates
//! it, in the direction that makes marginal trades look acceptable.

use ethers_core::types::U256;
use apex_types::cost::GasUsed;

/// What a transaction burns when it reverts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailureProfile {
    /// Gas consumed before the revert. Distinct from `apex_types::cost::GasLimit`
    /// on purpose:
    /// this is a COST variable (INV-19), and the limit is a scheduling one.
    pub gas_on_failure: GasUsed,
    /// Probability of failure, in parts per million. An integer because a
    /// probability that participates in a wei-denominated product should not
    /// introduce floating point into the cost model.
    pub failure_ppm: u32,
}

/// Expected cost of the failure branch, in wei.
///
/// `p_fail * (gas_on_failure * gas_price + l1_data_fee)` — with the L1 fee
/// inside the parentheses, not outside, because it is paid whether or not the
/// execution succeeds.
pub fn expected_failure_cost(
    profile: FailureProfile,
    gas_price_wei: U256,
    l1_data_fee: U256,
) -> U256 {
    let l2 = U256::from(profile.gas_on_failure.0).saturating_mul(gas_price_wei);
    l2.saturating_add(l1_data_fee)
        .saturating_mul(U256::from(profile.failure_ppm))
        / U256::from(1_000_000u64)
}

/// The failure cost as a fraction of the success cost, in basis points.
///
/// Reported rather than assumed. On Base this lands far higher than intuition
/// suggests, and seeing the number is the point.
pub fn failure_share_bps(
    profile: FailureProfile,
    success_gas: GasUsed,
    gas_price_wei: U256,
    l1_data_fee: U256,
) -> u32 {
    let on_success = U256::from(success_gas.0)
        .saturating_mul(gas_price_wei)
        .saturating_add(l1_data_fee);
    if on_success.is_zero() {
        return 0;
    }
    let on_failure = U256::from(profile.gas_on_failure.0)
        .saturating_mul(gas_price_wei)
        .saturating_add(l1_data_fee);
    let bps = on_failure.saturating_mul(U256::from(10_000u64)) / on_success;
    bps.min(U256::from(u32::MAX)).as_u32()
}

/// A gas LIMIT is not a gas cost (INV-19, §23.4).
///
/// The limit decides which Flashblock a transaction is eligible for; the used
/// amount decides what it pays. Conflating them is wrong on any chain where the
/// limit affects scheduling, and Base is one.
///
/// `scripts/ci/no_gas_conversion.sh` guards this because Rust has no negative
/// trait bounds. That guard is a grep; these are the same claim in the
/// language, version-independent, and paired with a compiling twin so a
/// snippet that broke for an unrelated reason would take its twin down too.
///
/// ```compile_fail
/// use apex_types::cost::{GasLimit, GasUsed};
/// let limit = GasLimit(200_000);
/// // There is no conversion, and §23.4 says there must not be one.
/// let used: GasUsed = limit.into();
/// ```
///
/// ```compile_fail
/// use apex_types::cost::{GasLimit, GasUsed};
/// let used = GasUsed(180_000);
/// let limit: GasLimit = GasLimit::from(used);
/// ```
///
/// The twin: going through the raw number is possible, and is deliberately the
/// only way, because it is a line somebody has to write on purpose.
///
/// ```
/// use apex_types::cost::{GasLimit, GasUsed};
/// let limit = GasLimit(200_000);
/// let used = GasUsed(limit.0);
/// assert_eq!(used.0, 200_000);
/// ```
pub const GAS_LIMIT_IS_NOT_GAS_USED: () = ();

#[cfg(test)]
mod tests {
    use super::*;

    const GWEI: u64 = 1_000_000_000;

    /// The L1 data fee is paid in full on failure. This is the whole module.
    #[test]
    fn a_reverted_transaction_still_pays_the_whole_l1_data_fee() {
        let l1 = U256::from(50_000_000_000_000u64); // 0.00005 ETH
        let profile = FailureProfile {
            gas_on_failure: GasUsed(0), // revert at the first opcode
            failure_ppm: 1_000_000,     // certainty, to read the cost directly
        };
        assert_eq!(
            expected_failure_cost(profile, U256::from(GWEI), l1),
            l1,
            "a transaction that burns no L2 gas still pays the L1 fee in full"
        );
    }

    /// On Base the failure branch is nearly as expensive as the success
    /// branch, because the larger component is identical in both.
    #[test]
    fn a_failure_costs_almost_as_much_as_a_success() {
        // Measured shape: ~1 cent of gas, of which the L1 data fee dominates.
        let l1 = U256::from(18_000_000_000_000u64);
        let gas_price = U256::from(10_000_000u64); // 0.01 gwei, Base L2
        let share = failure_share_bps(
            FailureProfile {
                gas_on_failure: GasUsed(60_000),
                failure_ppm: 0,
            },
            GasUsed(200_000),
            gas_price,
            l1,
        );
        assert!(
            share > 9_000,
            "failure share was {share} bps; the L1 fee should dominate and keep \
             it near 10,000"
        );
    }

    /// Scaling the total by a failure fraction understates the cost. Stated as
    /// a comparison so the size of the error is visible, not asserted away.
    #[test]
    fn scaling_the_total_by_a_gas_ratio_understates_the_failure_cost() {
        let l1 = U256::from(18_000_000_000_000u64);
        let gas_price = U256::from(10_000_000u64);
        let profile = FailureProfile {
            gas_on_failure: GasUsed(60_000),
            failure_ppm: 1_000_000,
        };
        let correct = expected_failure_cost(profile, gas_price, l1);

        // The naive model: total success cost scaled by the gas ratio.
        let success_total = U256::from(200_000u64) * gas_price + l1;
        let naive = success_total * U256::from(60_000u64) / U256::from(200_000u64);

        assert!(
            correct > naive,
            "the naive model ({naive}) should understate the real failure cost \
             ({correct})"
        );
        let understatement_bps = (correct - naive) * U256::from(10_000u64) / correct;
        assert!(
            understatement_bps > U256::from(5_000u64),
            "understatement was only {understatement_bps} bps -- fixture no longer \
             demonstrates the effect"
        );
    }

    #[test]
    fn probability_scales_the_expected_cost_linearly() {
        let l1 = U256::from(20_000_000_000_000u64);
        let at = |ppm| {
            expected_failure_cost(
                FailureProfile {
                    gas_on_failure: GasUsed(100_000),
                    failure_ppm: ppm,
                },
                U256::from(GWEI),
                l1,
            )
        };
        assert_eq!(at(0), U256::zero());
        let half = at(500_000);
        let full = at(1_000_000);
        assert!(full >= half * 2 && full <= half * 2 + U256::one());
    }
}
