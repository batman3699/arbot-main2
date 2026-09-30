//! **§28's hard gate**, wired to the real eligibility conjunction, breaker and
//! posture.
//!
//! Three gates in series, and the order is the part worth testing: posture and
//! breaker are facts about the *system*, so a halted chain must reject every
//! candidate with one cause rather than a histogram of nine economic clauses.

mod support;

use apex_econ::eligibility::{Clause, EligibilityPolicy};
use apex_risk::breaker::CircuitBreaker;
use apex_risk::posture::{PostureLadder, RiskTrigger};
use apex_runtime::plane::{Decline, RiskGate};
use apex_runtime::risk::{context_for, LiveRiskGate};
use apex_types::candidate::Candidate;
use apex_types::risk::RiskPosture;
use apex_types::route::CertificateStatus;
use apex_types::sim::{SimulationResult, SimulationTier};
use apex_types::time::{DurationNanos, UnixNanos};
use alloy_primitives::U256;
use support::*;

const NOW: UnixNanos = UnixNanos(1_000_000_000);

fn permissive_policy() -> EligibilityPolicy {
    EligibilityPolicy {
        min_robustness_margin_bps: 50,
        max_state_age: DurationNanos(2_000_000_000),
        min_simulation_tier: 1,
        max_cost_confidence_bps: 10_000,
        min_probability_of_profit_ppm: 500_000,
    }
}

fn gate_with(posture: PostureLadder, breaker: CircuitBreaker) -> LiveRiskGate {
    LiveRiskGate::new(
        posture,
        breaker,
        permissive_policy(),
        Box::new(apex_capture::ManualClock::at(NOW.0)),
    )
}

fn healthy_gate() -> LiveRiskGate {
    gate_with(
        PostureLadder::new(),
        // Limits high enough that nothing here trips them.
        CircuitBreaker::new(U256::from(u128::MAX), U256::from(u128::MAX), 100),
    )
}

/// A candidate that clears all nine clauses.
fn admissible() -> Candidate {
    let mut c = candidate(1, 47_079_437, 320_000_000_000);
    c.robust_ev = 320_000_000_000;
    c.robustness_margin = 0.30;
    c.state_age = DurationNanos(500_000_000);
    c.capture_probability = 0.60;
    c.certificate_status = CertificateStatus::Proven;
    c
}

fn good_sim(c: &Candidate) -> SimulationResult {
    let mut r = SimulationResult {
        tier: SimulationTier::Tier2FullEvm,
        success: true,
        revert: None,
        gas_used: 240_000,
        balance_deltas: std::collections::BTreeMap::new(),
        loan_repaid: true,
        profit_invariant_held: true,
        token_residues: std::collections::BTreeMap::new(),
        state_after: c.state_fingerprint.clone(),
        simulated_at_state: c.state_fingerprint.clone(),
        result_hash: alloy_primitives::B256::ZERO,
        elapsed: DurationNanos(3_000_000),
    };
    r.result_hash = r.canonical_hash();
    r
}

/// The baseline: a healthy system admits an admissible candidate. Without it,
/// every refusal test below is satisfied by a gate that refuses everything.
#[test]
fn a_healthy_system_admits_an_admissible_candidate() {
    let c = admissible();
    assert!(healthy_gate().admit(&c, &good_sim(&c)).is_ok());
}

/// **Posture answers first, and the reason names the posture rather than a
/// clause.**
///
/// A halted chain rejects every candidate for one reason. Evaluating eligibility
/// first would file a per-candidate economic reason for what is actually one
/// system-level stop — the largest and least informative bucket, arriving by a
/// different route than Task 8.1 warned about.
#[test]
fn a_halted_posture_rejects_before_any_clause_is_evaluated() {
    let mut ladder = PostureLadder::new();
    // §28's trigger list. A contract-code fingerprint change is not a market
    // condition and the ladder treats it accordingly.
    let posture = ladder.observe(RiskTrigger::ContractCodeFingerprintChange, NOW);
    assert!(!posture.permits_new_live_tickets(), "the trigger must halt: {posture:?}");

    let gate = gate_with(
        ladder,
        CircuitBreaker::new(U256::from(u128::MAX), U256::from(u128::MAX), 100),
    );
    let c = admissible();
    let err = gate.admit(&c, &good_sim(&c)).expect_err("a halted posture refuses");

    let Decline::RiskRefused { rule } = err else { panic!("{err:?}") };
    assert!(rule.contains("posture"), "the reason names the posture: {rule}");
    for clause in Clause::ALL {
        assert!(
            !rule.contains(clause.label()),
            "a system-level stop must not be reported as clause {}",
            clause.label()
        );
    }
    assert_eq!(gate.posture(), posture);
}

