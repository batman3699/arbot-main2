//! **Engine D: event templates (§12.4, BP-063).**
//!
//! # Why this engine is first rather than last
//!
//! §12.2 lists it fourth. This repository's measurements put it first:
//! event-triggered sampling produced the **first net-positive arbitrage samples
//! this project has ever recorded**, and 88% of them followed a swap. The
//! matching continuous-sampling control over quiet blocks had a median of
//! −1.64 bps against the swap-triggered +0.04. Continuous sampling does not
//! measure a worse opportunity; it measures the wrong *moment*.
//!
//! # Three of the eight classes do not mean "reprice"
//!
//! §12.4's eight classes read as one list, and BP-063 asks that each map "to the
//! template set it should revalue". Working through them, three of the eight do
//! not say a price moved — they say a **carried attribute of the template is now
//! wrong**:
//!
//! | Class | What actually happened |
//! |---|---|
//! | Tick transition | The pool crossed a tick, so `tick_neighborhood` no longer contains it |
//! | Hook mutation | A V4 hook changed, so `hook_fingerprint` is stale |
//! | Fee change | A dynamic fee moved, so `fee_variants` is stale |
//!
//! Repricing on those would price **against the stale attribute** — and that is
//! not a hypothetical failure mode here. The fast path carrying no tick ladder is
//! the measured mechanism behind a ~140 bps gap between local pricing and the
//! quoter; a template repriced across a tick transition reaches the same error by
//! a second route.
//!
//! So the response is two-valued: [`TemplateAction::Revalue`] for the five where
//! a price moved, [`TemplateAction::Invalidate`] for the three where a fact did.
//! An invalidated template is not deleted — it is marked, because deleting it
//! loses the recency signal that took a live run to earn.
//!
//! # `mempool.rs` is the input, not the design
//!
//! §12.2 says "ADAPT `mempool.rs`". Its decode machinery is real and works; its
//! *shape* is not adaptable. It carries five upward dependencies — `ingestion`,
//! `math`, `metrics`, `token_refresh`, `util` — into three different future
//! crates, and the crate-split discipline this repository learned the hard way is
//! that a module with upward dependencies is rebuilt and differentialled rather
//! than moved. What survives is the decode; what it emits is a typed
//! [`StateEvent`], which `apex-state` now owns.

use crate::frontier::{Frontier, Revalued, RouteId};
use apex_state::feed::event::{EventClass, EventKind, StateEvent};

/// What an event means for a template that touches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateAction {
    /// A price moved. The template's carried attributes still hold, so reprice
    /// it against the new state.
    Revalue,
    /// A carried attribute is now wrong. Repricing would price against the stale
    /// value, so the template must be refreshed before it can be traded on.
    Invalidate(StaleAttribute),
}

/// Which of the eight carried attributes an event invalidated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StaleAttribute {
    /// The pool crossed out of the range the template's price assumed.
    TickNeighborhood,
    /// A hook changed. §10.4: an unmodelled hook forces `Exactness::Approximate`,
    /// which INV-17 then blocks from live dispatch.
    HookFingerprint,
    /// A dynamic fee moved, so the cheapest variant may no longer be cheapest —
    /// and fee selection is the largest term in whether a route clears.
    FeeVariants,
}

impl StaleAttribute {
    pub const fn label(self) -> &'static str {
        match self {
            Self::TickNeighborhood => "tick_neighborhood",
            Self::HookFingerprint => "hook_fingerprint",
            Self::FeeVariants => "fee_variants",
        }
    }
}

/// §12.4's mapping, exhaustive.
///
/// Matched rather than looked up in a table, so a ninth class cannot be added
/// without deciding whether it moves a price or invalidates a fact — which is
/// the only question this engine asks.
pub const fn action_for(class: EventClass) -> TemplateAction {
    match class {
        EventClass::LargeSwap
        | EventClass::LiquidityChange
        | EventClass::Liquidation
        | EventClass::OracleMutation
        | EventClass::StableDislocation => TemplateAction::Revalue,
        EventClass::TickTransition => TemplateAction::Invalidate(StaleAttribute::TickNeighborhood),
        EventClass::HookMutation => TemplateAction::Invalidate(StaleAttribute::HookFingerprint),
        EventClass::FeeChange => TemplateAction::Invalidate(StaleAttribute::FeeVariants),
    }
}

