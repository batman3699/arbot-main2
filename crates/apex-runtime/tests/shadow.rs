//! Task 8.5 — **shadow authorization**: a heuristic route may reach the null
//! dispatcher, and never a dispatcher that sends.
//!
//! # Why the waiver exists
//!
//! INV-17 says an approximate route may rank and propose but may not authorize.
//! `LiveEconomics` certifies every candidate `Heuristic` until Phase 12's
//! `certify` exists, so a shadow run that enforced INV-17 would refuse every
//! candidate at the risk gate: nothing would be signed, nothing dispatched, and
//! `SYSTEM_CAPTURE_ASSURANCE` would stay `Undefined` for fourteen days. The run
//! could not pass its own gate.
//!
//! INV-17 guards **live dispatch**. A shadow plane's only dispatcher is §16.1's
//! null one, which sends nothing, so the operator decided (2026-09-30) that a
//! shadow plane waives INV-17 — and that the waiver must be *structurally*
//! unable to reach a lane that sends. These tests are that claim.
//!
//! # What is and is not waived
//!
//! Exactly one clause, and it is named on the outcome. Every other clause, the
//! posture and the breaker still refuse; the simulation still has to succeed;
//! last-mile revalidation still runs; the payload is still signed. The only
//! thing a shadow ticket skips is the transaction.

mod support;

use apex_capture::dispatch::{AckLadder, DispatchError, DispatchRequest, Dispatcher, LifecycleStage};
use apex_capture::recover::{DispatchGate, DispatchPermit};
use apex_capture::registry::TicketRegistry;
use apex_capture::scheduler::CaptureAssurance;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_capture::{InMemoryJournal, ManualClock, NullDispatcher};
use apex_chain::adapter::SignedPayload;
use apex_chain::base::observe::TransactionObservation;
use apex_econ::eligibility::{Clause, EligibilityPolicy};
use apex_risk::breaker::CircuitBreaker;
use apex_risk::posture::PostureLadder;
use apex_runtime::plane::{
    Admission, Decline, DispatchLane, Handled, LaneKind, LiveReadings, Plane, Ports, RiskGate,
    SettlementFeed, Signer,
};
use apex_runtime::risk::LiveRiskGate;
use apex_types::candidate::Candidate;
use apex_types::cost::GasLimit;
use apex_types::ids::SignerLaneId;
use apex_types::route::CertificateStatus;
use apex_types::sim::SimulationResult;
use apex_types::ticket::{TerminalFailure, TicketOutcome, TicketStatus};
use apex_types::time::UnixNanos;
use alloy_primitives::{B256, U256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use support::*;

const BOOT: UnixNanos = UnixNanos(1_000_000_000);

// ------------------------------------------------------------------ doubles

/// The registry's clock, shared, so a test can move time between two steps.
#[derive(Clone)]
struct SharedClock(Arc<ManualClock>);

impl apex_capture::clock::Clock for SharedClock {
    fn now(&self) -> UnixNanos {
        self.0.now()
    }
}

/// Counts signatures, and can spend time while signing — which is the one
/// window between last-mile revalidation and dispatch where a ticket can go
/// late without anything upstream having refused it.
#[derive(Default)]
struct CountingSigner {
    calls: AtomicUsize,
    spend: Option<(Arc<ManualClock>, u64)>,
}

impl CountingSigner {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Signer for CountingSigner {
    fn sign(
        &self,
        auth: &apex_capture::revalidate::SigningAuthorization,
        call: &apex_exec::call::ExecutorCall,
        gas_limit: GasLimit,
        fees: apex_runtime::plane::FeeCaps,
    ) -> Result<SignedPayload, Decline> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some((clock, nanos)) = &self.spend {
            clock.advance(*nanos);
        }
        EchoSigner.sign(auth, call, gas_limit, fees)
    }
}

/// A dispatcher that **sends** — or would. Counts, so a test can assert it was
/// never reached.
#[derive(Default)]
struct SendingDispatcher {
    calls: AtomicUsize,
}

impl Dispatcher for SendingDispatcher {
    fn dispatch(
        &self,
        _req: &DispatchRequest,
        _permit: &DispatchPermit<'_>,
        now: UnixNanos,
    ) -> Result<AckLadder, DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut ladder = AckLadder::new(now);
        let _ = ladder.observe(LifecycleStage::TransportAccepted, now);
        Ok(ladder)
    }
}

