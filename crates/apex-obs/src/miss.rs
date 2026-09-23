//! The missed-opportunity ledger (Blueprint §33, §27). **INV-40.**
//!
//! > Every economically attractive but unexecuted candidate receives a
//! > machine-readable reason code.
//!
//! §27 calls this "the counterfactual dataset deciding where the next
//! engineering dollar goes", and that is the load-bearing sentence. A log of
//! what happened is cheap and nearly useless; a ledger of what *did not*
//! happen, with a reason attached to each, is the only thing that can answer
//! "which of our gates is costing us the most".
//!
//! Two properties make it answer that rather than merely accumulate:
//!
//! - **[`MissReason`] has no catch-all.** "Other" would silently become the
//!   largest bucket, and the largest bucket is exactly where the answer would
//!   be hiding.
//! - **Recording takes a rejection, not a reason.** [`MissLedger::record`]
//!   derives the bucket from the rejection value through
//!   [`ExplainsMiss`], so a caller cannot pick a convenient one. The rejection
//!   is kept too: the bucket is what you aggregate, the rejection is what you
//!   read when a bucket spikes.

use apex_types::ids::CandidateId;
use apex_types::miss::{ExplainsMiss, MissReason, MissRecord, ObservedOutcome, SearchPath};
use apex_types::state::StateFingerprint;
use apex_types::ticket::SubmissionPolicy;
use std::collections::BTreeMap;

/// What the caller knows about the candidate, independent of why it was
/// rejected.
#[derive(Clone, Debug, PartialEq)]
pub struct MissContext {
    pub candidate_id: CandidateId,
    pub state_fingerprint: StateFingerprint,
    pub simulated_ev: i128,
    pub estimated_capture_probability: f64,
    pub path: SearchPath,
    pub submission_policy: SubmissionPolicy,
}

/// One recorded miss, with the rejection that caused it kept alongside the
/// bucket it aggregates into.
#[derive(Clone, Debug, PartialEq)]
pub struct Miss {
    pub record: MissRecord,
    /// The rejection's own `Debug`, which is the diagnosis `MissReason`
    /// deliberately does not carry. Stored as text because the ledger is
    /// heterogeneous by construction -- every crate's rejection type lands here.
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct MissLedger {
    misses: Vec<Miss>,
}

impl MissLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// **The only way in, and it does not take a reason.**
    ///
    /// The bucket comes from the rejection through [`ExplainsMiss`]. A
    /// `record(ctx, MissReason::LowEv)` signature would let a caller under
    /// deadline pressure file anything as low-EV, which is how the largest
    /// bucket becomes the least informative one.
    pub fn record<R: ExplainsMiss + std::fmt::Debug>(
        &mut self,
        ctx: &MissContext,
        rejection: &R,
    ) -> MissReason {
        let reason = rejection.miss_reason();
        self.misses.push(Miss {
            record: MissRecord {
                candidate_id: ctx.candidate_id,
                state_fingerprint: ctx.state_fingerprint.clone(),
                simulated_ev: ctx.simulated_ev,
                estimated_capture_probability: ctx.estimated_capture_probability,
                reason,
                path: ctx.path,
                submission_policy: ctx.submission_policy,
                // Filled in later by `observe`. `None` is honest: at the moment
                // of rejection nobody knows what happened next, and a default
                // here would be a claim.
                later_realized_outcome: None,
            },
            detail: format!("{rejection:?}"),
        });
        reason
    }

    /// What actually happened afterwards. §33: this is what turns the ledger
    /// from a log into a calibration dataset -- a miss nobody followed up on
    /// cannot tell you whether the gate that caused it was right.
    pub fn observe(&mut self, candidate: CandidateId, outcome: ObservedOutcome) -> bool {
        let mut found = false;
        for m in self.misses.iter_mut().filter(|m| m.record.candidate_id == candidate) {
            m.record.later_realized_outcome = Some(outcome.clone());
            found = true;
        }
        found
    }

    pub fn len(&self) -> usize {
        self.misses.len()
    }
    pub fn is_empty(&self) -> bool {
        self.misses.is_empty()
    }
    pub fn misses(&self) -> &[Miss] {
        &self.misses
    }

    /// Counts by bucket. The histogram §8 names as INV-40's metric.
    pub fn by_reason(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for m in &self.misses {
            *out.entry(m.record.reason.label()).or_insert(0) += 1;
        }
        out
    }

    /// Counts by plane. Separate from `by_reason` because §3's candidate log
    /// mixed both and had to be filtered on `edges_scanned == 0` to isolate
    /// fast-path rejections -- the filter that was easy to forget.
    pub fn by_path(&self) -> BTreeMap<SearchPath, usize> {
        let mut out = BTreeMap::new();
        for m in &self.misses {
            *out.entry(m.record.path).or_insert(0) += 1;
        }
        out
    }

    /// **The question the ledger exists to answer.** Total EV declined, by
    /// bucket -- which is not the same ranking as the count, and the difference
    /// is the point: a thousand dust rejections matter less than three large
    /// ones, and a count-only histogram says the opposite.
    pub fn declined_ev_by_reason(&self) -> BTreeMap<&'static str, i128> {
        let mut out = BTreeMap::new();
        for m in &self.misses {
            let slot = out.entry(m.record.reason.label()).or_insert(0i128);
            *slot = slot.saturating_add(m.record.simulated_ev);
        }
        out
    }

    /// Misses that a competitor later landed. §33's calibration signal: a gate
    /// that keeps rejecting trades somebody else then executes is a gate that
    /// is wrong, not a market that was unprofitable.
    pub fn taken_by_competitors(&self) -> Vec<&Miss> {
        self.misses
            .iter()
            .filter(|m| {
                m.record
                    .later_realized_outcome
                    .as_ref()
                    .is_some_and(|o| o.landed_by_competitor.is_some())
            })
            .collect()
    }
}
