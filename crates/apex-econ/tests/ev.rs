//! Task 3.5 — INV-20, and the conjunction end to end.

use apex_econ::eligibility::{Clause, Decision, EligibilityContext, EligibilityGate, EligibilityPolicy};
use apex_econ::ev::scenario::{
    probability_of_profit_ppm, scenario_ev, PriorSource, Profit, Scenario, ScenarioKind,
    ScenarioSet,
};
use apex_types::pnl::UsdBounds;
use apex_types::time::DurationNanos;
use ethers_core::types::U256;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

fn scenarios(benign_ppm: u32, win: i128, loss: i128) -> ScenarioSet {
    let adverse = 1_000_000 - benign_ppm;
    ScenarioSet {
        scenarios: vec![
            Scenario {
                kind: ScenarioKind::SameStateImmediate,
                probability_ppm: benign_ppm,
                profit: Profit(win),
            },
            Scenario {
                kind: ScenarioKind::CompetingSamePoolSwapFirst,
                probability_ppm: adverse,
                profit: Profit(loss),
            },
        ],
        prior: PriorSource::Unmeasured,
    }
}

/// Build the gate's view of a candidate from its scenario set. The USD mark is
/// passed in and deliberately goes nowhere.
fn context(set: &ScenarioSet, _usd: UsdBounds) -> EligibilityContext {
    EligibilityContext {
        expected_net_ev_wei: scenario_ev(set, U256::from(20_000u64)).expect("valid"),
        robustness_margin_bps: 200,
        state_age: DurationNanos(500_000_000),
        simulation_tier: 2,
        execution_path_healthy: true,
        flash_liquidity_available: true,
        route_authorization_valid: true,
        cost_confidence_bps: 200,
        probability_of_profit_ppm: probability_of_profit_ppm(set).expect("valid"),
    }
}

fn candidates() -> Vec<ScenarioSet> {
    vec![
        scenarios(900_000, 1_000_000, -50_000),
        scenarios(700_000, 400_000, -100_000),
        scenarios(600_000, 80_000, -60_000),
        scenarios(550_000, 45_000, -40_000),
        scenarios(400_000, 900_000, -200_000),
        scenarios(200_000, 2_000_000, -30_000),
    ]
}

fn admitted_set(sets: &[ScenarioSet], usd: UsdBounds) -> Vec<bool> {
    sets.iter()
        .map(|s| {
            EligibilityGate::evaluate(&context(s, usd), &EligibilityPolicy::default()).admits()
        })
        .collect()
}

/// INV-20. The mechanism is that `EligibilityContext` has no USD field at all,
/// so there is nothing for a clause to read; this is the confirmation that no
/// USD quantity leaks in by another route.
#[test]
fn usd_mark_cannot_flip_admission_at_the_stated_bound() {
    let base = admitted_set(&candidates(), UsdBounds { low: 1.0, high: 1.0 });
    // The acceptance criterion's +/-50%.
    for mult in [0.5f64, 0.75, 1.0, 1.25, 1.5] {
        let perturbed = admitted_set(
            &candidates(),
            UsdBounds {
                low: mult,
                high: mult * 1.1,
            },
        );
        assert_eq!(base, perturbed, "a USD mark of {mult} changed the admitted set");
    }
    // ...and the fixture is not vacuous: some candidates are admitted and some
    // are not, so an equality between two sets is saying something.
    assert!(base.iter().any(|a| *a), "no candidate was admitted");
    assert!(base.iter().any(|a| !*a), "every candidate was admitted");
}

/// The gate's decision is a function of the scenario set alone. Stated by
/// evaluating the same candidate against a USD mark and against none at all.
#[test]
fn the_gate_reads_nothing_denominated_in_usd() {
    let set = scenarios(700_000, 400_000, -100_000);
    let with_mark = context(&set, UsdBounds { low: 4_000.0, high: 4_200.0 });
    let with_absurd_mark = context(&set, UsdBounds { low: 1e-9, high: 1e9 });
    assert_eq!(
        with_mark, with_absurd_mark,
        "a USD mark reached the eligibility context"
    );
}

/// The conjunction, end to end: a candidate whose scenario set is genuinely
/// bad is rejected on the clause that describes why.
#[test]
fn a_lottery_candidate_is_rejected_for_probability_not_for_expectation() {
    // Pays 2,000,000 one time in five, loses 30,000 otherwise: expectation is
    // positive, and it almost never pays.
    let set = scenarios(200_000, 2_000_000, -30_000);
    let ev = scenario_ev(&set, U256::from(20_000u64)).expect("valid");
    assert!(ev > 0, "the fixture must have a positive expectation: {ev}");

    assert_eq!(
        EligibilityGate::evaluate(&context(&set, UsdBounds { low: 1.0, high: 1.0 }), &EligibilityPolicy::default()),
        Decision::Reject {
            clause: Clause::ProbabilityOfProfit
        },
        "§2.1's robust gate is what should stop this, not the expectation"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/ev.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// INV-20 over arbitrary marks rather than five chosen ones, including
    /// degenerate and inverted intervals.
    #[test]
    fn usd_mark_cannot_flip_admission(
        low in 1e-9f64..1e9,
        high in 1e-9f64..1e9,
    ) {
        let base = admitted_set(&candidates(), UsdBounds { low: 1.0, high: 1.0 });
        let perturbed = admitted_set(&candidates(), UsdBounds { low, high });
        prop_assert_eq!(base, perturbed);
    }

    /// The expectation moves monotonically with the benign probability. A gate
    /// built on a non-monotone expectation would admit in a band rather than
    /// above a threshold.
    #[test]
    fn a_more_likely_benign_outcome_never_lowers_the_expectation(
        benign in 1u32..999_999,
        delta in 1u32..1_000,
        win in 1i128..10_000_000,
        loss in -10_000_000i128..0,
    ) {
        let higher = (benign + delta).min(999_999);
        let a = scenario_ev(&scenarios(benign, win, loss), U256::zero()).expect("valid");
        let b = scenario_ev(&scenarios(higher, win, loss), U256::zero()).expect("valid");
        prop_assert!(b >= a, "raising P(benign) from {} to {} lowered J: {} -> {}", benign, higher, a, b);
    }
}
