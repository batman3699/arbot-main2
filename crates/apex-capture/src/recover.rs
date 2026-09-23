//! Boot-time recovery (§16.8, §57.1.4, INV-39).
//!
//! ```text
//! replay ticket journal
//!   -> for each non-terminal ticket: query chain outcome (receipt / nonce / balance)
//!   -> close with a terminal code
//!   -> only when zero unreconciled tickets remain: enable live dispatch
//! ```
//!
//! B-6 recorded that the legacy system has no durable ticket state at all, so a
//! crash between sign and receipt leaves an outcome nobody can classify. This is
//! the other half of the fix: the journal makes the record exist, and this makes
//! the record *binding* on restart.
//!
//! # The journal is authoritative, and that is a claim about write order
//!
//! §17.1: "a transition that is not recorded did not happen." That is what lets
//! recovery divide the survivors in two -- a ticket the journal last saw below
//! [`TicketStatus::Signed`] cannot have a transaction on chain, because no
//! signature was produced for it. The division is only sound if the protocol
//! **journals the intent before the irreversible act**: `advance(Signed)` must
//! return `Ok` before the signer is called, and `advance(Dispatching)` before
//! anything is sent. Write-ahead, in the ordinary database sense. Task 6.3's
//! signer and Task 6.6's dispatcher are bound by that ordering, and it is the
//! reason [`TicketStatus::requires_durable_write`] starts at `Authorized`
//! rather than at `Signed`.
//!
//! # Why the two halves are kept apart
//!
//! A ticket that was never signed can be closed from local knowledge alone. A
//! ticket that may be on chain cannot -- closing it as abandoned when it in fact
//! landed would tell the P&L ledger a trade never happened while the money moved
//! (§2.10), and closing it as executed when it did not would invent a fill. So
//! the chain source is consulted for those and only those, and a chain source
//! that is *down* still lets recovery make partial progress rather than none.

use crate::journal::{Journal, JournalEntry};
use crate::registry::{RegistryError, TicketRegistry};
use apex_types::ids::TicketId;
use apex_types::ticket::{OpportunityTicket, TerminalFailure, TicketOutcome, TicketStatus};
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

/// What the journal says about one ticket that had no `Closed` entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Disposition {
    /// Last recorded below [`TicketStatus::Signed`]: no signature was produced,
    /// so no transaction bearing this ticket's nonce can exist. Closeable
    /// without touching the chain.
    NeverSigned(Box<OpportunityTicket>),
    /// Last recorded at [`TicketStatus::Signed`] or later. The chain may or may
    /// not carry it, and §46.1 does not permit a guess.
    MaybeOnChain(Box<OpportunityTicket>),
}

impl Disposition {
    pub fn ticket(&self) -> &OpportunityTicket {
        match self {
            Self::NeverSigned(t) | Self::MaybeOnChain(t) => t,
        }
    }
    pub fn id(&self) -> TicketId {
        self.ticket().ticket_id
    }
    pub const fn needs_the_chain(&self) -> bool {
        matches!(self, Self::MaybeOnChain(_))
    }
}

/// What a replay found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JournalScan {
    pub entries_read: usize,
    pub tickets_seen: usize,
    pub tickets_closed: usize,
    /// Non-terminal at the moment of the crash, in journal order.
    pub in_flight: Vec<Disposition>,
    /// The highest id the journal ever mentioned. A restarted registry must not
    /// reissue one of these -- two tickets with one id is how an outcome gets
    /// overwritten by an unrelated ticket's.
    pub max_ticket_id: u64,
}

impl JournalScan {
    pub fn unreconciled(&self) -> usize {
        self.in_flight.len()
    }
}

