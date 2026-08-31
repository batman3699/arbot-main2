//! Which snapshots can be validated, and at which block.
//!
//! Split out from the validation task so the block-pinning rule is testable
//! without a provider. Getting it wrong does not fail loudly — it measures
//! market movement and calls it decoder divergence.

use crate::continuity::Ordinal;

/// How far back a snapshot may be and still be validated.
///
/// Reading state at an old block needs archive access; 128 blocks is ~4 minutes
/// on Base, comfortably inside any provider's recent-state window.
pub const MAX_VALIDATION_LAG_BLOCKS: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectOutcome {
    /// Validate, reading chain state AT THIS BLOCK — never at the head.
    Check { block: u64 },
    /// Anchored snapshot: came from RPC, nothing to compare.
    NoOrdinal,
    /// The snapshot's block may still receive more logs for this pool, so the
    /// snapshot could be a MID-block state. Comparing it against `eth_call`,
    /// which returns END-of-block state, would compare different instants.
    Unsettled,
    TooOld { lag: u64 },
}

/// Decide whether a snapshot can be validated, and at which block.
///
/// The returned block is the SNAPSHOT'S OWN block, not the chain head. A
/// log-derived snapshot describes the pool at the moment its log was emitted;
/// comparing it against current state measures how far the pool has moved
/// since, which is market activity, not decoder error. That mistake produces a
/// plausible number and no error, so it is enforced here rather than left to
/// the caller's discipline.
#[allow(dead_code)]
/// `settled_through` is the highest block from which a log has been APPLIED.
/// A snapshot at block N is comparable only once that exceeds N: the cursor
/// guarantees ordering, so seeing block N+1 means every block-N log was
/// delivered and our snapshot is the end-of-block state for that pool.
pub fn select(ordinal: Option<Ordinal>, head_block: u64, settled_through: u64) -> SelectOutcome {
    let Some(o) = ordinal else {
        return SelectOutcome::NoOrdinal;
    };
    // Checked before the lag budget so an unsettled snapshot is never
    // mislabelled as merely stale.
    if o.block >= settled_through {
        return SelectOutcome::Unsettled;
    }
    // Saturating: the log socket and the head socket race, so a snapshot can
    // legitimately be ahead of our head. That is not an error.
    let lag = head_block.saturating_sub(o.block);
    if lag > MAX_VALIDATION_LAG_BLOCKS {
        return SelectOutcome::TooOld { lag };
    }
    SelectOutcome::Check { block: o.block }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuity::Ordinal;

    fn ord(block: u64) -> Option<Ordinal> {
        Some(Ordinal {
            block,
            tx_index: 0,
            log_index: 0,
        })
    }

    /// The read MUST be pinned to the snapshot's own block. Comparing against
    /// `latest` measures how much the pool moved since, not whether the
    /// decoder is right.
    #[test]
    fn selects_the_snapshots_own_block_not_the_head() {
        assert_eq!(
            select(ord(1_000), 1_050, 1_050),
            SelectOutcome::Check { block: 1_000 }
        );
    }

    /// Previously this asserted a snapshot at the head was checkable. It is
    /// not: while its own block is the newest one we have applied, more logs
    /// from that block may still arrive, so the snapshot may be a mid-block
    /// state. It becomes comparable one block later.
    #[test]
    fn a_snapshot_at_the_settled_frontier_waits_one_block() {
        assert_eq!(select(ord(1_050), 1_050, 1_050), SelectOutcome::Unsettled);
        assert_eq!(
            select(ord(1_050), 1_051, 1_051),
            SelectOutcome::Check { block: 1_050 }
        );
    }

    /// An anchored snapshot came from RPC, not a log — there is nothing to
    /// validate and no ordinal to pin to.
    /// THE FIX. A snapshot is comparable only once its block is SETTLED --
    /// meaning we have applied a log from a strictly later block, so the
    /// cursor's ordering guarantee says every log in the snapshot's block was
    /// delivered and our snapshot is the end-of-block state for that pool.
    ///
    /// Without this the validator compares a MID-block snapshot against
    /// end-of-block chain state. Proven live: for pool 0xb5f0b4ae66 at block
    /// 50679816 our snapshot held the state after event 2 of 4 while eth_call
    /// returned the state after event 4 -- both correct, different instants.
    /// Replay reconstructed the chain value exactly, so the arithmetic was
    /// never wrong; the comparison was.
    #[test]
    fn a_snapshot_in_an_unsettled_block_is_not_comparable() {
        // settled_through == the snapshot's own block: more logs from that
        // block may still arrive.
        assert_eq!(select(ord(1_000), 1_050, 1_000), SelectOutcome::Unsettled);
    }

    #[test]
    fn a_snapshot_becomes_comparable_once_a_later_block_is_applied() {
        assert_eq!(
            select(ord(1_000), 1_050, 1_001),
            SelectOutcome::Check { block: 1_000 }
        );
    }

    /// Fails closed at startup: nothing applied yet means nothing is settled.
    #[test]
    fn nothing_is_settled_before_any_log_is_applied() {
        assert_eq!(select(ord(1_000), 1_050, 0), SelectOutcome::Unsettled);
    }

    /// Settledness is checked BEFORE the lag budget, so an unsettled snapshot
    /// is never mislabelled as merely stale.
    #[test]
    fn unsettled_outranks_too_old() {
        let head = 10_000;
        let old = head - MAX_VALIDATION_LAG_BLOCKS - 1;
        assert_eq!(select(ord(old), head, old), SelectOutcome::Unsettled);
    }

    #[test]
    fn a_snapshot_without_an_ordinal_is_not_checkable() {
        assert_eq!(select(None, 1_050, 1_050), SelectOutcome::NoOrdinal);
    }

    /// Reading far-back state needs deep archive. Decline rather than depend
    /// on it.
    #[test]
    fn a_snapshot_older_than_the_lag_budget_is_skipped() {
        let head = 10_000;
        let old = head - MAX_VALIDATION_LAG_BLOCKS - 1;
        assert_eq!(
            select(ord(old), head, head),
            SelectOutcome::TooOld {
                lag: MAX_VALIDATION_LAG_BLOCKS + 1
            }
        );
    }

    #[test]
    fn a_snapshot_exactly_at_the_lag_budget_is_still_checked() {
        let head = 10_000;
        let edge = head - MAX_VALIDATION_LAG_BLOCKS;
        assert_eq!(select(ord(edge), head, head), SelectOutcome::Check { block: edge });
    }

    /// A snapshot ahead of our notion of the head is not an error — heads and
    /// logs arrive on different sockets and race. It is simply not settled yet,
    /// which is a "come back later", not a rejection.
    #[test]
    fn a_snapshot_ahead_of_the_head_is_unsettled_not_rejected() {
        assert_eq!(select(ord(1_051), 1_050, 1_050), SelectOutcome::Unsettled);
    }
}
