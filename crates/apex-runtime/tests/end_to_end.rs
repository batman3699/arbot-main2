//! Task 8.4's failing test: **a recorded event stream, driven end to end, until
//! a ticket reaches `Reconciled`.**
//!
//! # Reaching `Reconciled` is not the assertion
//!
//! `TicketStatus::Reconciled` is the last variant, and `TicketGuard::advance`
//! accepts any forward move. So a plane that did nothing but
//! `advance(Reconciled)` would satisfy the task's wording exactly, with no
//! reservation, no revalidation, no dispatch and no receipt. The destination is
//! cheap; the **path** is the claim.
//!
//! So the assertion is over the journal. Every transition appends an `Advanced`
//! entry, which is BP-177's "every control-plane transition is observable" in its
//! literal form, and the recorded sequence is what a skipped step shows up in. A
//! plane that jumps to the end produces a journal with two entries and fails
//! here.

mod support;

use apex_capture::journal::JournalEntry;
use apex_capture::recover::DispatchGate;
use apex_capture::registry::TicketRegistry;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_capture::{InMemoryJournal, ManualClock, NullDispatcher};
use apex_runtime::plane::{
    Economics, Handled, Plane, Ports, SettlementFeed,
};
use apex_types::ids::SignerLaneId;
use apex_types::ticket::TicketStatus;
use apex_types::time::UnixNanos;
use std::sync::Arc;
use std::time::Duration;
use support::*;

const BOOT: UnixNanos = UnixNanos(1_000_000_000);

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

fn plane_with(
    search: Arc<FixedSearch>,
    econ: Arc<dyn Economics>,
    settlement: Arc<dyn SettlementFeed>,
) -> Plane {
    Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(
            Box::new(InMemoryJournal::new()),
            Box::new(ManualClock::at(BOOT.0)),
        )),
        pool: Arc::new(pool()),
        gate: Arc::new(DispatchGate::shut()),
        dispatcher: Arc::new(NullDispatcher::new()),
        chain: Arc::new(FakeChain::landing()),
        search,
        econ,
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        signer: Arc::new(EchoSigner),
        live: Arc::new(FixedReadings(readings())),
        settlement,
    })
}

fn landing_plane() -> Plane {
    plane_with(
        Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        Arc::new(PassThroughEconomics),
        Arc::new(LandsAndFinalizes),
    )
}

/// **The task's test.** The recorded stream goes in; tickets come out the far end
/// having been reconciled against the chain.
#[tokio::test]
async fn the_recorded_stream_drives_a_ticket_to_reconciled() {
    let plane = landing_plane();
    plane.boot(&NoChain, BOOT).expect("boot on an empty journal");

    let stream = recorded_stream();
    assert_eq!(stream.len(), 4, "the fixture is four events, one of them a redelivery");

    let mut reconciled = 0;
    for event in &stream {
        for handled in plane.on_event(event).await {
            match handled {
                Handled::Closed { outcome, .. } => {
                    assert!(outcome.is_success(), "the fixture lands: {outcome:?}");
                    reconciled += 1;
                }
                other => panic!("expected a closed ticket, got {other:?}"),
            }
        }
    }

    assert_eq!(reconciled, 4, "one ticket per delivered event");
    let m = plane.registry().metrics();
    assert_eq!(m.tickets_terminal_success, reconciled);
    assert_eq!(m.ticket_drop_count(), 0, "INV-01: nothing may disappear");
    assert_eq!(plane.commitments_in_flight(), 0, "every lock was released");
}

