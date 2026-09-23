//! Base submission lanes (§21.1, §21.5, §24.2, §24.4). **INV-10, INV-34, B-14.**
//!
//! # The feed policy, preserved
//!
//! §21.1: the application connects through a Flashblocks-aware **RPC provider**
//! endpoint, not the raw node-operator infrastructure stream. That is what
//! `base_fast.rs` already does and what Blueprint §22 endorses, so it is
//! preserved here as a type: [`EndpointKind::RawInfrastructureStream`] exists
//! so a lane can be *described*, and [`SubmissionLane::may_dispatch`] refuses
//! it. Describing the stream is necessary — it is a real thing the system
//! reads from — and dispatching to it is a category error.
//!
//! # B-14: a policy is a claim, and a claim needs evidence
//!
//! Base's lanes use **provider-level** MEV protection rather than builder
//! relays, which is the right architecture for a sequencer chain. The gap B-14
//! records is that `SubmissionPolicy` was an assertion nobody had verified,
//! while §14.1 prices lanes by that policy — so a silently flipped provider
//! toggle would degrade capture with no signal. The protection is a **dashboard
//! setting**; it can change without a deploy, which is why the evidence carries
//! a TTL and why a stale attestation is treated as none.

use apex_types::ack::LifecycleStage;
use apex_types::ids::SubmissionLaneId;
use apex_types::ticket::SubmissionPolicy;
use apex_types::time::{DurationNanos, UnixNanos};
use serde::{Deserialize, Serialize};

/// What sits at the other end of a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointKind {
    /// §21.1's endorsed shape: a Flashblocks-aware RPC provider.
    FlashblocksAwareRpc,
    /// The node-operator infrastructure stream. A **read** source, never a
    /// dispatch target — see the module header.
    RawInfrastructureStream,
    /// §24.4: the fallback, never the default.
    PublicRpc,
}

impl EndpointKind {
    pub const fn is_dispatchable(self) -> bool {
        !matches!(self, Self::RawInfrastructureStream)
    }
}

/// B-14. Why a lane's [`SubmissionPolicy`] is believed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivacyEvidence {
    /// The provider says so — a documented per-endpoint setting. Admissible,
    /// and weaker than an observation: the setting is a dashboard toggle and
    /// nothing here watched it change.
    ProviderAttested { source: &'static str, checked_at: UnixNanos, ttl: DurationNanos },
    /// We looked. A probe that submitted and observed pre-inclusion visibility
    /// (or its absence) is evidence about the world rather than about a
    /// document.
    Measured { probe: &'static str, checked_at: UnixNanos, ttl: DurationNanos },
}

impl PrivacyEvidence {
    /// **A vendor doc is weaker evidence than an observed mempool-absence
    /// probe. Both are admissible; §14.1's EV model weights them differently.**
    ///
    /// The numbers are a starting point, and what matters is the strict
    /// ordering: nothing downstream may treat an attestation as equivalent to a
    /// measurement, and the ordering is what a test can pin.
    pub const fn confidence(&self) -> f64 {
        match self {
            Self::ProviderAttested { .. } => 0.6,
            Self::Measured { .. } => 0.95,
        }
    }

    pub const fn checked_at(&self) -> UnixNanos {
        match self {
            Self::ProviderAttested { checked_at, .. } | Self::Measured { checked_at, .. } => {
                *checked_at
            }
        }
    }

    pub const fn ttl(&self) -> DurationNanos {
        match self {
            Self::ProviderAttested { ttl, .. } | Self::Measured { ttl, .. } => *ttl,
        }
    }

    /// Evidence past its TTL is **not weaker evidence, it is none**. The
    /// guarantee is a setting somebody can change without a deploy, so an old
    /// attestation says what was true then and nothing about now.
    pub const fn is_fresh(&self, now: UnixNanos) -> bool {
        now.0.saturating_sub(self.checked_at().0) < self.ttl().0
    }

    pub const fn age(&self, now: UnixNanos) -> DurationNanos {
        DurationNanos(now.0.saturating_sub(self.checked_at().0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmissionLane {
    pub id: SubmissionLaneId,
    pub endpoint: EndpointKind,
    pub policy: SubmissionPolicy,
    /// `None` is correct for a `Public` lane: there is nothing to attest to.
    pub privacy_evidence: Option<PrivacyEvidence>,
}

/// Why a lane may not be dispatched through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaneRefusal {
    /// §21.1.
    NotADispatchTarget(EndpointKind),
    /// B-14: a protected policy with nothing behind it.
    UnevidencedPolicy { policy: SubmissionPolicy },
    /// The evidence exists and has expired.
    StaleEvidence { age: DurationNanos, ttl: DurationNanos },
}

impl std::fmt::Display for LaneRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotADispatchTarget(k) => write!(f, "{k:?} is a read source, not a lane"),
            Self::UnevidencedPolicy { policy } => {
                write!(f, "the lane claims {policy:?} with no evidence")
            }
            Self::StaleEvidence { age, ttl } => {
                write!(f, "evidence is {} ns old against a {} ns TTL", age.0, ttl.0)
            }
        }
    }
}

impl std::error::Error for LaneRefusal {}

impl SubmissionLane {
    /// Everything that must be true before anything is sent here.
    pub fn may_dispatch(&self, now: UnixNanos) -> Result<(), LaneRefusal> {
        if !self.endpoint.is_dispatchable() {
            return Err(LaneRefusal::NotADispatchTarget(self.endpoint));
        }
        if self.policy == SubmissionPolicy::Public {
            return Ok(());
        }
        let Some(ev) = self.privacy_evidence else {
            return Err(LaneRefusal::UnevidencedPolicy { policy: self.policy });
        };
        if !ev.is_fresh(now) {
            return Err(LaneRefusal::StaleEvidence { age: ev.age(now), ttl: ev.ttl() });
        }
        Ok(())
    }

    /// What §14.1 should weight this lane's protection by. Zero when the lane
    /// makes no protection claim or cannot support the one it makes — never a
    /// default, because a defaulted confidence is a lane being priced as
    /// protected on the strength of nobody having checked.
    pub fn privacy_confidence(&self, now: UnixNanos) -> f64 {
        match self.privacy_evidence {
            Some(ev) if ev.is_fresh(now) && self.policy != SubmissionPolicy::Public => {
                ev.confidence()
            }
            _ => 0.0,
        }
    }
}

/// Base's per-endpoint MEV protection, as configured today.
///
/// Seeded as `ProviderAttested` because that is what it is: BlockPI's keyed
/// Base endpoint has MEV protection **on by default**, alongside a bundle
/// service. Recording it as `Measured` would overstate what is known — nothing
/// has observed a transaction's absence from the public mempool — and §14.1
/// would then price the lane above its evidence.
///
/// The one-hour TTL is deliberately short for an attestation about a dashboard
/// setting. Task 7.3a Step 4 upgrades a lane to `Measured` once the Phase 9
/// competitor model can observe pre-inclusion visibility.
pub const BLOCKPI_ATTESTATION_TTL: DurationNanos = DurationNanos(3_600_000_000_000);

pub fn blockpi_base_lane(id: SubmissionLaneId, checked_at: UnixNanos) -> SubmissionLane {
    SubmissionLane {
        id,
        endpoint: EndpointKind::FlashblocksAwareRpc,
        policy: SubmissionPolicy::Private,
        privacy_evidence: Some(PrivacyEvidence::ProviderAttested {
            source: "blockpi: per-endpoint MEV protection, on by default",
            checked_at,
            ttl: BLOCKPI_ATTESTATION_TTL,
        }),
    }
}

/// `base_transactionStatus`, and what each answer is evidence of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BaseTransactionStatus {
    /// The node has not seen it.
    Unknown,
    /// **The preconfirmation node received it.** §21.5: this is *not* evidence
    /// it has been placed in a Flashblock.
    Known,
    /// The sequencer has committed to including it.
    Preconfirmed,
    /// It is in a block.
    Included,
    /// The node rejected it outright.
    Rejected,
}

