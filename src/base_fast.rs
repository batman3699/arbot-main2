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

// The feed is wired (main.rs routes Base through `spawn` behind
// ARBOT_BASE_FAST), but the SCAN LOOP does not consume the dirty set yet, so
// the accessors it will use -- `feed`, `pools`, `stall_limit`, `is_monitored`,
// `mean_apply_micros` -- have no non-test caller.
//
// An earlier version of this comment claimed the allow could come off as soon
// as the module was wired. That was wrong: wiring the FEED is not the same as
// wiring the CONSUMER. It comes off when `process_base_flashblock` drains the
// dirty set.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ethers::types::{Address, Log, H256};
use serde_json::{json, Value};

use ethers::providers::Middleware;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, info, warn};

use crate::ingestion::{
    idle_action, next_before_stall, IdleAction, StreamStep, SUBSCRIPTION_IDLE_TICK,
    SUBSCRIPTION_STALL_LIMIT, WS_MAX_CONNECTION_AGE,
};
use crate::live_state::{ApplyOutcome, LiveState, UnknownReason};
use crate::metrics::Metrics;
use crate::util::connect_ws_provider_with_fallbacks;

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
    /// Shared with the scan loop, which drains it via `drain_and_resolve`. A `Mutex<HashSet>` rather
    /// than a `DashMap` because the drain must be a single atomic swap — the
    /// collect-then-clear form loses any insert landing between the two.
    touched: Arc<StdMutex<HashSet<Address>>>,
    metrics: Option<Arc<Metrics>>,
    ws_endpoints: Vec<String>,
    ws_backoff: Duration,
    pub stats: Arc<FastPathStats>,
}

impl BaseFastPath {
    /// `live` MUST have no other writer.
    ///
    /// `LiveState` holds one global cursor and requires monotonic ordinals.
    /// Preconfirmed and sealed delivery are two orderings of the same log
    /// stream, so pointing this at the pool monitor's state makes every
    /// interleaving look like disorder. Measured 2026-09-02 when they shared
    /// one: 27,085 events, 37 applies, 584 continuity breaks, each invalidating
    /// all 683 pools -- the canonical path's candidate count fell to zero.
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
            ws_endpoints: Vec::new(),
            ws_backoff: Duration::from_secs(5),
            stats: Arc::new(FastPathStats::default()),
        }
    }

    pub fn feed(&self) -> &FlashFeed {
        &self.feed
    }

    pub fn pools(&self) -> &[Address] {
        &self.pools
    }

    /// The dirty set, for the scan loop to drain.
    pub fn touched(&self) -> Arc<StdMutex<HashSet<Address>>> {
        Arc::clone(&self.touched)
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

/// What one flashblock's worth of dirty pools resolved to.
///
/// Deliberately reports what was DROPPED as well as what was selected: a hot
/// path that returns 32 cycles without saying it considered 400 reads as
/// exhaustive, and the whole point of the cap is that it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedCycles {
    /// Cycles to reprice, shortest first.
    pub cycles: Vec<crate::cycle_index::CycleId>,
    /// Cycles touched in total, before the cap.
    pub total_touched: usize,
    /// Dirty pools the universe could not resolve to a token hop. Non-zero
    /// means the fast path is marking pools the search graph does not know
    /// about, so their moves can never become candidates.
    pub unresolved_pools: usize,
}

/// Dirty pools -> token hops -> affected cycles.
///
/// This is the join the whole fast path exists for: `pendingLogs` reports which
/// POOL moved, `CycleIndex` is keyed by token HOP, and the canonical loop
/// bridged them by rebuilding everything. Both hop directions are queried
/// because a pool serves an unordered pair while cycles traverse a direction.
///
/// Pure and synchronous on purpose — no quoting, no RPC. It is the measurable
/// stage between "a log arrived" and "there is something to price", and keeping
/// it free of I/O is what makes that measurement mean anything.
pub fn touched_cycles(
    universe: &crate::cycle_index::PoolUniverse,
    index: &crate::cycle_index::CycleIndex,
    dirty: &HashSet<Address>,
    max_cycles: usize,
) -> TouchedCycles {
    let mut hops: Vec<(Address, Address)> = Vec::with_capacity(dirty.len() * 2);
    let mut unresolved = 0usize;
    for pool in dirty {
        match universe.pair_of(*pool) {
            Some((a, b)) => {
                hops.push((a, b));
                hops.push((b, a));
            }
            None => unresolved += 1,
        }
    }
    let (cycles, total_touched) = index.cycles_touching_limited(hops, max_cycles);
    TouchedCycles {
        cycles,
        total_touched,
        unresolved_pools: unresolved,
    }
}