/// The breaker answers second, and also before any clause.
#[test]
fn a_tripped_breaker_rejects_before_any_clause() {
    let mut breaker = CircuitBreaker::new(U256::from(1_000u64), U256::from(1_000u64), 1);
    breaker.record_failure(U256::from(5_000u64), NOW);

    let gate = gate_with(PostureLadder::new(), breaker);
    let c = admissible();
    let err = gate.admit(&c, &good_sim(&c)).expect_err("a tripped breaker refuses");

    let Decline::RiskRefused { rule } = err else { panic!("{err:?}") };
    assert!(rule.contains("circuit breaker"), "{rule}");
    for clause in Clause::ALL {
        assert!(!rule.contains(clause.label()), "not a clause: {rule}");
    }
}

/// **§2.3's nine clauses, each reachable.** A clause no candidate can fail is a
/// clause nothing tests — the same argument as the `sums_to` mutation that
/// survived Task 8.3.
#[test]
fn every_clause_can_stop_a_candidate() {
    let gate = healthy_gate();
    let base = admissible();

    // One perturbation per clause, each failing exactly that clause. Ordered so
    // that an earlier clause is satisfied when a later one is under test, because
    // the gate returns the FIRST failure.
    let mut cases: Vec<(Clause, Candidate, SimulationResult)> = Vec::new();

    let mut c = base.clone();
    c.robust_ev = 0;
    cases.push((Clause::ExpectedNetEvPositive, c.clone(), good_sim(&c)));

    let mut c = base.clone();
    c.robustness_margin = 0.0;
    cases.push((Clause::RobustnessMargin, c.clone(), good_sim(&c)));

    let mut c = base.clone();
    c.state_age = DurationNanos(60_000_000_000);
    cases.push((Clause::StateFreshness, c.clone(), good_sim(&c)));

    let c = base.clone();
    let mut sim = good_sim(&c);
    sim.tier = SimulationTier::Tier0Analytic;
    cases.push((Clause::SimulationFidelity, c, sim));

    let c = base.clone();
    let mut sim = good_sim(&c);
    sim.success = false;
    cases.push((Clause::ExecutionPathHealthy, c, sim));

    let mut c = base.clone();
    if let Some(f) = c.flash_source.as_mut() {
        f.availability_probability = 0.0;
    } else {
        c.flash_source = Some(unavailable_flash());
    }
    cases.push((Clause::FlashLiquidityAvailable, c.clone(), good_sim(&c)));

    let mut c = base.clone();
    c.certificate_status = CertificateStatus::Heuristic;
    cases.push((Clause::RouteAuthorizationValid, c.clone(), good_sim(&c)));

    let mut c = base.clone();
    c.total_execution_cost.gas_used_distribution.p99 = apex_types::cost::GasUsed(u64::MAX / 2);
    cases.push((Clause::CostEstimateConfidence, c.clone(), good_sim(&c)));

    let mut c = base.clone();
    c.capture_probability = 0.01;
    cases.push((Clause::ProbabilityOfProfit, c.clone(), good_sim(&c)));

    assert_eq!(cases.len(), 9, "one perturbation per clause");
    for (clause, c, sim) in cases {
        let err = gate.admit(&c, &sim).expect_err("must be refused");
        let Decline::RiskRefused { rule } = err else { panic!("{clause:?}: {err:?}") };
        assert!(
            rule.contains(clause.label()),
            "expected {} to stop it, got: {rule}",
            clause.label()
        );
        assert!(rule.contains(clause.source()), "the reason cites the section: {rule}");
    }
}

fn unavailable_flash() -> apex_types::flash::FlashSourceQuote {
    apex_types::flash::FlashSourceQuote {
        provider: apex_types::ids::FlashProviderId(1),
        asset: apex_types::ids::TokenId {
            chain: BASE,
            address: alloy_primitives::Address::repeat_byte(0x01),
        },
        amount: U256::from(1u64),
        premium: U256::ZERO,
        gas_overhead: 0,
        callback_constraints: apex_types::flash::CallbackConstraints {
            repay_by_transfer: true,
            reentrancy_permitted: false,
            max_callback_gas: 500_000,
        },
        availability_probability: 0.0,
        state_dependencies: Vec::new(),
        reliability_score: 1.0,
    }
}

/// **A route with no flash source satisfies the liquidity clause rather than
/// failing it.** §19: a route that borrows nothing needs no loan, and the census
/// priced $300–$1,000 trades against inventory.
#[test]
fn borrowing_nothing_is_not_a_liquidity_failure() {
    let mut c = admissible();
    c.flash_source = None;
    let ctx = context_for(&c, &good_sim(&c), NOW);
    assert!(ctx.flash_liquidity_available, "None means borrows nothing, not unavailable");
    assert!(healthy_gate().admit(&c, &good_sim(&c)).is_ok());
}

/// **INV-17.** An approximate route may rank and propose; it may not authorize.
#[test]
fn only_a_proven_certificate_authorizes() {
    let gate = healthy_gate();
    for status in [CertificateStatus::Heuristic, CertificateStatus::InvalidForCertification] {
        let mut c = admissible();
        c.certificate_status = status;
        assert!(
            gate.admit(&c, &good_sim(&c)).is_err(),
            "{status:?} must not authorize a live ticket"
        );
    }
    let mut proven = admissible();
    proven.certificate_status = CertificateStatus::Proven;
    assert!(gate.admit(&proven, &good_sim(&proven)).is_ok());
}

