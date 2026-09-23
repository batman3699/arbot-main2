//! Task 3.2 — every cost component is present, and none of them defaults.

use apex_econ::cost::failure::{expected_failure_cost, FailureProfile};
use apex_econ::cost::l1_data::{
    CompressedSize, L1FeeModel, L1FeeParameters, Validation, FJORD_DIVISOR, INTERCEPT,
};
use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
use ethers_core::types::U256;

/// Every field of `TotalExecutionCost`, named.
///
/// The same device as `apex_venues::admission::REQUIRED_FIELDS`: the list has
/// to be kept in step with the struct, and the test below proves it is by
/// comparing against what serde emits. A component silently added and left
/// unpriced is the failure this guards.
const COMPONENTS: &[&str] = &[
    "l2_execution_fee",
    "l1_data_fee",
    "priority_fee",
    "builder_payment",
    "sequencer_payment",
    "flash_fee",
    "dex_fees",
    "expected_failure_cost",
    "calldata_bytes",
    "compressed_data_estimate",
    "gas_limit",
    "gas_used_distribution",
];

fn complete() -> TotalExecutionCost {
    TotalExecutionCost {
        l2_execution_fee: 1_000,
        l1_data_fee: 18_000_000_000_000,
        priority_fee: 2_000,
        builder_payment: 3_000,
        sequencer_payment: 4_000,
        flash_fee: 5_000,
        dex_fees: 6_000,
        expected_failure_cost: 7_000,
        calldata_bytes: 512,
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

#[test]
fn the_component_list_covers_every_field() {
    let json = serde_json::to_value(complete()).expect("serialisable");
    let mut actual: Vec<&str> = json
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort_unstable();
    let mut listed = COMPONENTS.to_vec();
    listed.sort_unstable();
    assert_eq!(listed, actual, "COMPONENTS has drifted from TotalExecutionCost");
}

/// No `Default`. A defaulted zero for `l1_data_fee` or `expected_failure_cost`
/// is not a missing field, it is a silently optimistic trade.
///
/// ```compile_fail
/// use apex_types::cost::TotalExecutionCost;
/// let cost = TotalExecutionCost::default();
/// ```
///
/// The twin, differing only in naming every component:
///
/// ```
/// use apex_types::cost::{GasDistribution, GasLimit, GasUsed, TotalExecutionCost};
/// let cost = TotalExecutionCost {
///     l2_execution_fee: 0, l1_data_fee: 0, priority_fee: 0, builder_payment: 0,
///     sequencer_payment: 0, flash_fee: 0, dex_fees: 0, expected_failure_cost: 0,
///     calldata_bytes: 0, compressed_data_estimate: 0, gas_limit: GasLimit(0),
///     gas_used_distribution: GasDistribution {
///         p50: GasUsed(0), p90: GasUsed(0), p99: GasUsed(0), max_observed: GasUsed(0),
///     },
/// };
/// assert_eq!(cost.conservative_total(1), 0);
/// ```
#[test]
fn total_execution_cost_has_no_default() {
    // The claim is in the doctests above; this asserts the consequence, which
    // is that every component participates in the conservative total.
    let base = complete().conservative_total(1);
    for bump in [
        |c: &mut TotalExecutionCost| c.l1_data_fee += 1,
        |c: &mut TotalExecutionCost| c.priority_fee += 1,
        |c: &mut TotalExecutionCost| c.builder_payment += 1,
        |c: &mut TotalExecutionCost| c.sequencer_payment += 1,
        |c: &mut TotalExecutionCost| c.flash_fee += 1,
        |c: &mut TotalExecutionCost| c.dex_fees += 1,
        |c: &mut TotalExecutionCost| c.expected_failure_cost += 1,
    ] {
        let mut c = complete();
        bump(&mut c);
        assert_eq!(
            c.conservative_total(1),
            base + 1,
            "a component does not reach the conservative total"
        );
    }
}

/// §23.1: the risk gate prices at p99, not p50. A distribution collapsed to a
/// point estimate cannot serve both.
#[test]
fn the_conservative_total_prices_gas_at_p99() {
    // Vary one percentile at a time and watch which one the total follows.
    // Comparing the total's magnitude against the gas term would not
    // discriminate -- on Base the L1 data fee dwarfs both percentiles, so a
    // size comparison says nothing about which one was used.
    let base = complete().conservative_total(1_000);

    let mut lower_p50 = complete();
    lower_p50.gas_used_distribution.p50 = GasUsed(1);
    assert_eq!(
        lower_p50.conservative_total(1_000),
        base,
        "the conservative total moved when p50 did -- it is not pricing at p99"
    );

    let mut higher_p99 = complete();
    higher_p99.gas_used_distribution.p99 = GasUsed(240_001);
    assert_eq!(
        higher_p99.conservative_total(1_000),
        base + 1_000,
        "one more unit of p99 gas must cost exactly one gas price"
    );
}

/// The L1 fee and the failure cost compose the way §23.1 says: the failure
/// branch carries the whole L1 fee, so a cost model that adds them is not
/// double-counting — it is pricing two different branches.
#[test]
fn the_l1_fee_reaches_both_branches_of_the_cost() {
    let params = L1FeeParameters {
        l1_base_fee: U256::from(10_000_000_000u64),
        l1_blob_base_fee: U256::from(1_000_000_000u64),
        base_fee_scalar: 1_368,
        blob_base_fee_scalar: 810_949,
    };
    let l1 = L1FeeModel::unvalidated()
        .fee(CompressedSize::Estimated(400), params)
        .wei;
    assert!(l1 > U256::zero());

    let failure = expected_failure_cost(
        FailureProfile {
            gas_on_failure: GasUsed(0),
            failure_ppm: 1_000_000,
        },
        U256::from(10_000_000u64),
        l1,
    );
    assert_eq!(failure, l1, "the failure branch must carry the whole L1 fee");
}

/// The Fjord constants, pinned. They are published values rather than derived
/// ones, so the only defence against a transcription slip is stating them in
/// two places and comparing.
#[test]
fn the_fjord_constants_are_what_they_should_be() {
    assert_eq!(INTERCEPT, -42_585_600, "intercept is negative and specific");
    assert_eq!(FJORD_DIVISOR, 1_000_000_000_000);
}

/// The model's own honesty, from outside the crate.
#[test]
fn the_default_l1_model_admits_it_is_unvalidated() {
    assert_eq!(
        L1FeeModel::default().validation(),
        Validation::FromPublishedConstantsOnly
    );
    assert!(!L1FeeModel::default().validation().may_price_a_live_dispatch());
}
