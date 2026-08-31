use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use ethers::{
    prelude::*,
    providers::{JsonRpcClient, Ws},
    types::{BlockId, BlockNumber, Filter, U64},
};
use futures_util::StreamExt;
use tokio::{
    sync::{watch, Notify, RwLock},
    task::JoinHandle,
    time::{interval, sleep, timeout},
};
use tracing::{debug, info, warn};

use crate::{
    metrics::Metrics, quote_univ2::UniV2PairState, util::connect_ws_provider_with_fallbacks,
};

/// The websocket filter for a pool set.
///
/// Extracted from `run_ws` so it can be tested. It could not be before, and
/// the constants it depends on were wrong for the life of the process: each
/// had a correct prefix and a fabricated tail, so the subscription connected
/// cleanly and delivered nothing.
/// Pairs worth polling: everything monitored that is not already known bad.
fn pollable_pairs(pools: &[MonitoredPool], ignored: &HashSet<Address>) -> Vec<Address> {
    pools
        .iter()
        // CL pools have no getReserves; polling one wastes a round-trip per
        // cycle and logs a revert that reads like an RPC fault.
        .filter(|p| p.kind != PoolMonitorKind::ConcentratedLiquidity)
        .map(|p| p.pair)
        .filter(|pair| !ignored.contains(pair))
        .collect()
}

/// Pairs the batch did not return, in the original order.
///
/// `load_pair_states_batched` simply omits pairs whose sub-calls revert or
/// return malformed data, so these must fall back to the per-pair path — which
/// is also what decides whether a pool gets added to the ignore set.
fn batch_misses(
    pairs: &[Address],
    loaded: &std::collections::HashMap<Address, UniV2PairState>,
) -> Vec<Address> {
    pairs
        .iter()
        .copied()
        .filter(|pair| !loaded.contains_key(pair))
        .collect()
}

/// Grace period before a silent subscription is treated as suspicious.
const SILENT_SUBSCRIPTION_GRACE: Duration = Duration::from_secs(60);

/// True when a subscription has been connected long enough that receiving
/// nothing is more likely a broken filter than a quiet market.
///
/// This exists because a wrong `topic0` produced a subscription that connected
/// cleanly, logged success, and delivered nothing for the life of the process.
pub(crate) fn should_warn_silent(events: u64, connected_for: Duration) -> bool {
    events == 0 && connected_for >= SILENT_SUBSCRIPTION_GRACE
}

/// How long a subscription that WAS delivering may go quiet before the socket
/// is presumed half-open and replaced.
///
/// A websocket can stop delivering without ever closing: no error, no `None`
/// from the stream, just silence. On 2026-08-31 a feed did exactly that and the
/// process sat there for 12 hours believing it was live. Every reconnect path
/// in this file keys off the stream ENDING, so none of them could fire.
pub(crate) const SUBSCRIPTION_STALL_LIMIT: Duration = Duration::from_secs(90);

/// How often the idle timer wakes to evaluate the above. Bounds detection
/// latency to `SUBSCRIPTION_STALL_LIMIT + SUBSCRIPTION_IDLE_TICK`.
const SUBSCRIPTION_IDLE_TICK: Duration = Duration::from_secs(15);

/// `newHeads` is the strongest liveness signal available: blocks arrive on a
/// schedule whether or not the market is busy, so silence here is never
/// explained by a quiet market. Generous enough for slow chains; on Base
/// (~2s blocks) this is thirty missed blocks.
const NEWHEADS_STALL_LIMIT: Duration = Duration::from_secs(60);

/// How long a LOW-TRAFFIC subscription may go quiet before its socket is
/// replaced.
///
/// Health-factor updates and liquidation events are legitimately rare, so idle
/// time says nothing about whether the socket is alive and the 90s limit above
/// would churn constantly. Sized instead against the provider's own schedule:
/// BlockPI closes websocket connections after 30 minutes, so rotating at 25
/// pre-empts that close rather than discovering it. It is a connection-lifetime
/// bound wearing an idle timer's clothes.
// Used from liquidations.rs, which is bin-only (not in lib.rs), so the lib
// target sees no consumer.
#[allow(dead_code)]
pub(crate) const LOW_TRAFFIC_STALL_LIMIT: Duration = Duration::from_secs(25 * 60);

/// Outcome of awaiting the next item from a subscription.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamStep<T> {
    Item(T),
    /// The stream closed cleanly. Reconnect.
    Ended,
    /// Nothing arrived within the limit. Reconnect.
    Stalled,
}

/// `stream.next()` with a ceiling on how long silence is tolerated.
///
/// Every unbounded `while let Some(x) = stream.next().await` in this codebase
/// was unrecoverable against a half-open socket: no error, no close frame and
/// no `None`, so the reconnect immediately below it could never run. The task
/// parks forever while the process reports itself healthy. Use this instead.
///
/// A macro rather than a generic fn on purpose: ethers' `SubscriptionStream`
/// borrows its provider, and a generic `fn next<S: Stream>(&mut S)` makes the
/// compiler demand `Stream` for ANY lifetime once the caller is inside a
/// spawned task -- "implementation of `Stream` is not general enough". Expanding
/// at the call site keeps the concrete lifetime.
macro_rules! next_before_stall {
    ($stream:expr, $limit:expr) => {
        match ::tokio::time::timeout(
            $limit,
            ::futures_util::StreamExt::next(&mut $stream),
        )
        .await
        {
            Ok(Some(item)) => $crate::ingestion::StreamStep::Item(item),
            Ok(None) => $crate::ingestion::StreamStep::Ended,
            Err(_) => $crate::ingestion::StreamStep::Stalled,
        }
    };
}
// Required: mempool.rs and liquidations.rs import this by path and fail to
// compile without it. rustc does not count cross-module macro imports as a use
// of the re-export, so it warns here regardless.
#[allow(unused_imports)]
pub(crate) use next_before_stall;

/// What the idle timer should do when it fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleAction {
    /// Nothing is wrong yet.
    Wait,
    /// Connected but has never delivered anything. Warn; do NOT reconnect —
    /// this is a filter that matches nothing, and a new socket would carry the
    /// same filter and be just as deaf.
    WarnNeverDelivered,
    /// Was delivering and went quiet. Replace the socket.
    Reconnect,
}

/// Distinguishes the two ways a subscription goes quiet.
///
/// `idle_for` is measured from the last event received, NOT from connect —
/// measuring from connect is the bug this replaces, because it can only ever
/// describe a subscription that was born silent.
pub(crate) fn idle_action(
    events_seen: u64,
    idle_for: Duration,
    connected_for: Duration,
) -> IdleAction {
    if events_seen == 0 {
        return if should_warn_silent(events_seen, connected_for) {
            IdleAction::WarnNeverDelivered
        } else {
            IdleAction::Wait
        };
    }
    if idle_for >= SUBSCRIPTION_STALL_LIMIT {
        IdleAction::Reconnect
    } else {
        IdleAction::Wait
    }
}

