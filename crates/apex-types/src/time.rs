//! Time types.
//!
//! Deliberately NOT `std::time::Instant`. PLAN.md §7 sketched `Instant` for
//! deadlines and that is wrong for these types: `Instant` is monotonic but
//! process-local and not serialisable, and every deadline here has to survive a
//! `SIGKILL` and be read back by the recovery path (§16.8, INV-39). A ticket
//! whose `dispatch_deadline` cannot be persisted cannot be reconciled.
//!
//! So: wall-clock unix nanoseconds for anything that is written down, and the
//! monotonic clock kept separately by `apex-capture` for in-process deadline
//! arithmetic. The two must not be conflated -- wall-clock can step backwards.

use serde::{Deserialize, Serialize};

/// Wall-clock nanoseconds since the unix epoch. Serialisable, comparable across
/// processes, and therefore the only timestamp that belongs in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnixNanos(pub u64);

impl UnixNanos {
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Saturating, because a deadline in the past must read as zero remaining,
    /// never wrap to a huge duration.
    pub const fn saturating_since(self, earlier: Self) -> DurationNanos {
        DurationNanos(self.0.saturating_sub(earlier.0))
    }
}

/// A span in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DurationNanos(pub u64);

impl DurationNanos {
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms.saturating_mul(1_000_000))
    }

    pub const fn as_millis(self) -> u64 {
        self.0 / 1_000_000
    }
}
