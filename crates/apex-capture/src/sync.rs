//! The concurrency primitives, swapped for `loom`'s under `--cfg loom`.
//!
//! `tests/nonce_loom.rs` asserts that no interleaving of two threads can hand
//! the same nonce out twice. loom can only see that if the types it is
//! exploring are its own, so every `Mutex` on the signer path comes from here
//! rather than from `std` directly. The shim is three lines and it is the whole
//! price of admission.

#[cfg(loom)]
pub(crate) use loom::sync::{Mutex, MutexGuard};

#[cfg(not(loom))]
pub(crate) use std::sync::{Mutex, MutexGuard};

/// A poisoned lock recovers rather than propagating -- see the note on
/// `TicketRegistry::lock`, and `no_runtime_panics.sh`, which forbids the
/// `.expect()` that would otherwise go here.
pub(crate) fn recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    #[cfg(loom)]
    {
        // loom's Mutex does not poison; its LockResult is infallible in
        // practice, and `unwrap_or_else` on it would not compile the same way.
        match m.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        }
    }
    #[cfg(not(loom))]
    {
        match m.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
