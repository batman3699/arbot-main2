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
    /// Times a websocket that WAS delivering went silent past the stall limit
    /// and had to be replaced. A socket can stop delivering without closing, so
    /// this is the only counter that distinguishes "quiet market" from "dead
    /// feed" -- alert on it.
    pub ingestion_ws_stalls: Counter,
    /// Pool-list refreshes adopted without tearing down the websocket. Each one
    /// is a gap not taken, worth ~432 liquidity deltas by measurement.
    pub ingestion_resubscribes_avoided: Counter,
    /// Logs decoded and applied to live pool state (shadow mode).
    pub live_state_applied: Counter,
    /// Logs delivered that no known topic decoder handles. A large value means
    /// a venue is emitting something we do not understand — most likely
    /// PancakeSwap V3, which is deliberately not decoded.
    pub live_state_undecodable: Counter,
    /// Mint/Burn deltas dropped because the pool's base snapshot was
    /// invalidated by a websocket gap. The running cost of every gap.
    pub live_state_untrusted_base: Counter,
    /// Reorgs and out-of-order logs. Frequent breaks invalidate the
    /// lossless-dirty-set argument.
    pub continuity_breaks: Counter,
    /// Signed divergence of log-derived state from a fresh RPC read, by venue.
    pub live_state_divergence_bps: HistogramVec,
    /// Validation attempts by outcome: measured, unreachable, unsettled,
    /// skipped_lag, no_ordinal. `unsettled` means the snapshot's block may still
    /// receive more logs, so comparing it against end-of-block chain state
    /// would manufacture a divergence.
    pub live_state_checks: CounterVec,
    pub live_state_trusted: Gauge,
    /// MEASURED AND FAILED only. Not "everything that is not trusted" — an
    /// expired pass is counted separately, because conflating them makes the
    /// gate score re-check latency as untrustworthiness.
    pub live_state_untrusted: Gauge,
    /// Had a verdict that aged out. Says nothing about correctness.
    pub live_state_expired: Gauge,
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
    pub tx_relay_rejected: CounterVec,
    pub rpc_errors: CounterVec,
    pub native_price_probes: CounterVec,
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
    pub populate_full_total: CounterVec,
    pub populate_incremental_total: CounterVec,
    pub populate_skipped_total: CounterVec,
    pub sim_revm_success_total: Counter,
    pub sim_revm_failure_total: Counter,
    pub sim_revm_fallback_total: Counter,
    pub sim_revm_prefetch_accounts_total: Counter,
    pub sim_revm_prefetch_ms: Histogram,
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

        let ingestion_resubscribes_avoided = Counter::with_opts(Opts::new(
            "ingestion_resubscribes_avoided_total",
            "Pool-list refreshes that needed no new subscription",
        ))?;
        registry
            .register(Box::new(ingestion_resubscribes_avoided.clone()))
            .context("register ingestion_resubscribes_avoided_total counter")?;

        let live_state_untrusted_base = Counter::with_opts(Opts::new(
            "live_state_untrusted_base_total",
            "Liquidity deltas dropped because the base snapshot was invalidated by a gap",
        ))?;
        registry
            .register(Box::new(live_state_untrusted_base.clone()))
            .context("register live_state_untrusted_base_total counter")?;

        let ingestion_ws_stalls = Counter::with_opts(Opts::new(
            "ingestion_ws_stalls_total",
            "Websocket subscriptions replaced after delivering nothing past the stall limit",
        ))?;
        registry
            .register(Box::new(ingestion_ws_stalls.clone()))
            .context("register ingestion_ws_stalls_total counter")?;

        let live_state_applied = Counter::with_opts(Opts::new(
            "live_state_applied_total",
            "Logs decoded and applied to live pool state",
        ))?;
        registry
            .register(Box::new(live_state_applied.clone()))
            .context("register live_state_applied_total counter")?;

        let live_state_undecodable = Counter::with_opts(Opts::new(
            "live_state_undecodable_total",
            "Logs delivered but not decodable by any known topic",
        ))?;
        registry
            .register(Box::new(live_state_undecodable.clone()))
            .context("register live_state_undecodable_total counter")?;

        let continuity_breaks = Counter::with_opts(Opts::new(
            "continuity_breaks_total",
            "Continuity breaks (reorg or out-of-order log)",
        ))?;
        registry
            .register(Box::new(continuity_breaks.clone()))
            .context("register continuity_breaks_total counter")?;

        let live_state_divergence_bps = HistogramVec::new(
            HistogramOpts::new(
                "live_state_divergence_bps",
                "Signed divergence of log-derived state from a fresh RPC read",
            )
            .buckets(vec![
                -10000.0, -1000.0, -100.0, -5.0, 0.0, 5.0, 100.0, 1000.0, 10000.0,
            ]),
            &["venue"],
        )?;
        registry
            .register(Box::new(live_state_divergence_bps.clone()))
            .context("register live_state_divergence_bps histogram")?;

        let live_state_checks = CounterVec::new(
            Opts::new(
                "live_state_checks_total",
                "State validation attempts by outcome",
            ),
            &["outcome"],
        )?;
        registry
            .register(Box::new(live_state_checks.clone()))
            .context("register live_state_checks_total counter")?;

        let live_state_trusted = Gauge::with_opts(Opts::new(
            "live_state_trusted_pools",
            "Pools whose log-derived state currently passes the state gate",
        ))?;
        registry
            .register(Box::new(live_state_trusted.clone()))
            .context("register live_state_trusted_pools gauge")?;

        let live_state_untrusted = Gauge::with_opts(Opts::new(
            "live_state_untrusted_pools",
            "Pools measured against the chain and found divergent",
        ))?;
        registry
            .register(Box::new(live_state_untrusted.clone()))
            .context("register live_state_untrusted_pools gauge")?;

        let live_state_expired = Gauge::with_opts(Opts::new(
            "live_state_expired_pools",
            "Pools whose verdict aged out; says nothing about correctness",
        ))?;
        registry
            .register(Box::new(live_state_expired.clone()))
            .context("register live_state_expired_pools gauge")?;

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

        let tx_relay_rejected = CounterVec::new(
            Opts::new(
                "tx_relay_rejected_total",
                "Total private-relay rejections (per chain/strategy) — the tx was \
                 refused by the private relay/builder before inclusion",
            ),
            &["chain", "strategy"],
        )?;
        registry
            .register(Box::new(tx_relay_rejected.clone()))
            .context("register tx_relay_rejected_total counter")?;

        let rpc_errors = CounterVec::new(
            Opts::new("rpc_errors_total", "Total RPC errors observed (per chain)"),
            &["chain"],
        )?;
        registry
            .register(Box::new(rpc_errors.clone()))
            .context("register rpc_errors_total counter")?;

        // Splits native-price probes by outcome. `unknown` rising means the
        // engine is flying blind on pricing (transport failures), which
        // silently starves both cycle detection and candidate evaluation —
        // previously the single most consequential unobserved failure mode.
        let native_price_probes = CounterVec::new(
            Opts::new(
                "native_price_probes_total",
                "Native-price probe outcomes (per chain/result: priced|no_route|unknown)",
            ),
            &["chain", "result"],
        )?;
        registry
            .register(Box::new(native_price_probes.clone()))
            .context("register native_price_probes_total counter")?;

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

        let populate_full_total = CounterVec::new(
            Opts::new(
                "populate_full_total",
                "Full edge populate runs (all pools re-quoted)",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(populate_full_total.clone()))
            .context("register populate_full_total counter")?;

        let populate_incremental_total = CounterVec::new(
            Opts::new(
                "populate_incremental_total",
                "Incremental edge populate runs (touched pools only)",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(populate_incremental_total.clone()))
            .context("register populate_incremental_total counter")?;

        let populate_skipped_total = CounterVec::new(
            Opts::new(
                "populate_skipped_total",
                "Populate runs skipped (digest unchanged, no touched pools)",
            ),
            &["chain"],
        )?;
        registry
            .register(Box::new(populate_skipped_total.clone()))
            .context("register populate_skipped_total counter")?;

        let sim_revm_success_total = Counter::with_opts(Opts::new(
            "sim_revm_success_total",
            "Successful revm fork simulations",
        ))?;
        registry
            .register(Box::new(sim_revm_success_total.clone()))
            .context("register sim_revm_success_total counter")?;

        let sim_revm_failure_total = Counter::with_opts(Opts::new(
            "sim_revm_failure_total",
            "Failed revm fork simulations (revert/halt/error)",
        ))?;
        registry
            .register(Box::new(sim_revm_failure_total.clone()))
            .context("register sim_revm_failure_total counter")?;

        let sim_revm_fallback_total = Counter::with_opts(Opts::new(
            "sim_revm_fallback_total",
            "Revm sim skipped; eth_call fallback used",
        ))?;
        registry
            .register(Box::new(sim_revm_fallback_total.clone()))
            .context("register sim_revm_fallback_total counter")?;

        let sim_revm_prefetch_accounts_total = Counter::with_opts(Opts::new(
            "sim_revm_prefetch_accounts_total",
            "Accounts prefetched before revm fork simulation",
        ))?;
        registry
            .register(Box::new(sim_revm_prefetch_accounts_total.clone()))
            .context("register sim_revm_prefetch_accounts_total counter")?;

        let sim_revm_prefetch_ms = Histogram::with_opts(HistogramOpts::new(
            "sim_revm_prefetch_ms",
            "Latency of revm account prefetch in milliseconds",
        ))?;
        registry
            .register(Box::new(sim_revm_prefetch_ms.clone()))
            .context("register sim_revm_prefetch_ms histogram")?;

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
            ingestion_ws_stalls,
            live_state_untrusted_base,
            ingestion_resubscribes_avoided,
            live_state_applied,
            live_state_undecodable,
            continuity_breaks,
            live_state_divergence_bps,
            live_state_checks,
            live_state_trusted,
            live_state_untrusted,
            live_state_expired,
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
            tx_relay_rejected,
            rpc_errors,
            native_price_probes,
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
            populate_full_total,
            populate_incremental_total,
            populate_skipped_total,
            sim_revm_success_total,
            sim_revm_failure_total,
            sim_revm_fallback_total,
            sim_revm_prefetch_accounts_total,
            sim_revm_prefetch_ms,
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

    pub fn record_relay_rejected(&self, chain: &str, strategy: &str) {
        self.tx_relay_rejected
            .with_label_values(&[chain, strategy])
            .inc();
    }

    pub fn record_tx_reverted(&self, chain: &str, strategy: &str) {
        self.tx_reverted.with_label_values(&[chain, strategy]).inc();
    }

    pub fn record_rpc_error(&self, chain: &str) {
        self.rpc_errors.with_label_values(&[chain]).inc();
    }

    /// `result` is `NativePriceProbe::label()` — `priced`, `no_route`, or
    /// `unknown`. A sustained `unknown` rate is the signal that pricing is
    /// starved and detection is silently losing edges.
    pub fn record_native_price_probe(&self, chain: &str, result: &str) {
        self.native_price_probes
            .with_label_values(&[chain, result])
            .inc();
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

#[cfg(test)]
mod tests {

    #[test]
    fn validation_metrics_are_registered_and_labelled() {
        let m = Metrics::new().expect("metrics");
        m.live_state_divergence_bps
            .with_label_values(&["cl"])
            .observe(3.0);
        m.live_state_checks.with_label_values(&["measured"]).inc();
        m.live_state_checks.with_label_values(&["unreachable"]).inc();
        m.live_state_trusted.set(5.0);
        m.live_state_untrusted.set(2.0);
        assert_eq!(m.live_state_trusted.get(), 5.0);
        assert_eq!(
            m.live_state_checks.with_label_values(&["measured"]).get(),
            1.0
        );
    }

    #[test]
    fn shadow_metrics_are_registered_and_start_at_zero() {
        let m = Metrics::new().expect("metrics");
        assert_eq!(m.live_state_applied.get(), 0.0);
        assert_eq!(m.live_state_undecodable.get(), 0.0);
        assert_eq!(m.continuity_breaks.get(), 0.0);
        m.live_state_applied.inc();
        assert_eq!(m.live_state_applied.get(), 1.0);
    }

    use super::*;

    // Registration smoke test: every counter/gauge/histogram (including the new
    // tx_relay_rejected funnel counter) must register without a name collision,
    // and the funnel recorders must accept their labels and gather cleanly.
    #[test]
    fn metrics_register_and_record_funnel_stages() {
        let metrics = Metrics::new().expect("metrics register without collision");

        metrics.record_detection();
        metrics.record_opportunity_seen("base", "cycle");
        metrics.record_simulation("base", "cycle", true);
        metrics.record_simulation("base", "cycle", false);
        metrics.record_tx_sent("base", "cycle");
        metrics.record_tx_confirmed("base", "cycle");
        metrics.record_tx_reverted("base", "cycle");
        metrics.record_relay_rejected("base", "cycle");
        metrics.record_relay_rejected("base", "unknown");

        // The relay-reject counter is exported under its Prometheus name.
        let families = metrics.registry.gather();
        assert!(
            families
                .iter()
                .any(|f| f.get_name() == "tx_relay_rejected_total"),
            "tx_relay_rejected_total must be registered and gatherable"
        );
    }
}
