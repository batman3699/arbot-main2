//! The race ledger (Task 8.5 R25): one line per handled proposal, written off
//! the capture path. A full channel drops the line and counts it; capture
//! never waits on a file.
//!
//! A line carries the trigger's position (`block`, `log_index`, its
//! transaction) and every stage's time: the analyzer finds the trigger's
//! transaction index from its receipt, the flashblock that carried it from the
//! flashblock ledger, and where a rival and we would have landed.

use super::funnel::{decline_label, outcome_label};
use super::queue::Pending;
use crate::plane::Handled;
use apex_state::feed::event::EventKind;
use apex_types::ticket::TicketOutcome;
use apex_types::time::UnixNanos;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RaceTrigger {
    pub block: u64,
    pub log_index: u64,
    /// The triggering swap's transaction; empty for an event that names none.
    pub tx: String,
}

/// What became of the proposal. The labels are the funnel's, so a line and
/// the report count the same thing the same way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RaceOutcome {
    /// Older than `queue::MAX_PROPOSAL_AGE` when picked.
    Stale,
    Declined { reason: String },
    Suppressed,
    Redelivered,
    /// The null dispatcher took it.
    Dispatched,
    /// A ticket closed without being dispatched.
    Failed { class: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RaceLine {
    pub route_hash: String,
    pub net_hint_wei: Option<String>,
    pub trigger: RaceTrigger,
    /// When the feed published the event (after its burst went quiet).
    pub observed_ns: u64,
    /// When the capture loop took it: the end of detection (G1).
    pub received_ns: u64,
    /// When the search priced the route.
    pub proposed_ns: u64,
    /// When the queue picked it.
    pub picked_ns: u64,
    /// When the plane was done with it.
    pub handled_ns: u64,
    pub outcome: RaceOutcome,
    /// The ticket, when one was admitted: its stage times are in the journal.
    pub ticket: Option<u64>,
}

/// A handled proposal's line: picked from the queue at `picked`, done with
/// at `handled_at`.
pub fn race_line(p: &Pending, picked: UnixNanos, handled_at: UnixNanos, outcome: (RaceOutcome, Option<u64>)) -> RaceLine {
    let tx = match &p.event.kind {
        EventKind::PendingSwap { target, .. } => format!("{target:#x}"),
        _ => String::new(),
    };
    RaceLine {
        route_hash: format!("{:#x}", p.proposal.route.route_hash),
        net_hint_wei: p.proposal.net_hint.map(|n| n.to_string()),
        trigger: RaceTrigger { block: p.event.at.block, log_index: p.event.at.log_index, tx },
        observed_ns: p.event.observed_at.0,
        received_ns: p.received.0,
        proposed_ns: p.proposal.found_at.0,
        picked_ns: picked.0,
        handled_ns: handled_at.0,
        outcome: outcome.0,
        ticket: outcome.1,
    }
}

/// The outcome, and the ticket id when one was admitted.
pub fn outcome_of(h: &Handled) -> (RaceOutcome, Option<u64>) {
    match h {
        Handled::Closed { ticket, outcome } => {
            let o = match &**outcome {
                TicketOutcome::Success { .. } | TicketOutcome::ShadowDispatched { .. } => RaceOutcome::Dispatched,
                other => RaceOutcome::Failed { class: outcome_label(other) },
            };
            (o, Some(ticket.0))
        }
        Handled::Declined(d) => (RaceOutcome::Declined { reason: decline_label(d) }, None),
        Handled::Suppressed(_) => (RaceOutcome::Suppressed, None),
        Handled::Redelivered { .. } => (RaceOutcome::Redelivered, None),
    }
}

/// The capture loop's end of the ledger: never blocks.
pub struct RaceWriter {
    tx: mpsc::Sender<RaceLine>,
    written: AtomicU64,
    dropped: AtomicU64,
}

impl RaceWriter {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<RaceLine>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (Self { tx, written: AtomicU64::new(0), dropped: AtomicU64::new(0) }, rx)
    }

    pub fn record(&self, line: RaceLine) {
        match self.tx.try_send(line) {
            Ok(()) => self.written.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.dropped.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Lines handed to the writer task.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Lines dropped because the writer task had fallen behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
