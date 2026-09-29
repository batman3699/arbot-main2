//! Graceful shutdown with ticket reconciliation (§46.1).
//!
//! # The rule this module exists to state
//!
//! **A drain must not classify a ticket whose fate it cannot observe.**
//!
//! A ticket the journal last saw at [`TicketStatus::Signed`] or later may
//! already be in a mempool. Closing it as
//! [`TerminalFailure::Abandoned`](apex_types::ticket::TerminalFailure::Abandoned)
//! on the way out would write a terminal *failure* for a transaction that is
//! about to land — which puts a falsehood in the one dataset §46.1 says must
//! never contain one, tells the P&L ledger a trade never happened while the
//! money moved (§2.10), and releases a nonce that is still in use.
//!
//! So an unfinished signed ticket is **handed over, not closed**. The journal
//! plus `apex_capture::recover` is the only thing that can ask the chain what
//! happened, and it asks at the next boot, before the dispatch gate reopens
//! (INV-39). A tidy shutdown that leaves nothing outstanding is not the goal; an
//! *honest* one is.
//!
//! # The line is drawn in exactly one place
//!
//! [`TicketStatus::may_be_on_chain`] is the predicate, and `recover::scan` uses
//! the same one to build its `Disposition`. That matters more than it looks: if
//! shutdown closed a ticket that recovery would have gone to the chain about,
//! boot would find nothing to reconcile and the discrepancy would be invisible
//! in both directions. `the_drain_and_recovery_draw_the_same_line` pins it.
//!
//! # Three phases, in this order, and the order is the design
//!
//! 1. **Stop admitting.** [`Shutdown::begin`]. A drain that runs while the feed
//!    is still producing tickets never finishes.
//! 2. **Stop the sources.** [`Shutdown::stop_workers`] joins the supervised
//!    background workers and returns [`WorkersStopped`] — which [`Drain::run`]
//!    *requires*, so "drain after the feeds are off" is a type, not a comment.
//! 3. **Drain the sinks.** In-flight per-ticket work is not a supervised worker;
//!    it is a future the plane is already inside. The grace period is for that,
//!    and it bounds the wait rather than the honesty of the answer.

use crate::supervise::SupervisedHandle;
use apex_capture::registry::TicketRegistry;
use apex_types::ticket::{TerminalFailure, TicketOutcome, TicketStatus};
use apex_types::time::{DurationNanos, UnixNanos};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

/// The process-wide shutdown latch. One-way: there is no `resume`, because a
/// system that can un-decide to shut down can be half-way through deciding.
#[derive(Debug)]
pub struct Shutdown {
    tx: watch::Sender<bool>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx }
    }

    /// Latch it. Idempotent.
    pub fn begin(&self) {
        // `send_replace` rather than `send`: `send` fails when there are no
        // receivers, and "nothing is listening yet" must not prevent the latch
        // from being set for whoever subscribes next.
        let was = self.tx.send_replace(true);
        if !was {
            info!("shutdown signalled: admission is closed");
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.tx.borrow()
    }

    /// §46.1's first phase, phrased as the question a caller actually asks.
    pub fn admitting(&self) -> bool {
        !self.is_shutting_down()
    }

    pub fn subscribe(&self) -> ShutdownSignal {
        ShutdownSignal { rx: self.tx.subscribe() }
    }

    /// Signal, then join every supervised worker. Returns the proof [`Drain`]
    /// needs.
    pub async fn stop_workers(&self, handles: &mut [SupervisedHandle]) -> WorkersStopped {
        self.begin();
        let mut stopped = 0usize;
        for h in handles.iter_mut() {
            let exit = h.join().await;
            info!(worker = h.worker(), ?exit, restarts = h.restarts(), "worker stopped");
            stopped += 1;
        }
        WorkersStopped { stopped, _sealed: () }
    }
}

/// A worker's view of the latch.
#[derive(Debug, Clone)]
pub struct ShutdownSignal {
    rx: watch::Receiver<bool>,
}

