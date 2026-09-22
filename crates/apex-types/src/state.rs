//! State versioning and provenance (Blueprint §5.2, §5.6, §45).

use crate::ids::{ChainId, FeedSourceId, VenueId};
use crate::time::UnixNanos;
use alloy_primitives::B256;
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
