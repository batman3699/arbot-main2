//! Base flashblock fast path: preconfirmed logs into local state.
//!
//! Base seals an L2 block every ~2s but publishes *flashblocks* every ~200ms.
//! The canonical scan loop measured 4.20s median end to end, which is 2x a
//! block and 21x a flashblock — so an opportunity is gone long before the
//! scanner looks at it. This module is the first half of closing that: it
//! consumes preconfirmed logs and marks pools dirty, and does nothing else.
//!
//! # Why `pendingLogs` and not the flashblocks stream
//!
//! `wss://mainnet.flashblocks.base.org/ws` is real, but it is node-operator
//! infrastructure: it pushes Brotli-compressed binary frames with no
//! subscription handshake, so consuming it means a new decompression
//! dependency and a bespoke framing layer.
//!
//! `pendingLogs` is ordinary `eth_subscribe` over the provider websocket we
//! already use. Verified against the configured provider 2026-09-02: **215
//! notifications in 30s across 4 pools, first at 576ms**, carrying the same
//! `topic0` values `log_decode::monitored_topics()` already decodes. It reuses
//! the entire decode and `LiveState` path, which is field-proven, and needs no
//! new crate.
//!
//! Base documents an eventual `Denim` upgrade replacing flashblocks with native
//! 200ms blocks, so the feed sits behind [`FlashFeed`] — the state and search
//! engine above it must survive that migration untouched.
//!
//! # What this module deliberately does NOT do
//!
//! No quoting, no graph work, no RPC, no simulation. Those belong downstream of
//! the dirty set. A websocket task that blocks on an `eth_call` stops draining
//! its socket, and a feed that falls behind is worse than no feed — it reports
//! stale state as fresh.

// Staged, not yet wired: `main.rs` does not route Base through this module
// until the socket task lands, so every item here is dead from the binary's
// view. The allow goes when `spawn` is implemented and Base is routed through
// `BaseFastPath` -- if it is still needed then, something did not get wired.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ethers::types::{Address, Log, H256};
use serde_json::{json, Value};

use crate::ingestion::{idle_action, IdleAction, SUBSCRIPTION_STALL_LIMIT};
use crate::live_state::{ApplyOutcome, LiveState};
use crate::metrics::Metrics;

/// How the fast path receives preconfirmed logs.
///
/// An enum rather than a bare URL so the Denim migration — native 200ms blocks
/// replacing flashblocks — is a variant here and not a rewrite of everything
/// downstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlashFeed {
    /// `eth_subscribe("pendingLogs", ...)` over a flashblocks-capable provider.
    PendingLogs { ws_url: String },
}

impl FlashFeed {
    pub fn ws_url(&self) -> &str {
        match self {
            FlashFeed::PendingLogs { ws_url } => ws_url,
        }
    }
}

/// Subscription parameters for `eth_subscribe`.
///
/// Split out from the task so the filter is testable without a socket. The
/// original pool monitor shipped a filter whose `topic0` values were fabricated:
/// it connected cleanly, logged success, and matched nothing for the life of the
/// process. The topics here come from `monitored_topics()`, which derives every
/// one from its event signature, and the address list is asserted non-empty —
/// an address-less `pendingLogs` subscribes to the entire chain.
pub fn pending_logs_params(pools: &[Address]) -> Value {
    json!([
        "pendingLogs",
        {
            "address": pools,
            "topics": [crate::log_decode::monitored_topics()],
        }
    ])
}

/// True when a subscription would be worth opening.
///
/// An empty pool list must never subscribe: `pendingLogs` with no address
/// filter is a firehose of every log on Base, which this task would then try to
/// decode one at a time.
pub fn worth_subscribing(pools: &[Address]) -> bool {
    !pools.is_empty()
}

/// Counters for the one thing this module exists to prove: that preconfirmed
/// state arrives fast enough to matter.
#[derive(Default)]
pub struct FastPathStats {
    /// Logs accepted into `LiveState`.
    pub applied: AtomicU64,
    /// Logs the decoders recognised but state rejected (duplicate, superseded,
    /// untrusted base). Expected traffic, not a fault.
    pub declined: AtomicU64,
    /// Logs no decoder recognised. A real signal: some venue emits something we
    /// do not understand.
    pub undecodable: AtomicU64,
    /// Sum of receive→applied microseconds, and the count, so the mean is
    /// derivable without a histogram.
    pub apply_micros_total: AtomicU64,
    pub apply_samples: AtomicU64,
}

