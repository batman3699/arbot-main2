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
pub fn select(ordinal: Option<Ordinal>, head_block: u64) -> SelectOutcome {
    let Some(o) = ordinal else {
        return SelectOutcome::NoOrdinal;
    };
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
            select(ord(1_000), 1_050),
            SelectOutcome::Check { block: 1_000 }
        );
    }

    #[test]
    fn a_snapshot_at_the_head_is_checked_at_the_head() {
        assert_eq!(
            select(ord(1_050), 1_050),
            SelectOutcome::Check { block: 1_050 }
        );
    }

    /// An anchored snapshot came from RPC, not a log — there is nothing to
    /// validate and no ordinal to pin to.
    #[test]
    fn a_snapshot_without_an_ordinal_is_not_checkable() {
        assert_eq!(select(None, 1_050), SelectOutcome::NoOrdinal);
    }

    /// Reading far-back state needs deep archive. Decline rather than depend
    /// on it.
    #[test]
    fn a_snapshot_older_than_the_lag_budget_is_skipped() {
        let head = 10_000;
        let old = head - MAX_VALIDATION_LAG_BLOCKS - 1;
        assert_eq!(
            select(ord(old), head),
            SelectOutcome::TooOld {
                lag: MAX_VALIDATION_LAG_BLOCKS + 1
            }
        );
    }

    #[test]
    fn a_snapshot_exactly_at_the_lag_budget_is_still_checked() {
        let head = 10_000;
        let edge = head - MAX_VALIDATION_LAG_BLOCKS;
        assert_eq!(select(ord(edge), head), SelectOutcome::Check { block: edge });
    }

    /// A snapshot ahead of our notion of the head is not an error to divide by:
    /// heads and logs arrive on different sockets and can race.
    #[test]
    fn a_snapshot_ahead_of_the_head_is_checked_not_rejected() {
        assert_eq!(
            select(ord(1_051), 1_050),
            SelectOutcome::Check { block: 1_051 }
        );
    }
}