/// BP-177, and the reason the test above is not enough on its own. The journal
/// must show the ticket walking every status, in order.
#[tokio::test]
async fn the_journal_records_every_transition_in_order() {
    let plane = landing_plane();
    plane.boot(&NoChain, BOOT).expect("boot");

    let stream = recorded_stream();
    let first = stream.first().expect("at least one event");
    let handled = plane.on_event(first).await;
    assert!(matches!(handled.first(), Some(Handled::Closed { .. })), "{handled:?}");

    let entries = plane.registry().journal().replay().expect("replay");
    let advanced: Vec<TicketStatus> = entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Advanced { to, .. } => Some(*to),
            _ => None,
        })
        .collect();

    // Every status from `Reserved` to `Reconciled`. `Observed` is the status a
    // ticket is admitted at, so it is an `Admitted` entry rather than an
    // `Advanced` one.
    assert_eq!(
        advanced,
        vec![
            TicketStatus::Reserved,
            TicketStatus::Exacting,
            TicketStatus::Simulated,
            TicketStatus::Authorized,
            TicketStatus::Signed,
            TicketStatus::Dispatching,
            TicketStatus::Acknowledged,
            TicketStatus::Preconfirmed,
            TicketStatus::Included,
            TicketStatus::Finalized,
            TicketStatus::Reconciled,
        ],
        "a skipped step is a missing entry here"
    );

    assert!(
        matches!(entries.first(), Some(JournalEntry::Admitted { .. })),
        "the first entry is the admission"
    );
    assert!(
        matches!(entries.last(), Some(JournalEntry::Closed { .. })),
        "the last entry is the terminal outcome"
    );
}

/// **The other half of the sequence claim.** A chain with no preconfirmation
/// window must produce a journal with no `Preconfirmed` entry — and a ticket that
/// still reaches `Reconciled`.
///
/// Without this test, the one above is satisfied by a plane that walks a hardcoded
/// list of eleven statuses. With it, the walk has to be driven by what was
/// actually observed.
#[tokio::test]
async fn an_unobserved_stage_is_not_recorded() {
    let plane = plane_with(
        Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        Arc::new(PassThroughEconomics),
        Arc::new(IncludesWithoutPreconfirming),
    );
    plane.boot(&NoChain, BOOT).expect("boot");

    let stream = recorded_stream();
    let handled = plane.on_event(stream.first().expect("an event")).await;
    let Some(Handled::Closed { outcome, .. }) = handled.first() else {
        panic!("expected a closed ticket, got {handled:?}");
    };
    assert!(outcome.is_success(), "an included transaction reconciles: {outcome:?}");

    let advanced: Vec<TicketStatus> = plane
        .registry()
        .journal()
        .replay()
        .expect("replay")
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Advanced { to, .. } => Some(*to),
            _ => None,
        })
        .collect();

    assert!(
        !advanced.contains(&TicketStatus::Preconfirmed),
        "nothing preconfirmed it, so nothing may say it did: {advanced:?}"
    );
    assert!(
        !advanced.contains(&TicketStatus::Finalized),
        "the L1 batch was never reported finalized: {advanced:?}"
    );
    assert!(advanced.contains(&TicketStatus::Included));
    assert_eq!(advanced.last(), Some(&TicketStatus::Reconciled));
}

/// §17.4 / §25: `ExecutionCommitment::hash()` is the deduplication key, and an
/// in-flight ticket with an identical commitment suppresses a new one.
#[tokio::test]
async fn an_identical_commitment_cannot_be_locked_twice() {
    let plane = landing_plane();
    let c = candidate(1, 47_079_437, 320_000_000_000);

    let a = plane.lock_exclusive(&c);
    let b = plane.lock_exclusive(&c);
    assert!(a.is_ok(), "the first lock takes the commitment");
    let Err(suppressed) = b else { panic!("the second lock must be suppressed") };
    assert_eq!(
        suppressed.commitment,
        a.as_ref().map(|l| l.hash()).unwrap_or_default(),
        "the suppression names the commitment it collided with"
    );

    drop(a);
    assert!(plane.lock_exclusive(&c).is_ok(), "releasing the lock frees the commitment");
}