impl FastPathStats {
    /// Mean receive→applied latency in microseconds, or `None` before the first
    /// sample. This is the number that says whether the feed is keeping up.
    pub fn mean_apply_micros(&self) -> Option<u64> {
        let n = self.apply_samples.load(Ordering::Relaxed);
        if n == 0 {
            return None;
        }
        Some(self.apply_micros_total.load(Ordering::Relaxed) / n)
    }
}

/// Consumes preconfirmed logs and marks pools dirty. Nothing else.
pub struct BaseFastPath {
    feed: FlashFeed,
    pools: Vec<Address>,
    live: Arc<LiveState>,
    /// Shared with the scan loop, which drains it. A `Mutex<HashSet>` rather
    /// than a `DashMap` because the drain must be a single atomic swap — the
    /// collect-then-clear form loses any insert landing between the two.
    touched: Arc<StdMutex<HashSet<Address>>>,
    metrics: Option<Arc<Metrics>>,
    pub stats: Arc<FastPathStats>,
}

impl BaseFastPath {
    pub fn new(
        feed: FlashFeed,
        pools: Vec<Address>,
        live: Arc<LiveState>,
        touched: Arc<StdMutex<HashSet<Address>>>,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self {
            feed,
            pools,
            live,
            touched,
            metrics,
            stats: Arc::new(FastPathStats::default()),
        }
    }

    pub fn feed(&self) -> &FlashFeed {
        &self.feed
    }

    pub fn pools(&self) -> &[Address] {
        &self.pools
    }

    /// Apply one preconfirmed log.
    ///
    /// Separated from the socket so the hot path is testable without a network:
    /// the pool monitor's equivalent logic could not be tested for months
    /// because it was welded inside the subscription loop.
    ///
    /// The pool is marked dirty on every *state-bearing* outcome, including the
    /// declines. A `Duplicate` or `Superseded` still means someone traded that
    /// pool, and the scan loop wants to look at it regardless of whether our
    /// snapshot moved.
    pub fn apply(&self, log: &Log, received_at: Instant) -> ApplyOutcome {
        let pool = log.address;
        let outcome = self.live.apply_log(log);
        match outcome {
            ApplyOutcome::Applied { .. } => {
                self.stats.applied.fetch_add(1, Ordering::Relaxed);
                self.mark(pool);
            }
            ApplyOutcome::Undecodable => {
                self.stats.undecodable.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.stats.declined.fetch_add(1, Ordering::Relaxed);
                self.mark(pool);
            }
        }
        let micros = received_at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.stats
            .apply_micros_total
            .fetch_add(micros, Ordering::Relaxed);
        self.stats.apply_samples.fetch_add(1, Ordering::Relaxed);
        if let Some(m) = &self.metrics {
            m.ingestion_ws_events.inc();
        }
        outcome
    }

    fn mark(&self, pool: Address) {
        if let Ok(mut g) = self.touched.lock() {
            g.insert(pool);
        }
    }

    /// What to do when the feed goes quiet.
    ///
    /// Delegates to the same decision function the pool monitor uses, so the
    /// distinction it encodes is not re-derived here and cannot drift: silence
    /// from birth means the filter matches nothing and a new socket would be
    /// just as deaf, while silence *after* delivery means a half-open socket
    /// that a reconnect does fix.
    pub(crate) fn idle_verdict(
        &self,
        events_seen: u64,
        idle_for: Duration,
        connected_for: Duration,
    ) -> IdleAction {
        idle_action(events_seen, idle_for, connected_for)
    }

    /// The idle budget for this feed.
    ///
    /// `pendingLogs` on a monitored pool set is high traffic — measured 215
    /// notifications in 30s across 4 pools — so prolonged silence is a dead
    /// socket, not a quiet market, and the 90s idle budget applies.
    pub fn stall_limit(&self) -> Duration {
        SUBSCRIPTION_STALL_LIMIT
    }
}

