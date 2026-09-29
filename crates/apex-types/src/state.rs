//! State versioning and provenance (Blueprint §5.2, §5.6, §45).

use crate::ids::{ChainId, FeedSourceId, VenueId};
use crate::time::UnixNanos;
use alloy_primitives::{keccak256, B256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Monotonic within a process. Not meaningful across restarts; pair it with the
/// fingerprint when identity has to survive one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StateVersion(pub u64);

/// Branch 0 is canonical; everything else is speculative (§5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StateBranchId(pub u32);

impl StateBranchId {
    pub const CANONICAL: Self = Self(0);

    pub const fn is_canonical(self) -> bool {
        self.0 == 0
    }
}

/// Blueprint §5.2. Every field is required; there is deliberately no `Default`.
///
/// A defaulted fingerprint would compare equal to another defaulted one, which
/// is exactly the false "state unchanged" conclusion §5.6 forbids ("a feed gap
/// may never be silently converted into 'probably unchanged'").
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StateFingerprint {
    pub chain_id: ChainId,
    pub parent_block_hash: B256,
    pub confirmed_block_number: u64,
    pub preconf_sequence: Option<u64>,
    pub flashblock_index: Option<u32>,
    pub state_root_or_equivalent: Option<B256>,
    pub block_hash_if_available: Option<B256>,
    pub state_delta_hash: B256,
    pub venue_state_version: BTreeMap<VenueId, u64>,
    pub external_dependency_fingerprint: Option<B256>,
}

/// Domain separator for [`StateFingerprint::hash`], so it cannot collide with
/// any other keccak in this system.
pub const STATE_FINGERPRINT_DOMAIN: &[u8] = b"apex.state.fingerprint.v1";

impl StateFingerprint {
    /// The value §25's `ExecutionCommitment` carries as `state_fingerprint_hash`.
    ///
    /// # Length prefixes, and why this one needs them
    ///
    /// `venue_state_version` is a **variable-length** map, so without a count in
    /// front `{A:1, B:2}` and `{A:1}` followed by whatever came next could
    /// serialise identically — the boundary problem `ExecutionCommitment::hash`
    /// documents for `exact_inputs` and `slippage_constraints`.
    ///
    /// Worth contrasting with `apex_venues::fingerprint`, which has **no** length
    /// prefix and is right not to: every element there is fixed-width, so the
    /// preimage parses uniquely. The rule is about the elements, not about hashes
    /// in general, and stating it once in each place is how it stays true.
    ///
    /// # Every `Option` writes a tag
    ///
    /// `None` is a byte, not an absence. Skipping an absent field would let a
    /// fingerprint with `preconf_sequence: None` produce the same preimage as one
    /// where the next field happened to start with the same bytes — the same
    /// boundary problem in a different costume.
    ///
    /// `BTreeMap` rather than `HashMap` for `venue_state_version` is already the
    /// type's choice and it is load-bearing here: the hash must be order-stable,
    /// and a `HashMap` iterates in an unspecified order, so two processes holding
    /// the same fingerprint would disagree about its hash.
    pub fn hash(&self) -> B256 {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(STATE_FINGERPRINT_DOMAIN);
        buf.extend_from_slice(&self.chain_id.0.to_be_bytes());
        buf.extend_from_slice(self.parent_block_hash.as_slice());
        buf.extend_from_slice(&self.confirmed_block_number.to_be_bytes());

        let mut tagged_u64 = |v: Option<u64>| match v {
            None => buf.push(0),
            Some(x) => {
                buf.push(1);
                buf.extend_from_slice(&x.to_be_bytes());
            }
        };
        tagged_u64(self.preconf_sequence);
        match self.flashblock_index {
            None => buf.push(0),
            Some(x) => {
                buf.push(1);
                buf.extend_from_slice(&x.to_be_bytes());
            }
        }
        for opt in [self.state_root_or_equivalent, self.block_hash_if_available] {
            match opt {
                None => buf.push(0),
                Some(h) => {
                    buf.push(1);
                    buf.extend_from_slice(h.as_slice());
                }
            }
        }
        buf.extend_from_slice(self.state_delta_hash.as_slice());

        buf.extend_from_slice(&(self.venue_state_version.len() as u32).to_be_bytes());
        for (venue, version) in &self.venue_state_version {
            buf.extend_from_slice(&venue.0.to_be_bytes());
            buf.extend_from_slice(&version.to_be_bytes());
        }

        match self.external_dependency_fingerprint {
            None => buf.push(0),
            Some(h) => {
                buf.push(1);
                buf.extend_from_slice(h.as_slice());
            }
        }
        keccak256(&buf)
    }
}

/// How a state version came to exist (§5.6). Carried so that two disagreeing
/// feeds can be resolved by parentage rather than by vote (INV-13).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateProvenance {
    pub parent_state_id: Option<StateVersion>,
    pub fingerprint: StateFingerprint,
    pub feed_source_id: FeedSourceId,
    pub sequence_range: (u64, u64),
    pub first_seen: UnixNanos,
    pub last_seen: UnixNanos,
    pub canonicality: Canonicality,
    pub reconstruction: ReconstructionStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Canonicality {
    Confirmed,
    Preconfirmed,
    Speculative,
    Orphaned,
}

/// §5.6: only `Verified` may authorize a live ticket.
///
/// No `Default` and no `Unknown`. A default would have to be one of these three,
/// and whichever was chosen would be wrong: defaulting to `Verified` authorizes
/// trades on unverified state, and defaulting to `Unsafe` makes the safe path
/// the one you get by forgetting. Callers must say which they mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconstructionStatus {
    Verified,
    Rebuilding,
    Unsafe,
}

impl ReconstructionStatus {
    /// The single predicate the ticket-admission path consults (INV-08).
    pub const fn may_authorize_live_ticket(self) -> bool {
        matches!(self, Self::Verified)
    }
}
