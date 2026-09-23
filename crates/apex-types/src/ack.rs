//! The acknowledgement ladder's vocabulary (§24.3, §24.8). **INV-34.**
//!
//! > An RPC success response proves only that an endpoint **accepted the
//! > request** — not network receipt, ordering, preconfirmation or inclusion.
//!
//! Lives here rather than in `apex-capture` because both sides need it: the
//! Capture Assurance Controller keeps the per-transaction ladder, and a
//! `ChainExecutionAdapter` returns an acknowledgement *at a stage*. The
//! controller calls adapters, so the adapter crate cannot depend on the
//! controller — which leaves exactly one honest place for the shared words.
//!
//! The ladder itself (`AckLadder`) stays with the controller: it is accounting,
//! not vocabulary.
//!
//! # A note on §21.5
//!
//! §21.5's prose lists six stages and omits `builder_acknowledged`. §24.3 and
//! INV-34 both say seven. Seven is taken as authoritative: a builder that has
//! not acknowledged is a distinguishable state on a chain with a builder
//! market, and dropping it would put two different failures in one bucket.

use crate::time::DurationNanos;
use serde::{Deserialize, Serialize};

/// §24.3, in order. The derived `Ord` is the ladder, so variant order is
/// load-bearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

