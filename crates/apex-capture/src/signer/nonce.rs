//! Per-lane nonce management (§18.3, §27.2).
//!
//! **ADAPTed from `arb-exec-legacy/src/main.rs:2694-2764`, not rewritten.** Its
//! authoritative-pending-nonce start point and its gap-recovery path encode a
//! real historical bug fix, and the comments that record why migrate with the
//! code. Two things change:
//!
//! 1. It is **per lane**. The legacy manager was one object for one wallet;
//!    §18.2 needs independent nonce streams and independent pending-state
//!    tracking per lane, and sharing either is how one lane's stall becomes
//!    every lane's stall.
//!
//! 2. **The chain's pending nonce is an input, not a fetch.** The legacy
//!    version called `get_transaction_count` inside `get_next`, which makes the
//!    allocator async, provider-shaped and impossible to model-check. Here the
//!    caller supplies it. That is the same crate-split seam used throughout:
//!    the decision is pure, the I/O is the caller's.
//!
//! Nonce release is tied to a ticket's terminal close rather than to the
//! ad-hoc `mark_failed`/`mark_confirmed` pair, so a lane cannot leak a
//! reservation by forgetting which one it was supposed to call.

use apex_types::ids::SignerLaneId;
use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::{BTreeMap, BTreeSet};

/// How long a reservation stays in the in-flight set before it is considered
/// stale. From the legacy code: "beyond any realistic inclusion window".
pub const STALE_RESERVATION: DurationNanos = DurationNanos(120_000_000_000);

/// A nonce this lane has committed to. Not `u64`: a bare integer can be passed
/// to the wrong lane's signer, and nothing in the type system would notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReservedNonce {
    lane: SignerLaneId,
    nonce: u64,
}

impl ReservedNonce {
    pub const fn lane(&self) -> SignerLaneId {
        self.lane
    }
    pub const fn get(&self) -> u64 {
        self.nonce
    }
}

/// §18.3's five fields, per lane.
#[derive(Clone, Debug)]
pub struct NonceLane {
    lane: SignerLaneId,
    /// Highest nonce observed confirmed on chain, if any.
    confirmed_nonce: Option<u64>,
    /// The chain's pending count as last supplied by the caller.
    pending_nonce: Option<u64>,
    /// The local high-water mark: the next nonce this lane will hand out. The
    /// legacy code's `current`.
    reserved_nonce: Option<u64>,
    /// Reservations handed out and not yet released, with when. The legacy
    /// code's `pending` map.
    in_flight: BTreeMap<u64, UnixNanos>,
    /// Reservations that reached the wire.
    submitted_nonce: BTreeSet<u64>,
    /// Nonces carrying an outstanding replacement transaction (§27.4).
    replacement_set: BTreeSet<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceError {
    /// The reservation belongs to a different lane. Only reachable by passing a
    /// `ReservedNonce` across lanes, which is the thing the type exists to make
    /// visible.
    WrongLane { expected: SignerLaneId, got: SignerLaneId },
    /// Released twice, or never reserved here.
    NotInFlight(u64),
}

impl std::fmt::Display for NonceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongLane { expected, got } => {
                write!(f, "nonce belongs to lane {} but was used on lane {}", got.0, expected.0)
            }
            Self::NotInFlight(n) => write!(f, "nonce {n} is not in flight on this lane"),
        }
    }
}

impl std::error::Error for NonceError {}

impl NonceLane {
    pub const fn new(lane: SignerLaneId) -> Self {
        Self {
            lane,
            confirmed_nonce: None,
            pending_nonce: None,
            reserved_nonce: None,
            in_flight: BTreeMap::new(),
            submitted_nonce: BTreeSet::new(),
            replacement_set: BTreeSet::new(),
        }
    }