/// Counts every question about the chain. A shadow ticket has no transaction,
/// so asking where it is would be asking about nothing.
#[derive(Default)]
struct CountingSettlement {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl SettlementFeed for CountingSettlement {
    async fn observe(&self, _tx: B256) -> Result<Vec<TransactionObservation>, Decline> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

/// A gate that grants a shadow-only admission **whatever lane it is told**. The
/// real gate never does that; this one stands in for a gate that someday gets
/// it wrong, so the plane's own check is what is under test.
struct RogueGate;

impl RiskGate for RogueGate {
    fn admit(
        &self,
        _c: &Candidate,
        _sim: &SimulationResult,
        _lane: LaneKind,
    ) -> Result<Admission, Decline> {
        Ok(Admission::ShadowOnly { waived: vec![Clause::RouteAuthorizationValid] })
    }
}

// ------------------------------------------------------------------ fixtures

/// The policy the shadow run holds: the default. It once needed a permissive
/// cost-confidence cap here, while the clause read the gas estimate's width
/// against itself and refused every candidate; read in money, against the
/// profit, the default admits what the economics produces.
fn real_gate() -> Arc<LiveRiskGate> {
    Arc::new(LiveRiskGate::new(
        PostureLadder::new(),
        CircuitBreaker::new(U256::from(u128::MAX), U256::from(u128::MAX), 100),
        EligibilityPolicy::default(),
        Box::new(ManualClock::at(BOOT.0)),
    ))
}

/// What `LiveEconomics` produces today: everything clears except INV-17.
fn heuristic() -> Candidate {
    let mut c = candidate(1, 47_079_437, 320_000_000_000);
    c.certificate_status = CertificateStatus::Heuristic;
    c
}

fn pool() -> SignerPool {
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&EXECUTOR_VERSION.to_be_bytes());
    SignerPool::new(
        ExecutorAuth { chain: BASE, executor: EXECUTOR, executor_version: version },
        vec![LaneConfig {
            id: SignerLaneId(1),
            address: [0x22; 20],
            gas_reserve_wei: 1_000_000_000_000_000_000,
        }],
    )
}

struct Rig {
    dispatch: DispatchLane,
    risk: Arc<dyn RiskGate>,
    candidate: Candidate,
    signer: Arc<CountingSigner>,
    settlement: Arc<CountingSettlement>,
    clock: Arc<ManualClock>,
    readings: LiveReadings,
}

impl Rig {
    fn shadow(null: &Arc<NullDispatcher>) -> Self {
        Self {
            dispatch: DispatchLane::Shadow(Arc::clone(null)),
            risk: real_gate(),
            candidate: heuristic(),
            signer: Arc::new(CountingSigner::default()),
            settlement: Arc::new(CountingSettlement::default()),
            clock: Arc::new(ManualClock::at(BOOT.0)),
            readings: readings(),
        }
    }

    fn live(sender: &Arc<SendingDispatcher>) -> Self {
        let null = Arc::new(NullDispatcher::new());
        Self {
            dispatch: DispatchLane::Live(Arc::clone(sender) as Arc<dyn Dispatcher + Send + Sync>),
            ..Self::shadow(&null)
        }
    }