/// The same thing through the plane, concurrently — which is the case that
/// actually happens. A websocket reconnect replays a block while the first
/// delivery is still in flight.
///
/// The barriered economics is what makes this deterministic rather than a race:
/// the first call parks inside §46.2's join, so the second is guaranteed to reach
/// the lock while the first still holds it.
#[tokio::test]
async fn a_concurrent_redelivery_produces_one_ticket() {
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let plane = plane_with(
        Arc::new(FixedSearch::new(vec![candidate(1, 47_079_438, 320_000_000_000)])),
        Arc::new(BarrieredEconomics { barrier }),
        Arc::new(LandsAndFinalizes),
    );
    plane.boot(&NoChain, BOOT).expect("boot");

    let stream = recorded_stream();
    // Events 2 and 3 of the fixture are byte-identical.
    let (second, third) = (&stream[1], &stream[2]);
    assert_eq!(second, third, "the fixture's redelivery must be identical");

    // Bounded, and not merely out of caution. `BarrieredEconomics` makes a serial
    // §46.2 join **deadlock** rather than fail, which is the right shape for
    // `workers::independent_tasks_run_concurrently` -- and would hang this test
    // forever without a bound. A hanging test is worse than a failing one: CI
    // reports a timeout with no name attached to it.
    let (a, b) = tokio::time::timeout(
        Duration::from_secs(5),
        async { tokio::join!(plane.on_event(second), plane.on_event(third)) },
    )
    .await
    .expect("the plane did not park: a serial refine would deadlock the barrier");
    let all: Vec<&Handled> = a.iter().chain(b.iter()).collect();

    let closed = all.iter().filter(|h| matches!(h, Handled::Closed { .. })).count();
    let suppressed = all.iter().filter(|h| matches!(h, Handled::Suppressed(_))).count();
    assert_eq!(closed, 1, "one trade for one opportunity: {all:?}");
    assert_eq!(suppressed, 1, "and the duplicate says so rather than vanishing: {all:?}");

    assert_eq!(plane.registry().metrics().tickets_admitted, 1);
    // A suppression is NOT a miss: the opportunity was captured, by the other
    // delivery. Filing it would put trades the system took into the dataset that
    // decides where engineering effort goes.
    assert_eq!(plane.misses().len(), 0, "a suppressed duplicate is not a missed opportunity");
}

/// ...and suppression is about concurrency, not about history.
///
/// A redelivery arriving after the first ticket closed produces a second ticket,
/// and that is correct: §17.4's words are "an **in-flight** ticket". The plane does
/// not second-guess the search about whether the same edge is worth taking twice —
/// a plane that suppressed on history would refuse the second leg of a genuinely
/// repeating opportunity, which on Base is most of them.
#[tokio::test]
async fn a_sequential_redelivery_is_a_new_opportunity() {
    let plane = landing_plane();
    plane.boot(&NoChain, BOOT).expect("boot");
    let stream = recorded_stream();

    let first = plane.on_event(&stream[1]).await;
    let again = plane.on_event(&stream[2]).await;

    assert!(matches!(first.first(), Some(Handled::Closed { .. })), "{first:?}");
    assert!(matches!(again.first(), Some(Handled::Closed { .. })), "{again:?}");
    assert_eq!(plane.registry().metrics().tickets_admitted, 2);
}

/// INV-39, reached through the plane rather than through `DispatchGate` in
/// isolation. A plane that never booted admits **no ticket at all** — §46.1's
/// wording is "before new live dispatch is re-enabled", and a ticket admitted
/// here could only ever reach the drain.
#[tokio::test]
async fn a_plane_that_never_booted_admits_nothing() {
    let plane = landing_plane();
    // No `boot`.
    let stream = recorded_stream();
    let handled = plane.on_event(stream.first().expect("an event")).await;

    assert!(
        matches!(handled.first(), Some(Handled::Declined(_))),
        "the gate must refuse before a ticket exists: {handled:?}"
    );
    assert_eq!(plane.registry().metrics().tickets_admitted, 0);
    assert_eq!(plane.registry().metrics().ticket_drop_count(), 0);
    assert_eq!(plane.misses().len(), 1, "the refusal is still a recorded miss");
}

