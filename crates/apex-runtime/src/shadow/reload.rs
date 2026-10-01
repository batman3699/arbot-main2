//! The reload task's queue (R9, from what R3 learned).
//!
//! **One reload at a time, requests coalesced.** The feed asks for reloads —
//! a pool whose swap left its ladder, a `Mint` or `Burn`, a reorged log, a gap
//! — far faster than a reload completes, and the book refuses a read older
//! than one it holds, so concurrent reloads would race each other into
//! `ReloadError::Older`. Requests therefore accumulate here and the reload task
//! takes them all at once: a full request swallows any pool requests beside
//! it, and pool requests merge into one set.

use alloy_primitives::Address;
use std::collections::BTreeSet;
use std::sync::Mutex;
use tokio::sync::Notify;

/// One reload's worth of work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Work {
    /// Every pool the book was built over.
    Full,
    Pools(Vec<Address>),
}

#[derive(Debug, Default)]
struct Pending {
    full: bool,
    pools: BTreeSet<Address>,
}

#[derive(Debug, Default)]
pub struct ReloadQueue {
    pending: Mutex<Pending>,
    wake: Notify,
}

impl ReloadQueue {
    fn with<R>(&self, f: impl FnOnce(&mut Pending) -> R) -> R {
        let mut g = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut g)
    }

    pub fn request(&self, pools: impl IntoIterator<Item = Address>) {
        self.with(|p| p.pools.extend(pools));
        self.wake.notify_one();
    }

    pub fn request_full(&self) {
        self.with(|p| p.full = true);
        self.wake.notify_one();
    }

    /// Everything requested since the last take, as one reload.
    pub fn take(&self) -> Option<Work> {
        self.with(|p| {
            let pending = std::mem::take(p);
            if pending.full {
                Some(Work::Full)
            } else if pending.pools.is_empty() {
                None
            } else {
                Some(Work::Pools(pending.pools.into_iter().collect()))
            }
        })
    }

    /// Wait for work. A request made between a `take` that found nothing and
    /// this wait is not lost: `Notify` keeps one permit.
    pub async fn next(&self) -> Work {
        loop {
            if let Some(w) = self.take() {
                return w;
            }
            self.wake.notified().await;
        }
    }
}
