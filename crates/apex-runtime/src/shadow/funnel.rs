//! What the shadow run did with every event, counted.
//!
//! The plane answers each event with a `Handled` per proposal; this tallies
//! them under labels fine enough to answer the questions the run is for. A
//! decline is keyed by its miss reason **and** what refused it — a Tier 2
//! failure by its revert class, since R7 left open how many are hop 1's
//! `MinOutNotMet`; a risk refusal by its rule; a last-mile refusal by its check.
//! A closed ticket is keyed by its outcome.

use crate::plane::{Decline, Handled};
use apex_types::miss::ExplainsMiss;
use apex_types::ticket::TicketOutcome;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// A variant's name, from its `Debug`: the text before the first `{`, `(` or
/// space.
fn variant(debug: &str) -> &str {
    debug.split(['{', '(', ' ']).next().unwrap_or(debug)
}

/// `REASON/what` for a decline.
pub fn decline_label(d: &Decline) -> String {
    let what = match d {
        Decline::SimulationFailed { class: Some(c) } => format!("{c:?}"),
        Decline::SimulationFailed { class: None } => "no_result".into(),
        Decline::RiskRefused { rule } => rule.clone(),
        Decline::Revalidation(check) => check.name().into(),
        Decline::ChainRejected(r) => variant(&format!("{r:?}")).into(),
        other => variant(&format!("{other:?}")).into(),
    };
    format!("{}/{what}", d.miss_reason().label())
}

/// A closed ticket's outcome, as a label.
pub fn outcome_label(o: &TicketOutcome) -> String {
    match o {
        TicketOutcome::Success { .. } => "success".into(),
        TicketOutcome::ShadowDispatched { in_time: true, .. } => "shadow_dispatched/in_time".into(),
        TicketOutcome::ShadowDispatched { in_time: false, .. } => "shadow_dispatched/late".into(),
        TicketOutcome::ExplicitFailure { code, .. } => format!("failed/{}", variant(&format!("{code:?}"))),
    }
}

/// The counts, as reported.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Events the capture worker received.
    pub events: u64,
    /// Not handed to the plane: the book was not `Verified`, so nothing priced
    /// from it could be (INV-08).
    pub skipped_unverified: u64,
    pub redelivered: u64,
    pub suppressed: u64,
    pub declined: BTreeMap<String, u64>,
    pub closed: BTreeMap<String, u64>,
}

#[derive(Debug, Default)]
pub struct Funnel {
    counts: Mutex<Counts>,
}

impl Funnel {
    fn with<R>(&self, f: impl FnOnce(&mut Counts) -> R) -> R {
        let mut g = self.counts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut g)
    }

    pub fn event(&self) {
        self.with(|c| c.events += 1);
    }

    pub fn skipped_unverified(&self) {
        self.with(|c| c.skipped_unverified += 1);
    }

    pub fn record(&self, handled: &[Handled]) {
        self.with(|c| {
            for h in handled {
                match h {
                    Handled::Closed { outcome, .. } => *c.closed.entry(outcome_label(outcome)).or_default() += 1,
                    Handled::Declined(d) => *c.declined.entry(decline_label(d)).or_default() += 1,
                    Handled::Suppressed(_) => c.suppressed += 1,
                    Handled::Redelivered { .. } => c.redelivered += 1,
                }
            }
        });
    }

    pub fn counts(&self) -> Counts {
        self.with(|c| c.clone())
    }
}