/// Decode-only helper: which monitored pool a log belongs to, if any.
///
/// `pendingLogs` is filtered server-side, but a provider that ignores the
/// address filter would silently widen the feed to the whole chain. Checking
/// membership locally makes that a dropped log rather than an unbounded decode
/// cost.
pub fn is_monitored(log: &Log, pools: &HashSet<Address>) -> bool {
    pools.contains(&log.address)
}

/// Topics this feed subscribes to, for assertions and diagnostics.
pub fn subscribed_topics() -> Vec<H256> {
    crate::log_decode::monitored_topics()
}

/// Placeholder for the socket task, deliberately not wired yet.
///
/// Returning an error rather than silently doing nothing: a fast path that
/// reports success while consuming no logs is exactly the failure this module's
/// documentation exists to prevent.
pub async fn spawn(_fast: Arc<BaseFastPath>) -> Result<()> {
    anyhow::bail!("base_fast::spawn is not wired yet; apply() is the tested entry point")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Bytes;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn sync_log(pool: Address, r0: u128, r1: u128, block: u64, li: u64) -> Log {
        let mut data = Vec::new();
        for v in [r0, r1] {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            data.extend_from_slice(&w);
        }
        Log {
            address: pool,
            topics: vec![*crate::log_decode::TOPIC_SOLIDLY_SYNC],
            data: Bytes::from(data),
            block_number: Some(block.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(li.into()),
            removed: Some(false),
            ..Default::default()
        }
    }

    fn fast(pools: Vec<Address>) -> (BaseFastPath, Arc<StdMutex<HashSet<Address>>>) {
        let touched = Arc::new(StdMutex::new(HashSet::new()));
        let f = BaseFastPath::new(
            FlashFeed::PendingLogs {
                ws_url: "wss://example.invalid".into(),
            },
            pools,
            Arc::new(LiveState::new()),
            touched.clone(),
            None,
        );
        (f, touched)
    }

    /// The filter must carry topics DERIVED from signatures. The pool monitor
    /// once shipped fabricated `topic0` values with correct prefixes and
    /// invented tails; it connected, logged success, and matched nothing for the
    /// life of the process.
    #[test]
    fn the_subscription_filter_carries_derived_topics() {
        let p = pending_logs_params(&[addr(1)]);
        assert_eq!(p[0], "pendingLogs");
        let topics = p[1]["topics"][0].as_array().expect("topic0 array");
        assert!(!topics.is_empty(), "an empty topic list subscribes to everything");
        let want = crate::log_decode::monitored_topics();
        assert_eq!(topics.len(), want.len());
        for t in &want {
            let hex = format!("{t:#x}");
            assert!(
                topics.iter().any(|v| v.as_str() == Some(hex.as_str())),
                "derived topic {hex} missing from the subscription"
            );
        }
    }

    /// `pendingLogs` with no address filter is every log on Base. Subscribing
    /// to that by accident would bury the decoder.
    #[test]
    fn an_empty_pool_set_never_subscribes() {
        assert!(!worth_subscribing(&[]));
        assert!(worth_subscribing(&[addr(1)]));
    }

    #[test]
    fn the_filter_pins_the_monitored_addresses() {
        let p = pending_logs_params(&[addr(1), addr(2)]);
        let addrs = p[1]["address"].as_array().expect("address array");
        assert_eq!(addrs.len(), 2);
    }

    /// The point of the module: a preconfirmed log lands in state AND marks the
    /// pool dirty, so the scan loop has something to react to.
    #[test]
    fn an_applied_log_marks_the_pool_dirty() {
        let pool = addr(7);
        let (f, touched) = fast(vec![pool]);
        let out = f.apply(&sync_log(pool, 10, 20, 100, 0), Instant::now());
        assert!(matches!(out, ApplyOutcome::Applied { .. }));
        assert!(touched.lock().unwrap().contains(&pool));
        assert_eq!(f.stats.applied.load(Ordering::Relaxed), 1);
    }

    /// A duplicate still means someone traded that pool. The scan loop wants to
    /// look at it even though our snapshot did not move, so a decline must
    /// still dirty the pool — dropping it would silently narrow the scan.
    #[test]
    fn a_declined_log_still_marks_the_pool_dirty() {
        let pool = addr(8);
        let (f, touched) = fast(vec![pool]);
        let log = sync_log(pool, 10, 20, 100, 0);
        f.apply(&log, Instant::now());
        touched.lock().unwrap().clear();

        let out = f.apply(&log, Instant::now());
        assert_eq!(out, ApplyOutcome::Duplicate);
        assert!(
            touched.lock().unwrap().contains(&pool),
            "a repeat trade on this pool is still news to the scan loop"
        );
        assert_eq!(f.stats.declined.load(Ordering::Relaxed), 1);
    }

    /// An unrecognised topic is a real signal — some venue emits something we do
    /// not decode — so it is counted separately and must NOT dirty the pool,
    /// because nothing about our state changed.
    #[test]
    fn an_undecodable_log_is_counted_and_does_not_dirty() {
        let pool = addr(9);
        let (f, touched) = fast(vec![pool]);
        let log = Log {
            address: pool,
            topics: vec![H256::repeat_byte(0xab)],
            data: Bytes::from(vec![0u8; 32]),
            block_number: Some(100u64.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(0u64.into()),
            removed: Some(false),
            ..Default::default()
        };
        assert_eq!(f.apply(&log, Instant::now()), ApplyOutcome::Undecodable);
        assert_eq!(f.stats.undecodable.load(Ordering::Relaxed), 1);
        assert!(touched.lock().unwrap().is_empty());
    }

    /// The latency counter is the whole justification for the module: it has to
    /// produce a number, or we cannot show the 21x gap closing.
    #[test]
    fn apply_latency_is_measured() {
        let pool = addr(10);
        let (f, _t) = fast(vec![pool]);
        assert_eq!(f.stats.mean_apply_micros(), None, "no samples yet");
        f.apply(&sync_log(pool, 1, 2, 100, 0), Instant::now());
        assert!(f.stats.mean_apply_micros().is_some());
    }

    /// A provider that ignored the address filter would widen this to the whole
    /// chain; membership is checked locally so that costs a drop, not a stall.
    #[test]
    fn logs_outside_the_monitored_set_are_identifiable() {
        let pools: HashSet<Address> = [addr(1), addr(2)].into_iter().collect();
        assert!(is_monitored(&sync_log(addr(1), 1, 2, 1, 0), &pools));
        assert!(!is_monitored(&sync_log(addr(99), 1, 2, 1, 0), &pools));
    }

    /// The feed is an enum so the documented Denim migration — native 200ms
    /// blocks replacing flashblocks — is a variant, not a rewrite of the state
    /// and search engine above it.
    #[test]
    fn the_feed_is_addressed_behind_an_interface() {
        let f = FlashFeed::PendingLogs {
            ws_url: "wss://example.invalid/ws".into(),
        };
        assert_eq!(f.ws_url(), "wss://example.invalid/ws");
    }

    /// Silence handling is delegated, not re-derived, so it cannot drift from
    /// the pool monitor's: never-delivered warns (a new socket carries the same
    /// deaf filter), delivered-then-quiet reconnects.
    #[test]
    fn idle_handling_matches_the_pool_monitor() {
        let (f, _t) = fast(vec![addr(1)]);
        let long = Duration::from_secs(600);
        assert_eq!(f.idle_verdict(0, long, long), IdleAction::WarnNeverDelivered);
        assert_eq!(f.idle_verdict(5_000, long, long), IdleAction::Reconnect);
        assert_eq!(f.stall_limit(), SUBSCRIPTION_STALL_LIMIT);
    }

    /// Wiring the socket is the next commit. Until then this must fail loudly
    /// rather than return Ok and consume nothing — a fast path that reports
    /// success while delivering no logs is the exact failure this module is
    /// written to prevent.
    #[tokio::test]
    async fn spawn_refuses_rather_than_pretending() {
        let (f, _t) = fast(vec![addr(1)]);
        assert!(spawn(Arc::new(f)).await.is_err());
    }
}
