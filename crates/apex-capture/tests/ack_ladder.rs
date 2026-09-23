//! Task 6.6 — INV-34, §24.3, §24.8. C-09's fix.

use apex_capture::dispatch::{
    AckError, AckLadder, DispatchRequest, Dispatcher, Escalation, LifecycleStage, NullDispatcher,
};
use apex_capture::recover::{reconcile, scan, ChainOutcomeSource, DispatchGate};
use apex_capture::registry::TicketRegistry;
use apex_capture::revalidate::{doc_fixture, last_mile, SigningAuthorization};
use apex_types::ids::{ChainId, SubmissionLaneId};
use apex_types::ticket::{OpportunityTicket, SubmissionPolicy, TicketOutcome};
use apex_types::time::UnixNanos;

const T0: UnixNanos = UnixNanos(1_700_000_000_000_000_000);

fn at(offset_ns: u64) -> UnixNanos {
    UnixNanos(T0.0 + offset_ns)
}

/// **INV-34, and C-09's fix.** The legacy code treats a successful
/// `send_raw_transaction` as the submission outcome. Seven stages separate
/// those two facts, and this is the one that must never be crossed.
#[test]
fn ack_does_not_imply_inclusion() {
    let mut l = AckLadder::new(T0);
    for stage in [
        LifecycleStage::TransportAccepted,
        LifecycleStage::NodeKnown,
        LifecycleStage::SequencerReceived,
        LifecycleStage::BuilderAcknowledged,
        LifecycleStage::Preconfirmed,
    ] {
        l.observe(stage, at(1)).unwrap();
        assert!(!l.is_included(), "{stage} was read as inclusion");
    }
    l.observe(LifecycleStage::Included, at(2)).unwrap();
    assert!(l.is_included());
}

/// Exactly two of the seven imply inclusion, and the question is only asked in
/// one place.
#[test]
fn only_two_stages_imply_inclusion() {
    let implying: Vec<_> =
        LifecycleStage::ALL.iter().copied().filter(|s| s.implies_inclusion()).collect();
    assert_eq!(implying, vec![LifecycleStage::Included, LifecycleStage::Finalized]);
}

/// The plan's `each_stage_has_a_timeout`, as a table over all seven.
#[test]
fn each_stage_has_a_timeout() {
    let mut last = 0u64;
    for stage in LifecycleStage::ALL {
        let t = stage.timeout().0;
        assert!(t > 0, "{stage} has no timeout");
        assert!(t > last, "{stage}'s timeout ({t}) does not exceed the previous stage's ({last})");
        last = t;
    }
    assert_eq!(LifecycleStage::ALL.len(), 7, "§24.3 names seven stages");
}

/// And every stage has an escalation rule, distinct from "wait longer".
#[test]
fn each_stage_has_an_escalation_rule() {
    for stage in LifecycleStage::ALL {
        let e = stage.escalation();
        // INV-10: a stage that has been accepted somewhere must resend the SAME
        // bytes, never a re-signed equivalent -- an economically distinct
        // duplicate needs an explicit policy flag.
        if matches!(stage, LifecycleStage::NodeKnown | LifecycleStage::SequencerReceived) {
            assert_eq!(e, Escalation::ResendSameBytes, "{stage}");
        }
        // §27.4: no blind gas escalation. Nothing reaches for it.
        assert_ne!(format!("{e:?}"), "EscalateGas");
    }
}

/// Timeouts fire per stage, and the ladder escalates the stage it is *waiting
/// for* rather than the last rung.
#[test]
fn a_timeout_names_the_stage_it_is_waiting_for() {
    let mut l = AckLadder::new(T0);
    assert_eq!(l.timed_out(T0), None, "nothing has timed out at dispatch");

    let after_transport = at(LifecycleStage::TransportAccepted.timeout().0);
    assert_eq!(
        l.timed_out(after_transport),
        Some((LifecycleStage::TransportAccepted, Escalation::RetryAnotherLane))
    );

    l.observe(LifecycleStage::TransportAccepted, at(1)).unwrap();
    assert_eq!(l.timed_out(after_transport), None, "the next stage is not late yet");

    let after_node = at(LifecycleStage::NodeKnown.timeout().0);
    assert_eq!(
        l.timed_out(after_node),
        Some((LifecycleStage::NodeKnown, Escalation::ResendSameBytes)),
        "a ladder sitting at TransportAccepted is waiting for NodeKnown, not Finalized"
    );
}

/// A stage is a fact, not a counter.
#[test]
fn a_stage_cannot_be_observed_twice() {
    let mut l = AckLadder::new(T0);
    l.observe(LifecycleStage::TransportAccepted, at(1)).unwrap();
    assert_eq!(
        l.observe(LifecycleStage::TransportAccepted, at(2)),
        Err(AckError::AlreadyObserved(LifecycleStage::TransportAccepted))
    );
    assert_eq!(l.observed_at(LifecycleStage::TransportAccepted), Some(at(1)), "the first wins");
}

