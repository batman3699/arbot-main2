use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use ethers::types::U256;
use prometheus::{
    Counter, CounterVec, Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec, Opts,
    Registry, TextEncoder,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tracing::warn;

use crate::{capital::CapitalSnapshot, util::u256_to_f64};

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    pub opportunities_detected: Counter,
    pub opportunities_executed: Counter,
    pub opportunities_failed: Counter,
    pub cycle_search_timeouts: Counter,
    pub gross_profit_wei: Gauge,
    pub net_profit_wei: Gauge,
    pub gas_spent_wei: Gauge,
    pub operating_base_wei: Gauge,
    pub siphon_buffer_wei: Gauge,
    pub execution_latency_ms: Histogram,
    pub win_rate: Gauge,
    pub ingestion_ws_events: Counter,
    pub ingestion_poll_refresh: Counter,
    pub ingestion_stale_pools: Gauge,
    pub ingestion_active_pools: Gauge,
    pub mempool_txs_observed: Counter,
    pub sizing_quote_requests: Counter,
    pub multi_loan_rejections: CounterVec,
    pub opportunities_seen: CounterVec,
    pub simulations_passed: CounterVec,
    pub simulations_failed: CounterVec,
    pub tx_sent: CounterVec,
    pub tx_confirmed: CounterVec,
    pub tx_reverted: CounterVec,
    pub rpc_errors: CounterVec,
    pub gross_profit_native: GaugeVec,
    pub fees_native: GaugeVec,
    pub net_profit_native: GaugeVec,
    pub net_profit_usd: GaugeVec,
    pub stage_latency_ms: HistogramVec,
    pub graph_update_ms: Histogram,
    pub scan_ms: HistogramVec,
    pub edges_updated: CounterVec,
    pub liquidity_cache_hits: CounterVec,
    pub liquidity_cache_misses: CounterVec,
    pub liquidity_cache_evictions: CounterVec,
    pub worker_restarts: CounterVec,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        let opportunities_detected = Counter::with_opts(Opts::new(
            "opportunities_detected",
            "Total arbitrage opportunities identified",
        ))?;
        registry
            .register(Box::new(opportunities_detected.clone()))
            .context("register opportunities_detected counter")?;

        let opportunities_executed = Counter::with_opts(Opts::new(
            "opportunities_executed",
            "Total arbitrage opportunities executed successfully",
        ))?;
        registry
            .register(Box::new(opportunities_executed.clone()))
            .context("register opportunities_executed counter")?;

        let opportunities_failed = Counter::with_opts(Opts::new(
            "opportunities_failed",
            "Total arbitrage executions that reverted or failed",
        ))?;
        registry
            .register(Box::new(opportunities_failed.clone()))
            .context("register opportunities_failed counter")?;

        let cycle_search_timeouts = Counter::with_opts(Opts::new(
            "cycle_search_timeouts_total",
            "Total bellman-ford cycle searches aborted due to timeout",
        ))?;
        registry
            .register(Box::new(cycle_search_timeouts.clone()))
            .context("register cycle_search_timeouts_total counter")?;

        let gross_profit_wei = Gauge::with_opts(Opts::new(
            "gross_profit_wei",
            "Cumulative pre-executor-fee profit in wei (already net of flash-loan principal + lender fee)",
        ))?;
        registry
            .register(Box::new(gross_profit_wei.clone()))
            .context("register gross_profit_wei gauge")?;

        let net_profit_wei = Gauge::with_opts(Opts::new(
            "net_profit_wei",
            "Cumulative net profit measured in wei",
        ))?;
        registry
            .register(Box::new(net_profit_wei.clone()))
            .context("register net_profit_wei gauge")?;

        let gas_spent_wei = Gauge::with_opts(Opts::new(
            "gas_spent_wei",
            "Cumulative gas spend measured in wei",
        ))?;
        registry
            .register(Box::new(gas_spent_wei.clone()))
            .context("register gas_spent_wei gauge")?;

        let operating_base_wei = Gauge::with_opts(Opts::new(
            "operating_base_wei",
            "Current operating base amount after compounding (wei)",
        ))?;
        registry
            .register(Box::new(operating_base_wei.clone()))
            .context("register operating_base_wei gauge")?;

        let siphon_buffer_wei = Gauge::with_opts(Opts::new(
            "siphon_buffer_wei",
            "Accumulated profit earmarked for secure siphon (wei)",
        ))?;
        registry
            .register(Box::new(siphon_buffer_wei.clone()))
            .context("register siphon_buffer_wei gauge")?;

        let execution_latency_ms = Histogram::with_opts(HistogramOpts::new(
            "execution_latency_ms",
            "Observed inclusion latency for executed opportunities",
        ))?;
        registry
            .register(Box::new(execution_latency_ms.clone()))
            .context("register execution_latency_ms histogram")?;

        let win_rate = Gauge::with_opts(Opts::new(
            "win_rate",
            "Execution win rate (executed / detected)",
        ))?;
        registry
            .register(Box::new(win_rate.clone()))
            .context("register win_rate gauge")?;

        let ingestion_ws_events = Counter::with_opts(Opts::new(
            "ingestion_ws_events_total",
            "Total websocket events observed for pool monitoring",
        ))?;
        registry
            .register(Box::new(ingestion_ws_events.clone()))
            .context("register ingestion_ws_events_total counter")?;

        let ingestion_poll_refresh = Counter::with_opts(Opts::new(
            "ingestion_poll_refresh_total",
            "Total periodic poll refreshes executed for pools",
        ))?;
        registry
            .register(Box::new(ingestion_poll_refresh.clone()))
            .context("register ingestion_poll_refresh_total counter")?;

        let ingestion_stale_pools = Gauge::with_opts(Opts::new(
            "ingestion_stale_pools",
            "Number of pools whose cached state is stale",
        ))?;
        registry
            .register(Box::new(ingestion_stale_pools.clone()))
            .context("register ingestion_stale_pools gauge")?;

        let ingestion_active_pools = Gauge::with_opts(Opts::new(
            "ingestion_active_pools",
            "Number of pools actively monitored with fresh data",
        ))?;
        registry
            .register(Box::new(ingestion_active_pools.clone()))
            .context("register ingestion_active_pools gauge")?;

        let mempool_txs_observed = Counter::with_opts(Opts::new(
            "mempool_txs_observed_total",
            "Total pending transactions observed during monitoring",
        ))?;
        registry
            .register(Box::new(mempool_txs_observed.clone()))
            .context("register mempool_txs_observed_total counter")?;

        let sizing_quote_requests = Counter::with_opts(Opts::new(
            "sizing_quote_requests_total",
            "Total quote requests issued while sizing cycles",
        ))?;
        registry
            .register(Box::new(sizing_quote_requests.clone()))
            .context("register sizing_quote_requests_total counter")?;

        let multi_loan_rejections = CounterVec::new(
            Opts::new(
                "multi_loan_rejections_total",
                "Total plans rejected due to multi-loan allocations (per chain)",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(multi_loan_rejections.clone()))
            .context("register multi_loan_rejections_total counter")?;

        let opportunities_seen = CounterVec::new(
            Opts::new(
                "opportunities_seen_total",
                "Total opportunities evaluated (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(opportunities_seen.clone()))
            .context("register opportunities_seen_total counter")?;

        let simulations_passed = CounterVec::new(
            Opts::new(
                "simulations_passed_total",
                "Total simulations that passed profit checks (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(simulations_passed.clone()))
            .context("register simulations_passed_total counter")?;

        let simulations_failed = CounterVec::new(
            Opts::new(
                "simulations_failed_total",
                "Total simulations that failed (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(simulations_failed.clone()))
            .context("register simulations_failed_total counter")?;

        let tx_sent = CounterVec::new(
            Opts::new(
                "tx_sent_total",
                "Total transactions sent (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(tx_sent.clone()))
            .context("register tx_sent_total counter")?;

        let tx_confirmed = CounterVec::new(
            Opts::new(
                "tx_confirmed_total",
                "Total transactions confirmed (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(tx_confirmed.clone()))
            .context("register tx_confirmed_total counter")?;

        let tx_reverted = CounterVec::new(
            Opts::new(
                "tx_reverted_total",
                "Total transactions reverted/dropped (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(tx_reverted.clone()))
            .context("register tx_reverted_total counter")?;

        let rpc_errors = CounterVec::new(
            Opts::new("rpc_errors_total", "Total RPC errors observed (per chain)"),
            &["chain"],
        )?;
        registry
            .register(Box::new(rpc_errors.clone()))
            .context("register rpc_errors_total counter")?;

        let gross_profit_native = GaugeVec::new(
            Opts::new(
                "gross_profit_native",
                "Cumulative pre-executor-fee profit in native units, net of flash-loan principal + lender fee (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(gross_profit_native.clone()))
            .context("register gross_profit_native gauge")?;

        let fees_native = GaugeVec::new(
            Opts::new(
                "fees_native",
                "Cumulative fees in native units (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(fees_native.clone()))
            .context("register fees_native gauge")?;

        let net_profit_native = GaugeVec::new(
            Opts::new(
                "net_profit_native",
                "Cumulative net profit in native units (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(net_profit_native.clone()))
            .context("register net_profit_native gauge")?;

        let net_profit_usd = GaugeVec::new(
            Opts::new(
                "net_profit_usd",
                "Cumulative net profit in USD (per chain/strategy)",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(net_profit_usd.clone()))
            .context("register net_profit_usd gauge")?;

        let stage_latency_ms = HistogramVec::new(
            HistogramOpts::new(
                "stage_latency_ms",
                "Latency per scan stage (search/quote/simulate/broadcast)",
            ),
            &["chain", "stage"],
        )?;
        registry
            .register(Box::new(stage_latency_ms.clone()))
            .context("register stage_latency_ms histogram")?;

        let graph_update_ms = Histogram::with_opts(HistogramOpts::new(
            "graph_update_ms",
            "Latency for incremental graph adjacency updates",
        ))?;
        registry
            .register(Box::new(graph_update_ms.clone()))
            .context("register graph_update_ms histogram")?;

        let scan_ms = HistogramVec::new(
            HistogramOpts::new("scan_ms", "End-to-end scan latency in milliseconds"),
            &["chain"],
        )?;
        registry
            .register(Box::new(scan_ms.clone()))
            .context("register scan_ms histogram")?;

        let edges_updated = CounterVec::new(
            Opts::new(
                "edges_updated_total",
                "Total number of graph edges touched by incremental updates",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(edges_updated.clone()))
            .context("register edges_updated_total counter")?;

        let liquidity_cache_hits = CounterVec::new(
            Opts::new(
                "liquidity_cache_hits_total",
                "Total successful liquidity cache lookups",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(liquidity_cache_hits.clone()))
            .context("register liquidity_cache_hits_total counter")?;

        let liquidity_cache_misses = CounterVec::new(
            Opts::new(
                "liquidity_cache_misses_total",
                "Total liquidity cache misses (missing or stale entries)",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(liquidity_cache_misses.clone()))
            .context("register liquidity_cache_misses_total counter")?;

        let liquidity_cache_evictions = CounterVec::new(
            Opts::new(
                "liquidity_cache_evictions_total",
                "Total liquidity cache evictions",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(liquidity_cache_evictions.clone()))
            .context("register liquidity_cache_evictions_total counter")?;

        let worker_restarts = CounterVec::new(
            Opts::new(
                "worker_restarts_total",
                "Total supervised background worker restarts after exit or panic",
            ),
            &["chain", "worker"],
        )?;
        registry
            .register(Box::new(worker_restarts.clone()))
            .context("register worker_restarts_total counter")?;

        Ok(Self {
            registry,
            opportunities_detected,
            opportunities_executed,
            opportunities_failed,
            cycle_search_timeouts,
            gross_profit_wei,
            net_profit_wei,
            gas_spent_wei,
            operating_base_wei,
            siphon_buffer_wei,
            execution_latency_ms,
            win_rate,
            ingestion_ws_events,
            ingestion_poll_refresh,
            ingestion_stale_pools,
            ingestion_active_pools,
            mempool_txs_observed,
            sizing_quote_requests,
            multi_loan_rejections,
            opportunities_seen,
            simulations_passed,
            simulations_failed,
            tx_sent,
            tx_confirmed,
            tx_reverted,
            rpc_errors,
            gross_profit_native,
            fees_native,
            net_profit_native,
            net_profit_usd,
            stage_latency_ms,
            graph_update_ms,
            scan_ms,
            edges_updated,
            liquidity_cache_hits,
            liquidity_cache_misses,
            liquidity_cache_evictions,
            worker_restarts,
        })
    }

    pub fn record_sizing_quotes(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.sizing_quote_requests.inc_by(count as f64);
    }

    pub fn record_multi_loan_rejection(&self, chain: &str) {
        self.multi_loan_rejections.with_label_values(&[chain]).inc();
    }

    pub fn record_detection(&self) {
        self.opportunities_detected.inc();
        self.update_win_rate();
    }

    pub fn record_execution(&self, gross_wei: U256, net_wei: U256, gas_wei: U256, latency_ms: u64) {
        self.opportunities_executed.inc();
        self.gross_profit_wei.add(u256_to_f64(gross_wei));
        self.net_profit_wei.add(u256_to_f64(net_wei));
        self.gas_spent_wei.add(u256_to_f64(gas_wei));
        self.execution_latency_ms.observe(latency_ms as f64);
        self.update_win_rate();
    }

    pub fn record_failure(&self, estimated_loss_wei: U256) {
        self.opportunities_failed.inc();
        self.gas_spent_wei.add(u256_to_f64(estimated_loss_wei));
        self.update_win_rate();
    }

    pub fn record_opportunity_seen(&self, chain: &str, strategy: &str) {
        self.opportunities_seen
            .with_label_values(&[chain, strategy])
            .inc();
    }

    pub fn record_simulation(&self, chain: &str, strategy: &str, ok: bool) {
        let counter = if ok {
            &self.simulations_passed
        } else {
            &self.simulations_failed
        };
        counter.with_label_values(&[chain, strategy]).inc();
    }

    pub fn record_tx_sent(&self, chain: &str, strategy: &str) {
        self.tx_sent.with_label_values(&[chain, strategy]).inc();
    }

    pub fn record_tx_confirmed(&self, chain: &str, strategy: &str) {
        self.tx_confirmed
            .with_label_values(&[chain, strategy])
            .inc();
    }

    pub fn record_tx_reverted(&self, chain: &str, strategy: &str) {
        self.tx_reverted.with_label_values(&[chain, strategy]).inc();
    }

    pub fn record_rpc_error(&self, chain: &str) {
        self.rpc_errors.with_label_values(&[chain]).inc();
    }

    pub fn record_profit(
        &self,
        chain: &str,
        strategy: &str,
        gross: U256,
        fees: U256,
        net: U256,
        net_usd: Option<f64>,
    ) {
        let labels = &[chain, strategy];
        self.gross_profit_native
            .with_label_values(labels)
            .add(u256_to_f64(gross));
        self.fees_native
            .with_label_values(labels)
            .add(u256_to_f64(fees));
        self.net_profit_native
            .with_label_values(labels)
            .add(u256_to_f64(net));
        if let Some(net_usd) = net_usd {
            self.net_profit_usd.with_label_values(labels).add(net_usd);
        }
    }

    pub fn record_stage_latency(&self, chain: &str, stage: &str, ms: u64) {
        self.stage_latency_ms
            .with_label_values(&[chain, stage])
            .observe(ms as f64);
    }

    pub fn record_cycle_search_timeout(&self) {
        self.cycle_search_timeouts.inc();
    }

    pub fn record_capital(&self, snapshot: &CapitalSnapshot) {
        self.operating_base_wei
            .set(u256_to_f64(snapshot.base_amount));
        self.siphon_buffer_wei
            .set(u256_to_f64(snapshot.siphon_buffer));
    }

    fn update_win_rate(&self) {
        let detected = self.opportunities_detected.get();
        if detected <= f64::EPSILON {
            self.win_rate.set(0.0);
            return;
        }

        let executed = self.opportunities_executed.get();
        self.win_rate.set(executed / detected);
    }

    pub fn record_graph_update_ms(&self, ms: u64) {
        self.graph_update_ms.observe(ms as f64);
    }

    pub fn record_scan_latency(&self, chain: &str, ms: u64) {
        self.scan_ms.with_label_values(&[chain]).observe(ms as f64);
    }

    pub fn record_edges_updated(&self, chain: &str, count: usize) {
        if count > 0 {
            self.edges_updated
                .with_label_values(&[chain])
                .inc_by(count as f64);
        }
    }

    pub fn record_liquidity_cache_hit(&self, chain: &str) {
        self.liquidity_cache_hits.with_label_values(&[chain]).inc();
    }

    pub fn record_liquidity_cache_miss(&self, chain: &str) {
        self.liquidity_cache_misses
            .with_label_values(&[chain])
            .inc();
    }

    pub fn record_liquidity_cache_evictions(&self, chain: &str, count: u64) {
        if count > 0 {
            self.liquidity_cache_evictions
                .with_label_values(&[chain])
                .inc_by(count as f64);
        }
    }

    pub fn record_worker_restart(&self, chain: &str, worker: &str) {
        self.worker_restarts
            .with_label_values(&[chain, worker])
            .inc();
    }

    pub async fn export_to_prometheus(self: Arc<Self>, port: u16) -> Result<()> {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind prometheus exporter on {addr}"))?;

        loop {
            let (mut socket, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(error = %err, "Failed to accept Prometheus connection");
                    continue;
                }
            };

            let metrics = Arc::clone(&self);
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                if let Err(err) = socket.read(&mut buf).await {
                    warn!(peer = %peer, error = %err, "Failed to read Prometheus request");
                    return;
                }

                let encoder = TextEncoder::new();
                let metric_families = metrics.registry.gather();
                let mut body = Vec::with_capacity(1024);
                if let Err(err) = encoder.encode(&metric_families, &mut body) {
                    warn!(error = %err, "Failed to encode Prometheus metrics");
                    return;
                }

                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    encoder.format_type(),
                    body.len()
                );

                if let Err(err) = socket.write_all(header.as_bytes()).await {
                    warn!(peer = %peer, error = %err, "Failed to write Prometheus response header");
                    return;
                }

                if let Err(err) = socket.write_all(&body).await {
                    warn!(peer = %peer, error = %err, "Failed to write Prometheus response body");
                }
            });
        }
    }
}
