//! Task 6.7 — INV-42 and INV-43, as the table tests the plan asked for.

use apex_risk::breaker::{CircuitBreaker, DAY, HOUR};
use apex_risk::loss::{ClassBudget, Containment, LossLedger};
use apex_risk::posture::{PostureLadder, RiskTrigger, StepDownAuthority, StepDownRefused};
use alloy_primitives::U256;
use apex_types::risk::{LossClass, RiskPosture};
use apex_types::time::{DurationNanos, UnixNanos};

const T0: UnixNanos = UnixNanos(1_700_000_000_000_000_000);

fn at(ns: u64) -> UnixNanos {
    UnixNanos(T0.0 + ns)
}

// ------------------------------------------------------------------ INV-42

/// **The plan's `every_trigger_maps_to_a_posture`.** A total function over an
/// exhaustive enum: there is no trigger that produces an unhandled continue.
#[test]
fn every_trigger_maps_to_a_posture() {
    for t in RiskTrigger::ALL {
        let p = t.minimum_posture();
        assert_ne!(p, RiskPosture::Normal, "{t} produces no response at all");
        let mut ladder = PostureLadder::new();
        assert_eq!(ladder.observe(t, T0), p, "{t}");
        assert!(ladder.posture() >= p);
    }
    assert_eq!(RiskTrigger::ALL.len(), 14, "§25.1 names fourteen triggers");
}

/// The two that mean something other than the market is driving.
#[test]
fn only_a_foreign_driver_halts_globally() {
    let halting: Vec<_> = RiskTrigger::ALL
        .iter()
        .copied()
        .filter(|t| t.minimum_posture() == RiskPosture::GlobalHalt)
        .collect();
    assert_eq!(
        halting,
        vec![RiskTrigger::UnexpectedCallback, RiskTrigger::ContractCodeFingerprintChange]
    );
}

/// Every trigger whose floor disables something must actually stop new live
/// tickets, or the ladder's rungs are decorative.
#[test]
fn a_disabling_posture_stops_new_live_tickets() {
    for t in RiskTrigger::ALL {
        let mut ladder = PostureLadder::new();
        ladder.observe(t, T0);
        let expected = t.minimum_posture().permits_new_live_tickets();
        assert_eq!(ladder.permits_new_live_tickets(), expected, "{t}");
    }
}

/// The ladder never relaxes on its own. A mild trigger after a severe one must
/// not walk the posture back down.
#[test]
fn a_milder_trigger_cannot_relax_a_severe_posture() {
    let mut l = PostureLadder::new();
    l.observe(RiskTrigger::NodeDesynchronization, T0);
    assert_eq!(l.posture(), RiskPosture::ChainDisabled);
    l.observe(RiskTrigger::ProfitShortfall, at(1));
    assert_eq!(l.posture(), RiskPosture::ChainDisabled, "the ladder went down on a trigger");
}

/// §25.2: stepping down needs an explicit operator action or a measured
/// recovery window, and one rung at a time.
#[test]
fn stepping_down_needs_authority_and_goes_one_rung_at_a_time() {
    let mut l = PostureLadder::new();
    l.observe(RiskTrigger::NodeDesynchronization, T0);

    let a = StepDownAuthority::Operator { actor: "oncall" };
    assert_eq!(l.step_down(a, at(1)), Ok(RiskPosture::StrategyDisabled));
    assert_eq!(l.step_down(a, at(2)), Ok(RiskPosture::HighEvOnly));
    assert_eq!(l.step_down(a, at(3)), Ok(RiskPosture::ReducedSize));
    assert_eq!(l.step_down(a, at(4)), Ok(RiskPosture::Normal));
    assert_eq!(l.step_down(a, at(5)), Err(StepDownRefused::AlreadyNormal));
}

/// The recovery window must actually be quiet.
#[test]
fn a_recovery_window_with_a_trigger_in_it_does_not_count() {
    let window = DurationNanos(1_000);
    let mut l = PostureLadder::with_recovery_window(window);
    l.observe(RiskTrigger::RevertSpike, T0);

    let authority = StepDownAuthority::RecoveryWindow { elapsed: window };
    assert!(matches!(
        l.step_down(authority, at(500)),
        Err(StepDownRefused::WindowNotQuiet { .. })
    ));
    assert_eq!(l.step_down(authority, at(1_000)), Ok(RiskPosture::Normal));
}

