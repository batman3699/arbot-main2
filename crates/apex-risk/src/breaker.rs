//! The circuit breaker (§25.1). **Reimplemented, not moved — and the
//! reimplementation is differential-tested against the original.**
//!
//! The plan says MOVE `main.rs`'s `CircuitBreaker` with its tests. Measured
//! first, as the migration rule requires: the legacy type is `async fn`s over
//! `tokio::sync::Mutex`, keyed on `std::time::Instant`, denominated in `ethers`
//! `U256`, and it reads five thresholds from the process environment in
//! `configure_health_from_env`. Three structs in `main.rs` hold one. Moving it
//! would drag `tokio`, `ethers` and an env reader into a crate whose entire
//! purpose is to be pure and testable — which is what the split exists to
//! remove, not to relocate.
//!
//! So this is the fresh replacement, and
//! `arb-exec-legacy/tests/breaker_differential.rs` drives both with the same
//! event sequence and asserts they trip identically. That is a stronger claim
//! than a move would have made: a move preserves the code, a differential
//! preserves the *behaviour* and says so in a test that fails if either side
//! drifts.
//!
//! Three deliberate differences, all outside the trip decision:
//!
//! 1. **Time is a parameter.** The legacy version reads `Instant::now()` inside
//!    every method, so its window logic can only be tested by sleeping.
//! 2. **No environment.** Thresholds are constructor arguments.
//!    `configure_health_from_env` stays in the legacy binary, where reading the
//!    environment is already the established pattern.
//! 3. **Not async.** Nothing here awaits; the locks existed to make an
//!    `async fn` sound, and there is no `async fn`.
//!
//! The calibration comments are the valuable part of the original and they
//! migrate verbatim.

use alloy_primitives::U256;
use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::VecDeque;

pub const HOUR: DurationNanos = DurationNanos(3_600_000_000_000);
pub const DAY: DurationNanos = DurationNanos(86_400_000_000_000);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakerStatus {
    pub is_tripped: bool,
    pub reason: Option<String>,
    pub hourly_loss_wei: U256,
    pub daily_loss_wei: U256,
    pub consecutive_failures: u32,
}

