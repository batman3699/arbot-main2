//! `RiskGate`, wired to the real eligibility conjunction, breaker and posture.
//!
//! # Three gates in series, and each answers a different question
//!
//! §28's sentence is "**risk is a hard execution gate**, not advice", and the
//! plane has one port for it. Three things have to hold before a candidate may
//! authorize a live ticket, and folding them into one boolean would lose which:
//!
//! | Gate | Owner | The question |
//! |---|---|---|
//! | Posture | `apex_risk::PostureLadder` | May this chain and strategy open new live tickets at all? |
//! | Breaker | `apex_risk::CircuitBreaker` | Has recent loss or failure exceeded a limit? |
//! | Eligibility | `apex_econ::EligibilityGate` | Does *this* candidate clear §2.3's nine clauses and §2.1's robust gate? |
//!
//! The order is deliberate and it is not the cheapest-first order a latency
//! argument would give. **Posture and breaker are facts about the system**, so a
//! halted chain rejects every candidate with the same reason and an operator sees
//! one cause rather than a histogram of nine clauses. Evaluating eligibility
//! first would file a per-candidate economic reason for what is actually one
//! system-level stop — the largest and least informative bucket, arriving by a
//! different route than Task 8.1 warned about.
//!
//! # Rejection is one clause, not a set
//!
//! `EligibilityGate` returns the **first** failing clause, and that is its
//! choice rather than this module's: *"the histogram counts what stopped the
//! candidate, and a candidate failing five clauses is one rejection, not five."*
//!
//! # What this module does not do
//!
//! It does not *record* outcomes. `CircuitBreaker::record_failure` and
//! `LossLedger::record` are the write path, and they belong where a terminal
//! outcome is known — the plane's close, not its gate. A gate that also recorded
//! would need `&mut self`, and a gate holding a write lock is a gate on the
//! capture path that a reader waits behind (§2.4).
//!
//! The one exception is deliberate: the breaker's own `current_status` takes
//! `&mut self` because it evicts expired loss windows as it reads. That is
//! contained behind this module's lock rather than exposed, and the lock is on
//! the read path — which is a cost, stated here rather than hidden. It is
//! acceptable only because the alternative is a breaker that reports stale
//! windows, and a breaker that under-reports loss is worse than a microsecond.

use crate::plane::{Admission, Decline, LaneKind, RiskGate};
use apex_econ::eligibility::{Clause, Decision, EligibilityContext, EligibilityPolicy};
use apex_econ::eligibility::EligibilityGate;
use apex_risk::breaker::CircuitBreaker;
use apex_risk::posture::PostureLadder;
use apex_types::candidate::Candidate;
use apex_types::risk::RiskPosture;
use apex_types::sim::SimulationResult;
use apex_types::time::UnixNanos;
use std::sync::Mutex;

/// §28's hard gate.
pub struct LiveRiskGate {
    posture: Mutex<PostureLadder>,
    breaker: Mutex<CircuitBreaker>,
    policy: EligibilityPolicy,
    /// Read for the state-freshness clause. Injected, because a gate that read a
    /// clock could not be tested against a deadline without sleeping — the same
    /// argument `apex_capture::clock` makes.
    clock: Box<dyn apex_capture::clock::Clock>,
}

impl LiveRiskGate {
    pub fn new(
        posture: PostureLadder,
        breaker: CircuitBreaker,
        policy: EligibilityPolicy,
        clock: Box<dyn apex_capture::clock::Clock>,
    ) -> Self {
        Self {
            posture: Mutex::new(posture),
            breaker: Mutex::new(breaker),
            policy,
            clock,
        }
    }

    pub fn policy(&self) -> EligibilityPolicy {
        self.policy
    }

    /// The current posture, for a caller reporting system state rather than
    /// asking about a candidate.
    pub fn posture(&self) -> RiskPosture {
        self.with_posture(PostureLadder::posture)
    }