/// **A halt is not a market condition.** No amount of elapsed time is evidence
/// that whatever was driving has stopped, so time alone cannot leave a halt.
#[test]
fn time_alone_cannot_leave_a_global_halt() {
    let mut l = PostureLadder::with_recovery_window(DurationNanos(1));
    l.observe(RiskTrigger::UnexpectedCallback, T0);
    assert_eq!(
        l.step_down(StepDownAuthority::RecoveryWindow { elapsed: DurationNanos(u64::MAX) }, at(u64::MAX / 2)),
        Err(StepDownRefused::RequiresOperator(RiskPosture::GlobalHalt))
    );
    assert_eq!(
        l.step_down(StepDownAuthority::Operator { actor: "oncall" }, at(1)),
        Ok(RiskPosture::ChainDisabled)
    );
}

// ------------------------------------------------------------------ INV-43

/// **INV-43**, named as §8 names it.
///
/// "Every loss is classified into one of the §28.2 classes." The enforcement is
/// that `LossClass` has no catch-all variant and the ledger takes a class
/// rather than inferring one — so a caller with a loss it cannot name has no
/// way to record it, which is the point. This asserts the closed world and that
/// the ledger accepts every member of it.
#[test]
fn every_loss_is_classified() {
    let mut ledger = LossLedger::with_default_budgets();
    // The taxonomy is closed: nine classes, no `Other`. An exhaustive match is
    // the check -- a tenth variant added later fails to compile here.
    for c in LossClass::ALL {
        let label = match c {
            LossClass::Pricing => "pricing",
            LossClass::State => "state",
            LossClass::Simulation => "simulation",
            LossClass::Venue => "venue",
            LossClass::Inclusion => "inclusion",
            LossClass::FeeModel => "fee_model",
            LossClass::Contract => "contract",
            LossClass::OperatorConfig => "operator_config",
            LossClass::ExternalProtocol => "external_protocol",
        };
        assert!(!label.is_empty());
        // And every one of them can actually be recorded. A class the ledger
        // silently ignored would be a loss that happened and was not counted.
        ledger.record(c, U256::from(1u64), T0);
        assert_eq!(ledger.count_in_window(c, T0), 1, "{c:?} was not recorded");
    }
    assert_eq!(LossClass::ALL.len(), 9, "§28.2 names nine classes");
}

/// Every class has a budget; a class without one is a class whose gate never
/// tightens, which is the silent failure INV-43 exists to prevent.
#[test]
fn every_loss_class_has_a_budget() {
    let ledger = LossLedger::with_default_budgets();
    for c in LossClass::ALL {
        let b = ledger.budget(c).unwrap_or_else(|| panic!("{c:?} has no budget"));
        assert!(b.window.0 > 0, "{c:?}");
        assert_ne!(b.on_breach, RiskPosture::Normal, "{c:?} breaches into no response");
    }
    assert_eq!(LossClass::ALL.len(), 9, "§28.2 names nine classes");
}

/// And every class can actually breach, which is the table the plan asked for.
#[test]
fn every_loss_class_tightens_its_own_gate() {
    for c in LossClass::ALL {
        let mut ledger = LossLedger::with_default_budgets();
        let budget = ledger.budget(c).expect("budgeted");
        let mut last = Containment::None;
        for i in 0..=u64::from(budget.expected_in_window) {
            last = ledger.record(c, U256::from(1u64), at(i));
        }
        match last {
            Containment::Tighten { class, posture, .. } => {
                assert_eq!(class, c);
                assert_eq!(posture, budget.on_breach);
            }
            Containment::None => panic!("{c:?} never tightened after {} events", budget.expected_in_window + 1),
        }
    }
}