/// Why an event produced no work.
///
/// **Not a candidate rejection**, and it does not implement `ExplainsMiss`: no
/// candidate was evaluated, so filing it as `LOW_EV` would be a claim nobody
/// measured. A below-threshold swap might have carried an opportunity; the engine
/// declined to look, which is a *coverage* decision and §28's auditor is what
/// measures its cost. So these are **counted** rather than filed — an
/// uncounted threshold is one nobody can tell is set wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// A block or preconfirmation: real, and it names no pools, so there is
    /// nothing for a template to be keyed on.
    NotATemplateClass,
    /// Below the notional floor.
    BelowNotionalFloor,
    /// The event named pools, and none of them is in the resident set.
    NoResidentTemplate,
}

/// What Engine D did with one event.
#[derive(Debug)]
pub struct Response {
    pub class: Option<EventClass>,
    pub revalue: Vec<RouteId>,
    pub invalidate: Vec<(RouteId, StaleAttribute)>,
    /// Set when nothing came of the event. Counted by the caller, never filed as
    /// a miss — see [`Skipped`].
    pub skipped: Option<Skipped>,
    /// §12.1's permit. Present **whatever** happened, because the frontier was
    /// consulted either way and broad discovery is exactly what an empty
    /// revaluation is for.
    pub permit: Revalued,
}

impl Response {
    pub fn is_empty(&self) -> bool {
        self.revalue.is_empty() && self.invalidate.is_empty()
    }
}

/// §12.4's engine.
#[derive(Debug, Clone, Copy)]
pub struct EventEngine {
    notional_floor_usd: f64,
}

impl EventEngine {
    /// The census that found this repository's first net-positive samples
    /// triggered on swaps **≥ $5,000**. It is the default and not a constant: the
    /// same census swept the depth floor across $100k / $50k / $25k and the
    /// middle one was best by an order of magnitude on dollars per 45 minutes, so
    /// the thresholds around this engine are measured values that will move.
    pub const MEASURED_NOTIONAL_FLOOR_USD: f64 = 5_000.0;

    pub const fn new(notional_floor_usd: f64) -> Self {
        Self { notional_floor_usd }
    }

    pub const fn measured() -> Self {
        Self::new(Self::MEASURED_NOTIONAL_FLOOR_USD)
    }

    pub const fn notional_floor_usd(&self) -> f64 {
        self.notional_floor_usd
    }

    /// Classify, revalue, and say what each touched template needs.
    pub fn respond(&self, event: &StateEvent, frontier: &Frontier) -> Response {
        let (hits, permit) = frontier.revalue(event);

        let Some(class) = event.kind.class() else {
            return Response {
                class: None,
                revalue: Vec::new(),
                invalidate: Vec::new(),
                skipped: Some(Skipped::NotATemplateClass),
                permit,
            };
        };

        if !self.clears_the_floor(&event.kind) {
            return Response {
                class: Some(class),
                revalue: Vec::new(),
                invalidate: Vec::new(),
                skipped: Some(Skipped::BelowNotionalFloor),
                permit,
            };
        }

        if hits.is_empty() {
            return Response {
                class: Some(class),
                revalue: Vec::new(),
                invalidate: Vec::new(),
                skipped: Some(Skipped::NoResidentTemplate),
                permit,
            };
        }

        let (mut revalue, mut invalidate) = (Vec::new(), Vec::new());
        match action_for(class) {
            TemplateAction::Revalue => revalue = hits,
            TemplateAction::Invalidate(attr) => {
                invalidate = hits.into_iter().map(|id| (id, attr)).collect();
            }
        }

        Response { class: Some(class), revalue, invalidate, skipped: None, permit }
    }

    /// Only a swap carries a notional, and an **unmeasured** one is admitted.
    ///
    /// "We did not measure this swap's size" and "this swap was small" are
    /// different facts, and the asymmetry decides which way to lean: admitting an
    /// unmeasured swap costs one `BTreeMap` lookup, and skipping one costs an
    /// opportunity nobody will ever know about. The lookup is cheap enough that
    /// the conservative direction is also the affordable one.
    fn clears_the_floor(&self, kind: &EventKind) -> bool {
        match kind {
            EventKind::PendingSwap { notional_usd, .. } => {
                notional_usd.is_none_or(|n| n >= self.notional_floor_usd)
            }
            _ => true,
        }
    }
}

impl Default for EventEngine {
    fn default() -> Self {
        Self::measured()
    }
}
