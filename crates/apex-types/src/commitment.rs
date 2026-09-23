//! Deterministic execution commitment (Blueprint §25).

use crate::ids::{ChainId, FlashProviderId, VenueId};
use crate::ticket::SubmissionPolicy;
use alloy_primitives::{keccak256, Address, B256, U256};
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

/// Domain separator, so a commitment hash cannot collide with any other
/// keccak in this system.
pub const COMMITMENT_DOMAIN: &[u8] = b"apex.exec.commitment.v1";

impl ExecutionCommitment {
    /// The value the executor recomputes on-chain and the signer refuses to
    /// sign against a mismatch (INV-06).
    ///
    /// # Every variable-length field carries its length
    ///
    /// Without a length prefix, `[1, 2] ++ [3]` and `[1] ++ [2, 3]` serialise
    /// to the same bytes, so two different routes hash the same and the
    /// commitment stops distinguishing them. Each sequence is therefore
    /// written as its length followed by its elements, which is the same
    /// boundary problem the Solidity side solves by hashing each step's
    /// payload rather than concatenating them.
    ///
    /// # Order is content, not presentation
    ///
    /// `exact_inputs` and `slippage_constraints` are per hop, so their order
    /// is part of what the plan says and is preserved. `venue_fingerprints` is
    /// a `BTreeMap` precisely so its order is the key order rather than an
    /// insertion accident — a `HashMap` there would make the hash differ
    /// between two processes holding the same commitment.
    pub fn hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(COMMITMENT_DOMAIN);
        buf.extend_from_slice(&self.chain_id.0.to_be_bytes());
        buf.extend_from_slice(self.executor_address.as_slice());
        buf.extend_from_slice(&self.executor_version.to_be_bytes());

        buf.extend_from_slice(&(self.venue_fingerprints.len() as u32).to_be_bytes());
        for (venue, fingerprint) in &self.venue_fingerprints {
            buf.extend_from_slice(&venue.0.to_be_bytes());
            buf.extend_from_slice(fingerprint.as_slice());
        }

        buf.extend_from_slice(&self.flash_source.0.to_be_bytes());
        buf.extend_from_slice(self.state_fingerprint_hash.as_slice());
        buf.extend_from_slice(self.route_hash.as_slice());

        buf.extend_from_slice(&(self.exact_inputs.len() as u32).to_be_bytes());
        for amount in &self.exact_inputs {
            buf.extend_from_slice(&amount.to_be_bytes::<32>());
        }

        buf.extend_from_slice(&self.min_profit.to_be_bytes::<32>());

        buf.extend_from_slice(&(self.slippage_constraints.len() as u32).to_be_bytes());
        for bps in &self.slippage_constraints {
            buf.extend_from_slice(&bps.to_be_bytes());
        }

        buf.extend_from_slice(&self.deadline.to_be_bytes());
        buf.push(self.submission_policy as u8);
        keccak256(&buf)
    }
}
