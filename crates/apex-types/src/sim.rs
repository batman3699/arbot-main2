//! Simulation results and revert taxonomy (Blueprint §20, §45).

use crate::ids::TokenId;
use crate::state::StateFingerprint;
use crate::time::DurationNanos;
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SimulationTier {
    Tier0Analytic,
    Tier1LocalExact,
    Tier2FullEvm,
    Tier3Adversarial,
    /// §20: "never a latency technique, and never a substitute for simulation."
    Tier4Canary,
}

/// Venue adapters classify reverts rather than returning opaque bytes, so the
/// risk engine can attribute a loss to a class (§28.2) instead of counting
/// undifferentiated failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RevertClass {
    MinOutNotMet,
    InsufficientLiquidity,
    Expired,
    Unauthorized,
    FlashRepaymentShortfall,
    ProfitInvariantViolated,
    TokenTransferFailed,
    HookRejected,
    OutOfGas,
    Unknown,
}

/// Blueprint §20 Tier 2 checks, in one record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimulationResult {
    pub tier: SimulationTier,
    pub success: bool,
    pub revert: Option<(RevertClass, Vec<u8>)>,
    pub gas_used: u64,
    /// Signed: a leg can consume as well as produce. §7.2 requires non-standard
    /// tokens be measured from actual balance deltas, not nominal amounts.
    pub balance_deltas: BTreeMap<TokenId, i128>,
    pub loan_repaid: bool,
    pub profit_invariant_held: bool,
    pub token_residues: BTreeMap<TokenId, u128>,
    pub state_after: StateFingerprint,
    /// The state this ran against. Recorded separately from `state_after`
    /// because `sim_quorum` pins verifiers to the block the primary simulated
    /// at -- a call against different state answers a different question.
    pub simulated_at_state: StateFingerprint,
    pub result_hash: B256,
    pub elapsed: DurationNanos,
}
