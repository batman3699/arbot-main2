//! Simulation results and revert taxonomy (Blueprint §20, §45).

use crate::ids::TokenId;
use crate::state::StateFingerprint;
use crate::time::DurationNanos;
use alloy_primitives::{keccak256, B256};
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

/// What a `result_hash` is computed over, and — more importantly — what it is
/// not.
///
/// The hash exists so two backends can be compared by one equality rather than
/// by ten. That only works if it covers exactly the **semantic outcome** and
/// nothing about how the outcome was produced:
///
/// | Included | Excluded | Why |
/// |---|---|---|
/// | `success`, `revert` | `tier` | Tier 1 and Tier 2 agreeing is the *point*. A hash including the tier can never show agreement between them, which is the one thing §35's red/blue comparison needs it for. |
/// | `gas_used` | `elapsed` | Two correct runs take different amounts of time. A hash including duration disagrees with itself. |
/// | `balance_deltas`, `token_residues` | `result_hash` | Self-reference. |
/// | `loan_repaid`, `profit_invariant_held` | | The invariants the settlement contract enforces. |
/// | `simulated_at_state` | | Two results about **different states are not the same result**, however identical their numbers. Excluding this would make the quorum agree across a reorg. |
/// | `state_after` | | The outcome includes where it left the world. |
pub const RESULT_HASH_DOMAIN: &[u8] = b"apex.sim.result.v1";

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

impl SimulationResult {
    /// The hash this result's `result_hash` must equal.
    ///
    /// Deterministic across processes and machines: every component is
    /// serialised in a fixed order with explicit lengths, and the maps are
    /// `BTreeMap`, so iteration order is the key order rather than an
    /// insertion accident.
    pub fn canonical_hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(RESULT_HASH_DOMAIN);
        buf.push(u8::from(self.success));

        match &self.revert {
            None => buf.push(0),
            Some((class, data)) => {
                buf.push(1);
                buf.push(*class as u8);
                buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
                buf.extend_from_slice(data);
            }
        }

        buf.extend_from_slice(&self.gas_used.to_be_bytes());

        buf.extend_from_slice(&(self.balance_deltas.len() as u32).to_be_bytes());
        for (token, delta) in &self.balance_deltas {
            buf.extend_from_slice(&token.chain.0.to_be_bytes());
            buf.extend_from_slice(token.address.as_slice());
            buf.extend_from_slice(&delta.to_be_bytes());
        }

        buf.push(u8::from(self.loan_repaid));
        buf.push(u8::from(self.profit_invariant_held));

        buf.extend_from_slice(&(self.token_residues.len() as u32).to_be_bytes());
        for (token, residue) in &self.token_residues {
            buf.extend_from_slice(&token.chain.0.to_be_bytes());
            buf.extend_from_slice(token.address.as_slice());
            buf.extend_from_slice(&residue.to_be_bytes());
        }

        // Both fingerprints. Two results about different states are not the
        // same result, whatever their numbers say.
        fingerprint_into(&self.state_after, &mut buf);
        fingerprint_into(&self.simulated_at_state, &mut buf);

        keccak256(&buf)
    }

    /// Whether `result_hash` is the hash of what this result actually says.
    ///
    /// A backend that sets the field from anything other than the content has
    /// produced a record that compares equal to nothing, and the quorum would
    /// read that as unanimous disagreement.
    pub fn hash_is_consistent(&self) -> bool {
        self.result_hash == self.canonical_hash()
    }
}

fn fingerprint_into(fp: &StateFingerprint, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&fp.chain_id.0.to_be_bytes());
    buf.extend_from_slice(fp.parent_block_hash.as_slice());
    buf.extend_from_slice(&fp.confirmed_block_number.to_be_bytes());
    // `Option`s are tagged rather than flattened: `None` and `Some(0)` are
    // different facts, and a flattened encoding makes them the same bytes.
    match fp.preconf_sequence {
        None => buf.push(0),
        Some(v) => {
            buf.push(1);
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    match fp.flashblock_index {
        None => buf.push(0),
        Some(v) => {
            buf.push(1);
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    for opt in [
        &fp.state_root_or_equivalent,
        &fp.block_hash_if_available,
        &fp.external_dependency_fingerprint,
    ] {
        match opt {
            None => buf.push(0),
            Some(h) => {
                buf.push(1);
                buf.extend_from_slice(h.as_slice());
            }
        }
    }
    buf.extend_from_slice(fp.state_delta_hash.as_slice());
    buf.extend_from_slice(&(fp.venue_state_version.len() as u32).to_be_bytes());
    for (venue, version) in &fp.venue_state_version {
        buf.extend_from_slice(&venue.0.to_be_bytes());
        buf.extend_from_slice(&version.to_be_bytes());
    }
}