/// INV-40 through the plane: a candidate the economics decline leaves a miss with
/// the declining stage's own reason, and no ticket.
#[tokio::test]
async fn a_declined_candidate_files_a_miss_and_admits_no_ticket() {
    use apex_runtime::plane::Decline;
    let plane = plane_with(
        Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 5)])),
        Arc::new(DecliningEconomics(Decline::NoProfitableSize)),
        Arc::new(LandsAndFinalizes),
    );
    plane.boot(&NoChain, BOOT).expect("boot");

    let stream = recorded_stream();
    let handled = plane.on_event(stream.first().expect("an event")).await;

    // The ticket was already admitted when the decline happened, so INV-01
    // applies: it closes with an explicit code rather than disappearing.
    let Some(Handled::Closed { outcome, .. }) = handled.first() else {
        panic!("expected an explicit failure, got {handled:?}");
    };
    assert!(!outcome.is_success());
    assert_eq!(plane.registry().metrics().ticket_drop_count(), 0);

    let ledger = plane.misses();
    assert_eq!(ledger.len(), 1, "the decline is in the ledger");
    assert_eq!(
        ledger.by_reason().keys().copied().collect::<Vec<_>>(),
        vec!["LOW_EV"],
        "no profitable size is a LOW_EV miss, not a catch-all"
    );
}

/// §16.2 step 9, and INV-34. A transport that accepted the bytes has accepted the
/// bytes: with nothing observed past `Acknowledged`, the ticket must close as an
/// explicit failure rather than as a success.
#[tokio::test]
async fn a_transport_acknowledgement_is_not_an_inclusion() {
    let plane = plane_with(
        Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        Arc::new(PassThroughEconomics),
        Arc::new(NeverLands),
    );
    plane.boot(&NoChain, BOOT).expect("boot");

    let stream = recorded_stream();
    let handled = plane.on_event(stream.first().expect("an event")).await;
    let Some(Handled::Closed { outcome, .. }) = handled.first() else {
        panic!("expected a closed ticket, got {handled:?}");
    };
    assert!(
        !outcome.is_success(),
        "the null dispatcher reached TransportAccepted and nothing else: {outcome:?}"
    );

    let advanced: Vec<TicketStatus> = plane
        .registry()
        .journal()
        .replay()
        .expect("replay")
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Advanced { to, .. } => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(advanced.last(), Some(&TicketStatus::Acknowledged), "{advanced:?}");
    assert_eq!(plane.commitments_in_flight(), 0);
}

/// **Write-ahead, and it is what makes boot recovery sound.**
///
/// `recover::scan` divides crash survivors on whether the journal saw `Signed`:
/// below it, no signature was produced, so the ticket can be closed without asking
/// the chain. That division is only true if the intent is journalled *before* the
/// irreversible act. Get it backwards and a crash mid-sign looks like a ticket that
/// was never signed — and the next boot closes it as abandoned while the
/// transaction lands.
///
/// No assertion about the recorded status *sequence* can show this: the sequence is
/// the same either way. So the signer and the dispatcher are asked what status they
/// saw when they were called.
#[tokio::test]
async fn the_status_is_journalled_before_the_irreversible_act() {
    let registry = Arc::new(TicketRegistry::new(
        Box::new(InMemoryJournal::new()),
        Box::new(ManualClock::at(BOOT.0)),
    ));
    let signer = Arc::new(StatusWatchingSigner::new(Arc::clone(&registry)));
    let dispatcher = Arc::new(StatusWatchingDispatcher::new(Arc::clone(&registry)));

    let plane = Plane::new(Ports {
        registry: Arc::clone(&registry),
        pool: Arc::new(pool()),
        gate: Arc::new(DispatchGate::shut()),
        dispatcher: Arc::clone(&dispatcher) as Arc<dyn apex_capture::dispatch::Dispatcher + Send + Sync>,
        chain: Arc::new(FakeChain::landing()),
        search: Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        econ: Arc::new(PassThroughEconomics),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        signer: Arc::clone(&signer) as Arc<dyn apex_runtime::plane::Signer>,
        live: Arc::new(FixedReadings(readings())),
        settlement: Arc::new(LandsAndFinalizes),
    });
    plane.boot(&NoChain, BOOT).expect("boot");

    let stream = recorded_stream();
    let handled = plane.on_event(stream.first().expect("an event")).await;
    assert!(matches!(handled.first(), Some(Handled::Closed { .. })), "{handled:?}");

    assert_eq!(
        signer.seen(),
        vec![("sign", Some(TicketStatus::Signed))],
        "the journal already said Signed when the signer was called"
    );
    assert_eq!(
        dispatcher.seen(),
        vec![("dispatch", Some(TicketStatus::Dispatching))],
        "and already said Dispatching when anything was sent"
    );
}