/// Zero-budget classes tighten on the first event. `Contract` and
/// `OperatorConfig` losses should never happen at all: one is a deployment that
/// does not do what we think, the other is us.
#[test]
fn a_zero_budget_class_tightens_immediately() {
    for c in [LossClass::Contract, LossClass::OperatorConfig] {
        let mut ledger = LossLedger::with_default_budgets();
        assert_eq!(
            ledger.record(c, U256::from(1u64), T0).posture_floor(),
            Some(RiskPosture::GlobalHalt),
            "{c:?}"
        );
    }
}

/// Events age out of their window, so a slow trickle never accumulates into a
/// breach it did not earn.
#[test]
fn events_age_out_of_their_window() {
    let mut ledger = LossLedger::with_default_budgets();
    let budget = ledger.budget(LossClass::Pricing).expect("budgeted");
    for i in 0..=u64::from(budget.expected_in_window) {
        ledger.record(LossClass::Pricing, U256::from(1u64), at(i));
    }
    assert!(matches!(ledger.worst(at(10)), Containment::Tighten { .. }));

    let past = at(budget.window.0 + 1_000);
    assert_eq!(ledger.count_in_window(LossClass::Pricing, past), 0);
    assert_eq!(ledger.worst(past), Containment::None);
}

/// `worst` reports the most severe class in breach, not the most recent.
#[test]
fn worst_reports_the_most_severe_breach() {
    let mut ledger = LossLedger::with_default_budgets();
    ledger.record(LossClass::Contract, U256::from(1u64), T0);
    for i in 0..200u64 {
        ledger.record(LossClass::Inclusion, U256::from(1u64), at(i));
    }
    assert_eq!(ledger.worst(at(300)).posture_floor(), Some(RiskPosture::GlobalHalt));
}

/// The ledger feeds the ladder: a breach is a trigger's worth of evidence and
/// the posture floor it names is the floor the ladder ends up at.
#[test]
fn a_breach_raises_the_ladder_to_the_floor_it_names() {
    let mut ledger = LossLedger::with_default_budgets();
    let mut ladder = PostureLadder::new();
    for i in 0..10u64 {
        if let Containment::Tighten { posture, .. } =
            ledger.record(LossClass::Simulation, U256::from(1u64), at(i))
        {
            assert_eq!(posture, RiskPosture::StrategyDisabled);
            ladder.observe(RiskTrigger::SimulationDivergence, at(i));
        }
    }
    assert_eq!(ladder.posture(), RiskPosture::StrategyDisabled);
    assert!(!ladder.permits_new_live_tickets());
}

/// A custom budget replaces the default, because §28.2's frequencies are
/// per-strategy and the defaults here are a starting point, not a law.
#[test]
fn a_budget_can_be_replaced() {
    let mut ledger = LossLedger::with_default_budgets();
    ledger.set_budget(
        LossClass::Inclusion,
        ClassBudget {
            expected_in_window: 1,
            window: DurationNanos(1_000),
            on_breach: RiskPosture::ChainDisabled,
        },
    );
    ledger.record(LossClass::Inclusion, U256::from(1u64), T0);
    assert_eq!(
        ledger.record(LossClass::Inclusion, U256::from(1u64), at(1)).posture_floor(),
        Some(RiskPosture::ChainDisabled)
    );
}

// ------------------------------------------------------------- the breaker

/// The window edge is inclusive, matching the original's `now - at > window`.
#[test]
fn the_loss_windows_keep_an_entry_exactly_at_the_edge() {
    let mut b = CircuitBreaker::new(U256::from(10u64), U256::ZERO, 0);
    b.record_failure(U256::from(6u64), T0);
    b.record_failure(U256::from(6u64), at(HOUR.0));
    assert!(b.current_status(at(HOUR.0)).is_tripped, "12 > 10 within the hour");
    // One nanosecond later the first has aged out and only 6 remains.
    assert!(!b.current_status(at(HOUR.0 + 1)).is_tripped);
}

/// A zero limit disables that trigger -- the original's `!limit.is_zero()`.
#[test]
fn a_zero_limit_disables_its_trigger() {
    let mut b = CircuitBreaker::new(U256::ZERO, U256::ZERO, 0);
    for i in 0..1_000u64 {
        b.record_failure(U256::MAX, at(i));
    }
    assert!(!b.current_status(at(2_000)).is_tripped, "zero limits must not trip");
}

