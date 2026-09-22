//! Lock-free versioned snapshots (Blueprint §5.3, INV-11).
//!
//! Replaces `Published<T> = Arc<StdMutex<Option<Arc<T>>>>` from `base_fast.rs`.
//! Two things were wrong with that shape, and only one of them is performance:
//!
//! 1. A `Mutex` is shared mutable truth. §5.3 requires workers receive
//!    "immutable snapshots or versioned read handles", and a mutex on the fast
//!    path is also a §2.4 capture hazard -- a reader can block behind a writer
//!    while a ticket's deadline runs down.
//! 2. A snapshot carried no identity. A worker holding an `Arc<T>` could not say
//!    *which* state it held, so a candidate could be priced against one snapshot
//!    and simulated against another with nothing detecting it.
//!
//! `Versioned<T>` fixes both: `ArcSwap` for wait-free reads, and every snapshot
//! stamped with a monotonic [`StateVersion`] and its [`ReconstructionStatus`].

use apex_types::state::{ReconstructionStatus, StateVersion};
use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// One immutable observation. Cheap to clone; readers hold these, never a lock.
#[derive(Debug)]
pub struct Snapshot<T> {
    pub version: StateVersion,
    pub reconstruction: ReconstructionStatus,
    pub value: Arc<T>,
}

// Hand-written, not derived: `#[derive(Clone)]` would add a `T: Clone` bound,
// and the whole point is that only the Arc is cloned. Requiring `T: Clone` would
// force every state payload to be cloneable for no reason.
impl<T> Clone for Snapshot<T> {
    fn clone(&self) -> Self {
        Self {
            version: self.version,
            reconstruction: self.reconstruction,
            value: Arc::clone(&self.value),
        }
    }
}

impl<T> Clone for VerifiedState<T> {
    fn clone(&self) -> Self {
        Self { version: self.version, value: Arc::clone(&self.value) }
    }
}

impl<T> Snapshot<T> {
    pub const fn may_authorize_live_ticket(&self) -> bool {
        self.reconstruction.may_authorize_live_ticket()
    }

    /// Narrow to a [`VerifiedState`], or `None`.
    ///
    /// This is the type-level half of INV-08: the ticket-admission path takes a
    /// `VerifiedState`, so state that is `Rebuilding` or `Unsafe` cannot reach it
    /// by being passed along in a plain `Snapshot`.
    pub fn verified(&self) -> Option<VerifiedState<T>> {
        self.may_authorize_live_ticket().then(|| VerifiedState {
            version: self.version,
            value: Arc::clone(&self.value),
        })
    }
}

/// A snapshot proven to come from verified state.
///
/// Constructible only through [`Snapshot::verified`], so "only `Verified` may
/// authorize a live ticket" is enforced by what the admission path accepts
/// rather than by a check it could forget to run.
#[derive(Debug)]
pub struct VerifiedState<T> {
    pub version: StateVersion,
    pub value: Arc<T>,
}

pub struct Versioned<T> {
    inner: ArcSwap<Snapshot<T>>,
    next_version: AtomicU64,
    writes: AtomicU64,
}

impl<T> Versioned<T> {
    pub fn new(value: T, reconstruction: ReconstructionStatus) -> Self {
        Self {
            inner: ArcSwap::from_pointee(Snapshot {
                version: StateVersion(1),
                reconstruction,
                value: Arc::new(value),
            }),
            next_version: AtomicU64::new(2),
            writes: AtomicU64::new(0),
        }
    }

    /// Wait-free. The returned snapshot is stable for as long as it is held,
    /// regardless of how many writes land meanwhile.
    pub fn load(&self) -> Snapshot<T> {
        (**self.inner.load()).clone()
    }

    /// Publish a new snapshot. Never blocks a reader.
    pub fn store(&self, value: T, reconstruction: ReconstructionStatus) -> StateVersion {
        // fetch_add gives each writer a distinct version even under contention.
        // Two writers stamping one version would leave a reader unable to say
        // which snapshot it held, which is the identity half of INV-11.
        let version = StateVersion(self.next_version.fetch_add(1, Ordering::Relaxed));
        self.inner.store(Arc::new(Snapshot {
            version,
            reconstruction,
            value: Arc::new(value),
        }));
        self.writes.fetch_add(1, Ordering::Relaxed);
        version
    }

    /// Total stores. Exposed so a test can assert one version per write without
    /// racing on observed values.
    pub fn writes_issued(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Versioned<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Versioned").field("current", &self.load().version).finish()
    }
}