#[derive(Debug)]
pub enum RecoveryError {
    Journal(std::io::Error),
    /// The journal mentioned a ticket it never admitted. Only reachable if the
    /// file was edited or concatenated from two runs; recovery refuses rather
    /// than reconstructing a ticket it never saw.
    OrphanEntry(TicketId),
    /// One id admitted twice. The journal then has two tickets under one name
    /// and no way to say which a given `Closed` entry settled, so there is no
    /// safe reading of it -- and the unsafe reading is the dangerous one: the
    /// second ticket inherits the first's closure and recovery walks past a
    /// transaction that may be on chain. A process booting on an existing
    /// journal must reserve ids through [`JournalScan::max_ticket_id`] before
    /// admitting anything, which is what [`reconcile`] does.
    DuplicateAdmission(TicketId),
    Registry(RegistryError),
    /// The chain source could not answer for this ticket. Recovery is NOT
    /// complete, and the dispatch gate stays shut.
    ChainUnavailable { id: TicketId, detail: String },
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Journal(e) => write!(f, "journal replay failed: {e}"),
            Self::OrphanEntry(id) => write!(f, "journal mentions ticket {} without admitting it", id.0),
            Self::DuplicateAdmission(id) => write!(
                f,
                "journal admits ticket {} twice; one id naming two tickets cannot be reconciled",
                id.0
            ),
            Self::Registry(e) => write!(f, "{e}"),
            Self::ChainUnavailable { id, detail } => {
                write!(f, "chain outcome for ticket {} is unknown: {detail}", id.0)
            }
        }
    }
}

impl std::error::Error for RecoveryError {}

/// Replay a journal and say what was in flight when it stopped.
pub fn scan(journal: &dyn Journal) -> Result<JournalScan, RecoveryError> {
    let entries = journal.replay().map_err(RecoveryError::Journal)?;
    let mut tickets: BTreeMap<TicketId, OpportunityTicket> = BTreeMap::new();
    let mut closed: BTreeMap<TicketId, ()> = BTreeMap::new();
    let mut order: Vec<TicketId> = Vec::new();
    let mut max_id = 0u64;

    for e in &entries {
        max_id = max_id.max(e.ticket_id().0);
        match e {
            JournalEntry::Admitted { id, ticket, .. } => {
                if tickets.insert(*id, (**ticket).clone()).is_some() {
                    return Err(RecoveryError::DuplicateAdmission(*id));
                }
                order.push(*id);
            }
            JournalEntry::Advanced { id, to, .. } => {
                let t = tickets.get_mut(id).ok_or(RecoveryError::OrphanEntry(*id))?;
                // Assigned, not `advance`d: replay must reproduce the recorded
                // history rather than re-adjudicate it. A journal that recorded
                // a non-monotonic move is a corrupt journal, and the place to
                // refuse that is the write path, which already does.
                t.status = *to;
            }
            JournalEntry::Closed { id, .. } => {
                if !tickets.contains_key(id) {
                    return Err(RecoveryError::OrphanEntry(*id));
                }
                closed.insert(*id, ());
            }
        }
    }

    let in_flight = order
        .iter()
        .filter(|id| !closed.contains_key(id))
        .filter_map(|id| tickets.get(id).cloned())
        .map(|t| {
            if t.status >= TicketStatus::Signed {
                Disposition::MaybeOnChain(Box::new(t))
            } else {
                Disposition::NeverSigned(Box::new(t))
            }
        })
        .collect();

    Ok(JournalScan {
        entries_read: entries.len(),
        tickets_seen: tickets.len(),
        tickets_closed: closed.len(),
        in_flight,
        max_ticket_id: max_id,
    })
}

/// Where a possibly-landed ticket's real outcome comes from: a receipt lookup, a
/// nonce comparison, a balance diff (§16.8). Fallible on purpose -- an RPC that
/// cannot answer must not be allowed to look like "it did not land".
pub trait ChainOutcomeSource {
    fn resolve(&self, ticket: &OpportunityTicket) -> Result<TicketOutcome, String>;
}

/// Proof that reconciliation finished. **Unforgeable outside this crate**, which
/// is what makes INV-39 structural rather than a convention: a caller who merely
/// believes recovery went well has nothing to pass to [`DispatchGate::open`].
///
/// `compile_fail` doctests rather than `trybuild`, per the convention in
/// `apex_types::candidate`: `trybuild` pins the expected stderr to a rustc
/// version, while `compile_fail` asserts only the actual claim. Each is paired
/// with a twin differing *only* in the forbidden step, so a snippet broken for
/// an unrelated reason takes its twin down too.
///
/// The proof cannot be built literally -- the fields are private:
///
/// ```compile_fail
/// use apex_capture::recover::ReconciliationComplete;
/// let forged = ReconciliationComplete { tickets_reconciled: 0, _sealed: () };
/// apex_capture::recover::DispatchGate::shut().open(forged);
/// ```
///
/// The twin, differing only in going through `reconcile`:
///
/// ```
/// use apex_capture::recover::{reconcile, scan, DispatchGate};
/// use apex_capture::registry::TicketRegistry;
/// let reg = TicketRegistry::in_memory();
/// let found = scan(reg.journal()).unwrap();
/// struct NoChain;
/// impl apex_capture::recover::ChainOutcomeSource for NoChain {
///     fn resolve(&self, _t: &apex_types::ticket::OpportunityTicket)
///         -> Result<apex_types::ticket::TicketOutcome, String> { Err("x".into()) }
/// }
/// let proof = reconcile(&reg, &found, &NoChain, apex_types::time::UnixNanos(0)).unwrap();
/// let gate = DispatchGate::shut();
/// assert!(gate.permit().is_none());
/// gate.open(proof);
/// assert!(gate.permit().is_some());
/// ```
///
/// Nor can it be defaulted into existence:
///
/// ```compile_fail
/// use apex_capture::recover::ReconciliationComplete;
/// let forged = ReconciliationComplete::default();
/// ```
#[derive(Debug)]
pub struct ReconciliationComplete {
    tickets_reconciled: usize,
    _sealed: (),
}

