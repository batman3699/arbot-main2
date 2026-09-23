//! The ticket registry: INV-01, INV-02, INV-03.
//!
//! INV-01 is the whole reason this type exists -- *every admitted live ticket
//! reaches exactly one terminal outcome*. That is a statement about the paths a
//! ticket can leave by, so the design is about closing paths, not about adding
//! checks:
//!
//! - There is **one** removal path, [`TicketRegistry::close`], and it records an
//!   outcome before it removes. So a ticket cannot leave without one.
//! - Work happens under a [`TicketGuard`], whose `Drop` closes a still-live
//!   ticket as [`TerminalFailure::Abandoned`]. So a cancelled task, a `?` that
//!   returns early, or a panic upstream cannot silently orphan a ticket.
//! - A ticket nobody ever checks out is closed by [`TicketRegistry::sweep`] at
//!   its dispatch deadline (INV-03). So the "forgotten entirely" case is
//!   covered too, which is the one the RAII guard structurally cannot see.
//!
//! Those three together are what `ticket_always_terminates` actually tests.
//!
//! ## `Drop` records; it does not panic
//!
//! §8's INV-01 row says the guard "panics in debug and records
//! `ExplicitFailure{cause:"dropped"}` in release". It records in both, and the
//! deviation is deliberate. A panic in `Drop` during unwinding from another
//! panic aborts the process -- so the debug behaviour would convert a single
//! recoverable fault into a hard abort precisely when a ticket is most likely
//! to be dropped. It would also make this module's own property test
//! unrunnable, since that test drops guards on purpose. Loudness is kept where
//! it belongs: `tickets_closed_by_guard_drop` is a distinct counter, and a
//! non-zero value in a shadow run is a defect to chase.

