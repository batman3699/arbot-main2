//! Sequence accounting for one state feed (Blueprint §5.6).
//!
//! "A feed gap may never be silently converted into 'probably unchanged.'"
//!
//! The rule that makes this load-bearing is the recovery path, not the
//! detection: a gap marks the feed `Unsafe`, and clean traffic afterwards does
//! NOT clear it. Only an explicit, fingerprint-verified rebuild does. Otherwise
//! a feed repairs its own reputation while the missed deltas are still missing,
//! which is precisely the silent conversion §5.6 forbids.

use apex_types::ids::FeedSourceId;
use apex_types::state::ReconstructionStatus;

#[derive(Debug, Clone)]
pub struct FeedIntegrity {
    source: FeedSourceId,
    last_sequence: Option<u64>,
    expected_sequence: Option<u64>,
    gap_count: u64,
    missing_count: u64,
    duplicate_count: u64,
    out_of_order_count: u64,
    reconnect_count: u64,
    status: ReconstructionStatus,
}

impl FeedIntegrity {
    pub fn new(source: FeedSourceId) -> Self {
        Self {
            source,
            last_sequence: None,
            expected_sequence: None,
            gap_count: 0,
            missing_count: 0,
            duplicate_count: 0,
            out_of_order_count: 0,
            reconnect_count: 0,
            status: ReconstructionStatus::Verified,
        }
    }

    /// Record one delivery.
    ///
    /// Gaps, duplicates and out-of-order deliveries are counted separately
    /// because they mean different things and want different responses. The
    /// legacy `continuity.rs` makes the same distinction and is explicit that a
    /// filtered subscription's index gaps carry no information -- this counts
    /// FEED sequence numbers, which do.
    pub fn observe(&mut self, sequence: u64) {
        match self.last_sequence {
            None => {}
            Some(last) if sequence == last => {
                self.duplicate_count += 1;
                return;
            }
            Some(last) if sequence < last => {
                self.out_of_order_count += 1;
                return;
            }
            Some(last) if sequence > last + 1 => {
                self.gap_count += 1;
                self.missing_count += sequence - last - 1;
                // Fails closed. Recovery is explicit; see rebuild_verified.
                self.status = ReconstructionStatus::Unsafe;
            }
            Some(_) => {}
        }
        self.last_sequence = Some(sequence);
        self.expected_sequence = Some(sequence + 1);
    }

    pub fn note_reconnect(&mut self) {
        self.reconnect_count += 1;
    }

    /// Enter rebuild. Not tradeable, and distinct from `Unsafe` so an operator
    /// can tell "we know it is broken" from "we are fixing it".
    pub fn begin_rebuild(&mut self) {
        self.status = ReconstructionStatus::Rebuilding;
    }

    /// Recovery, after the rebuilt state's fingerprint has been verified.
    ///
    /// Takes the sequence the rebuild is anchored at so the caller cannot
    /// declare recovery without saying from where. Gap history is deliberately
    /// NOT reset: the counters are the record that this feed lost data, and
    /// that record outlives the incident.
    pub fn rebuild_verified(&mut self, anchored_at: u64) {
        self.last_sequence = Some(anchored_at);
        self.expected_sequence = Some(anchored_at + 1);
        self.status = ReconstructionStatus::Verified;
    }

    pub const fn source(&self) -> FeedSourceId { self.source }
    pub const fn status(&self) -> ReconstructionStatus { self.status }
    pub const fn gap_count(&self) -> u64 { self.gap_count }
    pub const fn missing_count(&self) -> u64 { self.missing_count }
    pub const fn duplicate_count(&self) -> u64 { self.duplicate_count }
    pub const fn out_of_order_count(&self) -> u64 { self.out_of_order_count }
    pub const fn reconnect_count(&self) -> u64 { self.reconnect_count }
    pub const fn last_sequence(&self) -> Option<u64> { self.last_sequence }
}
