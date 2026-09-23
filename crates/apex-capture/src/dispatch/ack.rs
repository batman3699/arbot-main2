//! The acknowledgement ladder (§24.3, §24.8). **INV-34.**
//!
//! > An RPC success response proves only that an endpoint **accepted the
//! > request** — not network receipt, ordering, preconfirmation or inclusion.
//!
//! C-09 records what the legacy code does instead: `dispatch_call` treats a
//! successful `send_raw_transaction` as the submission outcome, and
//! `classify_receipt` then jumps straight to the mined receipt. The seven
//! stages between those two points are exactly where a transaction is lost, and
//! collapsing them means every one of those losses is reported as the same
//! thing.
//!
//! So [`LifecycleStage::implies_inclusion`] is `true` for two variants out of
//! seven, and nothing else in this module asks the question a second way.
//!
//! # A note on §21.5
//!
//! §21.5's prose lists six stages and omits `builder_acknowledged`. §24.3 and
//! INV-34 both say seven. Seven is taken as authoritative here: a builder that
//! has not acknowledged is a distinguishable state on a chain with a builder
//! market, and dropping it would put two different failures in one bucket.

use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::BTreeMap;

/// §24.3, in order. The derived `Ord` is the ladder, so variant order is
/// load-bearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LifecycleStage {
    TransportAccepted,
    NodeKnown,
    SequencerReceived,
    BuilderAcknowledged,
    Preconfirmed,
    Included,
    Finalized,
}

/// What to do when a stage times out. Judgement calls, not blueprint text --
/// §24.3 says "each stage has its own timeout and escalation rule" without
/// enumerating them, so these are stated here where they can be argued with
/// rather than buried in a scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escalation {
    /// The transport never took it. Try a different submission lane.
    RetryAnotherLane,
    /// It was accepted but has not propagated. **Resend the same signed
    /// bytes** -- INV-10: transport redundancy is identical bytes, and an
    /// economically distinct duplicate needs an explicit policy flag.
    ResendSameBytes,
    /// §16.2 step 9: if state changes before inclusion, reprice or explicitly
    /// abandon. Never blind escalation (§27.4).
    RepriceOrAbandon,
    /// Included but not finalized past the timeout. Nothing to resend; the
    /// question is what the chain says, which is recovery's job.
    ReconcileOnChain,
}

impl LifecycleStage {
    pub const ALL: [Self; 7] = [
        Self::TransportAccepted,
        Self::NodeKnown,
        Self::SequencerReceived,
        Self::BuilderAcknowledged,
        Self::Preconfirmed,
        Self::Included,
        Self::Finalized,
    ];

    /// **The whole point.** Two of seven.
    pub const fn implies_inclusion(self) -> bool {
        matches!(self, Self::Included | Self::Finalized)
    }

    /// Time allowed from dispatch to reaching this stage. Base's 2-second
    /// blocks and 200 ms Flashblocks set the scale.
    pub const fn timeout(self) -> DurationNanos {
        DurationNanos(match self {
            Self::TransportAccepted => 250_000_000,
            Self::NodeKnown => 500_000_000,
            Self::SequencerReceived => 1_000_000_000,
            Self::BuilderAcknowledged => 1_500_000_000,
            Self::Preconfirmed => 2_000_000_000,
            Self::Included => 6_000_000_000,
            Self::Finalized => 900_000_000_000,
        })
    }

    pub const fn escalation(self) -> Escalation {
        match self {
            Self::TransportAccepted | Self::BuilderAcknowledged => Escalation::RetryAnotherLane,
            Self::NodeKnown | Self::SequencerReceived => Escalation::ResendSameBytes,
            Self::Preconfirmed | Self::Included => Escalation::RepriceOrAbandon,
            Self::Finalized => Escalation::ReconcileOnChain,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::TransportAccepted => "transport_accepted",
            Self::NodeKnown => "node_known",
            Self::SequencerReceived => "sequencer_received",
            Self::BuilderAcknowledged => "builder_acknowledged",
            Self::Preconfirmed => "preconfirmed",
            Self::Included => "included",
            Self::Finalized => "finalized",
        }
    }
}