/// Drain the dirty set and resolve it, timing the whole stage.
///
/// The drain is a single atomic swap, so a log landing mid-drain is not lost.
/// Returns the elapsed time because "flashblock -> candidate" is the first row
/// of the acceptance table and nothing else measures it.
pub fn drain_and_resolve(
    touched: &StdMutex<HashSet<Address>>,
    universe: &crate::cycle_index::PoolUniverse,
    index: &crate::cycle_index::CycleIndex,
    max_cycles: usize,
) -> (TouchedCycles, usize, Duration) {
    let started = Instant::now();
    let dirty = match touched.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(p) => std::mem::take(&mut *p.into_inner()),
    };
    let pools = dirty.len();
    let out = touched_cycles(universe, index, &dirty, max_cycles);
    (out, pools, started.elapsed())
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

impl BaseFastPath {
    /// Endpoints for rebuilding the transport, and the backoff between tries.
    ///
    /// Without these the task holds one `Provider<Ws>` for the life of the
    /// process, and once that socket is closed every resubscribe is against a
    /// corpse. The pool monitor shipped exactly that bug: it could detect the
    /// stall and never recover from it.
    pub fn with_ws_reconnect(mut self, endpoints: Vec<String>, backoff: Duration) -> Self {
        self.ws_endpoints = endpoints;
        self.ws_backoff = backoff;
        self
    }

