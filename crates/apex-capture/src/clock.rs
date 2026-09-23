//! Time, injected rather than read.
//!
//! The registry stamps every terminal outcome with a time and closes tickets
//! whose deadline has passed (INV-03). Both are untestable against
//! `SystemTime::now` -- a deadline test would have to sleep, and a sleeping test
//! is a slow test that still races. So the clock is a parameter.

use apex_types::time::UnixNanos;
use std::sync::atomic::{AtomicU64, Ordering};

pub trait Clock: Send + Sync {
    fn now(&self) -> UnixNanos;
}

/// Production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixNanos {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // Saturating rather than wrapping: a clock that reads backwards is a
        // problem to surface, not one to silently alias into a plausible time.
        UnixNanos(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }
}

/// Tests. Monotonic by construction -- it can only be advanced.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn at(nanos: u64) -> Self {
        Self(AtomicU64::new(nanos))
    }
    pub fn advance(&self, nanos: u64) {
        self.0.fetch_add(nanos, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> UnixNanos {
        UnixNanos(self.0.load(Ordering::SeqCst))
    }
}
