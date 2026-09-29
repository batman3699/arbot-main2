//! Worker supervision, migrated from the legacy `main.rs:10121`.
//!
//! # The original rationale, preserved
//!
//! > Supervised replacement for fire-and-forget `tokio::spawn` of long-lived
//! > background workers (pool refreshers, mempool monitors, exporters).
//! >
//! > These workers feed the trading path with liquidity/competition data; if one
//! > dies silently the bot keeps trading on progressively staler state. The
//! > supervisor isolates panics in an inner task, logs every exit, bumps the
//! > `worker_restarts_total{chain,worker}` metric, and restarts the worker with
//! > capped exponential backoff (2s..64s) so a persistent fault degrades to a
//! > loud periodic retry instead of a silent feature loss.
//!
//! That reasoning is still correct and is why this function exists rather than a
//! bare `tokio::spawn`. §34's migration note requires it be preserved, and the
//! workspace `Cargo.toml` carries the other half: **`panic` must stay at
//! `unwind`**, because the restart depends on `JoinError::is_panic()`.
//! `panic = "abort"` would turn one worker's panic into a process abort and
//! strand every live ticket in memory — which, after Phase 6, means stranding
//! them in a journal that boot recovery then has to reconcile against the chain.
//!
//! # What changed, and why it had to
//!
//! The legacy version loops forever and leaves only when tokio cancels it. That
//! is adequate for fire-and-forget and useless for an orderly drain: a
//! supervisor that cannot be asked to stop is a process that cannot be asked to
//! stop, and §46.1's shutdown has to reconcile tickets *before* the runtime goes
//! away. So the supervisor takes a [`crate::shutdown::ShutdownSignal`] and races
//! it against both the worker and the backoff sleep — a worker mid-backoff must
//! not hold shutdown open for up to 64 seconds.
//!
//! Restarts and panics are counted separately. "It restarted twice" and "it
//! panicked twice" are the same number only by coincidence; a worker that
//! returns cleanly in a tight loop is a different fault from one that panics,
//! and the legacy single counter could not tell them apart.

use crate::shutdown::ShutdownSignal;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// Why a supervised worker's supervisor returned. There is no `Failed` variant:
/// a failing worker is restarted, so the supervisor only ever leaves on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerExit {
    /// Shutdown was signalled and the worker stopped.
    ShutDown,
    /// The runtime cancelled the supervisor itself — process teardown.
    Cancelled,
}

#[derive(Debug, Default)]
struct Counters {
    restarts: AtomicU64,
    panics: AtomicU64,
}

/// A handle on a supervised worker. Counters stay readable after [`Self::join`]
/// because they are what a post-mortem asks about.
#[derive(Debug)]
pub struct SupervisedHandle {
    counters: Arc<Counters>,
    task: tokio::task::JoinHandle<WorkerExit>,
    worker: &'static str,
}

impl SupervisedHandle {
    pub fn worker(&self) -> &'static str {
        self.worker
    }

    pub fn restarts(&self) -> u64 {
        self.counters.restarts.load(Ordering::SeqCst)
    }

    pub fn panics(&self) -> u64 {
        self.counters.panics.load(Ordering::SeqCst)
    }

    /// Wait for the supervisor to stop. `&mut self` rather than `self` so the
    /// counters can still be read afterwards.
    pub async fn join(&mut self) -> WorkerExit {
        // A supervisor that itself panicked or was cancelled is reported as
        // `Cancelled` rather than unwrapped: this is the shutdown path, and a
        // panic here would take down the drain that is trying to save tickets.
        (&mut self.task).await.unwrap_or(WorkerExit::Cancelled)
    }

    /// Stop waiting and let the runtime drop it. Used when the grace period has
    /// expired and the remaining work is recovery's problem, not shutdown's.
    pub fn abort(&self) {
        self.task.abort();
    }
}

/// Backoff bounds, kept as constants because the legacy comment names them and a
/// reader checking the claim should find the numbers.
const BACKOFF_BASE_SECS: u64 = 2;
const BACKOFF_MAX_SHIFT: u32 = 5; // 2^(1+5) = 64s

fn backoff(restarts: u32) -> Duration {
    Duration::from_secs(BACKOFF_BASE_SECS.saturating_pow(1 + restarts.min(BACKOFF_MAX_SHIFT)))
}

/// Spawn `factory`'s future under supervision, restarting it until shutdown.
///
/// `factory` is `FnMut` because a restart needs a *fresh* future: re-polling a
/// completed one is undefined, and a factory that captured its state by move
/// would hand the restart whatever the failed run left behind.
pub fn spawn_supervised<F, Fut>(
    worker: &'static str,
    chain: String,
    mut shutdown: ShutdownSignal,
    mut factory: F,
) -> SupervisedHandle
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let counters = Arc::new(Counters::default());
    let counters_task = Arc::clone(&counters);

    let task = tokio::spawn(async move {
        loop {
            if shutdown.triggered() {
                return WorkerExit::ShutDown;
            }

            // Inner spawn so a worker panic is contained and observable instead
            // of unwinding this supervisor. This is the line that requires
            // `panic = "unwind"`.
            let mut inner = tokio::spawn(factory());

            let outcome = tokio::select! {
                joined = &mut inner => Some(joined),
                () = shutdown.wait() => None,
            };

            let Some(joined) = outcome else {
                // Shutdown won the race. Stop the worker rather than leaving a
                // detached task writing to a journal the drain is reading.
                inner.abort();
                let _ = inner.await;
                info!(worker, chain = %chain, "supervised worker stopped for shutdown");
                return WorkerExit::ShutDown;
            };

            match joined {
                Ok(()) => warn!(
                    worker,
                    chain = %chain,
                    restarts = counters_task.restarts.load(Ordering::SeqCst),
                    "supervised worker exited unexpectedly; restarting"
                ),
                Err(err) if err.is_panic() => {
                    counters_task.panics.fetch_add(1, Ordering::SeqCst);
                    error!(
                        worker,
                        chain = %chain,
                        restarts = counters_task.restarts.load(Ordering::SeqCst),
                        error = %err,
                        "supervised worker PANICKED; restarting"
                    );
                }
                Err(err) => {
                    info!(worker, chain = %chain, error = %err, "supervised worker cancelled");
                    return WorkerExit::Cancelled;
                }
            }

            let restarts = counters_task.restarts.fetch_add(1, Ordering::SeqCst);
            // The sleep races shutdown too. A worker in its 64-second backoff
            // must not hold the drain open for 64 seconds.
            let delay = backoff(u32::try_from(restarts).unwrap_or(u32::MAX));
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shutdown.wait() => return WorkerExit::ShutDown,
            }
        }
    });

    SupervisedHandle { counters, task, worker }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_capped_at_the_documented_ceiling() {
        assert_eq!(backoff(0), Duration::from_secs(2));
        assert_eq!(backoff(1), Duration::from_secs(4));
        assert_eq!(backoff(5), Duration::from_secs(64));
        // The cap holds however many times it has failed, which is what makes a
        // persistent fault "a loud periodic retry" rather than an ever-quieter
        // one.
        assert_eq!(backoff(u32::MAX), Duration::from_secs(64));
    }
}
