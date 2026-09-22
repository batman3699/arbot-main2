//! Position of an observation in a chain's total order (Blueprint §5.2).
//!
//! Extends the legacy `continuity::Ordinal` with the fields Base's
//! preconfirmation stream needs. That module's doc comment anticipated the
//! change:
//!
//! > "When flashblocks land this gains `payload_id` and `flashblock_index`
//! >  ahead of `block`; the state machine below does not change."
//!
//! **It is followed for `flashblock_index` and NOT for `payload_id`**, and the
//! difference is not cosmetic. Placing `payload_id` above `block` makes payload
//! identity dominate block ordering, so an anchor read in block 100 sorts after
//! an observation in block 101 -- the total order breaks across blocks entirely.
//! A first draft here did exactly that and the anchor test caught it.
//!
//! `block` is therefore the coarsest key. `payload_id` is an IDENTITY, not an
//! ordering key: two payloads for the same block are ordered by
//! `flashblock_index`, and the id only breaks exact ties.
//!
//! **Field order IS the comparison order.** `derive(Ord)` compares
//! lexicographically top to bottom. Reordering these to group them prettily
//! silently changes what "earlier" means, so don't.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Ordinal {
    /// Coarsest key. Dominates everything below it.
    pub block: u64,
    /// Orders observations within one block before it is sealed. A confirmed
    /// observation uses 0; an anchor read uses `u32::MAX` so it sorts last
    /// within its block.
    pub flashblock_index: u32,
    pub tx_index: u64,
    pub log_index: u64,
    /// Identity of the flashblock payload, NOT an ordering key -- it only
    /// breaks exact ties. See the module docs.
    pub payload_id: u64,
}

impl Ordinal {
    /// A confirmed-block position, with no preconfirmation context.
    ///
    /// Zeroes sort first, so a confirmed observation of a block orders before
    /// any flashblock-tagged observation of a LATER block, and the legacy
    /// (block, tx_index, log_index) ordering is preserved exactly among
    /// confirmed observations.
    pub const fn confirmed(block: u64, tx_index: u64, log_index: u64) -> Self {
        Self { block, flashblock_index: 0, tx_index, log_index, payload_id: 0 }
    }

    /// A position observed inside a flashblock payload.
    pub const fn preconfirmed(
        payload_id: u64,
        flashblock_index: u32,
        block: u64,
        tx_index: u64,
        log_index: u64,
    ) -> Self {
        Self { block, flashblock_index, tx_index, log_index, payload_id }
    }

    /// An `eth_call` read has no position in the log stream, so it is placed at
    /// the end of its block.
    ///
    /// Carried over from the legacy `Ordinal::end_of_block`, which exists
    /// because an anchor read at HEAD cannot be ordered against in-flight logs
    /// any other way, and ordering it earlier would let a delta be applied on
    /// top of an anchor that already contained it.
    pub const fn end_of_block(block: u64) -> Self {
        Self {
            block,
            flashblock_index: u32::MAX,
            tx_index: u64::MAX,
            log_index: u64::MAX,
            payload_id: u64::MAX,
        }
    }

    pub const fn is_preconfirmed(&self) -> bool {
        self.payload_id != 0 && self.payload_id != u64::MAX
    }
}