use crate::clock::{Clock, SystemClock};
use crate::journal::{InMemoryJournal, Journal, JournalEntry};
use apex_types::ids::TicketId;
use apex_types::ticket::{
    MonotonicityError, OpportunityTicket, TerminalFailure, TicketOutcome, TicketStatus,
};
use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    UnknownTicket(TicketId),
    AlreadyClosed(TicketId),
    AlreadyCheckedOut(TicketId),
    NotMonotonic(MonotonicityError),
    Journal(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTicket(id) => write!(f, "no ticket {}", id.0),
            Self::AlreadyClosed(id) => write!(f, "ticket {} is already terminal", id.0),
            Self::AlreadyCheckedOut(id) => write!(f, "ticket {} is checked out elsewhere", id.0),
            Self::NotMonotonic(e) => write!(f, "{e}"),
            Self::Journal(e) => write!(f, "journal write failed: {e}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// INV-01 and INV-02's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegistryMetrics {
    pub tickets_admitted: u64,
    pub tickets_terminal_success: u64,
    pub tickets_terminal_failure: u64,
    pub tickets_live: u64,
    /// Closed by a guard going out of scope. Not a loss -- the outcome is
    /// recorded -- but every one of these is a code path that let go of a
    /// ticket without deciding, so a shadow run wants this at zero.
    pub tickets_closed_by_guard_drop: u64,
    /// Closed by [`TicketRegistry::sweep`] at the dispatch deadline (INV-03).
    pub tickets_closed_by_deadline: u64,
}

impl RegistryMetrics {
    /// **INV-02.** Tickets that left the registry with no terminal outcome.
    ///
    /// Derived rather than incremented, on purpose: a counter that is only
    /// bumped where the author remembered to bump it proves the author's
    /// attention, not the invariant. This subtracts what is accounted for from
    /// what was admitted, so any removal path that forgets to record shows up
    /// here whether or not it knew about this field.
    pub const fn ticket_drop_count(&self) -> i64 {
        self.tickets_admitted as i64
            - self.tickets_terminal_success as i64
            - self.tickets_terminal_failure as i64
            - self.tickets_live as i64
    }
}

struct Inner {
    live: BTreeMap<TicketId, OpportunityTicket>,
    checked_out: BTreeMap<TicketId, ()>,
    outcomes: BTreeMap<TicketId, TicketOutcome>,
    next_id: u64,
    metrics: RegistryMetrics,
}

pub struct TicketRegistry {
    inner: Mutex<Inner>,
    journal: Box<dyn Journal>,
    clock: Box<dyn Clock>,
}

impl TicketRegistry {
    pub fn new(journal: Box<dyn Journal>, clock: Box<dyn Clock>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                live: BTreeMap::new(),
                checked_out: BTreeMap::new(),
                outcomes: BTreeMap::new(),
                next_id: 1,
                metrics: RegistryMetrics::default(),
            }),
            journal,
            clock,
        }
    }

    pub fn in_memory() -> Self {
        Self::new(Box::new(InMemoryJournal::new()), Box::new(SystemClock))
    }

    pub fn journal(&self) -> &dyn Journal {
        self.journal.as_ref()
    }

    pub fn now(&self) -> UnixNanos {
        self.clock.now()
    }

    /// Admit a ticket. The id in `ticket` is ignored -- the registry assigns
    /// one, because two callers that both invent ids is how a ticket ends up
    /// overwriting another one's outcome.
    pub fn admit(&self, mut ticket: OpportunityTicket) -> Result<TicketId, RegistryError> {
        let at = self.clock.now();
        let mut inner = self.lock();
        let id = TicketId(inner.next_id);
        inner.next_id += 1;
        ticket.ticket_id = id;

        self.journal
            .append(&JournalEntry::Admitted { id, at, ticket: Box::new(ticket.clone()) })
            .map_err(|e| RegistryError::Journal(e.to_string()))?;

        inner.live.insert(id, ticket);
        inner.metrics.tickets_admitted += 1;
        inner.metrics.tickets_live += 1;
        Ok(id)
    }

    /// Take exclusive working possession of a live ticket.
    ///
    /// Exclusive because two holders would each believe they were responsible
    /// for closing it, and the second `Drop` would find it already terminal --
    /// which is indistinguishable from the bug where one holder closed it out
    /// from under the other.
    pub fn checkout(&self, id: TicketId) -> Result<TicketGuard<'_>, RegistryError> {
        let mut inner = self.lock();
        if inner.outcomes.contains_key(&id) {
            return Err(RegistryError::AlreadyClosed(id));
        }
        if !inner.live.contains_key(&id) {
            return Err(RegistryError::UnknownTicket(id));
        }
        if inner.checked_out.insert(id, ()).is_some() {
            return Err(RegistryError::AlreadyCheckedOut(id));
        }
        Ok(TicketGuard { registry: self, id, closed: false })
    }

    pub fn outcome(&self, id: TicketId) -> Option<TicketOutcome> {
        self.lock().outcomes.get(&id).cloned()
    }

    pub fn status(&self, id: TicketId) -> Option<TicketStatus> {
        self.lock().live.get(&id).map(|t| t.status)
    }

    pub fn ticket(&self, id: TicketId) -> Option<OpportunityTicket> {
        self.lock().live.get(&id).cloned()
    }

    pub fn metrics(&self) -> RegistryMetrics {
        self.lock().metrics
    }

    pub fn live_ids(&self) -> Vec<TicketId> {
        self.lock().live.keys().copied().collect()
    }

    /// The only removal path.
    pub fn close(&self, id: TicketId, outcome: TicketOutcome) -> Result<(), RegistryError> {
        let at = self.clock.now();
        let mut inner = self.lock();
        self.close_locked(&mut inner, id, outcome, at, Closer::Explicit)
    }

    /// **INV-03.** Close every live ticket whose dispatch deadline has passed.
    ///
    /// §17.3 wants this to run *before* the deadline so expiry is always
    /// explained rather than observed after the fact; the registry provides the
    /// mechanism and the scheduler (Task 6.5) provides the cadence. Returns how
    /// many it closed.
    pub fn sweep(&self, now: UnixNanos) -> usize {
        let mut inner = self.lock();
        let expired: Vec<(TicketId, TicketStatus, UnixNanos)> = inner
            .live
            .values()
            .filter(|t| t.is_past_deadline(now))
            // A checked-out ticket has an owner who is responsible for it. Its
            // guard will close it -- as `Abandoned` at worst. Closing it from
            // under that owner is the double-close this type exists to prevent.
            .filter(|t| !inner.checked_out.contains_key(&t.ticket_id))
            .map(|t| (t.ticket_id, t.status, t.dispatch_deadline))
            .collect();

        let mut closed = 0;
        for (id, _status, deadline) in expired {
            let Some(fingerprint) = inner.live.get(&id).map(|t| t.state_fingerprint.clone())
            else {
                continue;
            };
            let outcome = TicketOutcome::ExplicitFailure {
                code: TerminalFailure::DispatchTimeout {
                    deadline,
                    elapsed: DurationNanos(now.0.saturating_sub(deadline.0)),
                },
                at: now,
                state: Box::new(fingerprint),
                cause: "dispatch deadline passed with the ticket still queued".to_string(),
            };
            if self.close_locked(&mut inner, id, outcome, now, Closer::Deadline).is_ok() {
                closed += 1;
            }
        }
        closed
    }

    fn close_locked(
        &self,
        inner: &mut Inner,
        id: TicketId,
        outcome: TicketOutcome,
        at: UnixNanos,
        by: Closer,
    ) -> Result<(), RegistryError> {
        if inner.outcomes.contains_key(&id) {
            return Err(RegistryError::AlreadyClosed(id));
        }
        if !inner.live.contains_key(&id) {
            return Err(RegistryError::UnknownTicket(id));
        }

        // Journal BEFORE removing. If the append fails the ticket stays live and
        // the caller gets an error -- which leaves a ticket to reconcile, not a
        // ticket that vanished. The other order loses it on exactly the failure
        // the journal exists to survive.
        self.journal
            .append(&JournalEntry::Closed { id, at, outcome: Box::new(outcome.clone()) })
            .map_err(|e| RegistryError::Journal(e.to_string()))?;

        inner.live.remove(&id);
        inner.metrics.tickets_live = inner.metrics.tickets_live.saturating_sub(1);
        if outcome.is_success() {
            inner.metrics.tickets_terminal_success += 1;
        } else {
            inner.metrics.tickets_terminal_failure += 1;
        }
        match by {
            Closer::Explicit => {}
            Closer::GuardDrop => inner.metrics.tickets_closed_by_guard_drop += 1,
            Closer::Deadline => inner.metrics.tickets_closed_by_deadline += 1,
        }
        inner.outcomes.insert(id, outcome);
        Ok(())
    }

    fn advance_locked(&self, id: TicketId, to: TicketStatus) -> Result<(), RegistryError> {
        let at = self.clock.now();
        let mut inner = self.lock();
        let ticket = inner.live.get_mut(&id).ok_or(RegistryError::UnknownTicket(id))?;
        ticket.advance(to).map_err(RegistryError::NotMonotonic)?;
        // Journalled after the in-memory move succeeds, so the journal never
        // claims a transition the state machine refused.
        self.journal
            .append(&JournalEntry::Advanced { id, at, to })
            .map_err(|e| RegistryError::Journal(e.to_string()))
    }

    fn release(&self, id: TicketId) {
        self.lock().checked_out.remove(&id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned registry means a panic happened while its accounting was
        // being updated. Recovering rather than propagating is deliberate, and
        // it is safe *because* of how the invariant is measured: an accounting
        // update interrupted half-way shows up in `ticket_drop_count()`, which
        // is derived by subtraction rather than incremented. So the damage
        // announces itself through the metric that exists to detect it, instead
        // of being converted into an abort that would strand every other live
        // ticket in memory.
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[derive(Clone, Copy)]
enum Closer {
    Explicit,
    GuardDrop,
    Deadline,
}

/// Working possession of one live ticket. Closing it is not optional: if this
/// goes out of scope still open, the ticket is closed as
/// [`TerminalFailure::Abandoned`].
pub struct TicketGuard<'r> {
    registry: &'r TicketRegistry,
    id: TicketId,
    closed: bool,
}

impl std::fmt::Debug for TicketGuard<'_> {
    // Hand-written because the registry behind the reference is not `Debug` and
    // should not be: printing it would print every live ticket.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketGuard")
            .field("id", &self.id.0)
            .field("closed", &self.closed)
            .finish()
    }
}

