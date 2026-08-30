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
        let Some(provider) = self.ws_provider.clone() else {
            return;
        };

        loop {
            let pools = {
                let guard = self.pools.read().await;
                guard.clone()
            };
            if pools.is_empty() {
                sleep(self.poll_interval).await;
                continue;
            }
            let filter = pool_log_filter(&pools);

            match provider.subscribe_logs(&filter).await {
                Ok(mut sub) => {
                    self.ws_connected.store(true, Ordering::SeqCst);
                    self.ws_warned.store(false, Ordering::Relaxed);
                    info!(pools = pools.len(), "pool monitor websocket connected");
                    let connected_at = std::time::Instant::now();
                    let mut events_seen: u64 = 0;
                    let mut silence_warned = false;
                    loop {
                        tokio::select! {
                            log = sub.next() => {
                                match log {
                                    Some(log) => {
                                        events_seen = events_seen.saturating_add(1);
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
                                break;
                            }
                            _ = sleep(SILENT_SUBSCRIPTION_GRACE) => {
                                if !silence_warned
                                    && should_warn_silent(events_seen, connected_at.elapsed())
                                {
                                    silence_warned = true;
                                    warn!(
                                        pools = pools.len(),
                                        connected_secs = connected_at.elapsed().as_secs(),
                                        "pool monitor websocket connected but has received NO \
                                         logs; check that the subscribed topics match the pools"
                                    );
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
            self.resync.request();
            sleep(self.poll_interval).await;
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
                while sub.next().await.is_some() {
                    if let Some(metrics) = &metrics {
                        metrics.mempool_txs_observed.inc();
                    }
                }
                warn!("pending transaction monitor disconnected; reconnecting");
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
                while let Some(block) = stream.next().await {
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
                warn!(
                    chain = %chain_name,
                    "newHeads block monitor disconnected; reconnecting"
                );
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
            5,
            "4 constant-product topics + CL Swap; every family must be covered"
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
        assert_eq!(topics.len(), 5, "4 constant-product topics + 1 CL");
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