    fn plane(&self) -> Plane {
        let plane = Plane::new(Ports {
            registry: Arc::new(TicketRegistry::new(
                Box::new(InMemoryJournal::new()),
                Box::new(SharedClock(Arc::clone(&self.clock))),
            )),
            pool: Arc::new(pool()),
            gate: Arc::new(DispatchGate::shut()),
            dispatch: self.dispatch.clone(),
            chain: Arc::new(FakeChain::landing()),
            search: Arc::new(FixedSearch::new(vec![self.candidate.clone()])),
            econ: Arc::new(PassThroughEconomics(self.candidate.clone())),
            sim: Arc::new(AlwaysSucceeds),
            risk: Arc::clone(&self.risk),
            commitments: Arc::new(FixtureCommitments),
            calls: Arc::new(FixtureCalls),
            signer: Arc::clone(&self.signer) as Arc<dyn Signer>,
            live: Arc::new(FixedReadings(self.readings.clone())),
            settlement: Arc::clone(&self.settlement) as Arc<dyn SettlementFeed>,
        });
        plane.boot(&NoChain, BOOT).expect("boot on an empty journal");
        plane
    }
}

/// One event through the plane, expecting exactly one closed ticket.
async fn one_outcome(plane: &Plane, event: usize) -> TicketOutcome {
    let handled = plane.on_event(&recorded_stream()[event]).await;
    match &handled[..] {
        [Handled::Closed { outcome, .. }] => (**outcome).clone(),
        other => panic!("expected one closed ticket, got {other:?}"),
    }
}

fn refused_by(outcome: &TicketOutcome) -> String {
    match outcome {
        TicketOutcome::ExplicitFailure { code: TerminalFailure::RiskRejected { rule }, .. } => {
            rule.clone()
        }
        other => panic!("expected a risk refusal, got {other:?}"),
    }
}

// ------------------------------------------------------------------ the waiver

/// **The decision, as a test.** A heuristic route — which is every route
/// `LiveEconomics` produces today — goes through signing to the null dispatcher
/// on a shadow plane, and the outcome names the one clause that was waived.
#[tokio::test]
async fn a_heuristic_route_reaches_the_null_dispatcher_in_shadow() {
    let null = Arc::new(NullDispatcher::new());
    let rig = Rig::shadow(&null);
    let plane = rig.plane();

    match one_outcome(&plane, 0).await {
        TicketOutcome::ShadowDispatched { stage, in_time, waived, .. } => {
            assert_eq!(stage, TicketStatus::Acknowledged);
            assert!(in_time);
            assert_eq!(waived, vec![Clause::RouteAuthorizationValid.label().to_string()]);
        }
        other => panic!("expected a shadow dispatch, got {other:?}"),
    }
    assert_eq!(null.count(), 1, "the null dispatcher recorded it");
    assert_eq!(rig.signer.calls(), 1, "the payload was signed");
}

/// The same route on a lane that sends is refused **at the gate**, before a
/// signature exists, and the refusal names INV-17's clause.
#[tokio::test]
async fn a_heuristic_route_is_refused_on_a_live_lane() {
    let sender = Arc::new(SendingDispatcher::default());
    let rig = Rig::live(&sender);
    let plane = rig.plane();

    // Exactly the GATE's refusal. The plane's own INV-17 backstop would also
    // refuse this ticket, and its message names the same clause -- so a looser
    // assertion passed with the gate waiving on a live lane, the backstop
    // covering for it. Two guards covering for each other is how neither gets
    // tested; this pins the first one.
    let clause = Clause::RouteAuthorizationValid;
    let rule = refused_by(&one_outcome(&plane, 0).await);
    assert_eq!(rule, format!("{} ({})", clause.label(), clause.source()));
    assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.signer.calls(), 0, "a signature was produced for a refused route");
}

/// **The structural half.** A gate that grants a shadow-only admission on a
/// live plane — the real one never does — is refused by the plane itself,
/// before signing. The waiver cannot reach a lane that sends even when the
/// component that grants it is wrong.
#[tokio::test]
async fn a_shadow_waiver_cannot_reach_a_live_lane() {
    let sender = Arc::new(SendingDispatcher::default());
    let rig = Rig { risk: Arc::new(RogueGate), ..Rig::live(&sender) };
    let plane = rig.plane();

    let rule = refused_by(&one_outcome(&plane, 0).await);
    assert!(rule.contains("INV-17"), "refused for: {rule}");
    assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.signer.calls(), 0);
}

/// Exactly one clause is waivable. A heuristic route that *also* fails another
/// clause is refused on a shadow plane for that other clause — otherwise the
/// shadow run would be measuring trades nothing would ever authorize.
#[tokio::test]
async fn only_route_authorization_is_waived() {
    let null = Arc::new(NullDispatcher::new());
    let mut rig = Rig::shadow(&null);
    rig.candidate.probability_of_profit_ppm = 100_000;
    let plane = rig.plane();

    let rule = refused_by(&one_outcome(&plane, 0).await);
    assert!(rule.contains(Clause::ProbabilityOfProfit.label()), "refused for: {rule}");
    assert_eq!(null.count(), 0);
    assert_eq!(rig.signer.calls(), 0);
}

/// A proven route on a shadow plane waives nothing, and says so. The waiver
/// list is evidence, not a label every shadow ticket carries.
#[tokio::test]
async fn a_proven_route_in_shadow_waives_nothing() {
    let null = Arc::new(NullDispatcher::new());
    let mut rig = Rig::shadow(&null);
    rig.candidate.certificate_status = CertificateStatus::Proven;
    let plane = rig.plane();

    match one_outcome(&plane, 0).await {
        TicketOutcome::ShadowDispatched { waived, .. } => assert!(waived.is_empty(), "{waived:?}"),
        other => panic!("expected a shadow dispatch, got {other:?}"),
    }
}

// ------------------------------------------------------------------ after dispatch