impl ShutdownSignal {
    pub fn triggered(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves once shutdown is signalled. Cancel-safe, which is what lets it
    /// sit in a `tokio::select!` beside a worker future.
    pub async fn wait(&mut self) {
        while !*self.rx.borrow() {
            // A dropped sender means the controller is gone, which is a
            // shutdown by any useful definition -- returning here rather than
            // waiting forever is what keeps a worker from outliving the process
            // that was supposed to stop it.
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Proof that the supervised workers have stopped. **Unforgeable outside this
/// module**, so the phase ordering in the module header is enforced by the
/// compiler rather than by a reader's care.
///
/// ```compile_fail
/// use apex_runtime::shutdown::WorkersStopped;
/// let forged = WorkersStopped { stopped: 0, _sealed: () };
/// ```
///
/// The twin, differing only in going through `stop_workers`:
///
/// ```
/// use apex_runtime::shutdown::Shutdown;
/// let rt = tokio::runtime::Builder::new_current_thread().build().expect("a runtime");
/// rt.block_on(async {
///     let shutdown = Shutdown::new();
///     let proof = shutdown.stop_workers(&mut []).await;
///     assert_eq!(proof.workers_stopped(), 0);
/// });
/// ```
#[derive(Debug)]
pub struct WorkersStopped {
    stopped: usize,
    _sealed: (),
}

impl WorkersStopped {
    pub const fn workers_stopped(&self) -> usize {
        self.stopped
    }
}

/// What a drain did. Two numbers, never summed into one: "closed" is an answer
/// and "handed to recovery" is a question, and a single total would hide which
/// is which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrainOutcome {
    /// Closed here, from local knowledge alone.
    pub closed: usize,
    /// Left live and journalled, for boot reconciliation to resolve against the
    /// chain. **Not a failure count.**
    pub handed_to_recovery: usize,
}

pub struct Drain {
    registry: Arc<TicketRegistry>,
}

impl Drain {
    pub const fn new(registry: Arc<TicketRegistry>) -> Self {
        Self { registry }
    }

    /// Phase 3. Waits up to `grace` for in-flight work to finish on its own, then
    /// classifies what is left.
    ///
    /// `proof` is the phase ordering: see the module header.
    pub async fn run(
        &self,
        proof: &WorkersStopped,
        now: UnixNanos,
        grace: DurationNanos,
    ) -> DrainOutcome {
        let _ = proof;
        self.await_quiescence(grace).await;

        let mut closed = 0usize;
        let mut handed = 0usize;

        for id in self.registry.live_ids() {
            let Some(ticket) = self.registry.ticket(id) else {
                // Closed between `live_ids` and here -- in-flight work winning
                // the race, which is the outcome this phase wants.
                continue;
            };

            if ticket.status.may_be_on_chain() {
                handed += 1;
                warn!(
                    ticket = id.0,
                    status = ?ticket.status,
                    "left for boot reconciliation: a signed ticket's fate is the chain's to report"
                );
                continue;
            }

            // Below `Signed`, so the drain *could* classify it -- and must not,
            // while somebody owns it. A `TicketGuard` outstanding means a task is
            // still working on this ticket, and closing it from here would write
            // a terminal outcome for work that is still happening. Its owner
            // closes it (as `Abandoned` at worst) if it finishes; if the process
            // exits first the journal keeps it non-terminal and boot recovery
            // resolves it. Either way it is not this drain's to answer, which is
            // the same column as a signed ticket: something else will say.
            if self.registry.is_checked_out(id) {
                handed += 1;
                info!(
                    ticket = id.0,
                    status = ?ticket.status,
                    "still owned at the end of the grace period; left to its holder or to recovery"
                );
                continue;
            }

            let outcome = TicketOutcome::ExplicitFailure {
                code: TerminalFailure::Abandoned { at_status: ticket.status },
                at: now,
                state: Box::new(ticket.state_fingerprint.clone()),
                cause: "process shut down before the payload was signed; no transaction exists"
                    .to_string(),
            };
            if self.registry.close(id, outcome).is_ok() {
                closed += 1;
            } else {
                // The close lost a race with whoever owned the ticket. It has an
                // outcome either way, so it is neither closed by us nor
                // outstanding -- counting it in either column would be wrong.
                info!(ticket = id.0, "already terminal by the time the drain reached it");
            }
        }

        info!(closed, handed_to_recovery = handed, "drain complete");
        DrainOutcome { closed, handed_to_recovery: handed }
    }

    /// Poll until nothing is live, or the grace period expires.
    ///
    /// Polling rather than a notification, and that is a deliberate trade: a
    /// `Notify` on every terminal close would put shutdown machinery on the
    /// capture path, where §2.4 counts every microsecond. A 5 ms poll on the way
    /// out costs nothing anybody measures.
    async fn await_quiescence(&self, grace: DurationNanos) {
        if grace.0 == 0 {
            return;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_nanos(grace.0);
        while tokio::time::Instant::now() < deadline {
            if self.registry.live_ids().is_empty() {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(remaining.min(Duration::from_millis(5))).await;
        }
    }
}

/// §17.5's split, asked of a status directly.
///
/// Lives here as a free function only so the doc comment has somewhere to sit;
/// the predicate itself is [`TicketStatus::may_be_on_chain`] in `apex-types`,
/// which is what `recover::scan` uses.
pub const fn may_be_on_chain(status: TicketStatus) -> bool {
    status.may_be_on_chain()
}