impl BreakerStatus {
    pub fn active_reason(&self) -> String {
        self.reason.clone().unwrap_or_else(|| "circuit breaker limits exceeded".to_string())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CircuitBreaker {
    hourly_loss_limit: U256,
    daily_loss_limit: U256,
    consecutive_fail_limit: u32,
    hourly_losses: VecDeque<(UnixNanos, U256)>,
    daily_losses: VecDeque<(UnixNanos, U256)>,
    consecutive_failures: u32,
    revert_window: VecDeque<(UnixNanos, bool)>,
    revert_window_dur: DurationNanos,
    revert_rate_limit: f64,
    revert_min_samples: usize,
    rpc_errors: VecDeque<UnixNanos>,
    rpc_error_window: DurationNanos,
    rpc_error_limit: usize,
}

impl CircuitBreaker {
    pub fn new(hourly_loss_limit: U256, daily_loss_limit: U256, consecutive_fail_limit: u32) -> Self {
        Self {
            hourly_loss_limit,
            daily_loss_limit,
            consecutive_fail_limit,
            hourly_losses: VecDeque::new(),
            daily_losses: VecDeque::new(),
            consecutive_failures: 0,
            // Calibrated for a BACKRUNNING workload, where a 55-65% revert rate
            // is the normal steady state, not a fault. Losing the race and
            // reverting is the executor working correctly -- it costs gas instead
            // of filling at a loss -- so the breaker must tolerate that band and
            // still catch a genuinely broken deploy.
            //
            // The previous defaults were not merely tight, they were INVERTED:
            // a 0.5 rate limit sits BELOW the expected 0.55-0.65 band, so normal
            // operation tripped the breaker permanently.
            //
            // rate limit 0.90 with 50 samples: at p=0.65 the sampling sigma is
            // sqrt(0.65*0.35/50) = 0.0675, so 0.90 is 3.7 sigma out -- a false
            // trip roughly once in 9,000 windows. A broken deploy reverting
            // 100% trips as soon as 50 samples accumulate.
            //
            // A 1800s window (was 600s) is needed for 50 samples to accumulate
            // at realistic fill rates; without that the rate trigger would never
            // arm, which is a fail-OPEN. Fast detection is the consecutive-
            // failure trigger's job, not this one's.
            revert_window: VecDeque::new(),
            revert_window_dur: DurationNanos(1_800_000_000_000),
            revert_rate_limit: 0.90,
            revert_min_samples: 50,
            rpc_errors: VecDeque::new(),
            rpc_error_window: DurationNanos(120_000_000_000),
            rpc_error_limit: 30,
        }
    }

    /// The five thresholds `configure_health_from_env` sets in the legacy
    /// binary. Arguments here; the environment read stays where reading the
    /// environment already lives.
    pub fn with_health_thresholds(
        mut self,
        revert_rate_limit: f64,
        revert_min_samples: usize,
        revert_window: DurationNanos,
        rpc_error_limit: usize,
        rpc_error_window: DurationNanos,
    ) -> Self {
        self.revert_rate_limit = revert_rate_limit;
        self.revert_min_samples = revert_min_samples.max(1);
        self.revert_window_dur = DurationNanos(revert_window.0.max(1));
        self.rpc_error_limit = rpc_error_limit;
        self.rpc_error_window = DurationNanos(rpc_error_window.0.max(1));
        self
    }

    /// Record an execution outcome (true = on-chain revert/failed inclusion).
    pub fn record_execution_outcome(&mut self, reverted: bool, now: UnixNanos) {
        self.revert_window.push_back((now, reverted));
        prune(&mut self.revert_window, now, self.revert_window_dur, |e| e.0);
    }

    /// Record a runtime RPC failure for the RPC-lag trigger.
    pub fn record_rpc_error(&mut self, now: UnixNanos) {
        self.rpc_errors.push_back(now);
        prune(&mut self.rpc_errors, now, self.rpc_error_window, |e| *e);
    }

    pub fn record_failure(&mut self, loss: U256, now: UnixNanos) -> BreakerStatus {
        self.hourly_losses.push_back((now, loss));
        prune(&mut self.hourly_losses, now, HOUR, |e| e.0);
        self.daily_losses.push_back((now, loss));
        prune(&mut self.daily_losses, now, DAY, |e| e.0);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.current_status(now)
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    pub fn reset(&mut self, now: UnixNanos) -> BreakerStatus {
        self.hourly_losses.clear();
        self.daily_losses.clear();
        self.consecutive_failures = 0;
        self.revert_window.clear();
        self.rpc_errors.clear();
        self.current_status(now)
    }

    pub fn current_status(&mut self, now: UnixNanos) -> BreakerStatus {
        prune(&mut self.hourly_losses, now, HOUR, |e| e.0);
        prune(&mut self.daily_losses, now, DAY, |e| e.0);
        prune(&mut self.revert_window, now, self.revert_window_dur, |e| e.0);
        prune(&mut self.rpc_errors, now, self.rpc_error_window, |e| *e);

        let hourly_total = sum(&self.hourly_losses);
        let daily_total = sum(&self.daily_losses);
        let revert_samples = self.revert_window.len();
        let revert_reverts = self.revert_window.iter().filter(|(_, r)| *r).count();
        let rpc_error_count = self.rpc_errors.len();

        let reason = self.evaluate_reason(
            hourly_total,
            daily_total,
            self.consecutive_failures,
            revert_samples,
            revert_reverts,
            rpc_error_count,
        );
        BreakerStatus {
            is_tripped: reason.is_some(),
            reason,
            hourly_loss_wei: hourly_total,
            daily_loss_wei: daily_total,
            consecutive_failures: self.consecutive_failures,
        }
    }

    /// The trip decision, byte for byte in its comparisons and its order.
    ///
    /// Note the asymmetry, which is the original's and is preserved: the loss
    /// and consecutive-failure limits are strictly-greater (`>`), while the
    /// RPC-error limit is greater-or-equal (`>=`). A "fix" that made them
    /// uniform would change when the breaker trips.
    fn evaluate_reason(
        &self,
        hourly_total: U256,
        daily_total: U256,
        consecutive_failures: u32,
        revert_samples: usize,
        revert_reverts: usize,
        rpc_error_count: usize,
    ) -> Option<String> {
        if !self.hourly_loss_limit.is_zero() && hourly_total > self.hourly_loss_limit {
            return Some(format!(
                "hourly loss {} exceeds limit {}",
                hourly_total, self.hourly_loss_limit
            ));
        }
        if !self.daily_loss_limit.is_zero() && daily_total > self.daily_loss_limit {
            return Some(format!(
                "daily loss {} exceeds limit {}",
                daily_total, self.daily_loss_limit
            ));
        }
        if self.consecutive_fail_limit > 0 && consecutive_failures > self.consecutive_fail_limit {
            return Some(format!(
                "consecutive failures {} exceeds limit {}",
                consecutive_failures, self.consecutive_fail_limit
            ));
        }
        if self.revert_rate_limit > 0.0 && revert_samples >= self.revert_min_samples {
            let rate = revert_reverts as f64 / revert_samples as f64;
            if rate > self.revert_rate_limit {
                return Some(format!(
                    "revert rate {:.0}% ({}/{}) exceeds limit {:.0}%",
                    rate * 100.0,
                    revert_reverts,
                    revert_samples,
                    self.revert_rate_limit * 100.0
                ));
            }
        }
        if self.rpc_error_limit > 0 && rpc_error_count >= self.rpc_error_limit {
            return Some(format!(
                "rpc errors {} within {}s window exceed limit {}",
                rpc_error_count,
                self.rpc_error_window.0 / 1_000_000_000,
                self.rpc_error_limit
            ));
        }
        None
    }
}

/// `now - at > window`, matching the original: an entry exactly at the window
/// edge is kept.
fn prune<T>(q: &mut VecDeque<T>, now: UnixNanos, window: DurationNanos, at: impl Fn(&T) -> UnixNanos) {
    while let Some(front) = q.front() {
        if now.0.saturating_sub(at(front).0) > window.0 {
            q.pop_front();
        } else {
            break;
        }
    }
}

fn sum(q: &VecDeque<(UnixNanos, U256)>) -> U256 {
    q.iter().fold(U256::ZERO, |acc, (_, v)| acc.saturating_add(*v))
}