impl TicketGuard<'_> {
    pub const fn id(&self) -> TicketId {
        self.id
    }

    pub fn status(&self) -> Option<TicketStatus> {
        self.registry.status(self.id)
    }

    pub fn advance(&mut self, to: TicketStatus) -> Result<(), RegistryError> {
        self.registry.advance_locked(self.id, to)
    }

    /// Close with a decision. Consumes the guard, so the abandonment path and
    /// the deciding path cannot both run.
    pub fn close(mut self, outcome: TicketOutcome) -> Result<(), RegistryError> {
        let at = self.registry.clock.now();
        let mut inner = self.registry.lock();
        let r = self.registry.close_locked(&mut inner, self.id, outcome, at, Closer::Explicit);
        // Marked closed even on failure: a failed close leaves the ticket live
        // and journalled, which reconciliation handles. Re-entering the
        // abandonment path here would write a second outcome for a ticket whose
        // first one may yet be written.
        self.closed = true;
        drop(inner);
        r
    }
}

impl Drop for TicketGuard<'_> {
    fn drop(&mut self) {
        self.registry.release(self.id);
        if self.closed {
            return;
        }
        let at = self.registry.clock.now();
        let mut inner = self.registry.lock();
        let Some(ticket) = inner.live.get(&self.id) else { return };
        let (status, fingerprint) = (ticket.status, ticket.state_fingerprint.clone());
        let outcome = TicketOutcome::ExplicitFailure {
            code: TerminalFailure::Abandoned { at_status: status },
            at,
            state: Box::new(fingerprint),
            cause: "ticket guard dropped without a terminal decision".to_string(),
        };
        // Deliberately ignored: this is the last line of defence, and there is
        // nobody to return an error to. A journal failure here leaves the
        // ticket live, which is what reconciliation is for. See the module
        // header for why this does not panic.
        let _ = self.registry.close_locked(&mut inner, self.id, outcome, at, Closer::GuardDrop);
    }
}