    pub const fn lane(&self) -> SignerLaneId {
        self.lane
    }
    pub const fn confirmed_nonce(&self) -> Option<u64> {
        self.confirmed_nonce
    }
    pub const fn pending_nonce(&self) -> Option<u64> {
        self.pending_nonce
    }
    pub const fn reserved_nonce(&self) -> Option<u64> {
        self.reserved_nonce
    }
    pub fn in_flight(&self) -> impl Iterator<Item = u64> + '_ {
        self.in_flight.keys().copied()
    }
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }
    pub fn submitted(&self) -> &BTreeSet<u64> {
        &self.submitted_nonce
    }
    pub fn replacements(&self) -> &BTreeSet<u64> {
        &self.replacement_set
    }

    /// The legacy `get_next`, with the RPC call lifted out.
    ///
    /// `chain_pending` is the node's PENDING transaction count for this lane's
    /// address.
    pub fn reserve(&mut self, chain_pending: u64, now: UnixNanos) -> ReservedNonce {
        // Drop stale in-flight markers (beyond any realistic inclusion window).
        self.in_flight.retain(|_, at| now.0.saturating_sub(at.0) < STALE_RESERVATION.0);

        // Authoritative starting point: the chain's PENDING nonce accounts for our
        // txs already seen by the node's mempool. Using `Latest` (confirmed) here
        // was the historical bug -- it could hand out a nonce still occupied by an
        // unconfirmed tx, causing replacement wars or silently dropped txs.
        self.pending_nonce = Some(chain_pending);

        // Local high-water mark guards against an RPC pending view that lags our
        // privately-submitted bundles (relay txs may not be in this node's mempool).
        let local_floor = self.reserved_nonce.unwrap_or(chain_pending);
        let next = chain_pending.max(local_floor);

        self.reserved_nonce = Some(next.saturating_add(1));
        self.in_flight.insert(next, now);
        ReservedNonce { lane: self.lane, nonce: next }
    }

    /// The reservation reached the wire.
    pub fn mark_submitted(&mut self, n: ReservedNonce) -> Result<(), NonceError> {
        self.check_lane(n)?;
        if !self.in_flight.contains_key(&n.nonce) {
            return Err(NonceError::NotInFlight(n.nonce));
        }
        self.submitted_nonce.insert(n.nonce);
        Ok(())
    }

    /// A replacement transaction is outstanding for this nonce (§27.4). Not a
    /// new reservation -- a replacement by definition reuses the nonce.
    pub fn mark_replacement(&mut self, n: ReservedNonce) -> Result<(), NonceError> {
        self.check_lane(n)?;
        if !self.in_flight.contains_key(&n.nonce) {
            return Err(NonceError::NotInFlight(n.nonce));
        }
        self.replacement_set.insert(n.nonce);
        Ok(())
    }

    /// Release a reservation because its ticket reached a terminal outcome.
    ///
    /// `landed` says whether the transaction is known to be on chain. The
    /// legacy `mark_confirmed`/`mark_failed` split is folded into this one call
    /// on purpose: a lane that leaks a reservation because the caller picked
    /// the wrong one of two methods is a lane that stops allocating.
    pub fn release(&mut self, n: ReservedNonce, landed: bool) -> Result<(), NonceError> {
        self.check_lane(n)?;
        if self.in_flight.remove(&n.nonce).is_none() {
            return Err(NonceError::NotInFlight(n.nonce));
        }
        self.submitted_nonce.remove(&n.nonce);
        self.replacement_set.remove(&n.nonce);

        if landed {
            self.confirmed_nonce = Some(self.confirmed_nonce.map_or(n.nonce, |c| c.max(n.nonce)));
            return Ok(());
        }

        // Gap recovery: if the failed nonce was the highest we allocated and nothing
        // higher is still in flight, reclaim it so the next dispatch reuses it instead
        // of leaving a permanent mempool gap. If the tx actually landed despite the
        // local failure, the next reserve() reconciles via the chain pending nonce
        // (max(chain_pending, local_floor)), so we never reuse a nonce that confirmed.
        let highest_in_flight = self.in_flight.keys().copied().max();
        if highest_in_flight.is_none_or(|h| h < n.nonce) {
            self.reserved_nonce = Some(n.nonce);
        }
        Ok(())
    }

    fn check_lane(&self, n: ReservedNonce) -> Result<(), NonceError> {
        if n.lane != self.lane {
            return Err(NonceError::WrongLane { expected: self.lane, got: n.lane });
        }
        Ok(())
    }
}