/// **The tier read is the one that answered, not the one requested.**
///
/// `apex-sim`'s header: `SimulationResult::tier` records "which one was actually
/// asked". A candidate may request Tier 2 and be answered by Tier 0, and reading
/// the request would let an analytic screen satisfy a clause that wanted a node.
#[test]
fn the_tier_that_answered_is_what_the_clause_reads() {
    let mut c = admissible();
    // The candidate claims Tier 2...
    c.simulation_tier = SimulationTier::Tier2FullEvm;
    // ...and Tier 0 answered.
    let mut sim = good_sim(&c);
    sim.tier = SimulationTier::Tier0Analytic;

    let err = healthy_gate().admit(&c, &sim).expect_err("Tier 0 is below the policy floor");
    let Decline::RiskRefused { rule } = err else { panic!("{err:?}") };
    assert!(rule.contains(Clause::SimulationFidelity.label()), "{rule}");
}

/// §20: a canary is "never a latency technique, and never a substitute for
/// simulation", so it must not outrank a full EVM run.
#[test]
fn a_canary_does_not_outrank_a_full_evm_run() {
    let mut c = admissible();
    c.certificate_status = CertificateStatus::Proven;
    let mut canary = good_sim(&c);
    canary.tier = SimulationTier::Tier4Canary;

    let strict = LiveRiskGate::new(
        PostureLadder::new(),
        CircuitBreaker::new(U256::from(u128::MAX), U256::from(u128::MAX), 100),
        EligibilityPolicy { min_simulation_tier: 3, ..permissive_policy() },
        Box::new(apex_capture::ManualClock::at(NOW.0)),
    );
    assert!(
        strict.admit(&c, &canary).is_err(),
        "a canary must not satisfy a policy that wanted Tier 3"
    );
}

/// **A zero-p50 cost distribution is maximally wide, not maximally confident.**
///
/// A cost estimate of nothing is an absent estimate rather than a certain one,
/// and treating it as confident would admit a trade nobody priced.
#[test]
fn an_absent_cost_estimate_is_not_a_confident_one() {
    let mut c = admissible();
    c.total_execution_cost.gas_used_distribution.p50 = apex_types::cost::GasUsed(0);
    let ctx = context_for(&c, &good_sim(&c), NOW);
    assert_eq!(ctx.cost_confidence_bps, u32::MAX, "absent, so it fails at every policy");
    assert!(healthy_gate().admit(&c, &good_sim(&c)).is_err());
}

/// `execution_path_healthy` reads **three** facts from the simulation, and each
/// one alone is disqualifying.
///
/// A simulation that succeeded but left the loan unrepaid, or whose profit
/// invariant did not hold, is not a healthy execution path — and the clause test
/// above only perturbs `success`, so without this two of the three conjuncts
/// would go unexercised.
#[test]
fn an_unrepaid_loan_or_a_broken_invariant_is_not_a_healthy_path() {
    let gate = healthy_gate();
    let c = admissible();

    for (name, mutate) in [
        ("loan_repaid", (|s: &mut SimulationResult| s.loan_repaid = false) as fn(&mut SimulationResult)),
        ("profit_invariant_held", |s: &mut SimulationResult| s.profit_invariant_held = false),
    ] {
        let mut sim = good_sim(&c);
        mutate(&mut sim);
        let err = gate.admit(&c, &sim).expect_err("must be refused");
        let Decline::RiskRefused { rule } = err else { panic!("{name}: {err:?}") };
        assert!(
            rule.contains(Clause::ExecutionPathHealthy.label()),
            "{name} must fail the execution-path clause, got: {rule}"
        );
    }
}

/// The gate does not record outcomes. `record_failure` and `LossLedger::record`
/// are the write path and belong where a terminal outcome is known — a gate that
/// also recorded would need `&mut self`, and a gate holding a write lock is a
/// gate on the capture path that a reader waits behind (§2.4).
#[test]
fn the_gate_does_not_record_what_it_sees() {
    let gate = healthy_gate();
    let c = admissible();

    // Admit the same candidate repeatedly; nothing accumulates, so nothing trips.
    for _ in 0..50 {
        assert!(gate.admit(&c, &good_sim(&c)).is_ok());
    }
    assert_eq!(gate.posture(), RiskPosture::Normal, "a read path changed no state");

    // And a refusal does not step the posture either: a candidate failing a
    // clause is an economic answer, not a risk trigger.
    let mut poor = c.clone();
    poor.robust_ev = 0;
    assert!(gate.admit(&poor, &good_sim(&poor)).is_err());
    assert_eq!(gate.posture(), RiskPosture::Normal);
}