impl std::fmt::Display for LifecycleStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckError {
    /// Observed twice. A stage is a fact about the chain, not a counter.
    AlreadyObserved(LifecycleStage),
    /// Observed before a stage it depends on. Not refused -- see
    /// [`AckLadder::observe`] -- but reported, because it means one of the two
    /// observations is wrong and a silent accept would hide which.
    OutOfOrder { observed: LifecycleStage, missing: LifecycleStage },
}

impl std::fmt::Display for AckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyObserved(s) => write!(f, "{s} was already observed"),
            Self::OutOfOrder { observed, missing } => {
                write!(f, "{observed} observed without {missing}")
            }
        }
    }
}

impl std::error::Error for AckError {}

/// One transaction's progress. Every stage is separately observable and
/// separately timed; nothing here derives one stage from another.
#[derive(Clone, Debug, PartialEq)]
pub struct AckLadder {
    dispatched_at: UnixNanos,
    observed: BTreeMap<LifecycleStage, UnixNanos>,
}

impl AckLadder {
    pub const fn new(dispatched_at: UnixNanos) -> Self {
        Self { dispatched_at, observed: BTreeMap::new() }
    }

    pub const fn dispatched_at(&self) -> UnixNanos {
        self.dispatched_at
    }

    /// Record a stage.
    ///
    /// A gap is reported but **not rejected**: the stages are observed from
    /// different sources -- a transport response, a node query, a sequencer
    /// feed, a receipt -- and any of them can be missed while the later one is
    /// perfectly real. Refusing an `Included` because `NodeKnown` never arrived
    /// would throw away the most important observation the system makes. The
    /// error says a gap happened so it can be counted; the stage is recorded
    /// either way.
    pub fn observe(&mut self, stage: LifecycleStage, at: UnixNanos) -> Result<(), AckError> {
        if self.observed.contains_key(&stage) {
            return Err(AckError::AlreadyObserved(stage));
        }
        let missing = LifecycleStage::ALL
            .iter()
            .copied()
            .take_while(|s| *s < stage)
            .find(|s| !self.observed.contains_key(s));
        self.observed.insert(stage, at);
        match missing {
            Some(missing) => Err(AckError::OutOfOrder { observed: stage, missing }),
            None => Ok(()),
        }
    }

    pub fn reached(&self, stage: LifecycleStage) -> bool {
        self.observed.contains_key(&stage)
    }

    pub fn observed_at(&self, stage: LifecycleStage) -> Option<UnixNanos> {
        self.observed.get(&stage).copied()
    }

    /// Latency from dispatch to a stage, for the per-stage histogram.
    pub fn latency(&self, stage: LifecycleStage) -> Option<DurationNanos> {
        self.observed
            .get(&stage)
            .map(|at| DurationNanos(at.0.saturating_sub(self.dispatched_at.0)))
    }

    /// **INV-34.** Only `Included` or `Finalized` answers this. A transport
    /// acknowledgement is not inclusion and no accumulation of earlier stages
    /// adds up to one.
    pub fn is_included(&self) -> bool {
        self.observed.keys().any(|s| s.implies_inclusion())
    }

    /// The furthest stage reached, if any.
    pub fn furthest(&self) -> Option<LifecycleStage> {
        self.observed.keys().next_back().copied()
    }

    /// The first stage whose own timeout has passed without being observed,
    /// with what to do about it.
    ///
    /// Stops at the furthest stage reached plus one: a ladder sitting at
    /// `Preconfirmed` has not "timed out on `Finalized`", it is waiting for
    /// `Included`, and reporting the last rung would escalate the wrong thing.
    pub fn timed_out(&self, now: UnixNanos) -> Option<(LifecycleStage, Escalation)> {
        let elapsed = now.0.saturating_sub(self.dispatched_at.0);
        LifecycleStage::ALL
            .iter()
            .copied()
            .find(|s| !self.reached(*s))
            .filter(|s| elapsed >= s.timeout().0)
            .map(|s| (s, s.escalation()))
    }
}