    fn with_posture<R>(&self, f: impl FnOnce(&PostureLadder) -> R) -> R {
        // Recovering from a poisoned lock rather than propagating. A panic
        // happened while the ladder was being updated; refusing every candidate
        // for ever afterwards is a halt nobody asked for, and the posture it
        // reports is either the old value or the new one — both of which are
        // real postures this system has held.
        match self.posture.lock() {
            Ok(g) => f(&g),
            Err(p) => f(&p.into_inner()),
        }
    }
}

impl RiskGate for LiveRiskGate {
    fn admit(
        &self,
        c: &Candidate,
        sim: &SimulationResult,
        lane: LaneKind,
    ) -> Result<Admission, Decline> {
        // ---- 1. Posture. A fact about the system, so it answers first.
        let posture = self.posture();
        if !posture.permits_new_live_tickets() {
            return Err(Decline::RiskRefused {
                rule: format!("posture {posture:?} does not permit new live tickets"),
            });
        }

        // ---- 2. Breaker.
        let now = self.clock.now();
        let tripped = {
            let mut b = match self.breaker.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let status = b.current_status(now);
            status.is_tripped.then(|| status.active_reason())
        };
        if let Some(reason) = tripped {
            return Err(Decline::RiskRefused { rule: format!("circuit breaker: {reason}") });
        }

        // ---- 3. This candidate, against §2.3's nine clauses.
        let mut ctx = context_for(c, sim, now);

        // INV-17 guards LIVE dispatch: an approximate route may rank and
        // propose, and may not authorize a transaction. A shadow plane's lane
        // holds the null dispatcher -- a type that cannot send -- so on a shadow
        // lane the clause is waived, and **named** on the admission, so shadow
        // evidence is never read as a trade the live gate would have authorized.
        //
        // Exactly this clause and nothing else. Posture and breaker above are
        // never waived, and neither is any other clause: a shadow run measuring
        // trades that fail the EV or Pr(profit) clauses would be measuring trades
        // nothing would ever make. Operator decision, 2026-09-30.
        let waived = match lane {
            LaneKind::Shadow if !ctx.route_authorization_valid => {
                ctx.route_authorization_valid = true;
                vec![Clause::RouteAuthorizationValid]
            }
            LaneKind::Shadow | LaneKind::Live => Vec::new(),
        };

        match EligibilityGate::evaluate(&ctx, &self.policy) {
            Decision::Admit if waived.is_empty() => Ok(Admission::Full),
            Decision::Admit => Ok(Admission::ShadowOnly { waived }),
            Decision::Reject { clause } => Err(Decline::RiskRefused {
                rule: format!("{} ({})", clause.label(), clause.source()),
            }),
        }
    }
}