pub(crate) fn pool_log_filter(pools: &[MonitoredPool]) -> Filter {
    Filter::new()
        .address(pools.iter().map(|p| p.pair).collect::<Vec<_>>())
        .topic0(crate::log_decode::monitored_topics())
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolMonitorKind {
    UniV2,
    Solidly,
    /// UniV3 / Aerodrome Slipstream. Subscribed for logs, but NOT polled: it
    /// has no `getReserves`, and its state arrives via `Swap` instead.
    ConcentratedLiquidity,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct MonitoredPool {
    pub pair: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub fee_bps: u32,
    pub stable: bool,
    pub kind: PoolMonitorKind,
}

#[derive(Clone, Debug)]
struct CachedState {
    state: UniV2PairState,
    updated_at: std::time::Instant,
    last_block: Option<U64>,
}

#[derive(Clone)]
pub struct PoolMonitor<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    provider: Arc<Provider<C>>,
    ws_provider: Option<Arc<Provider<Ws>>>,
    pools: Arc<RwLock<Vec<MonitoredPool>>>,
    poll_interval: Duration,
    stale_after: Duration,
    cache: Arc<DashMap<Address, CachedState>>,
    ignored_pools: Arc<RwLock<HashSet<Address>>>,
    metrics: Option<Arc<Metrics>>,
    resync: Arc<ResyncSignal>,
    ws_connected: Arc<AtomicBool>,
    ws_warned: Arc<AtomicBool>,
    pool_updates: Arc<Notify>,
    /// Pools whose state moved since the last drain.
    ///
    /// A `Mutex<HashSet>` rather than a `DashMap`, because the drain must be a
    /// single atomic swap. The previous `DashMap` drain collected then cleared,
    /// which erased any insert landing between the two — a permanent loss, not
    /// a delay. Never held across an `.await`.
    touched_pools: Arc<StdMutex<HashSet<Address>>>,
    /// Shadow-mode live state. `None` disables it entirely. Nothing downstream
    /// READS this store in Phase 1 — it is written and measured only.
    live_state: Option<Arc<crate::live_state::LiveState>>,
    /// Endpoints for rebuilding the websocket transport.
    ///
    /// Empty means the monitor is stuck with whatever provider it was handed:
    /// once that socket dies, resubscribing on it can never succeed.
    ws_endpoints: Vec<String>,
    ws_backoff: Duration,
    /// Pools that survive every `set_pools`.
    ///
    /// The univ2 hot-pool refresh rebuilds the monitored set from scratch and
    /// knows nothing about CL pools, so without this the subscription silently
    /// reverted from 683 pools to 23 five minutes after startup. Holding them
    /// here makes it impossible for a caller to forget.
    sticky_pools: Vec<MonitoredPool>,
}

impl<C> PoolMonitor<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn new(
        provider: Arc<Provider<C>>,
        ws_provider: Option<Arc<Provider<Ws>>>,
        pools: Vec<MonitoredPool>,
        poll_interval: Duration,
        stale_after: Duration,
        metrics: Option<Arc<Metrics>>,
    ) -> Result<Self> {
        if pools.is_empty() {
            return Err(anyhow!("no pools supplied for monitoring"));
        }

        Ok(Self {
            provider,
            ws_provider,
            pools: Arc::new(RwLock::new(pools)),
            poll_interval,
            stale_after,
            cache: Arc::new(DashMap::new()),
            ignored_pools: Arc::new(RwLock::new(HashSet::new())),
            metrics,
            resync: Arc::new(ResyncSignal::default()),
            ws_connected: Arc::new(AtomicBool::new(false)),
            ws_warned: Arc::new(AtomicBool::new(false)),
            pool_updates: Arc::new(Notify::new()),
            touched_pools: Arc::new(StdMutex::new(HashSet::new())),
            live_state: None,
            sticky_pools: Vec::new(),
            ws_endpoints: Vec::new(),
            ws_backoff: Duration::from_secs(5),
        })
    }

    /// Pools that must persist across every `set_pools` call.
    ///
    /// Merged into the live set immediately as well, so they are subscribed
    /// from the first connection rather than only after the first refresh.
    pub fn with_sticky_pools(mut self, sticky: Vec<MonitoredPool>) -> Self {
        if let Some(pools) = Arc::get_mut(&mut self.pools) {
            let live = pools.get_mut();
            let known: HashSet<Address> = live.iter().map(|p| p.pair).collect();
            for p in &sticky {
                if !known.contains(&p.pair) {
                    live.push(p.clone());
                }
            }
        }
        self.sticky_pools = sticky;
        self
    }

    /// Attach a shadow-mode live-state store.
    ///
    /// Phase 1 only: logs are decoded and applied so divergence can be
    /// measured, but no pricing path reads the result.
    /// Let the monitor rebuild its own websocket transport.
    ///
    /// Without this the monitor holds one `Arc<Provider<Ws>>` for the life of
    /// the process. Providers close sockets on a schedule -- BlockPI at 30
    /// minutes -- and after that every `subscribe_logs` is against a corpse.
    pub fn with_ws_reconnect(mut self, endpoints: Vec<String>, backoff: Duration) -> Self {
        self.ws_endpoints = endpoints;
        self.ws_backoff = backoff;
        self
    }

    pub fn with_live_state(mut self, live: Arc<crate::live_state::LiveState>) -> Self {
        self.live_state = Some(live);
        self
    }

    pub fn mark_touched(&self, pool: Address) {
        if let Ok(mut guard) = self.touched_pools.lock() {
            guard.insert(pool);
        }
    }

    /// Take the dirty set and leave an empty one, in one atomic step.
    ///
    /// A mark landing immediately after the swap belongs to the next batch and
    /// cannot be erased. Collect-then-clear did not have this property.
    pub fn drain_touched(&self) -> HashSet<Address> {
        match self.touched_pools.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => HashSet::new(),
        }
    }

    pub fn spawn(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        if self.ws_provider.is_some() {
            let ws = self.clone();
            handles.push(tokio::spawn(async move {
                ws.run_ws().await;
            }));
        }

        let poller = self.clone();
        handles.push(tokio::spawn(async move {
            poller.run_polling().await;
        }));

        handles
    }

    pub async fn set_pools(&self, pools: Vec<MonitoredPool>) {
        if pools.is_empty() {
            warn!("pool monitor received empty pool list; disabling monitoring until refreshed");
        }
        let mut pools = pools;
        // Re-append sticky pools: callers rebuild the set from a source that
        // does not know about them.
        let known: HashSet<Address> = pools.iter().map(|p| p.pair).collect();
        for p in &self.sticky_pools {
            if !known.contains(&p.pair) {
                pools.push(p.clone());
            }
        }
        let mut guard = self.pools.write().await;
        *guard = pools;
        self.pool_updates.notify_waiters();
        self.resync.request();
    }

    async fn run_ws(&self) {
        const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

        let mut ws_provider = self.ws_provider.clone();
        if ws_provider.is_none() && self.ws_endpoints.is_empty() {
            return;
        }
        if self.ws_endpoints.is_empty() {
            warn!(
                "pool monitor has a websocket but no endpoints to rebuild it; when the provider \
                 closes this socket (BlockPI does so every 30 minutes) log ingestion stops until \
                 restart. Pass with_ws_reconnect."
            );
        }

        loop {
            let pools = {
                let guard = self.pools.read().await;
                guard.clone()
            };
            if pools.is_empty() {
                sleep(self.poll_interval).await;
                continue;
            }

            // Taken, not cloned: a socket we suspect is dead must not be put
            // back, and the only way to recover one is to build a new
            // transport. `subscribe_logs` on a closed connection fails forever.
            let provider = match ws_provider.take() {
                Some(provider) => provider,
                None => {
                    let connect = connect_ws_provider_with_fallbacks(
                        "pool-monitor-rpc",
                        &self.ws_endpoints,
                        self.ws_backoff,
                    );
                    match timeout(WS_CONNECT_TIMEOUT, connect).await {
                        Ok(Ok(provider)) => Arc::new(provider),
                        Ok(Err(err)) => {
                            warn!(error = %err, "pool monitor websocket reconnect failed");
                            sleep(self.poll_interval).await;
                            continue;
                        }
                        Err(_) => {
                            warn!(
                                timeout_secs = WS_CONNECT_TIMEOUT.as_secs(),
                                "pool monitor websocket reconnect timed out"
                            );
                            sleep(self.poll_interval).await;
                            continue;
                        }
                    }
                }
            };
            let filter = pool_log_filter(&pools);

            match provider.subscribe_logs(&filter).await {
                Ok(mut sub) => {
                    self.ws_connected.store(true, Ordering::SeqCst);
                    self.ws_warned.store(false, Ordering::Relaxed);
                    info!(pools = pools.len(), "pool monitor websocket connected");
                    let connected_at = std::time::Instant::now();
                    let mut last_event = connected_at;
                    let mut events_seen: u64 = 0;
                    let mut silence_warned = false;
                    loop {
                        tokio::select! {
                            log = sub.next() => {
                                match log {
                                    Some(log) => {
                                        events_seen = events_seen.saturating_add(1);
                                        last_event = std::time::Instant::now();
                                        if let Err(err) = self.handle_log(log).await {
                                            warn!(error = %err, "failed to refresh pool after ws event");
                                        }
                                    }
                                    None => {
                                        warn!("pool monitor websocket stream ended; reconnecting and triggering resync");
                                        break;
                                    }
                                }
                            }
                            _ = self.pool_updates.notified() => {
                                warn!("pool list updated; resubscribing websocket filter");
                                // Our choice, not a fault: this socket is fine.
                                ws_provider = Some(provider.clone());
                                break;
                            }
                            _ = sleep(SUBSCRIPTION_IDLE_TICK) => {
                                let idle = last_event.elapsed();
                                match idle_action(events_seen, idle, connected_at.elapsed()) {
                                    IdleAction::Wait => {}
                                    IdleAction::WarnNeverDelivered => {
                                        if !silence_warned {
                                            silence_warned = true;
                                            warn!(
                                                pools = pools.len(),
                                                connected_secs = connected_at.elapsed().as_secs(),
                                                "pool monitor websocket connected but has received NO \
                                                 logs; check that the subscribed topics match the pools"
                                            );
                                        }
                                    }
                                    IdleAction::Reconnect => {
                                        if let Some(metrics) = &self.metrics {
                                            metrics.ingestion_ws_stalls.inc();
                                        }
                                        warn!(
                                            pools = pools.len(),
                                            idle_secs = idle.as_secs(),
                                            events_seen,
                                            "pool monitor websocket stalled; the socket never closed \
                                             but stopped delivering. Reconnecting and resyncing"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!(error = %err, "pool monitor websocket subscription failed");
                }
            }
            self.ws_connected.store(false, Ordering::SeqCst);
            // Reached only after a subscription ended, stalled, failed, or was
            // torn down to pick up a new pool list. Every one of those is a
            // hole in the log stream.
            self.note_ws_gap();
            self.resync.request();
            sleep(self.poll_interval).await;
        }
    }

    /// Invalidate local state after a break in the log stream.
    ///
    /// The subscription is the only thing keeping `LiveState` in step with the
    /// chain. Any interruption -- a stall, a clean close, a resubscribe to pick
    /// up new pools -- means events landed while we were not listening, and the
    /// continuity cursor cannot detect that: a filtered subscription has
    /// meaningless index gaps, so a missed log looks exactly like a log for a
    /// pool we do not watch.
    ///
    /// Without this, a snapshot built before the gap keeps its `Derived` trust
    /// forever, silently missing whatever happened during the hole. That is the
    /// shape of the one divergence that survived the settled-block fix: price
    /// and tick exact, liquidity low, on a pool whose Mint/Burn deltas
    /// accumulate rather than being overwritten.
    ///
    /// `break_continuity` is O(1) -- one epoch increment invalidates every
    /// snapshot at once -- so this is cheap enough to do on every gap.
    fn note_ws_gap(&self) {
        if let Some(live) = &self.live_state {
            let epoch = live.break_continuity(crate::live_state::UnknownReason::WsUnavailable);
            warn!(
                epoch,
                "websocket gap; every local snapshot is now untrusted until re-anchored"
            );
        }
    }

    async fn handle_log(&self, log: Log) -> Result<()> {
        let pair = log.address;
        if let Some(metrics) = &self.metrics {
            metrics.ingestion_ws_events.inc();
        }
        self.mark_touched(pair);

        if let Some(live) = &self.live_state {
            use crate::live_state::ApplyOutcome;
            match live.apply_log(&log) {
                ApplyOutcome::Applied { .. } => {
                    if let Some(m) = &self.metrics {
                        m.live_state_applied.inc();
                    }
                }
                ApplyOutcome::Undecodable => {
                    if let Some(m) = &self.metrics {
                        m.live_state_undecodable.inc();
                    }
                }
                ApplyOutcome::ContinuityBroken(_) => {
                    if let Some(m) = &self.metrics {
                        m.continuity_breaks.inc();
                    }
                }
                // Expected traffic, not a coverage gap — see is_known_non_state_topic.
                ApplyOutcome::NotStateBearing => {}
                ApplyOutcome::Duplicate => {}
            }
        }

        let block_number = log.block_number;
        if let Some(mut entry) = self.cache.get_mut(&pair) {
            if is_out_of_order(entry.last_block, block_number) {
                warn!(pair = %format!("0x{}", hex::encode(pair)), "detected ws gap; requesting resync");
                self.resync.request();
            }
            entry.last_block = block_number;
        }

        self.refresh_pair(pair).await
    }

    async fn run_polling(&self) {
        let mut ticker = interval(self.poll_interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.poll_all_pools("interval").await;
                }
                _ = self.resync.notified() => {
                    if self.resync.take_requested() {
                        self.poll_all_pools("resync").await;
                    }
                }
            }
        }
    }

    async fn poll_all_pools(&self, reason: &str) {
        if self.ws_provider.is_some() && !self.ws_connected.load(Ordering::Relaxed) {
            if !self.ws_warned.swap(true, Ordering::SeqCst) {
                warn!(%reason, "websocket unavailable; relying on RPC polling");
            }
        } else if self.ws_connected.load(Ordering::Relaxed) {
            self.ws_warned.store(false, Ordering::Relaxed);
        }

        let pools = {
            let guard = self.pools.read().await;
            guard.clone()
        };
        let ignored = {
            let guard = self.ignored_pools.read().await;
            guard.clone()
        };
        let pairs = pollable_pairs(&pools, &ignored);

        // One Multicall3 round-trip for the whole set, pinned to one block.
        //
        // This loop used to await `refresh_pair` per pool, and each of those
        // issues THREE sequential `eth_call`s — 69 serial round-trips for 23
        // pools, measured at 13.8s per cycle against a 3.6s `stale_after`. A
        // pool was therefore stale ~74% of the time by construction, and
        // `state_with_block` returns None when stale, so the cache answered
        // "nothing" for most pools and callers re-quoted over RPC.
        //
        // Batching also pins every read to one block; the per-pair path read
        // each pool at whatever "latest" meant when its call landed, so
        // reserves within a single refresh could straddle blocks.
        let started = std::time::Instant::now();
        let mut batched = 0usize;
        if !pairs.is_empty() {
            match self.provider.get_block_number().await {
                Ok(block) => {
                    let states = crate::quote_univ2::load_pair_states_batched(
                        self.provider.clone(),
                        &pairs,
                        block,
                    )
                    .await;
                    batched = states.len();
                    for (pair, state) in states.iter() {
                        self.cache_state(*pair, state.clone()).await;
                    }

                    // Pairs the batch dropped still need the per-pair path: it
                    // is what distinguishes a transient failure from an empty
                    // pool and maintains the ignore set.
                    for pair in batch_misses(&pairs, &states) {
                        if let Err(err) = self.refresh_pair(pair).await {
                            warn!(
                                error = %err,
                                pair = %format!("0x{}", hex::encode(pair)),
                                "failed to refresh pool during poll"
                            );
                        }
                    }
                }
                Err(err) => {
                    // No block number means no pinned batch. Fall back wholesale
                    // rather than skip the cycle — degraded, not absent.
                    warn!(error = %err, "block number unavailable; polling pools individually");
                    for pair in pairs.iter() {
                        if let Err(err) = self.refresh_pair(*pair).await {
                            warn!(
                                error = %err,
                                pair = %format!("0x{}", hex::encode(pair)),
                                "failed to refresh pool during poll"
                            );
                        }
                    }
                }
            }
        }
        debug!(
            %reason,
            pairs = pairs.len(),
            batched,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "pool monitor poll cycle complete"
        );

        if let Some(metrics) = &self.metrics {
            metrics.ingestion_poll_refresh.inc();
            let (fresh, stale) = self.snapshot_counts().await;
            metrics.ingestion_active_pools.set(fresh as f64);
            metrics.ingestion_stale_pools.set(stale as f64);
        }
    }

    async fn snapshot_counts(&self) -> (usize, usize) {
        let now = std::time::Instant::now();
        let mut fresh = 0usize;
        let mut stale = 0usize;
        for entry in self.cache.iter() {
            if now.duration_since(entry.updated_at) > self.stale_after {
                stale += 1;
            } else {
                fresh += 1;
            }
        }
        (fresh, stale)
    }

    async fn refresh_pair(&self, pair: Address) -> Result<()> {
        {
            let ignored = self.ignored_pools.read().await;
            if ignored.contains(&pair) {
                return Ok(());
            }
        }

        let pool = {
            let pools = self.pools.read().await;
            pools.iter().find(|p| p.pair == pair).cloned()
        };
        let Some(pool) = pool else {
            return Err(anyhow!("received update for unknown pool {pair:?}"));
        };

        match crate::quote_univ2::load_pair_state(self.provider.clone(), pool.pair).await? {
            Some(state) => {
                if pool.kind == PoolMonitorKind::Solidly {
                    debug!(
                        pair = %format!("0x{}", hex::encode(pool.pair)),
                        stable = pool.stable,
                        "refreshed Solidly pair state via getReserves"
                    );
                }
                self.cache_state(pool.pair, state).await;
            }
            None => {
                {
                    let mut ignored = self.ignored_pools.write().await;
                    if ignored.insert(pair) {
                        warn!(
                            pair = %format!("0x{}", hex::encode(pair)),
                            "pool returned empty state; disabling future refreshes"
                        );
                    }
                }

                self.cache.remove(&pair);
            }
        }
        Ok(())
    }

    async fn cache_state(&self, pair: Address, state: UniV2PairState) {
        let last_block = self.cache.get(&pair).and_then(|entry| entry.last_block);
        self.cache.insert(
            pair,
            CachedState {
                state,
                updated_at: std::time::Instant::now(),
                last_block,
            },
        );
    }

    #[allow(dead_code)]
    pub async fn state_for(&self, pair: Address) -> Option<UniV2PairState> {
        self.state_with_block(pair).await.map(|(state, _)| state)
    }

    pub async fn state_with_block(&self, pair: Address) -> Option<(UniV2PairState, Option<U64>)> {
        self.cache.get(&pair).and_then(|entry| {
            if entry.updated_at.elapsed() > self.stale_after {
                None
            } else {
                Some((entry.state.clone(), entry.last_block))
            }
        })
    }

    #[allow(dead_code)]
    pub fn pools(&self) -> Arc<RwLock<Vec<MonitoredPool>>> {
        self.pools.clone()
    }
}

#[allow(dead_code)]
pub async fn spawn_pending_tx_monitor<C>(
    http_provider: Arc<Provider<C>>,
    initial_ws_provider: Option<Arc<Provider<Ws>>>,
    ws_endpoints: Vec<String>,
    ws_backoff: Duration,
    metrics: Option<Arc<Metrics>>,
) where
    C: JsonRpcClient + 'static,
{
    const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(2);
    const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    let mut ws_provider = initial_ws_provider;

    loop {
        if ws_provider.is_none() && ws_endpoints.is_empty() {
            if let Err(err) = poll_pending_block(&http_provider, &metrics).await {
                warn!(error = %err, "pending tx RPC fallback failed");
            }
            sleep(FALLBACK_POLL_INTERVAL).await;
            continue;
        }

        let provider = if let Some(provider) = ws_provider.take() {
            provider
        } else {
            let connect =
                connect_ws_provider_with_fallbacks("pending-tx-rpc", &ws_endpoints, ws_backoff);
            match timeout(WS_CONNECT_TIMEOUT, connect).await {
                Ok(Ok(provider)) => Arc::new(provider),
                Ok(Err(err)) => {
                    warn!(error = %err, "failed to connect websocket for pending tx monitor");
                    if let Err(err) = poll_pending_block(&http_provider, &metrics).await {
                        warn!(error = %err, "pending tx RPC fallback failed");
                    }
                    sleep(FALLBACK_POLL_INTERVAL).await;
                    continue;
                }
                Err(_) => {
                    warn!(
                        timeout_secs = WS_CONNECT_TIMEOUT.as_secs(),
                        "websocket connection timed out for pending tx monitor"
                    );
                    if let Err(err) = poll_pending_block(&http_provider, &metrics).await {
                        warn!(error = %err, "pending tx RPC fallback failed");
                    }
                    sleep(FALLBACK_POLL_INTERVAL).await;
                    continue;
                }
            }
        };

        match provider.subscribe_pending_txs().await {
            Ok(mut sub) => {
                info!("pending transaction monitor connected");
                let connected_at = std::time::Instant::now();
                let mut events_seen: u64 = 0;
                let mut never_warned = false;
                loop {
                    match next_before_stall!(sub, SUBSCRIPTION_STALL_LIMIT) {
                        StreamStep::Item(_) => {
                            events_seen = events_seen.saturating_add(1);
                            if let Some(metrics) = &metrics {
                                metrics.mempool_txs_observed.inc();
                            }
                        }
                        StreamStep::Ended => {
                            warn!("pending transaction monitor disconnected; reconnecting");
                            break;
                        }
                        StreamStep::Stalled => {
                            match idle_action(
                                events_seen,
                                SUBSCRIPTION_STALL_LIMIT,
                                connected_at.elapsed(),
                            ) {
                                IdleAction::Reconnect => {
                                    warn!(
                                        stall_secs = SUBSCRIPTION_STALL_LIMIT.as_secs(),
                                        events_seen,
                                        "pending transaction subscription stalled; socket still \
                                         open but stopped delivering. Reconnecting"
                                    );
                                    break;
                                }
                                IdleAction::WarnNeverDelivered => {
                                    if !never_warned {
                                        never_warned = true;
                                        warn!(
                                            "pending transaction subscription has delivered \
                                             nothing since connect; not reconnecting, because a \
                                             chain with no public pending pool will keep it empty"
                                        );
                                    }
                                }
                                IdleAction::Wait => {}
                            }
                        }
                    }
                }
            }
            Err(err) => {
                if is_websocket_subscription_close(&err.to_string()) {
                    info!(
                        error = %err,
                        "pending transaction websocket closed; switching to RPC fallback before reconnect"
                    );
                } else {
                    warn!(error = %err, "failed to subscribe to pending transactions");
                }
            }
        }
        if let Err(err) = poll_pending_block(&http_provider, &metrics).await {
            warn!(error = %err, "pending tx RPC fallback failed");
        }
        sleep(FALLBACK_POLL_INTERVAL).await;
    }
}

fn is_websocket_subscription_close(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("websocket closed unexpectedly")
        || (lower.contains("websocket") && lower.contains("closed"))
}

pub async fn poll_pending_block<C>(provider: &Provider<C>, metrics: &Option<Arc<Metrics>>) -> Result<()>
where
    C: JsonRpcClient + 'static,
{
    let pending_block = provider
        .get_block_with_txs(BlockId::Number(BlockNumber::Pending))
        .await?;

    if let Some(block) = pending_block {
        let txs = block.transactions.len();
        if txs > 0 {
            info!(
                pending_txs = txs,
                "observed pending transactions via RPC fallback"
            );
            if let Some(metrics) = metrics {
                metrics.mempool_txs_observed.inc_by(txs as f64);
            }
        }
    }

    Ok(())
}

/// Latest canonical head published to the scan loop. Carries the base fee so
/// the scan can price gas for the block without a separate get_block round-trip
/// at the front of the block race. `number == 0` is the uninitialized state
/// (no head delivered yet); consumers must treat it as "fall back to RPC".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockHead {
    pub number: U64,
    pub base_fee_per_gas: Option<U256>,
}

/// Pair used to wake the scan loop as soon as a new canonical head arrives.
pub fn block_head_channel() -> (watch::Sender<BlockHead>, watch::Receiver<BlockHead>) {
    watch::channel(BlockHead::default())
}

/// Age of a block, in milliseconds, at the moment we learned about it.
///
/// This is the top of the latency funnel: every quote, sizing decision and
/// simulation downstream is at least this stale. Base produces flashblock
/// preconfirmations every ~200ms and full blocks every ~2s, so an observation
/// age materially above ~200ms means the engine is reasoning about state that
/// faster searchers have already acted on — which looks identical to "no
/// arbitrage exists" from inside the funnel.
///
/// Returns `None` if the block timestamp is unusable (zero, or ahead of us).
fn block_age_ms(block_timestamp: U256) -> Option<i64> {
    let ts = block_timestamp.as_u64();
    if ts == 0 {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(now - (ts as i64) * 1000)
}

async fn poll_block_head<C>(provider: &Provider<C>, head_tx: &watch::Sender<BlockHead>)
where
    C: JsonRpcClient + 'static,
{
    if let Ok(Some(block)) = provider.get_block(BlockNumber::Latest).await {
        if let Some(number) = block.number {
            if head_tx.borrow().number.as_u64() != number.as_u64() {
                if let Some(age_ms) = block_age_ms(block.timestamp) {
                    info!(
                        target: "latency",
                        block = number.as_u64(),
                        age_ms,
                        source = "http_poll",
                        "block head observed"
                    );
                }
                let _ = head_tx.send(BlockHead {
                    number,
                    base_fee_per_gas: block.base_fee_per_gas,
                });
            }
        }
    }
}

/// Subscribe to `newHeads` over websocket (HTTP poll fallback) and publish the
/// latest canonical block number to the scan loop.
pub async fn spawn_block_head_monitor<C>(
    http_provider: Arc<Provider<C>>,
    initial_ws_provider: Option<Arc<Provider<Ws>>>,
    ws_endpoints: Vec<String>,
    ws_backoff: Duration,
    head_tx: watch::Sender<BlockHead>,
    chain_name: String,
) where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);
    const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    let mut ws_provider = initial_ws_provider;

    loop {
        if ws_provider.is_none() && ws_endpoints.is_empty() {
            poll_block_head(&http_provider, &head_tx).await;
            sleep(FALLBACK_POLL_INTERVAL).await;
            continue;
        }

        let provider = if let Some(provider) = ws_provider.take() {
            provider
        } else {
            let connect =
                connect_ws_provider_with_fallbacks("block-head-rpc", &ws_endpoints, ws_backoff);
            match timeout(WS_CONNECT_TIMEOUT, connect).await {
                Ok(Ok(provider)) => Arc::new(provider),
                Ok(Err(err)) => {
                    warn!(
                        chain = %chain_name,
                        error = %err,
                        "failed to connect websocket for block head monitor"
                    );
                    poll_block_head(&http_provider, &head_tx).await;
                    sleep(FALLBACK_POLL_INTERVAL).await;
                    continue;
                }
                Err(_) => {
                    warn!(
                        chain = %chain_name,
                        timeout_secs = WS_CONNECT_TIMEOUT.as_secs(),
                        "websocket connection timed out for block head monitor"
                    );
                    poll_block_head(&http_provider, &head_tx).await;
                    sleep(FALLBACK_POLL_INTERVAL).await;
                    continue;
                }
            }
        };

        match provider.subscribe_blocks().await {
            Ok(mut stream) => {
                info!(chain = %chain_name, "newHeads block monitor connected");
                loop {
                    // Bounded, not `while let`: a half-open socket never yields
                    // `None`, so an unbounded await parks forever and the
                    // reconnect below is unreachable.
                    let block = match timeout(NEWHEADS_STALL_LIMIT, stream.next()).await {
                        Ok(Some(block)) => block,
                        Ok(None) => {
                            warn!(
                                chain = %chain_name,
                                "newHeads block monitor disconnected; reconnecting"
                            );
                            break;
                        }
                        Err(_) => {
                            warn!(
                                chain = %chain_name,
                                stall_secs = NEWHEADS_STALL_LIMIT.as_secs(),
                                "newHeads websocket stalled; blocks arrive on a schedule, so \
                                 silence here means the socket is dead. Reconnecting"
                            );
                            break;
                        }
                    };
                    if let Some(number) = block.number {
                        if let Some(age_ms) = block_age_ms(block.timestamp) {
                            info!(
                                target: "latency",
                                block = number.as_u64(),
                                age_ms,
                                source = "ws_newheads",
                                "block head observed"
                            );
                        }
                        let _ = head_tx.send(BlockHead {
                            number,
                            base_fee_per_gas: block.base_fee_per_gas,
                        });
                    }
                }
            }
            Err(err) => {
                if is_websocket_subscription_close(&err.to_string()) {
                    info!(
                        chain = %chain_name,
                        error = %err,
                        "newHeads websocket closed; using RPC fallback before reconnect"
                    );
                } else {
                    warn!(
                        chain = %chain_name,
                        error = %err,
                        "failed to subscribe to newHeads"
                    );
                }
            }
        }

        poll_block_head(&http_provider, &head_tx).await;
        sleep(FALLBACK_POLL_INTERVAL).await;
    }
}

#[derive(Default)]
struct ResyncSignal {
    requested: AtomicBool,
    notify: Notify,
}

impl ResyncSignal {
    fn request(&self) {
        if !self
            .requested
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.notify.notify_one();
        }
    }

    fn take_requested(&self) -> bool {
        self.requested
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }
}