impl BaseTransactionStatus {
    /// **INV-34's boundary, in the one place it is crossed.**
    ///
    /// `Known → NodeKnown`, never `Included`. C-09 records the legacy code
    /// treating a successful `send_raw_transaction` as the submission outcome;
    /// this is the same mistake one rung higher, and it is the rung where it is
    /// most tempting — `Known` sounds like the transaction is safe.
    pub const fn stage(self) -> Option<LifecycleStage> {
        match self {
            Self::Unknown | Self::Rejected => None,
            Self::Known => Some(LifecycleStage::NodeKnown),
            Self::Preconfirmed => Some(LifecycleStage::Preconfirmed),
            Self::Included => Some(LifecycleStage::Included),
        }
    }
}

/// Why a redundant send stopped early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedundancyOutcome {
    /// Every eligible lane was sent the same bytes.
    SentToAll { lanes: Vec<SubmissionLaneId> },
    /// The opportunity went stale part-way through, so the remaining lanes were
    /// **not** sent. §16.2 step 9: reprice or explicitly abandon.
    CancelledAsStale { sent: Vec<SubmissionLaneId>, cancelled: Vec<SubmissionLaneId> },
}

/// **INV-10, structurally.**
///
/// > Transport redundancy sends the *same signed bytes*; economically distinct
/// > duplicates require an explicit policy flag.
///
/// This takes one `&[u8]` and hands the same slice to every lane. There is no
/// per-lane payload parameter, no builder, and no re-signing hook — so a lane
/// receiving different bytes is not a bug to catch in review, it is a signature
/// this function cannot produce.
///
/// `still_fresh` is consulted **before each send**, not once at the start: the
/// point of the check is that a fallback lane must not be dispatched to after
/// the opportunity has already died on the primary.
pub fn send_redundantly<E>(
    signed: &[u8],
    lanes: &[SubmissionLane],
    now: UnixNanos,
    mut still_fresh: impl FnMut() -> bool,
    mut send: impl FnMut(&SubmissionLane, &[u8]) -> Result<(), E>,
) -> RedundancyOutcome {
    let eligible: Vec<&SubmissionLane> =
        lanes.iter().filter(|l| l.may_dispatch(now).is_ok()).collect();

    let mut sent = Vec::new();
    for (i, lane) in eligible.iter().enumerate() {
        if !still_fresh() {
            return RedundancyOutcome::CancelledAsStale {
                sent,
                cancelled: eligible[i..].iter().map(|l| l.id).collect(),
            };
        }
        if send(lane, signed).is_ok() {
            sent.push(lane.id);
        }
    }
    RedundancyOutcome::SentToAll { lanes: sent }
}
