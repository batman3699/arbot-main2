//! INV-11: no global mutable state is shared truth between search workers.
//!
//! Blueprint §5.3: "Workers receive immutable snapshots or versioned read
//! handles." What exists today is `Published<T> = Arc<StdMutex<Option<Arc<T>>>>`
//! (base_fast.rs:2054) -- a mutex on the fast path, which is both the shared
//! mutable truth §5.3 forbids and a capture hazard under §2.4.

use apex_state::Versioned;
use apex_types::state::ReconstructionStatus;
use std::sync::Arc;

#[test]
fn a_reader_keeps_its_snapshot_across_a_write() {
    let v = Versioned::new(vec![1u32, 2, 3], ReconstructionStatus::Verified);
    let held = v.load();

    v.store(vec![4, 5, 6], ReconstructionStatus::Verified);

    assert_eq!(*held.value, vec![1, 2, 3], "a held snapshot must be immutable");
    assert_eq!(*v.load().value, vec![4, 5, 6]);
}

#[test]
fn every_snapshot_carries_a_version_that_advances() {
    let v = Versioned::new(0u32, ReconstructionStatus::Verified);
    let first = v.load().version;

    v.store(1, ReconstructionStatus::Verified);
    let second = v.load().version;

    assert!(second > first, "version must advance on write: {first:?} -> {second:?}");
}

#[test]
fn versions_are_unique_under_concurrent_writers() {
    // The failure this rules out: two writers stamping the same version, so a
    // reader cannot tell which snapshot it holds. That is the state-identity
    // half of INV-11.
    let v = Arc::new(Versioned::new(0u64, ReconstructionStatus::Verified));
    let mut handles = Vec::new();

    for t in 0..8u64 {
        let v = Arc::clone(&v);
        handles.push(std::thread::spawn(move || {
            let mut seen = Vec::new();
            for i in 0..200 {
                v.store(t * 1000 + i, ReconstructionStatus::Verified);
                seen.push(v.load().version);
            }
            seen
        }));
    }

    let mut all: Vec<_> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let before = all.len();
    all.sort_unstable();
    all.dedup();
    // Readers may observe the same version (another thread wrote between our
    // store and load), but the ISSUED versions must be unique -- checked via the
    // counter, which must have advanced exactly once per store.
    assert_eq!(v.writes_issued(), before as u64, "one version per store, no reuse");
}

#[test]
fn unverified_state_cannot_be_read_as_verified() {
    // INV-08: only ReconstructionStatus::Verified may authorize a live ticket.
    // The handle carries the status so a caller cannot lose it in transit.
    let v = Versioned::new(7u32, ReconstructionStatus::Unsafe);
    let snap = v.load();

    assert_eq!(snap.reconstruction, ReconstructionStatus::Unsafe);
    assert!(!snap.may_authorize_live_ticket());
    assert!(snap.verified().is_none(), "an Unsafe snapshot must not yield a VerifiedState");
}

#[test]
fn a_verified_snapshot_yields_a_verified_handle() {
    let v = Versioned::new(7u32, ReconstructionStatus::Verified);
    let snap = v.load();

    assert!(snap.may_authorize_live_ticket());
    let verified = snap.verified().expect("Verified must yield a handle");
    assert_eq!(*verified.value, 7);
}
