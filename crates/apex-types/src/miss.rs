//! Missed-opportunity accounting (Blueprint §33).

use crate::ids::CandidateId;
use crate::state::StateFingerprint;
use crate::ticket::SubmissionPolicy;
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// Blueprint §33. EXHAUSTIVE -- there is deliberately no catch-all variant.
///
/// A catch-all is where an unexplained miss goes to be forgotten, and the whole
/// value of this ledger (§27) is that it is the counterfactual dataset deciding
/// where the next engineering dollar goes. "Other" would silently become the
/// largest bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MissReason {
    LowEv,
    StaleState,
    TooSlow,
    CompetitorWon,
    SimFail,
    RiskFail,
    GasFail,
    L1DataCostFail,
    NoFlashLiquidity,
    VenueDisabled,
    ConflictRejected,
    PackingNotWorthwhile,
    EarliestFlashblockTooLate,
    HookModelIncomplete,
    BuilderRejected,
    SequencerRejected,
    NonceUnavailable,
}

impl MissReason {
    pub const ALL: [Self; 17] = [
        Self::LowEv,
        Self::StaleState,
        Self::TooSlow,
        Self::CompetitorWon,
        Self::SimFail,
        Self::RiskFail,
        Self::GasFail,
        Self::L1DataCostFail,
        Self::NoFlashLiquidity,
        Self::VenueDisabled,
        Self::ConflictRejected,
        Self::PackingNotWorthwhile,
        Self::EarliestFlashblockTooLate,
        Self::HookModelIncomplete,
        Self::BuilderRejected,
        Self::SequencerRejected,
        Self::NonceUnavailable,
    ];

    /// Stable wire label. Matched exhaustively on purpose: adding a variant
    /// without labelling it is a compile error, which is the point.
    pub const fn label(self) -> &'static str {
        match self {
            Self::LowEv => "LOW_EV",
            Self::StaleState => "STALE_STATE",
            Self::TooSlow => "TOO_SLOW",
            Self::CompetitorWon => "COMPETITOR_WON",
            Self::SimFail => "SIM_FAIL",
            Self::RiskFail => "RISK_FAIL",
            Self::GasFail => "GAS_FAIL",
            Self::L1DataCostFail => "L1_DATA_COST_FAIL",
            Self::NoFlashLiquidity => "NO_FLASH_LIQUIDITY",
            Self::VenueDisabled => "VENUE_DISABLED",
            Self::ConflictRejected => "CONFLICT_REJECTED",
            Self::PackingNotWorthwhile => "PACKING_NOT_WORTHWHILE",
            Self::EarliestFlashblockTooLate => "EARLIEST_FLASHBLOCK_TOO_LATE",
            Self::HookModelIncomplete => "HOOK_MODEL_INCOMPLETE",
            Self::BuilderRejected => "BUILDER_REJECTED",
            Self::SequencerRejected => "SEQUENCER_REJECTED",
            Self::NonceUnavailable => "NONCE_UNAVAILABLE",
        }
    }
}

/// Which plane produced the rejection.
///
/// Exists because the current candidate log mixes both and has to be filtered on
/// `edges_scanned == 0` to isolate fast-path rejections. Recording it directly
/// removes that filter and the chance of forgetting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchPath {
    Fast,
    Slow,
    CoverageAudit,
}

/// What actually happened afterwards, where observable. This is what turns the
/// ledger from a log into a calibration dataset.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservedOutcome {
    pub landed_by_competitor: Option<B256>,
    pub realized_profit_estimate: Option<i128>,
}

/// Blueprint §33 record, written for every economically attractive but
/// unexecuted candidate (INV-40).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MissRecord {
    pub candidate_id: CandidateId,
    pub state_fingerprint: StateFingerprint,
    pub simulated_ev: i128,
    pub estimated_capture_probability: f64,
    pub reason: MissReason,
    pub path: SearchPath,
    pub submission_policy: SubmissionPolicy,
    pub later_realized_outcome: Option<ObservedOutcome>,
}