impl ReconciliationComplete {
    pub const fn tickets_reconciled(&self) -> usize {
        self.tickets_reconciled
    }
}

/// §16.8, in order. Restores every in-flight ticket into `registry`, closes it,
/// and returns the proof only when **zero** remain unreconciled.
///
/// Never-signed tickets are closed first, so a chain source that is down still
/// leaves the system with less to reconcile than it had, rather than with the
/// same amount and a longer outage.
pub fn reconcile(
    registry: &TicketRegistry,
    scan: &JournalScan,
    chain: &dyn ChainOutcomeSource,
    at: UnixNanos,
) -> Result<ReconciliationComplete, RecoveryError> {
    registry.reserve_ids_through(scan.max_ticket_id);

    // Everything is restored FIRST, before anything is resolved. So if the
    // chain source dies half way, the registry still holds every ticket that is
    // still outstanding -- one place to ask what is owed, and `live_ids()` is
    // the retry's worklist. Resolving as we go would instead leave the
    // unresolved ones known only to a `JournalScan` the caller might drop.
    for d in &scan.in_flight {
        registry.restore(d.ticket().clone()).map_err(RecoveryError::Registry)?;
    }

    let mut done = 0usize;
    for d in scan.in_flight.iter().filter(|d| !d.needs_the_chain()) {
        let t = d.ticket();
        let outcome = TicketOutcome::ExplicitFailure {
            code: TerminalFailure::Abandoned { at_status: t.status },
            at,
            state: Box::new(t.state_fingerprint.clone()),
            cause: "process died before the payload was signed; no transaction exists".to_string(),
        };
        registry.close(t.ticket_id, outcome).map_err(RecoveryError::Registry)?;
        done += 1;
    }

    for d in scan.in_flight.iter().filter(|d| d.needs_the_chain()) {
        let t = d.ticket();
        let outcome = chain
            .resolve(t)
            .map_err(|detail| RecoveryError::ChainUnavailable { id: t.ticket_id, detail })?;
        registry.close(t.ticket_id, outcome).map_err(RecoveryError::Registry)?;
        done += 1;
    }

    Ok(ReconciliationComplete { tickets_reconciled: done, _sealed: () })
}

/// INV-39's gate. Dispatch is impossible, not merely discouraged, until
/// reconciliation produces its proof.
#[derive(Debug)]
pub struct DispatchGate {
    open: std::sync::atomic::AtomicBool,
}

impl Default for DispatchGate {
    fn default() -> Self {
        Self::shut()
    }
}

impl DispatchGate {
    /// The only constructor, and it starts shut. A gate that defaulted to open
    /// would make "we forgot to run recovery" indistinguishable from "recovery
    /// ran and passed".
    pub const fn shut() -> Self {
        Self { open: std::sync::atomic::AtomicBool::new(false) }
    }

    pub fn is_open(&self) -> bool {
        self.open.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn open(&self, proof: ReconciliationComplete) -> usize {
        self.open.store(true, std::sync::atomic::Ordering::SeqCst);
        proof.tickets_reconciled
    }

    /// The token every dispatch path must hold. `None` while shut.
    pub fn permit(&self) -> Option<DispatchPermit<'_>> {
        self.is_open().then_some(DispatchPermit { _gate: self })
    }
}

/// Capability to dispatch. Borrows the gate, so it cannot outlive it.
#[derive(Debug)]
pub struct DispatchPermit<'g> {
    _gate: &'g DispatchGate,
}
