//! Ordering state machine for the log stream.
//!
//! Detects duplicates, backwards movement and reorgs. It deliberately does NOT
//! detect MISSING logs: the subscription is filtered, so consecutive
//! `log_index` values are not expected and a gap carries no information. A
//! dropped log is caught downstream by `state_gate` divergence, not here — see
//! spec §8 I1, whose lossless claim covers the dirty set, not delivery.

/// Position of a log in the chain's total order.
///
/// Field order IS the comparison order — `derive(Ord)` compares block, then
/// tx_index, then log_index, which is exactly the lexicographic rule spec §4.4
/// specifies. When flashblocks land this gains `payload_id` and
/// `flashblock_index` ahead of `block`; the state machine below does not change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ordinal {
    pub block: u64,
    pub tx_index: u64,
    pub log_index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakReason {
    OutOfOrder,
    Reorg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Accept,
    Duplicate,
    Break(BreakReason),
}

#[derive(Debug, Default)]
pub struct Cursor {
    last: Option<Ordinal>,
}

// main.rs compiles its own copy of this module.
#[allow(dead_code)]
impl Cursor {
    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn last(&self) -> Option<Ordinal> {
        self.last
    }

    /// Classify an incoming log and advance the cursor.
    ///
    /// A break clears the baseline: after a reorg or a backwards jump the old
    /// position describes a chain state that no longer exists, so comparing
    /// against it would reject every subsequent log.
    pub fn observe(&mut self, ordinal: Ordinal, removed: bool) -> Observation {
        if removed {
            self.last = None;
            return Observation::Break(BreakReason::Reorg);
        }
        match self.last {
            None => {
                self.last = Some(ordinal);
                Observation::Accept
            }
            Some(prev) if ordinal > prev => {
                self.last = Some(ordinal);
                Observation::Accept
            }
            Some(prev) if ordinal == prev => Observation::Duplicate,
            Some(_) => {
                self.last = None;
                Observation::Break(BreakReason::OutOfOrder)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ord(b: u64, t: u64, l: u64) -> Ordinal {
        Ordinal {
            block: b,
            tx_index: t,
            log_index: l,
        }
    }

    #[test]
    fn ordinals_compare_lexicographically() {
        assert!(ord(1, 0, 0) < ord(1, 0, 1));
        assert!(ord(1, 0, 9) < ord(1, 1, 0), "tx_index outranks log_index");
        assert!(ord(1, 9, 9) < ord(2, 0, 0), "block outranks everything");
    }

    #[test]
    fn the_first_observation_is_always_accepted() {
        let mut c = Cursor::new();
        assert_eq!(c.observe(ord(10, 0, 0), false), Observation::Accept);
        assert_eq!(c.last(), Some(ord(10, 0, 0)));
    }

    #[test]
    fn strictly_greater_advances() {
        let mut c = Cursor::new();
        c.observe(ord(10, 0, 0), false);
        assert_eq!(c.observe(ord(10, 0, 1), false), Observation::Accept);
        assert_eq!(c.observe(ord(11, 0, 0), false), Observation::Accept);
    }

    /// A replayed log must change nothing at all — no version bump, no dirty
    /// mark. Idempotence is what makes reconnect-and-replay safe.
    #[test]
    fn an_equal_ordinal_is_a_duplicate() {
        let mut c = Cursor::new();
        c.observe(ord(10, 2, 3), false);
        assert_eq!(c.observe(ord(10, 2, 3), false), Observation::Duplicate);
        assert_eq!(c.last(), Some(ord(10, 2, 3)), "cursor must not move");
    }

    #[test]
    fn a_lower_ordinal_breaks_continuity() {
        let mut c = Cursor::new();
        c.observe(ord(10, 2, 3), false);
        assert_eq!(
            c.observe(ord(10, 2, 2), false),
            Observation::Break(BreakReason::OutOfOrder)
        );
    }

    /// A dropped preconfirmation and a reorg are the same signal.
    #[test]
    fn a_removed_log_breaks_continuity_even_when_ordered() {
        let mut c = Cursor::new();
        c.observe(ord(10, 0, 0), false);
        assert_eq!(
            c.observe(ord(11, 0, 0), true),
            Observation::Break(BreakReason::Reorg)
        );
    }

    /// After a break the cursor must re-baseline, or every subsequent log
    /// compares against a position the chain has abandoned.
    #[test]
    fn a_break_resets_the_baseline() {
        let mut c = Cursor::new();
        c.observe(ord(10, 5, 5), false);
        c.observe(ord(9, 0, 0), false);
        assert_eq!(c.last(), None, "break clears the cursor");
        assert_eq!(c.observe(ord(9, 0, 1), false), Observation::Accept);
    }
}
