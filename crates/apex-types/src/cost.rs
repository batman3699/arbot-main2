//! Execution cost (Blueprint §23).

use serde::{Deserialize, Serialize};

/// The SCHEDULING variable (§23.4). On Base this decides which Flashblock a
/// transaction is eligible for; it is not a cost.
///
/// Deliberately has no conversion to or from [`GasUsed`]. §23.4 says conflating
/// them "is incorrect on systems where gas limit affects scheduling", and this
/// separation is INV-19. Enforced by `scripts/ci/no_gas_conversion.sh` rather
/// than a `trybuild` compile-fail fixture: Rust has no negative trait bounds, so
/// the only in-language proof is a compile-fail test whose expected stderr is
/// tied to a rustc version. A grep guard fails just as loudly, costs no
/// dependency, and matches the existing `scripts/ci/` convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GasLimit(pub u64);

/// The COST variable (§23.4). Realized, and modelled as a distribution rather
/// than a point estimate -- see [`GasDistribution`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GasUsed(pub u64);

/// §23.1 requires a distribution, not a scalar: the risk gate prices at p99
/// while the EV estimate uses p50, and a single number cannot serve both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GasDistribution {
    pub p50: GasUsed,
    pub p90: GasUsed,
    pub p99: GasUsed,
    pub max_observed: GasUsed,
}

/// Blueprint §23.1. All twelve components; no `Default`, because a defaulted
/// zero for `l1_data_fee` or `expected_failure_cost` is a silently optimistic
/// trade rather than a missing field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TotalExecutionCost {
    pub l2_execution_fee: u128,
    pub l1_data_fee: u128,
    pub priority_fee: u128,
    pub builder_payment: u128,
    pub sequencer_payment: u128,
    pub flash_fee: u128,
    pub dex_fees: u128,
    pub expected_failure_cost: u128,
    pub calldata_bytes: u32,
    pub compressed_data_estimate: u32,
    pub gas_limit: GasLimit,
    pub gas_used_distribution: GasDistribution,
}

impl TotalExecutionCost {
    /// What the risk gate prices against: p99 gas and the full failure cost.
    /// Everything here is additive and already denominated in wei of the chain's
    /// native token.
    pub fn conservative_total(&self, gas_price_wei: u128) -> u128 {
        let gas = u128::from(self.gas_used_distribution.p99.0).saturating_mul(gas_price_wei);
        [
            gas,
            self.l1_data_fee,
            self.priority_fee,
            self.builder_payment,
            self.sequencer_payment,
            self.flash_fee,
            self.dex_fees,
            self.expected_failure_cost,
        ]
        .into_iter()
        .fold(0u128, |acc, x| acc.saturating_add(x))
    }
}
