//! Task 7.2 — INV-37, INV-38, §21.2, §22.2.

use apex_chain::base::flashblock::{
    earliest_eligible, earliest_eligible_from, Capacity, FlashblockObservation,
    MeasuredCapacityModel, ModelError,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

/// Recorded budgets. Ten flashblocks to a Base block, each adding roughly 7-8M
/// of cumulative room, with the per-block variation a real chain has.
///
/// **This is a fixture, not a measurement.** Egress from this environment is
/// blocked (HTTP 403 to Base), so acceptance criterion 1 -- eligibility
/// validated against >= 500 observed transactions at >= 90% accuracy -- is
/// outstanding and recorded as such in PLAN.md §4.7. What the fixture does
/// establish is that the *model* is learned rather than assumed: the numbers
/// below are not a tenth of anything, and swapping them changes the answers.
fn recorded_flashblock_budgets() -> Vec<FlashblockObservation> {
    let mut out = Vec::new();
    for block in 0..20u64 {
        // A little block-to-block variation, so p10 and p90 differ and the
        // conservative quantile is doing visible work.
        let wobble = (block % 5) * 400_000;
        for index in 0..10u32 {
            out.push(FlashblockObservation {
                block,
                index,
                cumulative_gas_budget: 7_500_000 * u64::from(index + 1) - wobble,
            });
        }
    }
    out
}

/// The same chain with two indices too thinly sampled to trust.
fn gapped_model() -> MeasuredCapacityModel {
    let mut obs = recorded_flashblock_budgets();
    obs.retain(|o| (o.index != 5 && o.index != 7) || o.block < 3);
    MeasuredCapacityModel::from_observations(&obs).expect("observations")
}

fn model() -> MeasuredCapacityModel {
    MeasuredCapacityModel::from_observations(&recorded_flashblock_budgets())
        .expect("observations")
}

/// The plan's Step 1 test.
#[test]
fn eligibility_uses_the_measured_capacity_model_not_a_constant() {
    let q = model();
    assert_eq!(earliest_eligible(1_000_000, &q), Some(0));
    assert_eq!(earliest_eligible(25_000_000, &q), Some(3)); // too big for early flashblocks
    // A hard-coded one-tenth rule would give a different answer; assert we do not use one.
    assert_ne!(earliest_eligible(25_000_000, &q), Some(0));
}

/// And the model is genuinely learned: different observations, different
/// answers. Without this the test above passes against a constant that happens
/// to agree with the fixture.
#[test]
fn a_different_chain_gives_different_eligibility() {
    let leaner: Vec<_> = recorded_flashblock_budgets()
        .into_iter()
        .map(|o| FlashblockObservation { cumulative_gas_budget: o.cumulative_gas_budget / 3, ..o })
        .collect();
    let q = MeasuredCapacityModel::from_observations(&leaner).expect("observations");
    assert_eq!(earliest_eligible(1_000_000, &q), Some(0));

    // The same transaction, a later window: 20M fits index 2 on the fixture and
    // index 8 on a chain with a third of the room.
    assert_eq!(earliest_eligible(20_000_000, &model()), Some(2));
    assert_eq!(earliest_eligible(20_000_000, &q), Some(8));

    // And 25M does not fit this chain at all. `None` is a real answer -- §21.3
    // turns it into a rejection before signing, which is the whole point of
    // asking before a signature exists.
    assert_eq!(earliest_eligible(25_000_000, &q), None);
}

/// There is no way to build a model without observations. A model built from a
/// fraction of a block gas limit would be the hard-coded rule §22.2 forbids,
/// wearing a measurement's name.
#[test]
fn a_model_cannot_be_built_from_nothing() {
    assert_eq!(MeasuredCapacityModel::from_observations(&[]), Err(ModelError::NoObservations));
}

/// An index with too few samples is `Unknown`, and eligibility skips it rather
/// than interpolating. §5.6: a gap is never converted into "probably the
/// usual" -- and at the moment of signing that conversion costs a nonce and a
/// window.
#[test]
fn a_thin_index_is_unknown_rather_than_interpolated() {
    let mut obs = recorded_flashblock_budgets();
    obs.retain(|o| o.index != 2 || o.block < 3); // only 3 samples at index 2
    let q = MeasuredCapacityModel::from_observations(&obs).expect("observations");

    assert!(matches!(q.capacity_at(2), Some(Capacity::Unknown { samples: 3, .. })));
    assert_eq!(q.q(2), None);
    // 20M fits index 2's real capacity, but index 2 is not measured -- so the
    // answer is index 3, not a guess about index 2.
    assert_eq!(earliest_eligible(20_000_000, &q), Some(3));
}

/// `Q(k)` reports the conservative end of the interval. A median would be right
/// half the time, and the wrong half is a transaction signed for a window it
/// does not fit.
#[test]
fn q_reports_the_conservative_end_of_the_interval() {
    let q = model();
    let Some(Capacity::Measured { p10, median, p90, .. }) = q.capacity_at(4) else {
        panic!("index 4 should be measured");
    };
    assert!(p10 < median && median <= p90, "p10 {p10} median {median} p90 {p90}");
    assert_eq!(q.q(4), Some(p10), "eligibility must use the reliable figure");
    assert_eq!(q.capacity_at(4).and_then(|c| c.interval()), Some((p10, p90)));
}

/// Every number in the model was observed. An interpolated quantile is a
/// capacity nobody saw, which is the opposite of a measured allocation policy.
#[test]
fn every_quantile_is_an_observed_value() {
    let obs = recorded_flashblock_budgets();
    let q = MeasuredCapacityModel::from_observations(&obs).expect("observations");
    for k in 0..q.windows() as u32 {
        let Some(Capacity::Measured { p10, median, p90, .. }) = q.capacity_at(k) else { continue };
        let seen: Vec<u64> =
            obs.iter().filter(|o| o.index == k).map(|o| o.cumulative_gas_budget).collect();
        for v in [p10, median, p90] {
            assert!(seen.contains(&v), "index {k}: {v} was never observed");
        }
    }
}

/// **The case a clamp gets wrong.**
///
/// Found by mutation: replacing the search in `earliest_eligible_from` with
/// `earliest_eligible(..).map(|k| k.max(current))` broke nothing, because on a
/// model whose capacity rises with the index, clamping upward always lands
/// somewhere with *more* room. The two differ only where the model has a gap --
/// and a gap is not hypothetical, it is what an index with too few samples
/// produces.
#[test]
fn locking_the_order_searches_rather_than_clamping() {
    let mut obs = recorded_flashblock_budgets();
    obs.retain(|o| o.index != 5 || o.block < 3); // index 5 goes Unknown
    let q = MeasuredCapacityModel::from_observations(&obs).expect("observations");
    assert_eq!(q.q(5), None);

    // 10M fits from index 1 onward. Asked from index 5, a clamp would answer 5
    // -- a window whose capacity was never measured. The search answers 6.
    assert_eq!(earliest_eligible(10_000_000, &q), Some(1));
    assert_eq!(
        earliest_eligible_from(5, 10_000_000, &q),
        Some(6),
        "a clamped answer would name the unmeasured index 5"
    );
}

/// **INV-38, the ordering lock.** The plan's Step 1 property test.
#[test]
fn no_retroactive_flashblock_entry() {
    proptest!(|(current in 0u32..10, gas in 21_000u64..30_000_000)| {
        let k = earliest_eligible_from(current, gas, &model());
        prop_assert!(k.is_none_or(|k| k >= current), "INV-38: ordering is locked");
    });
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 20_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/flashblock.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// And it is not achieved by clamping. A clamp would claim a window whose
    /// capacity was never checked; searching from `current` means the answer is
    /// always a window that actually fits.
    #[test]
    fn a_locked_answer_is_still_a_window_that_fits(
        current in 0u32..10,
        gas in 21_000u64..80_000_000,
    ) {
        // Both a dense model and one with a gap: on a dense model a clamp and
        // a search agree, so a property that only saw dense models would prove
        // nothing about the difference.
        for q in [model(), gapped_model()] {
            if let Some(k) = earliest_eligible_from(current, gas, &q) {
                prop_assert!(k >= current);
                prop_assert!(
                    q.q(k).is_some_and(|cap| gas <= cap),
                    "window {} does not fit {}", k, gas
                );
            }
        }
    }

    /// Monotone in size: a bigger transaction never lands earlier.
    #[test]
    fn a_larger_transaction_never_lands_earlier(
        small in 21_000u64..30_000_000,
        extra in 1u64..30_000_000,
    ) {
        let q = model();
        let a = earliest_eligible(small, &q);
        let b = earliest_eligible(small + extra, &q);
        match (a, b) {
            (Some(a), Some(b)) => prop_assert!(b >= a, "{} landed at {} but {} landed at {}", small + extra, b, small, a),
            (None, Some(_)) => prop_assert!(false, "a larger transaction found a window the smaller one did not"),
            _ => {}
        }
    }
}