/// Everything the nine clauses read, from a candidate and its simulation.
///
/// # `state_age` is measured, not carried
///
/// `Candidate::state_age` is what the search recorded when it found the route.
/// By the time the gate runs, more time has passed — and §2.3's freshness clause
/// is about the state the trade will execute against, not the state the search
/// saw. So the age is recomputed here from the clock. Trusting the carried value
/// would make every candidate look as fresh as the moment it was proposed, which
/// is exactly the measurement error the clause exists to catch.
///
/// # `simulation_tier` comes from the result, not the request
///
/// `SimulationResult::tier` records "which one was actually asked", per
/// `apex-sim`'s own header. A candidate may *request* Tier 2 and be answered by
/// Tier 0; reading the request would let an analytic screen satisfy a clause that
/// wanted a node.
pub fn context_for(
    c: &Candidate,
    sim: &SimulationResult,
    now: UnixNanos,
) -> EligibilityContext {
    EligibilityContext {
        expected_net_ev_wei: c.robust_ev,
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        robustness_margin_bps: (c.robustness_margin.clamp(0.0, 1.0) * 10_000.0) as u32,
        state_age: measured_state_age(c, now),
        simulation_tier: tier_ordinal(sim.tier),
        // A simulation that reverted or left the loan unrepaid is not a healthy
        // execution path, whatever the candidate said when it was proposed.
        execution_path_healthy: sim.success && sim.loan_repaid && sim.profit_invariant_held,
        // §19: a route with no flash source needs no flash liquidity. `None` here
        // means "borrows nothing", which satisfies the clause rather than failing
        // it — the census priced $300–$1,000 trades against inventory.
        flash_liquidity_available: c
            .flash_source
            .as_ref()
            .is_none_or(|f| f.availability_probability > 0.0),
        // INV-17: an approximate route may rank and propose; it may not
        // authorize. `CertificateStatus::Proven` is the only value that does.
        route_authorization_valid: matches!(
            c.certificate_status,
            apex_types::route::CertificateStatus::Proven
        ),
        cost_confidence_bps: cost_confidence_bps(c),
        // **`Pr(Π > 0)`, not `P(lands)`.** The first draft read
        // `capture_probability` here, because that was the only probability a
        // `Candidate` carried — and they are different quantities. A route that
        // lands every time and loses money on nine scenarios out of ten has a
        // capture probability of 1.0 and a profit probability of 0.1, so reading
        // the first for the second admits exactly the trades §2.1's robust gate
        // exists to refuse. `Candidate::probability_of_profit_ppm` was added for
        // this, filled by `apex-econ` from `probability_of_profit_ppm(set)`.
        probability_of_profit_ppm: c.probability_of_profit_ppm,
    }
}

/// The age of the state this candidate was priced against, as of `now`.
///
/// Falls back to the carried `state_age` only when the candidate's
/// `validity_start` cannot be recovered — which for a `Candidate` is always,
/// since it carries no timestamp of its own. So this is the carried value plus
/// nothing, and **that is a gap rather than a design**: `Candidate` has a
/// `state_age` and no `observed_at`, so the gate cannot tell how long the
/// candidate has been in flight. Recorded in PLAN.md; the honest interim is to
/// use what exists and say so, rather than compute a freshness number from a
/// field that does not mean what the clause needs.
fn measured_state_age(c: &Candidate, _now: UnixNanos) -> apex_types::time::DurationNanos {
    c.state_age
}

/// §20's tiers as the ordinal the policy compares against.
const fn tier_ordinal(tier: apex_types::sim::SimulationTier) -> u8 {
    use apex_types::sim::SimulationTier as T;
    match tier {
        T::Tier0Analytic => 0,
        T::Tier1LocalExact => 1,
        T::Tier2FullEvm => 2,
        T::Tier3Adversarial => 3,
        // §20: "never a latency technique, and never a substitute for
        // simulation". A canary is not a higher tier than a full EVM run and
        // must not satisfy a clause that wanted one, so it ranks at Tier 2's
        // level rather than above it.
        T::Tier4Canary => 2,
    }
}

/// How wide the cost estimate is, in bps of itself.
///
/// §23.1 requires a distribution rather than a scalar, and `TotalExecutionCost`
/// carries one: the p99/p50 spread of gas used is the width the clause is about.
/// **Zero p50 is refused as maximally wide rather than treated as certain** — a
/// cost estimate of nothing is not a confident estimate, it is an absent one, and
/// `u32::MAX` here fails the clause at every policy.
fn cost_confidence_bps(c: &Candidate) -> u32 {
    let d = &c.total_execution_cost.gas_used_distribution;
    let (p50, p99) = (d.p50.0, d.p99.0);
    if p50 == 0 {
        return u32::MAX;
    }
    let spread = p99.saturating_sub(p50);
    u32::try_from(spread.saturating_mul(10_000) / p50).unwrap_or(u32::MAX)
}

/// The clauses, exposed so a caller can report which one stopped a candidate
/// without re-deriving the list.
pub const CLAUSES: [Clause; 9] = Clause::ALL;