/// A gap is reported but not rejected. The stages come from different sources
/// -- a transport response, a node query, a sequencer feed, a receipt -- and
/// any of them can be missed while a later one is perfectly real. Refusing an
/// `Included` because `NodeKnown` never arrived would throw away the most
/// important observation the system makes.
#[test]
fn an_out_of_order_observation_is_reported_but_recorded() {
    let mut l = AckLadder::new(T0);
    let e = l.observe(LifecycleStage::Included, at(5)).unwrap_err();
    assert_eq!(
        e,
        AckError::OutOfOrder {
            observed: LifecycleStage::Included,
            missing: LifecycleStage::TransportAccepted
        }
    );
    assert!(l.is_included(), "the observation must still count");
    assert!(l.reached(LifecycleStage::Included));
}

#[test]
fn latency_is_measured_from_dispatch_per_stage() {
    let mut l = AckLadder::new(T0);
    l.observe(LifecycleStage::TransportAccepted, at(3_000_000)).unwrap();
    l.observe(LifecycleStage::NodeKnown, at(11_000_000)).unwrap();
    assert_eq!(l.latency(LifecycleStage::TransportAccepted).unwrap().0, 3_000_000);
    assert_eq!(l.latency(LifecycleStage::NodeKnown).unwrap().0, 11_000_000);
    assert_eq!(l.latency(LifecycleStage::Included), None);
    assert_eq!(l.furthest(), Some(LifecycleStage::NodeKnown));
}

// --------------------------------------------------------- the null dispatcher

struct NothingOutstanding;

impl ChainOutcomeSource for NothingOutstanding {
    fn resolve(&self, _t: &OpportunityTicket) -> Result<TicketOutcome, String> {
        Err("no ticket should need this".to_string())
    }
}

fn open_gate(reg: &TicketRegistry) -> DispatchGate {
    let found = scan(reg.journal()).unwrap();
    let proof = reconcile(reg, &found, &NothingOutstanding, T0).unwrap();
    let gate = DispatchGate::shut();
    gate.open(proof);
    gate
}

fn request() -> DispatchRequest {
    let ctx = doc_fixture();
    let proof = last_mile(&ctx).expect("a passing context");
    DispatchRequest::new(
        SigningAuthorization::new(ctx.ticket, ctx.nonce, proof),
        ChainId::BASE,
        SubmissionLaneId(1),
        SubmissionPolicy::Private,
        vec![0xAB; 250],
    )
}

/// **The null dispatcher returns `TransportAccepted` and nothing more.**
///
/// A null dispatcher that returned a ladder reaching `Included` would make
/// every shadow run report a 100% landing rate — and those numbers are what a
/// decision to go live would rest on.
#[test]
fn the_null_dispatcher_claims_only_what_it_did() {
    let reg = TicketRegistry::in_memory();
    let gate = open_gate(&reg);
    let permit = gate.permit().expect("reconciled");

    let d = NullDispatcher::new();
    let ladder = d.dispatch(&request(), &permit, T0).unwrap();

    assert!(ladder.reached(LifecycleStage::TransportAccepted));
    assert!(!ladder.is_included(), "a shadow run must never report inclusion");
    assert_eq!(ladder.furthest(), Some(LifecycleStage::TransportAccepted));
}

/// It records what would have been sent, which is what makes a shadow run a
/// measurement rather than a rehearsal.
#[test]
fn the_null_dispatcher_records_what_would_have_been_sent() {
    let reg = TicketRegistry::in_memory();
    let gate = open_gate(&reg);
    let permit = gate.permit().expect("reconciled");

    let d = NullDispatcher::new();
    let req = request();
    d.dispatch(&req, &permit, at(7)).unwrap();

    let sent = d.recorded();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].ticket, req.ticket());
    assert_eq!(sent[0].chain, ChainId::BASE);
    assert_eq!(sent[0].lane, SubmissionLaneId(1));
    assert_eq!(sent[0].policy, SubmissionPolicy::Private);
    assert_eq!(sent[0].nonce, req.authorization().nonce().get());
    assert_eq!(sent[0].bytes, 250);
    assert_eq!(sent[0].at, at(7));
}

/// **INV-39 reaches all the way to dispatch.** A `DispatchPermit` is required
/// by the trait method, and one cannot exist before boot-time reconciliation
/// produces its proof — so "dispatch before reconciling" is not a bug to catch
/// in review.
#[test]
fn dispatch_is_unreachable_before_reconciliation() {
    let gate = DispatchGate::shut();
    assert!(gate.permit().is_none());

    let reg = TicketRegistry::in_memory();
    let opened = open_gate(&reg);
    assert!(opened.permit().is_some());
}