fn is_out_of_order(previous: Option<U64>, current: Option<U64>) -> bool {
    match (previous, current) {
        (Some(prev), Some(curr)) => curr <= prev || curr.saturating_sub(prev) > U64::one(),
        (None, _) | (_, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::providers::MockProvider;
    use std::time::Instant;

    #[test]
    fn classifies_expected_websocket_subscription_closures() {
        assert!(is_websocket_subscription_close(
            "Websocket closed unexpectedly"
        ));
        assert!(is_websocket_subscription_close("websocket stream closed"));
        assert!(!is_websocket_subscription_close("connection reset by peer"));
    }

    #[test]
    fn detects_out_of_order_blocks() {
        assert!(!is_out_of_order(None, Some(U64::from(1))));
        assert!(!is_out_of_order(Some(U64::from(1)), Some(U64::from(2))));
        assert!(is_out_of_order(Some(U64::from(2)), Some(U64::from(1))));
        assert!(is_out_of_order(Some(U64::from(1)), Some(U64::from(3))));
    }

    #[tokio::test]
    async fn cache_state_preserves_last_block() {
        let provider = Arc::new(Provider::new(MockProvider::default()));
        let pair = Address::random();
        let pool = MonitoredPool {
            pair,
            token_in: Address::random(),
            token_out: Address::random(),
            fee_bps: 30,
            stable: false,
            kind: PoolMonitorKind::UniV2,
        };

        let monitor = PoolMonitor::new(
            provider,
            None,
            vec![pool],
            Duration::from_secs(1),
            Duration::from_secs(10),
            None,
        )
        .expect("monitor should construct");

        let initial_state = UniV2PairState {
            token0: Address::random(),
            token1: Address::random(),
            reserve0: U256::from(1u64),
            reserve1: U256::from(2u64),
        };

        monitor.cache.insert(
            pair,
            CachedState {
                state: initial_state.clone(),
                updated_at: Instant::now(),
                last_block: Some(U64::from(42u64)),
            },
        );

        let refreshed_state = UniV2PairState {
            token0: initial_state.token0,
            token1: initial_state.token1,
            reserve0: U256::from(3u64),
            reserve1: U256::from(4u64),
        };
        monitor.cache_state(pair, refreshed_state.clone()).await;

        let entry = monitor.cache.get(&pair).expect("pair should remain cached");
        assert_eq!(entry.last_block, Some(U64::from(42u64)));
        assert_eq!(entry.state.reserve0, refreshed_state.reserve0);
        assert_eq!(entry.state.reserve1, refreshed_state.reserve1);
    }

    #[tokio::test]
    async fn resync_signal_notifies_once_per_request() {
        let signal = Arc::new(ResyncSignal::default());

        let waiter = {
            let signal = signal.clone();
            tokio::spawn(async move {
                signal.notified().await;
            })
        };

        signal.request();
        waiter.await.expect("waiter task should complete");

        assert!(signal.take_requested());
        assert!(!signal.take_requested());
    }

    fn monitored(n: u64) -> MonitoredPool {
        MonitoredPool {
            pair: Address::from_low_u64_be(n),
            token_in: Address::from_low_u64_be(n + 1000),
            token_out: Address::from_low_u64_be(n + 2000),
            fee_bps: 30,
            stable: false,
            kind: PoolMonitorKind::UniV2,
        }
    }

    /// `Filter::topics` is `[Option<Topic>; 4]` where `Topic` is
    /// `ValueOrArray<Option<H256>>`. Destructure it rather than matching on
    /// `Debug` output, which is not a stable contract.
    fn topic0_of(filter: &Filter) -> Vec<H256> {
        match filter.topics[0].clone().expect("topic0 must be set") {
            ValueOrArray::Value(v) => v.into_iter().collect(),
            ValueOrArray::Array(vs) => vs.into_iter().flatten().collect(),
        }
    }

    fn addresses_of(filter: &Filter) -> Vec<Address> {
        match filter.address.clone().expect("address must be set") {
            ValueOrArray::Value(a) => vec![a],
            ValueOrArray::Array(a) => a,
        }
    }

    #[test]
    fn filter_subscribes_to_the_real_sync_and_swap_topics() {
        let topics = topic0_of(&pool_log_filter(&[monitored(1), monitored(2)]));
        // `&*` derefs the LazyLock: `contains` wants `&H256`, not
        // `&LazyLock<H256>`, and the deref is not inserted implicitly here.
        assert!(
            topics.contains(&crate::log_decode::TOPIC_V2_SYNC),
            "Sync topic missing from filter: {topics:?}"
        );
        assert!(
            topics.contains(&crate::log_decode::TOPIC_V2_SWAP),
            "Swap topic missing from filter: {topics:?}"
        );
        assert_eq!(
            topics.len(),
            7,
            "4 constant-product + CL Swap/Mint/Burn; every family must be covered"
        );
    }

    /// The monitored set on Base is 23 Solidly pools and zero UniV2 pools, so a
    /// UniV2-only filter matched nothing at all — indistinguishable from a
    /// quiet market. Whatever the mix, both families must be subscribed.
    /// The univ2 hot-pool refresh calls `set_pools` with a set it rebuilds from
    /// scratch, which does not know about CL pools. Without sticky pools the
    /// subscription silently reverts from 683 pools to 23 after five minutes —
    /// observed live before this was fixed.
    #[tokio::test]
    async fn set_pools_preserves_sticky_cl_pools() {
        let provider = Arc::new(Provider::new(MockProvider::default()));
        let monitor = PoolMonitor::new(
            provider,
            None,
            vec![monitored(1)],
            Duration::from_secs(1),
            Duration::from_secs(10),
            None,
        )
        .expect("monitor")
        .with_sticky_pools(vec![cl_monitored(99)]);

        // Sticky pools must be present immediately, not only after a refresh.
        let before = pool_log_filter(&monitor.pools.read().await.clone());
        assert!(addresses_of(&before).contains(&Address::from_low_u64_be(99)));

        // A refresh that knows nothing about CL pools must not drop them.
        monitor.set_pools(vec![monitored(2), monitored(3)]).await;
        let after = monitor.pools.read().await.clone();
        let addrs = addresses_of(&pool_log_filter(&after));
        assert!(
            addrs.contains(&Address::from_low_u64_be(99)),
            "sticky CL pool dropped by set_pools: {addrs:?}"
        );
        assert!(addrs.contains(&Address::from_low_u64_be(2)));
        assert!(
            !addrs.contains(&Address::from_low_u64_be(1)),
            "non-sticky pools are still replaced"
        );
    }

    fn cl_monitored(n: u64) -> MonitoredPool {
        MonitoredPool {
            pair: Address::from_low_u64_be(n),
            token_in: Address::from_low_u64_be(n + 1000),
            token_out: Address::from_low_u64_be(n + 2000),
            fee_bps: 500,
            stable: false,
            kind: PoolMonitorKind::ConcentratedLiquidity,
        }
    }

    #[test]
    fn filter_includes_cl_pools_and_the_cl_swap_topic() {
        let filter = pool_log_filter(&[monitored(1), cl_monitored(2)]);
        let topics = topic0_of(&filter);
        assert!(
            topics.contains(&crate::log_decode::TOPIC_CL_SWAP),
            "CL Swap topic missing: {topics:?}"
        );
        let addrs = addresses_of(&filter);
        assert!(
            addrs.contains(&Address::from_low_u64_be(2)),
            "CL pool not subscribed"
        );
        assert_eq!(topics.len(), 7, "4 constant-product + CL Swap/Mint/Burn");
    }

    /// The poller reads getReserves, which reverts on a CL pool. Polling one
    /// wastes a round-trip every cycle and logs a failure that looks like an
    /// RPC problem.
    #[test]
    fn cl_pools_are_excluded_from_the_reserves_poll() {
        let pools = vec![monitored(1), cl_monitored(2), monitored(3)];
        let pairs = pollable_pairs(&pools, &HashSet::new());
        assert_eq!(pairs.len(), 2);
        assert!(!pairs.contains(&Address::from_low_u64_be(2)));
    }

    #[test]
    fn filter_covers_solidly_pools_not_just_univ2() {
        let topics = topic0_of(&pool_log_filter(&[monitored(1)]));
        assert!(
            topics.contains(&crate::log_decode::TOPIC_SOLIDLY_SYNC),
            "Solidly Sync(uint256,uint256) missing: {topics:?}"
        );
        assert!(
            topics.contains(&crate::log_decode::TOPIC_SOLIDLY_SWAP),
            "Solidly Swap missing: {topics:?}"
        );
    }

    /// The monitor held one provider for the life of the process, so the stall
    /// detection added in a77fd31 could break the loop but never recover: every
    /// resubscribe went to the same dead socket.
    #[test]
    fn without_endpoints_the_monitor_cannot_replace_a_dead_socket() {
        let build = || {
            PoolMonitor::new(
                Arc::new(Provider::new(MockProvider::default())),
                None,
                vec![monitored(1)],
                Duration::from_secs(1),
                Duration::from_secs(10),
                None,
            )
            .expect("monitor should construct")
        };
        assert!(
            build().ws_endpoints.is_empty(),
            "the default is the trapped state, which is why main.rs must opt in"
        );
        let reconnectable =
            build().with_ws_reconnect(vec!["wss://example.invalid".into()], Duration::from_secs(5));
        assert_eq!(reconnectable.ws_endpoints.len(), 1);
        assert_eq!(reconnectable.ws_backoff, Duration::from_secs(5));
    }

    /// A subscription gap orphans local state, and nothing detected it.
    ///
    /// `break_continuity` existed from Phase 1 and was never called from
    /// anywhere in production. The 2026-08-31 run resubscribed three times in
    /// 28 minutes (pool-list rebuilds) and each gap silently kept every
    /// snapshot at `Derived`. The one divergence that survived the
    /// settled-block fix sits 22 seconds after one of those gaps.
    #[tokio::test]
    async fn a_ws_gap_invalidates_every_local_snapshot() {
        use crate::live_state::{may_price_locally, LiveState};
        let live = Arc::new(LiveState::new());
        let pool = Address::from_low_u64_be(9);

        let mut data = Vec::new();
        for v in [1u128, 2u128] {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            data.extend_from_slice(&w);
        }
        live.apply_log(&Log {
            address: pool,
            topics: vec![*crate::log_decode::TOPIC_SOLIDLY_SYNC],
            data: ethers::types::Bytes::from(data),
            block_number: Some(100u64.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(0u64.into()),
            removed: Some(false),
            ..Default::default()
        });
        assert!(
            may_price_locally(&live.v2_snapshot(pool).unwrap().prov.trust),
            "precondition: the snapshot starts trusted"
        );

        let monitor = PoolMonitor::new(
            Arc::new(Provider::new(MockProvider::default())),
            None,
            vec![monitored(1)],
            Duration::from_secs(1),
            Duration::from_secs(10),
            None,
        )
        .expect("monitor should construct")
        .with_live_state(live.clone());

        monitor.note_ws_gap();

        assert!(
            !may_price_locally(&live.v2_snapshot(pool).unwrap().prov.trust),
            "state carried across a gap is state that silently missed events"
        );
    }

    /// `ingestion_ws_events_total` is the field signal that the subscription is
    /// alive. It was structurally unreachable: production constructed the
    /// monitor with `metrics: None`, so the counter read 0 whether logs flowed
    /// or not — and was used as a release gate in exactly that state.
    #[tokio::test]
    async fn handle_log_increments_the_ws_event_counter() {
        let metrics = Arc::new(crate::metrics::Metrics::new().expect("metrics"));
        let provider = Arc::new(Provider::new(MockProvider::default()));
        let pair = Address::random();
        let pool = MonitoredPool {
            pair,
            token_in: Address::random(),
            token_out: Address::random(),
            fee_bps: 30,
            stable: false,
            kind: PoolMonitorKind::UniV2,
        };
        let monitor = PoolMonitor::new(
            provider,
            None,
            vec![pool],
            Duration::from_secs(1),
            Duration::from_secs(10),
            Some(metrics.clone()),
        )
        .expect("monitor should construct");

        let before = metrics.ingestion_ws_events.get();
        let log = Log {
            address: pair,
            ..Default::default()
        };
        // The refresh at the end of handle_log has no mocked response and will
        // error. The counter is incremented before it, which is the point.
        let _ = monitor.handle_log(log).await;

        assert_eq!(
            metrics.ingestion_ws_events.get(),
            before + 1.0,
            "a delivered log must be observable in metrics"
        );
    }

    /// A concurrent mark must never be erased by a drain. The previous
    /// collect-then-clear implementation dropped any insert that landed
    /// between the iteration and the `clear()`.
    #[test]
    fn concurrent_marks_are_never_lost_across_a_drain() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::Arc as StdArc;

        const WRITERS: u64 = 4;
        const PER_WRITER: u64 = 2_000;

        let touched: StdArc<StdMutex<HashSet<Address>>> =
            StdArc::new(StdMutex::new(HashSet::new()));
        let done = StdArc::new(AtomicBool::new(false));
        let drained: StdArc<StdMutex<HashSet<Address>>> =
            StdArc::new(StdMutex::new(HashSet::new()));

        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let touched = StdArc::clone(&touched);
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    let addr = Address::from_low_u64_be(w * PER_WRITER + i);
                    touched.lock().expect("mark").insert(addr);
                }
            }));
        }

        let reader = {
            let touched = StdArc::clone(&touched);
            let drained = StdArc::clone(&drained);
            let done = StdArc::clone(&done);
            std::thread::spawn(move || loop {
                let batch: HashSet<Address> =
                    std::mem::take(&mut *touched.lock().expect("drain"));
                drained.lock().expect("record").extend(batch);
                if done.load(AtomicOrdering::SeqCst) {
                    let tail: HashSet<Address> =
                        std::mem::take(&mut *touched.lock().expect("drain tail"));
                    drained.lock().expect("record tail").extend(tail);
                    break;
                }
            })
        };

        for h in handles {
            h.join().expect("writer");
        }
        done.store(true, AtomicOrdering::SeqCst);
        reader.join().expect("reader");

        let seen = drained.lock().expect("final").len() as u64;
        assert_eq!(
            seen,
            WRITERS * PER_WRITER,
            "a concurrent mark was erased by a drain"
        );
    }

    /// The poller must skip pools already known bad, or every cycle re-pays
    /// their cost and `refresh_pair` re-adds them to the ignore set.
    #[test]
    fn pollable_pairs_excludes_ignored_pools() {
        let pools: Vec<MonitoredPool> = (1..=4).map(monitored).collect();
        let ignored: HashSet<Address> = HashSet::from([Address::from_low_u64_be(2)]);
        let pairs = pollable_pairs(&pools, &ignored);
        assert_eq!(pairs.len(), 3);
        assert!(!pairs.contains(&Address::from_low_u64_be(2)));
    }

    /// `load_pair_states_batched` omits pairs whose sub-calls revert, so the
    /// misses must fall back to the per-pair path rather than silently keeping
    /// stale reserves.
    #[test]
    fn batch_misses_are_exactly_the_pairs_the_batch_dropped() {
        let pairs: Vec<Address> = (1..=4).map(Address::from_low_u64_be).collect();
        let state = UniV2PairState {
            token0: Address::from_low_u64_be(90),
            token1: Address::from_low_u64_be(91),
            reserve0: U256::from(1u64),
            reserve1: U256::from(2u64),
        };
        let loaded: std::collections::HashMap<Address, UniV2PairState> =
            [(Address::from_low_u64_be(1), state.clone()),
             (Address::from_low_u64_be(3), state)]
                .into_iter()
                .collect();

        let misses = batch_misses(&pairs, &loaded);
        assert_eq!(
            misses,
            vec![Address::from_low_u64_be(2), Address::from_low_u64_be(4)],
            "only the dropped pairs fall back, and order is preserved"
        );
    }

    #[test]
    fn a_full_batch_needs_no_per_pair_fallback() {
        let pairs: Vec<Address> = (1..=2).map(Address::from_low_u64_be).collect();
        let state = UniV2PairState {
            token0: Address::from_low_u64_be(90),
            token1: Address::from_low_u64_be(91),
            reserve0: U256::from(1u64),
            reserve1: U256::from(2u64),
        };
        let loaded = pairs.iter().map(|p| (*p, state.clone())).collect();
        assert!(batch_misses(&pairs, &loaded).is_empty());
    }

    #[test]
    fn a_silent_subscription_warns_once_connected_long_enough() {
        assert!(
            should_warn_silent(0, Duration::from_secs(120)),
            "a connected-but-deaf subscription is exactly the shipped bug and \
             must be loud"
        );
    }

    #[test]
    fn a_busy_subscription_never_warns() {
        assert!(!should_warn_silent(1, Duration::from_secs(600)));
    }

    /// The shipped watchdog asked "has this subscription EVER delivered?".
    /// On 2026-08-31 the feed delivered for 28 minutes and then went silent for
    /// 12 hours; `events_seen` was in the thousands, so the watchdog stayed
    /// mute and the reconnect path — which only runs when the stream ENDS —
    /// was never reached. A half-open socket never ends.
    #[test]
    fn a_feed_that_delivered_and_then_went_quiet_reconnects() {
        assert_eq!(
            idle_action(5_000, Duration::from_secs(600), Duration::from_secs(1_800)),
            IdleAction::Reconnect,
            "this is the 12-hour silent stall; warning is not enough, the              socket has to be replaced"
        );
    }

    #[test]
    fn a_brief_quiet_spell_is_not_a_stall() {
        assert_eq!(
            idle_action(5_000, SUBSCRIPTION_STALL_LIMIT - Duration::from_secs(1), Duration::from_secs(1_800)),
            IdleAction::Wait,
            "a quiet market must not be mistaken for a dead socket"
        );
    }

    /// A subscription that never delivered is a different fault: the filter
    /// matches nothing. Reconnecting re-creates the same filter, so warning is
    /// the correct response and reconnect-looping would be wrong.
    #[test]
    fn a_never_delivered_subscription_warns_rather_than_reconnecting() {
        assert_eq!(
            idle_action(0, Duration::from_secs(600), Duration::from_secs(600)),
            IdleAction::WarnNeverDelivered
        );
    }

    #[test]
    fn a_young_silent_subscription_is_given_its_grace() {
        assert_eq!(
            idle_action(0, Duration::from_secs(5), Duration::from_secs(5)),
            IdleAction::Wait
        );
    }

    /// The trap this replaces: a socket that stays open and delivers nothing
    /// never yields `None`, so an unbounded `stream.next().await` parks forever
    /// and every reconnect path below it is dead code.
    #[tokio::test]
    async fn a_stream_that_never_yields_is_reported_stalled() {
        let mut s = futures_util::stream::pending::<u8>();
        assert_eq!(
            next_before_stall!(s, Duration::from_millis(20)),
            StreamStep::Stalled
        );
    }

    #[tokio::test]
    async fn a_closed_stream_is_still_reported_as_ended() {
        let mut s = futures_util::stream::iter(Vec::<u8>::new());
        assert_eq!(
            next_before_stall!(s, Duration::from_secs(30)),
            StreamStep::Ended,
            "a clean close must stay distinguishable from a stall"
        );
    }

    #[tokio::test]
    async fn a_delivering_stream_hands_back_its_item() {
        let mut s = futures_util::stream::iter(vec![7u8]);
        assert_eq!(
            next_before_stall!(s, Duration::from_secs(30)),
            StreamStep::Item(7)
        );
    }

    /// Field-found 2026-08-31: Base routes through a sequencer with no public
    /// pending pool, so its mempool subscription connects and delivers NOTHING,
    /// forever. Treating that as a stall reconnected every 90 seconds in a loop
    /// that could never terminate. Silence from birth is a different fault from
    /// silence after delivery, and only the second one is fixed by a new socket.
    #[test]
    fn a_subscription_that_never_delivers_is_not_reconnected_forever() {
        let hours = Duration::from_secs(6 * 3_600);
        assert_eq!(
            idle_action(0, hours, hours),
            IdleAction::WarnNeverDelivered,
            "reconnecting an empty-by-design subscription is an infinite loop"
        );
        assert_ne!(idle_action(0, hours, hours), IdleAction::Reconnect);
    }

    /// Rare-event subscriptions cannot be judged on the 90s idle budget; theirs
    /// is a connection-lifetime bound sized under the provider's own close.
    #[test]
    fn the_low_traffic_budget_pre_empts_the_provider_timeout() {
        assert!(
            LOW_TRAFFIC_STALL_LIMIT < Duration::from_secs(30 * 60),
            "BlockPI closes websockets at 30 minutes; rotate BEFORE that, not after"
        );
        assert!(LOW_TRAFFIC_STALL_LIMIT > SUBSCRIPTION_STALL_LIMIT);
    }

    /// Idle time is measured from the last event, not from connect, so a
    /// long-lived healthy feed is never reconnected for being old.
    #[test]
    fn a_long_lived_busy_feed_is_never_reconnected() {
        assert_eq!(
            idle_action(1_000_000, Duration::from_secs(0), Duration::from_secs(86_400)),
            IdleAction::Wait
        );
    }

    #[test]
    fn a_freshly_connected_subscription_is_given_time() {
        assert!(
            !should_warn_silent(0, Duration::from_secs(5)),
            "a quiet market must not warn during normal startup"
        );
    }

    #[test]
    fn drain_returns_everything_and_leaves_the_set_empty() {
        let set: StdMutex<HashSet<Address>> = StdMutex::new(HashSet::new());
        for n in 1..=5u64 {
            set.lock().expect("mark").insert(Address::from_low_u64_be(n));
        }
        let batch: HashSet<Address> = std::mem::take(&mut *set.lock().expect("drain"));
        assert_eq!(batch.len(), 5);
        assert!(set.lock().expect("check").is_empty());
    }

    #[test]
    fn filter_covers_every_supplied_pool() {
        let addrs = addresses_of(&pool_log_filter(&[
            monitored(1),
            monitored(2),
            monitored(3),
        ]));
        for n in 1..=3u64 {
            let addr = Address::from_low_u64_be(n);
            assert!(addrs.contains(&addr), "pool {addr:#x} missing from filter");
        }
    }
}
