//! Deterministic execution commitment (Blueprint §25).

use crate::ids::{ChainId, FlashProviderId, VenueId};
use crate::ticket::SubmissionPolicy;
use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// keccak256 over every critical trade parameter. The executor recomputes this
/// on-chain and reverts on mismatch; the signer refuses to sign a payload whose
/// recomputed commitment differs from the ticket's (INV-06).
///
/// `BTreeMap` rather than `HashMap` for `venue_fingerprints`: the hash must be
/// order-stable, and a `HashMap` iterates in an unspecified order, which would
/// make `commitment_mismatch` fire nondeterministically.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionCommitment {
    pub chain_id: ChainId,
    pub executor_address: Address,
    pub executor_version: u32,
    pub venue_fingerprints: BTreeMap<VenueId, B256>,
    pub flash_source: FlashProviderId,
    pub state_fingerprint_hash: B256,
    pub route_hash: B256,
    pub exact_inputs: Vec<U256>,
    pub min_profit: U256,
    /// bps per hop
    pub slippage_constraints: Vec<u32>,
    /// unix seconds, matching the on-chain `block.timestamp` comparison
    pub deadline: u64,
    pub submission_policy: SubmissionPolicy,
}
