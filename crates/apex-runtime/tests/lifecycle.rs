//! Supervision and graceful shutdown.
//!
//! The two ends of a process's life, and both are places this system has already
//! been bitten: `spawn_supervised` exists in the legacy `main.rs` because
//! fire-and-forget `tokio::spawn` let a dead pool refresher leave the bot
//! trading on stale state, and B-6 recorded that there was no durable ticket
//! state at all, so a crash between sign and receipt left an outcome nobody
//! could classify.

mod support;

use apex_capture::journal::JournalEntry;
use apex_capture::registry::TicketRegistry;
use apex_capture::{InMemoryJournal, ManualClock};
use apex_runtime::shutdown::{Drain, DrainOutcome, Shutdown};
use apex_runtime::supervise::{spawn_supervised, WorkerExit};
use apex_types::ticket::TicketStatus;
use apex_types::time::{DurationNanos, UnixNanos};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use support::*;

/// A panicking worker is restarted, and the restart is counted. This is the
/// legacy behaviour preserved — the whole reason `panic = "unwind"` must never
/// become `abort` is that this relies on `JoinError::is_panic()`.
#[tokio::test]
async fn a_panicking_worker_is_restarted_and_counted() {
    let attempts = Arc::new(AtomicU64::new(0));
    let shutdown = Shutdown::new();
    let counter = Arc::clone(&attempts);

    let mut handle = spawn_supervised("panicker", "base".to_string(), shutdown.subscribe(), move || {
        let counter = Arc::clone(&counter);
        async move {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                panic!("deliberate");
            }
            // Third attempt: block until shutdown rather than exiting, so the
            // supervisor is not racing its own restart loop.
            std::future::pending::<()>().await;
        }
    });

    // Backoff is capped and starts small; three attempts is well inside this.
    tokio::time::timeout(Duration::from_secs(10), async {
        while attempts.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the supervisor restarted a panicking worker");

    assert_eq!(handle.restarts(), 2, "two panics, two restarts");
    assert_eq!(handle.panics(), 2, "and both were panics, not clean exits");

    shutdown.begin();
    let exit = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("the worker observed shutdown");
    assert_eq!(exit, WorkerExit::ShutDown);
}

/// A supervisor that cannot be stopped is a process that cannot be stopped. The
/// legacy version loops forever and only exits when tokio cancels it, which is
/// fine for a fire-and-forget worker and useless for an orderly drain.
#[tokio::test]
async fn a_worker_stops_on_shutdown_rather_than_being_cancelled() {
    let shutdown = Shutdown::new();
    let ran = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&ran);

    let mut handle = spawn_supervised("clean", "base".to_string(), shutdown.subscribe(), move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        while ran.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the worker started");

    shutdown.begin();
    let exit = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("it stopped");
    assert_eq!(exit, WorkerExit::ShutDown);
    assert_eq!(handle.restarts(), 0, "a clean shutdown is not a restart");
}

/// **The shutdown rule.** A drain must not classify a ticket whose fate it
/// cannot observe.
///
/// A ticket at `Signed` or later may already be in a mempool. Closing it as
/// `Abandoned` on the way out would write a terminal failure for a transaction
/// that is about to land, which is a lie in the one dataset §46.1 says must
/// never contain one — and it would also release the nonce. The journal plus
/// boot reconciliation is the only thing that can ask the chain, so an
/// unfinished signed ticket is *handed over*, not closed.
#[tokio::test]
async fn shutdown_does_not_classify_what_it_cannot_observe() {
    let clock = ManualClock::at(1_000_000_000);
    let registry = Arc::new(TicketRegistry::new(
        Box::new(InMemoryJournal::new()),
        Box::new(clock),
    ));

    // One ticket nobody owns and that never reached `Signed`, and one that a task
    // is still holding at `Signed`.
    //
    // The guard is how a real in-flight ticket is held, and it is kept alive
    // across the drain on purpose: a `Signed` ticket with no owner cannot occur,
    // because `TicketGuard::drop` closes it as `Abandoned` the moment its task
    // goes away. Dropping the guard before the drain would have been testing the
    // guard, not the drain.
    let early = registry.admit(ticket_at(TicketStatus::Observed)).expect("admit");
    let signed = registry.admit(ticket_at(TicketStatus::Observed)).expect("admit");
    let mut in_flight = registry.checkout(signed).expect("checkout");
    for to in [
        TicketStatus::Reserved,
        TicketStatus::Exacting,
        TicketStatus::Simulated,
        TicketStatus::Authorized,
        TicketStatus::Signed,
    ] {
        in_flight.advance(to).expect("advance");
    }

    // `WorkersStopped` is the phase ordering, as a type: a drain cannot run
    // until the supervised feeds are off. There are none here, which is the
    // honest shape of "nothing is still producing tickets".
    let stopped = Shutdown::new().stop_workers(&mut []).await;
    let drain = Drain::new(Arc::clone(&registry));
    let outcome = drain.run(&stopped, UnixNanos(1_000_000_001), DurationNanos(0)).await;

    assert_eq!(
        outcome,
        DrainOutcome { closed: 1, handed_to_recovery: 1 },
        "the pre-authorization ticket is closeable; the signed one is not"
    );

    // The early ticket has an explicit outcome...
    let early_outcome = registry.outcome(early).expect("the early ticket is closed");
    assert!(!early_outcome.is_success());

    // ...and the signed one is still live, with its journal intact for recovery.
    assert_eq!(registry.status(signed), Some(TicketStatus::Signed));
    assert!(registry.outcome(signed).is_none(), "shutdown must not have invented an outcome");

    let entries = registry.journal().replay().expect("replay");
    let closed_ids: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Closed { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(closed_ids, vec![early], "only the early ticket was closed");

    // Released only now: the assertions above are about the state while the task
    // still owned it.
    drop(in_flight);
}

/// The drain must not close a ticket out from under its owner **even when it could
/// classify it**.
///
/// A ticket at `Simulated` has no payload, so `Abandoned { at_status }` would be a
/// true code. It is still wrong to write: a task is inside the protocol with a
/// `TicketGuard`, and a terminal outcome written here is a terminal outcome for
/// work that is still happening. `TicketRegistry::sweep` already reasoned this way
/// about deadlines; the drain asks the registry the same question rather than
/// coming to its own conclusion.
#[tokio::test]
async fn the_drain_does_not_close_a_ticket_out_from_under_its_owner() {
    let registry = Arc::new(TicketRegistry::new(
        Box::new(InMemoryJournal::new()),
        Box::new(ManualClock::at(1_000_000_000)),
    ));
    let id = registry.admit(ticket_at(TicketStatus::Observed)).expect("admit");
    let mut owned = registry.checkout(id).expect("checkout");
    owned.advance(TicketStatus::Reserved).expect("advance");
    owned.advance(TicketStatus::Exacting).expect("advance");
    owned.advance(TicketStatus::Simulated).expect("advance");

    let stopped = Shutdown::new().stop_workers(&mut []).await;
    let drain = Drain::new(Arc::clone(&registry));
    let outcome = drain.run(&stopped, UnixNanos(1_000_000_001), DurationNanos(0)).await;

    assert_eq!(
        outcome,
        DrainOutcome { closed: 0, handed_to_recovery: 1 },
        "an owned ticket is nobody else's to close, whatever its status"
    );
    assert!(registry.outcome(id).is_none(), "the drain wrote no outcome");
    assert_eq!(registry.status(id), Some(TicketStatus::Simulated));

    // And when the owner does let go, the guard closes it -- as `Abandoned`,
    // which is the honest code for work that stopped without deciding.
    drop(owned);
    let closed = registry.outcome(id).expect("the guard closed it");
    assert!(!closed.is_success());
}

/// A `Signed` ticket that **nobody owns**, which is the case the two tests above
/// could not reach — and the one a mutation exposed.
///
/// Replacing the drain's `may_be_on_chain` check with `if false` broke nothing,
/// because every signed ticket in those tests was also checked out, so the
/// owner-check caught it. Two guards covering for each other is not two guards.
///
/// The state is real, and it is not exotic: `recover::reconcile` restores every
/// outstanding ticket into the registry *before* resolving any of them, so a
/// shutdown arriving during recovery finds restored tickets at their journalled
/// status with no guard at all. Closing one of those as abandoned would discard
/// exactly the record recovery had just recovered.
#[tokio::test]
async fn a_restored_signed_ticket_is_handed_over_even_with_no_owner() {
    let registry = Arc::new(TicketRegistry::new(
        Box::new(InMemoryJournal::new()),
        Box::new(ManualClock::at(1_000_000_000)),
    ));

    // `restore`, not `admit`: this is the shape boot recovery leaves behind, with
    // the ticket at the status the journal recorded and no owner.
    let mut ticket = ticket_at(TicketStatus::Signed);
    ticket.ticket_id = apex_types::ids::TicketId(41);
    registry.restore(ticket).expect("restore");
    assert!(!registry.is_checked_out(apex_types::ids::TicketId(41)));

    let stopped = Shutdown::new().stop_workers(&mut []).await;
    let drain = Drain::new(Arc::clone(&registry));
    let outcome = drain.run(&stopped, UnixNanos(1_000_000_001), DurationNanos(0)).await;

    assert_eq!(
        outcome,
        DrainOutcome { closed: 0, handed_to_recovery: 1 },
        "unowned or not, a signed ticket's fate is the chain's to report"
    );
    assert!(
        registry.outcome(apex_types::ids::TicketId(41)).is_none(),
        "the drain must not have invented an outcome"
    );
}

/// The complement, so the pair discriminates: an unowned ticket **below** `Signed`
/// is closeable, and the drain does close it. Without this, the test above is
/// satisfied by a drain that never closes anything.
#[tokio::test]
async fn a_restored_unsigned_ticket_is_closed() {
    let registry = Arc::new(TicketRegistry::new(
        Box::new(InMemoryJournal::new()),
        Box::new(ManualClock::at(1_000_000_000)),
    ));
    let mut ticket = ticket_at(TicketStatus::Authorized);
    ticket.ticket_id = apex_types::ids::TicketId(42);
    registry.restore(ticket).expect("restore");

    let stopped = Shutdown::new().stop_workers(&mut []).await;
    let drain = Drain::new(Arc::clone(&registry));
    let outcome = drain.run(&stopped, UnixNanos(1_000_000_001), DurationNanos(0)).await;

    assert_eq!(
        outcome,
        DrainOutcome { closed: 1, handed_to_recovery: 0 },
        "`Authorized` holds resources but has no payload, so it is answerable here"
    );
    let closed = registry.outcome(apex_types::ids::TicketId(42)).expect("closed");
    assert!(!closed.is_success());
}

/// The predicate is in one place, and this is the test that says so.
///
/// `recover::scan` divides crash survivors on `TicketStatus::may_be_on_chain`, and
/// so does the drain. If the two could disagree, a ticket shutdown closed would be
/// one boot never asked the chain about -- and the money could move with no record
/// on either side of the restart.
#[test]
fn the_drain_and_recovery_draw_the_same_line() {
    use apex_runtime::shutdown::may_be_on_chain;
    for status in [
        TicketStatus::Observed,
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
    ] {
        assert_eq!(
            may_be_on_chain(status),
            status >= TicketStatus::Signed,
            "{status:?}: the line is `Signed`, and it is drawn once"
        );
    }

    // And it is a LATER line than preemption's. An `Authorized` ticket holds
    // reserved resources -- so INV-09 will not preempt it -- and has no payload,
    // so it is still safely closeable. Two questions, two answers.
    assert!(TicketStatus::Authorized.is_authorized_or_later());
    assert!(!may_be_on_chain(TicketStatus::Authorized));
}

/// Draining while still admitting never finishes. Admission stops first, and
/// `Shutdown` is what the plane asks.
#[tokio::test]
async fn shutdown_stops_admission_before_draining() {
    let shutdown = Shutdown::new();
    assert!(shutdown.admitting(), "a fresh process admits");
    shutdown.begin();
    assert!(!shutdown.admitting(), "a draining process does not");
    assert!(shutdown.is_shutting_down());
}

/// A ticket still in flight past the grace period is handed over rather than
/// waited on forever. The grace period is a bound on the wait, not on the
/// honesty of the answer.
#[tokio::test]
async fn the_grace_period_bounds_the_wait_not_the_classification() {
    let clock = ManualClock::at(1_000_000_000);
    let registry = Arc::new(TicketRegistry::new(
        Box::new(InMemoryJournal::new()),
        Box::new(clock),
    ));
    let signed = registry.admit(ticket_at(TicketStatus::Observed)).expect("admit");
    let mut in_flight = registry.checkout(signed).expect("checkout");
    for to in [
        TicketStatus::Reserved,
        TicketStatus::Exacting,
        TicketStatus::Simulated,
        TicketStatus::Authorized,
        TicketStatus::Signed,
    ] {
        in_flight.advance(to).expect("advance");
    }

    let stopped = Shutdown::new().stop_workers(&mut []).await;
    let drain = Drain::new(Arc::clone(&registry));
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        drain.run(&stopped, UnixNanos(1_000_000_001), DurationNanos(50_000_000)),
    )
    .await
    .expect("the drain is bounded by its grace period");

    assert_eq!(outcome.handed_to_recovery, 1);
    assert!(registry.outcome(signed).is_none());
    drop(in_flight);
}