/// The revert trigger does not arm below its sample floor. Arming early would
/// trip on a handful of losses, which for a backrunning workload is the normal
/// steady state.
#[test]
fn the_revert_trigger_does_not_arm_below_its_sample_floor() {
    let mut b = CircuitBreaker::new(U256::ZERO, U256::ZERO, 0);
    for i in 0..49u64 {
        b.record_execution_outcome(true, at(i));
    }
    assert!(!b.current_status(at(50)).is_tripped, "49 samples must not arm the trigger");
    b.record_execution_outcome(true, at(50));
    assert!(b.current_status(at(51)).is_tripped, "50 samples at 100% must trip");
}

/// 65% reverting is the normal band, not a fault.
#[test]
fn the_normal_backrunning_revert_band_does_not_trip() {
    let mut b = CircuitBreaker::new(U256::ZERO, U256::ZERO, 0);
    for i in 0..200u64 {
        b.record_execution_outcome(i % 100 < 65, at(i));
    }
    assert!(!b.current_status(at(300)).is_tripped, "a 65% revert rate tripped the breaker");
}

/// The comparison asymmetry is the original's and is preserved: losses and
/// consecutive failures are strictly-greater, RPC errors are >=.
#[test]
fn the_comparison_asymmetry_is_preserved() {
    let mut b = CircuitBreaker::new(U256::ZERO, U256::ZERO, 3);
    for i in 0..3u64 {
        b.record_failure(U256::ZERO, at(i));
    }
    assert!(!b.current_status(at(4)).is_tripped, "3 failures is not > a limit of 3");
    b.record_failure(U256::ZERO, at(4));
    assert!(b.current_status(at(5)).is_tripped);

    let mut r = CircuitBreaker::new(U256::ZERO, U256::ZERO, 0);
    for i in 0..29u64 {
        r.record_rpc_error(at(i));
    }
    assert!(!r.current_status(at(30)).is_tripped);
    r.record_rpc_error(at(30));
    assert!(r.current_status(at(31)).is_tripped, "30 rpc errors IS >= a limit of 30");
}

#[test]
fn a_success_clears_the_consecutive_counter_but_not_the_losses() {
    let mut b = CircuitBreaker::new(U256::from(1u64), U256::ZERO, 2);
    b.record_failure(U256::from(5u64), T0);
    b.record_success();
    let s = b.current_status(at(1));
    assert_eq!(s.consecutive_failures, 0);
    assert_eq!(s.hourly_loss_wei, U256::from(5u64));
    assert!(s.is_tripped, "the loss limit is still breached");
}

#[test]
fn reset_clears_everything() {
    let mut b = CircuitBreaker::new(U256::from(1u64), U256::from(1u64), 1);
    b.record_failure(U256::from(500u64), T0);
    b.record_rpc_error(T0);
    b.record_execution_outcome(true, T0);
    assert!(b.current_status(at(1)).is_tripped);
    let after = b.reset(at(2));
    assert!(!after.is_tripped);
    assert_eq!(after.hourly_loss_wei, U256::ZERO);
    assert_eq!(after.daily_loss_wei, U256::ZERO);
}

/// The daily window is 24 hours, not 24 of something else.
#[test]
fn the_daily_window_is_a_day() {
    let mut b = CircuitBreaker::new(U256::ZERO, U256::from(10u64), 0);
    b.record_failure(U256::from(11u64), T0);
    assert!(b.current_status(at(DAY.0)).is_tripped);
    assert!(!b.current_status(at(DAY.0 + 1)).is_tripped);
}

/// Every status carries a reason, so an operator display never says only
/// "tripped".
#[test]
fn a_tripped_status_always_says_why() {
    let mut b = CircuitBreaker::new(U256::from(1u64), U256::ZERO, 0);
    b.record_failure(U256::from(5u64), T0);
    let s = b.current_status(at(1));
    assert!(s.is_tripped);
    assert!(s.reason.is_some());
    assert!(s.active_reason().contains("hourly loss"));
}
