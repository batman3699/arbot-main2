use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use ethers::{
    prelude::*,
    providers::{JsonRpcClient, Ws},
    types::{BlockId, BlockNumber, Filter, H256, U64},
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

const TOPIC_SYNC: H256 = H256([
    0x1c, 0x41, 0x68, 0xcd, 0xb0, 0xbe, 0xa3, 0xc4, 0x7c, 0xea, 0xd5, 0x56, 0x31, 0xe2, 0xd4, 0xf7,
    0x69, 0x59, 0x6b, 0x05, 0x6c, 0xc5, 0x0f, 0xaa, 0xa8, 0x3d, 0x72, 0x8a, 0xfa, 0xba, 0xf8, 0x5,
]);

const TOPIC_SWAP: H256 = H256([
    0xd7, 0x8a, 0xd9, 0x5f, 0xa4, 0x6c, 0x99, 0x4b, 0x65, 0x51, 0xd0, 0xda, 0x85, 0xfc, 0x27, 0x5f,
    0xe6, 0x13, 0xd2, 0xf6, 0xad, 0x69, 0x7f, 0xc0, 0x97, 0x1d, 0xf5, 0x40, 0x87, 0x19, 0x5c, 0x1,
]);

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolMonitorKind {
    UniV2,
    Solidly,
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
    touched_pools: Arc<DashMap<Address, ()>>,
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
            touched_pools: Arc::new(DashMap::new()),
        })
    }

    pub fn mark_touched(&self, pool: Address) {
        self.touched_pools.insert(pool, ());
    }

    pub fn drain_touched(&self) -> HashSet<Address> {
        let touched: HashSet<Address> = self.touched_pools.iter().map(|entry| *entry.key()).collect();
        self.touched_pools.clear();
        touched
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
            let filter = Filter::new()
                .address(pools.iter().map(|p| p.pair).collect::<Vec<_>>())
                .topic0(vec![TOPIC_SYNC, TOPIC_SWAP]);

            match provider.subscribe_logs(&filter).await {
                Ok(mut sub) => {
                    self.ws_connected.store(true, Ordering::SeqCst);
                    self.ws_warned.store(false, Ordering::Relaxed);
                    info!(pools = pools.len(), "pool monitor websocket connected");
                    loop {
                        tokio::select! {
                            log = sub.next() => {
                                match log {
                                    Some(log) => {
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
        self.touched_pools.insert(pair, ());

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

        for pool in pools.iter() {
            if let Err(err) = self.refresh_pair(pool.pair).await {
                warn!(
                    error = %err,
                    pair = %format!("0x{}", hex::encode(pool.pair)),
                    "failed to refresh pool during poll"
                );
            }
        }

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

async fn poll_block_head<C>(provider: &Provider<C>, head_tx: &watch::Sender<BlockHead>)
where
    C: JsonRpcClient + 'static,
{
    if let Ok(Some(block)) = provider.get_block(BlockNumber::Latest).await {
        if let Some(number) = block.number {
            if head_tx.borrow().number.as_u64() != number.as_u64() {
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
}