/// Nothing was sent, so there is nothing on chain to look for, and the ticket
/// is not a miss: the plane took the opportunity as far as a shadow plane can.
#[tokio::test]
async fn a_shadow_ticket_is_not_looked_for_on_chain_and_is_not_a_miss() {
    let null = Arc::new(NullDispatcher::new());
    let rig = Rig::shadow(&null);
    let plane = rig.plane();
    one_outcome(&plane, 0).await;

    assert_eq!(rig.settlement.calls.load(Ordering::SeqCst), 0);
    assert!(plane.misses().is_empty(), "filed as a miss: {:?}", plane.misses().by_reason());
}

/// A shadow close is **neither** a success nor a failure, and INV-02's
/// accounting still balances around it.
#[tokio::test]
async fn a_shadow_close_is_counted_as_itself() {
    let null = Arc::new(NullDispatcher::new());
    let plane = Rig::shadow(&null).plane();
    one_outcome(&plane, 0).await;

    let m = plane.registry().metrics();
    assert_eq!(m.tickets_terminal_shadow, 1);
    assert_eq!(m.tickets_terminal_success, 0);
    assert_eq!(m.tickets_terminal_failure, 0);
    assert_eq!(m.ticket_drop_count(), 0, "INV-02");
}

/// The nonce a shadow ticket reserved was never consumed on chain, so it goes
/// back. A shadow run that burnt a nonce per ticket would reserve nonces the
/// chain never saw, and every later ticket would sign against a gap.
#[tokio::test]
async fn a_shadow_run_does_not_burn_nonces() {
    let null = Arc::new(NullDispatcher::new());
    let plane = Rig::shadow(&null).plane();
    one_outcome(&plane, 0).await;
    one_outcome(&plane, 1).await;

    let sent = null.recorded();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].nonce, readings().chain_pending_nonce);
    assert_eq!(sent[1].nonce, sent[0].nonce, "the first ticket's nonce was not released");
}

// ------------------------------------------------------------------ criterion 1

/// `SYSTEM_CAPTURE_ASSURANCE = dispatched within deadline / admitted for live
/// dispatch`. Nothing authorized is `Undefined`, not 1.0; one ticket authorized
/// and dispatched on time is `Measured(1.0)`.
#[tokio::test]
async fn capture_assurance_is_measured_over_authorized_tickets() {
    let null = Arc::new(NullDispatcher::new());
    let plane = Rig::shadow(&null).plane();
    assert_eq!(plane.capture_assurance(), CaptureAssurance::Undefined);

    one_outcome(&plane, 0).await;
    assert_eq!(plane.capture_assurance(), CaptureAssurance::Measured(1.0));
}

/// A ticket that reaches the dispatcher after its deadline was authorized and
/// not captured in time. It still closes as a shadow dispatch — the transaction
/// would have been sent — and it counts against the ratio.
#[tokio::test]
async fn a_late_dispatch_counts_against_capture_assurance() {
    let null = Arc::new(NullDispatcher::new());
    let mut rig = Rig::shadow(&null);
    let past_deadline = rig.candidate.deadline.0 - BOOT.0 + 1;
    rig.signer = Arc::new(CountingSigner {
        calls: AtomicUsize::new(0),
        spend: Some((Arc::clone(&rig.clock), past_deadline)),
    });
    let plane = rig.plane();

    match one_outcome(&plane, 0).await {
        TicketOutcome::ShadowDispatched { in_time, .. } => assert!(!in_time),
        other => panic!("expected a shadow dispatch, got {other:?}"),
    }
    assert_eq!(plane.capture_assurance(), CaptureAssurance::Measured(0.0));
}

/// Authorized and then refused at the last mile: admitted, never dispatched.
/// That is precisely the engineering loss the figure exists to count.
#[tokio::test]
async fn a_ticket_refused_after_authorization_counts_against_capture_assurance() {
    let null = Arc::new(NullDispatcher::new());
    let mut rig = Rig::shadow(&null);
    rig.readings.executor = [0x99; 20];
    let plane = rig.plane();

    one_outcome(&plane, 0).await;
    assert_eq!(null.count(), 0);
    assert_eq!(plane.capture_assurance(), CaptureAssurance::Measured(0.0));
}

/// A refusal at the gate is not an admission. Counting it would make the
/// denominator include trades the system decided not to make, and the figure
/// would fall whenever the market went quiet.
#[tokio::test]
async fn a_refusal_at_the_gate_is_not_admitted() {
    let sender = Arc::new(SendingDispatcher::default());
    let plane = Rig::live(&sender).plane();
    one_outcome(&plane, 0).await;
    assert_eq!(plane.capture_assurance(), CaptureAssurance::Undefined);
}