    /// Start the feed. Returns immediately; the socket runs on its own task.
    ///
    /// Also starts a reporter. `FastPathStats` was collected for two runs
    /// before anything printed it, so the receive->applied number the module
    /// exists to produce was invisible in both -- the same miss as adding a
    /// counter and then killing the process without scraping it.
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        let reporter = Arc::clone(&self);
        tokio::spawn(async move { reporter.report_loop().await });
        tokio::spawn(async move { self.run().await })
    }

    /// Drain the dirty set on a fixed cadence and resolve it to cycles.
    ///
    /// This is the join step: the feed writes dirty pools at 3us and, until
    /// now, nothing read them — `dirty_pools` climbed monotonically to 159 in a
    /// six-minute run because the set had a writer and no consumer.
    ///
    /// Timed and reported because "flashblock -> candidate" is the first row of
    /// the acceptance table, and the drain latency is the part this project can
    /// control. Repricing and execution hang off the returned cycle ids; they
    /// are not done here, so the measurement stays free of quoting cost.
    pub fn spawn_drain(
        self: Arc<Self>,
        universe: Arc<crate::cycle_index::PoolUniverse>,
        index: Arc<StdMutex<Option<crate::cycle_index::CycleIndex>>>,
        cadence: Duration,
        max_cycles: usize,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = interval(cadence);
            tick.tick().await; // immediate first tick; discard
            let mut drains: u64 = 0;
            let mut pools_total: u64 = 0;
            let mut cycles_total: u64 = 0;
            let mut micros_total: u64 = 0;
            let mut unresolved_total: u64 = 0;
            let mut capped: u64 = 0;
            loop {
                tick.tick().await;
                let Some(idx) = index.lock().ok().and_then(|g| g.clone()) else {
                    // No index yet: still drain, or the set grows unbounded
                    // while the graph is warming up.
                    if let Ok(mut g) = self.touched.lock() {
                        g.clear();
                    }
                    continue;
                };
                let (out, pools, elapsed) =
                    drain_and_resolve(&self.touched, &universe, &idx, max_cycles);
                if pools == 0 {
                    continue;
                }
                drains += 1;
                pools_total += pools as u64;
                cycles_total += out.cycles.len() as u64;
                unresolved_total += out.unresolved_pools as u64;
                micros_total += elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
                if out.total_touched > out.cycles.len() {
                    capped += 1;
                }
                info!(
                    target: "latency",
                    dirty_pools = pools,
                    cycles = out.cycles.len(),
                    total_touched = out.total_touched,
                    unresolved_pools = out.unresolved_pools,
                    drain_us = elapsed.as_micros(),
                    mean_drain_us = micros_total / drains.max(1),
                    mean_cycles = cycles_total / drains.max(1),
                    mean_pools = pools_total / drains.max(1),
                    capped_drains = capped,
                    unresolved_total,
                    "flashblock to candidate"
                );
            }
        })
    }

    /// Print what the feed is actually doing, every 15s.
    ///
    /// Reports the DECLINE breakdown, not just the applies. A feed delivering
    /// thousands of events that apply almost none is the shape of the shared
    /// LiveState failure, and it is only distinguishable from a healthy feed by
    /// looking at the ratio.
    async fn report_loop(&self) {
        let mut tick = interval(Duration::from_secs(15));
        tick.tick().await; // immediate first tick; discard
        loop {
            tick.tick().await;
            let applied = self.stats.applied.load(Ordering::Relaxed);
            let declined = self.stats.declined.load(Ordering::Relaxed);
            let undecodable = self.stats.undecodable.load(Ordering::Relaxed);
            let seen = applied + declined + undecodable;
            if seen == 0 {
                warn!(
                    pools = self.pools.len(),
                    "base fast path has received NOTHING; the subscription is \
                     accepted but dead"
                );
                continue;
            }
            let dirty = self.touched.lock().map(|g| g.len()).unwrap_or(0);
            info!(
                target: "latency",
                seen,
                applied,
                declined,
                undecodable,
                applied_pct = (applied as f64 * 100.0 / seen as f64).round() as u64,
                mean_apply_us = self.stats.mean_apply_micros().unwrap_or(0),
                dirty_pools = dirty,
                "base fast path"
            );
        }
    }

    async fn run(&self) {
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
        if !worth_subscribing(&self.pools) {
            warn!("base fast path has no pools; refusing to subscribe (pendingLogs with no \
                   address filter is every log on Base)");
            return;
        }
        loop {
            // Built fresh every iteration: a socket we suspect is dead must
            // never be reused, and only a new transport recovers one.
            let provider = {
                {
                    if self.ws_endpoints.is_empty() {
                        warn!("base fast path has no websocket endpoints; stopping");
                        return;
                    }
                    let connect = connect_ws_provider_with_fallbacks(
                        "base-fast-rpc",
                        &self.ws_endpoints,
                        self.ws_backoff,
                    );
                    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
                        Ok(Ok(p)) => Arc::new(p),
                        Ok(Err(err)) => {
                            warn!(error = %err, "base fast path websocket connect failed");
                            sleep(self.ws_backoff).await;
                            continue;
                        }
                        Err(_) => {
                            warn!("base fast path websocket connect timed out");
                            sleep(self.ws_backoff).await;
                            continue;
                        }
                    }
                }
            };

            let params = pending_logs_params(&self.pools);
            let mut sub = match provider
                .subscribe::<serde_json::Value, Log>(params)
                .await
            {
                Ok(s) => {
                    info!(
                        pools = self.pools.len(),
                        "base fast path subscribed to pendingLogs"
                    );
                    s
                }
                Err(err) => {
                    warn!(error = %err, "pendingLogs subscription failed");
                    self.note_gap();
                    sleep(self.ws_backoff).await;
                    continue;
                }
            };

            let connected_at = Instant::now();
            let mut last_event = connected_at;
            let mut events_seen: u64 = 0;
            let mut never_warned = false;
            // An `Interval`, never a `sleep` in the `select!`: select rebuilds
            // its branch futures each iteration, so a sleep restarts on every
            // log and never fires on a feed carrying hundreds per minute.
            let mut lifetime = interval(WS_MAX_CONNECTION_AGE);
            lifetime.tick().await; // immediate first tick; discard

            // Every exit below wants a FRESH transport: a stall and a stream
            // end are faults, and the rotation exists precisely to replace the
            // socket before the provider closes it. So the provider is never
            // put back, which also keeps `sub`'s borrow of it uncontested.
            loop {
                tokio::select! {
                    step = async { next_before_stall!(sub, SUBSCRIPTION_IDLE_TICK) } => {
                        match step {
                            StreamStep::Item(log) => {
                                events_seen = events_seen.saturating_add(1);
                                last_event = Instant::now();
                                self.apply(&log, last_event);
                            }
                            StreamStep::Ended => {
                                warn!("pendingLogs stream ended; reconnecting");
                                break;
                            }
                            StreamStep::Stalled => {
                                match idle_action(events_seen, last_event.elapsed(), connected_at.elapsed()) {
                                    IdleAction::Wait => {}
                                    IdleAction::WarnNeverDelivered => {
                                        if !never_warned {
                                            never_warned = true;
                                            warn!(
                                                pools = self.pools.len(),
                                                "pendingLogs subscribed but has delivered NOTHING; \
                                                 check the address filter and topics. Not \
                                                 reconnecting -- a new socket carries the same filter"
                                            );
                                        }
                                    }
                                    IdleAction::Reconnect => {
                                        warn!(
                                            idle_secs = last_event.elapsed().as_secs(),
                                            events_seen,
                                            "pendingLogs stalled; socket open but no longer \
                                             delivering. Reconnecting"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    _ = lifetime.tick() => {
                        warn!(
                            age_secs = connected_at.elapsed().as_secs(),
                            events_seen,
                            "rotating the base fast path socket before the provider closes it"
                        );
                        break;
                    }
                }
            }

            // Every exit is a hole in the log stream, so local state derived
            // from it can no longer be trusted.
            self.note_gap();
            sleep(self.ws_backoff).await;
        }
    }

    /// Invalidate local state after a break in the feed.
    ///
    /// The continuity cursor cannot detect a missing log — a filtered
    /// subscription has meaningless index gaps — so an interruption has to
    /// invalidate explicitly or snapshots keep their trust across the hole.
    fn note_gap(&self) {
        let epoch = self.live.break_continuity(UnknownReason::WsUnavailable);
        debug!(epoch, "base fast path gap; local state invalidated");
    }
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

    /// The join the fast path exists for: a dirty POOL becomes affected CYCLES
    /// without rebuilding the universe. Both hop directions must be queried --
    /// a pool serves an unordered pair, cycles traverse a direction.
    #[test]
    fn dirty_pools_resolve_to_the_cycles_that_traverse_them() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let (t1, t2, t3) = (addr(1), addr(2), addr(3));
        let (p12, p23, p31) = (addr(11), addr(12), addr(13));
        let universe = PoolUniverse::from_pools([(p12, t1, t2), (p23, t2, t3), (p31, t3, t1)]);
        let index = CycleIndex::build(&universe, &[t1], CycleIndexLimits::default());

        let dirty: HashSet<Address> = [p12].into_iter().collect();
        let out = touched_cycles(&universe, &index, &dirty, 32);
        assert!(
            !out.cycles.is_empty(),
            "the triangle traverses this pool's hop; it must be selected"
        );
        assert_eq!(out.unresolved_pools, 0);
        assert_eq!(out.total_touched, out.cycles.len());
    }

    /// A dirty pool the search graph does not know about can never become a
    /// candidate. Counting it is how that shows up as a number instead of as
    /// mysteriously absent opportunities.
    #[test]
    fn pools_the_universe_does_not_know_are_counted_not_hidden() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let universe = PoolUniverse::from_pools([(addr(11), addr(1), addr(2))]);
        let index = CycleIndex::build(&universe, &[addr(1)], CycleIndexLimits::default());
        let dirty: HashSet<Address> = [addr(99)].into_iter().collect();
        let out = touched_cycles(&universe, &index, &dirty, 32);
        assert!(out.cycles.is_empty());
        assert_eq!(out.unresolved_pools, 1);
    }

    /// The drain must be a single atomic swap, and must empty the set — a
    /// collect-then-clear loses anything landing between the two, and leaving
    /// the set populated reprocesses the same pools every cycle.
    #[test]
    fn draining_empties_the_dirty_set_and_reports_its_size() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let universe = PoolUniverse::from_pools([(addr(11), addr(1), addr(2))]);
        let index = CycleIndex::build(&universe, &[addr(1)], CycleIndexLimits::default());
        let touched = StdMutex::new(HashSet::from([addr(11), addr(12)]));

        let (_out, pools, _elapsed) = drain_and_resolve(&touched, &universe, &index, 32);
        assert_eq!(pools, 2);
        assert!(
            touched.lock().unwrap().is_empty(),
            "a drain that leaves the set populated reprocesses forever"
        );
    }

    /// Why the fast path must own its `LiveState`, as a test rather than a
    /// comment. Two feeds delivering the same logs in different orders — which
    /// is exactly preconfirmed vs sealed — drive the single global cursor into
    /// repeated breaks, and each break invalidates every pool.
    #[test]
    fn two_orderings_of_one_stream_destroy_a_shared_live_state() {
        let shared = Arc::new(LiveState::new());
        let pool = addr(11);
        let before = shared.continuity_epoch();

        // Feed A is ahead (preconfirmed); feed B replays the same blocks behind
        // it (sealed). Interleaved, they are not monotonic.
        for block in [10u64, 11, 12] {
            shared.apply_log(&sync_log(pool, 1, 2, block + 5, 0));
            shared.apply_log(&sync_log(pool, 1, 2, block, 0));
        }

        assert!(
            shared.continuity_epoch() > before,
            "interleaved orderings must trip the cursor -- this is why the fast \
             path gets its own LiveState instead of the pool monitor's"
        );
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
    #[test]
    fn without_endpoints_the_task_cannot_rebuild_a_dead_socket() {
        let (f, _t) = fast(vec![addr(1)]);
        assert!(f.ws_endpoints.is_empty(), "the default is the trapped state");
        let f = f.with_ws_reconnect(vec!["wss://example.invalid".into()], Duration::from_secs(5));
        assert_eq!(f.ws_endpoints.len(), 1);
    }

    /// Rotation must beat the provider's own close. BlockPI closes websockets
    /// at 30 minutes and the original incident was that close arriving
    /// unannounced at 28.
    #[test]
    fn the_socket_rotates_before_the_provider_closes_it() {
        assert!(WS_MAX_CONNECTION_AGE < Duration::from_secs(30 * 60));
    }
}
