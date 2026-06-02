mod accounting;
mod bridge;
mod capital;
mod chain;
#[cfg(test)]
mod config_validation;
mod discovery;
mod fees;
mod flash_loan;
mod graph;
mod health;
mod hot_path;
mod hot_pools;
mod ingestion;
mod liquidations;
mod liquidity_cache;
mod math;
mod metrics;
mod ops_inputs;
mod plan;
mod pool_store;
mod quote_balancer;
mod quote_curve;
mod quote_solidly;
mod quote_univ2;
mod quote_univ3;
mod quote_univ4;
mod registry;
mod sandwich;
mod sizing;
mod token_refresh;
mod util;
mod venue_adapter;
mod venues;

use accounting::Accounting;
use anyhow::{anyhow, ensure, Context, Result};
use capital::{CapitalManager, CapitalSnapshot};
use chain::{
    load_chain_from_sources, probe_aave_pool_interface, production_mode_enabled,
    secret_looks_placeholder, validate_chain_cfg, ChainCfg,
};
use ethers::abi::{decode, ParamType};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::{
    abi::Token,
    prelude::*,
    providers::{JsonRpcClient, ProviderError, Ws},
    types::{Address, BlockId, BlockNumber, Bytes, NameOrAddress, TxHash, H256, U256, U64},
};
use math::mul_div;
use plan::{build_plan_for_cycle, GenericPreAction, JitConfig, StepData};
use std::{
    cmp::Ordering,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    convert::TryFrom,
    fs::OpenOptions,
    future::Future,
    io::Write,
    path::PathBuf,
    pin::Pin,
    str::FromStr,
    sync::{Arc, Mutex as StdMutex, MutexGuard},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{self, AsyncBufReadExt, BufReader},
    sync::{mpsc, mpsc::error::TryRecvError, watch, Mutex, OnceCell, Semaphore},
    time::{sleep, timeout, Duration},
};

use graph::{canonicalize_cycle, BellmanFordLimits, Edge, Graph, VenueEdge};
use hot_path::{HotPathCache, ProfitabilitySnapshot};
use hot_pools::{log_hot_pool_refresh, rank_univ2_pools, rank_univ3_pools, HotPoolConfig};
use ingestion::{spawn_pending_tx_monitor, MonitoredPool, PoolMonitor};
use pool_store::{
    load_pool_records, pool_data_path, univ2_configs_from_records, PoolRecord, ResolvedUniV2PoolCfg,
};
use registry::{apply_pool_env_overrides, maybe_load_registry, parse_address, RegistryChain};
use serde::Serialize;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use venues::populate_edges;

use crate::bridge::BridgePlanner;
use crate::discovery::{LowLiquidityPool, LowLiquidityScanner};
use crate::fees::{ArbitrumFeeConfig, FeeEstimate, FeeEstimator};
use crate::flash_loan::{FlashLoanProvider, FlashLoanQuote, FlashLoanSelection};
use crate::health::{HealthThresholds, HealthTracker};
use crate::liquidations::LiquidationMonitor;
use crate::liquidity_cache::PoolDepthCache;
use crate::metrics::Metrics;
use crate::ops_inputs::load_ops_inputs;
use crate::quote_univ3::{UniQuoter, UniV3ValidationConfig, FEE_TIERS};
use crate::sandwich::{SandwichMonitor, SandwichOpportunity};
use crate::sizing::{optimize_trade_size, OptimizeTradeParams};
use crate::token_refresh::TokenList;
use crate::util::{
    CandidateDecisionLogger, CandidateDecisionRecord,
    coerce_http_url, coerce_ws_url, connect_ws_provider_with_fallbacks, erc20_decimals,
    parse_endpoint_list, u256_to_f64, NativePrice, TradeSizing,
};

use crate::{quote_balancer::BalQuote, quote_curve::CurveQuote};
use arb_exec::abi::{ExecutorLoan, ExecutorPlan, ExecutorStep, MultiVenueArbExecutor};

const JIT_PRESWAP_ESTIMATED_GAS: u64 = 160_000;
const JIT_LP_ADD_ESTIMATED_GAS: u64 = 260_000;
const JIT_LP_REMOVE_ESTIMATED_GAS: u64 = 220_000;
const DEFAULT_UNIV3_QUOTE_CONCURRENCY: usize = 32;
const EXECUTOR_OP_UNIV3: u8 = 0;
const EXECUTOR_OP_BALANCER: u8 = 1;
const EXECUTOR_OP_GENERIC: u8 = 2;
const EXECUTOR_OP_BRIDGE: u8 = 3;
const EXECUTOR_OP_JIT_LP_ADD: u8 = 4;
const EXECUTOR_OP_JIT_LP_REMOVE: u8 = 5;

#[derive(Clone, Copy, Debug)]
struct FeatureGate {
    cycle_arb: bool,
    backrun: bool,
    sandwich: bool,
    liquidations: bool,
    bridge: bool,
}

#[derive(Debug)]
enum PlannerAllocationError {
    MultiLoanNotSupported { allocations: usize },
}

impl std::fmt::Display for PlannerAllocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlannerAllocationError::MultiLoanNotSupported { allocations } => write!(
                f,
                "executor supports a single flash loan (allocations={allocations})"
            ),
        }
    }
}

impl std::error::Error for PlannerAllocationError {}

fn ensure_single_loan_allocation(
    allocations: &[FlashLoanSelection],
) -> Result<(), PlannerAllocationError> {
    if allocations.len() == 1 {
        Ok(())
    } else {
        Err(PlannerAllocationError::MultiLoanNotSupported {
            allocations: allocations.len(),
        })
    }
}

impl FeatureGate {
    fn from_env() -> Self {
        Self {
            cycle_arb: read_feature_flag("FEATURE_CYCLE_ARB", true),
            backrun: read_feature_flag("FEATURE_BACKRUN", false),
            sandwich: read_feature_flag("FEATURE_SANDWICH", false),
            liquidations: read_feature_flag("FEATURE_LIQUIDATIONS", false),
            bridge: read_feature_flag("FEATURE_BRIDGE", false),
        }
    }
}

fn read_feature_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
}

#[derive(Clone, Debug)]
pub(crate) struct ExecutionSummary {
    pub start_token: Address,
    pub amount_in: U256,
    pub hops: usize,
    pub gross: U256,
    pub net: U256,
    pub gross_native: U256,
    pub net_native: U256,
    pub gas_cost: U256,
    pub gas_cost_native: U256,
    pub gas_limit: U256,
    pub gas_price: U256,
    pub max_fee_per_gas: Option<U256>,
    pub max_priority_fee_per_gas: Option<U256>,
    pub tx_hash: H256,
    pub edges_scanned: usize,
    pub max_slippage_bps: u32,
    pub inclusion_latency_ms: u64,
    pub private_relay_rejected: bool,
    pub competition_pressure: f64,
    pub competition_buffer_wei: U256,
    pub strategy: String,
    pub venue_path: Vec<String>,
}

#[derive(Serialize)]
struct TradeJsonLog {
    chain: String,
    relay: String,
    strategy: String,
    venue_path: Vec<String>,
    hops: usize,
    amount_in_wei: String,
    gross_profit_wei: String,
    net_profit_wei: String,
    gas_cost_wei: String,
    gas_price_wei: String,
    gas_limit: String,
    max_fee_per_gas_wei: String,
    max_priority_fee_per_gas_wei: String,
    tx_hash: String,
}

fn emit_trade_json(chain: &str, relay: &str, summary: &ExecutionSummary) {
    let payload = TradeJsonLog {
        chain: chain.to_string(),
        relay: relay.to_string(),
        strategy: summary.strategy.clone(),
        venue_path: summary.venue_path.clone(),
        hops: summary.hops,
        amount_in_wei: summary.amount_in.to_string(),
        gross_profit_wei: summary.gross.to_string(),
        net_profit_wei: summary.net.to_string(),
        gas_cost_wei: summary.gas_cost.to_string(),
        gas_price_wei: summary.gas_price.to_string(),
        gas_limit: summary.gas_limit.to_string(),
        max_fee_per_gas_wei: summary
            .max_fee_per_gas
            .map(|value| value.to_string())
            .unwrap_or_default(),
        max_priority_fee_per_gas_wei: summary
            .max_priority_fee_per_gas
            .map(|value| value.to_string())
            .unwrap_or_default(),
        tx_hash: format!("{:#x}", summary.tx_hash),
    };
    if let Ok(json) = serde_json::to_string(&payload) {
        info!(target: "trade_json", payload = %json, "trade");
    }
}

#[derive(Clone, Copy, Debug)]
enum RunnerState {
    Idle,
    Running,
    Error,
    Stopped,
}

#[derive(Clone, Debug)]
struct StatusSnapshot {
    state: RunnerState,
    detail: String,
    last_execution: Option<ExecutionSummary>,
}

impl StatusSnapshot {
    fn new(
        state: RunnerState,
        detail: impl Into<String>,
        last_execution: Option<ExecutionSummary>,
    ) -> Self {
        Self {
            state,
            detail: detail.into(),
            last_execution,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum MevRole {
    #[default]
    Searcher,
    Filler,
}

impl std::str::FromStr for MevRole {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "searcher" => Ok(MevRole::Searcher),
            "filler" => Ok(MevRole::Filler),
            other => Err(anyhow!("invalid MEV role `{other}`")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strategy {
    Arb,
    Backrun,
    Liquidation,
}

impl Strategy {
    fn as_str(self) -> &'static str {
        match self {
            Strategy::Arb => "arb",
            Strategy::Backrun => "backrun",
            Strategy::Liquidation => "liquidation",
        }
    }
}

#[derive(Clone)]
enum BroadcastEndpoint {
    #[allow(dead_code)]
    Public,
    Private {
        providers: Vec<RelayEndpoint>,
    },
}

impl BroadcastEndpoint {
    fn label(&self) -> &'static str {
        match self {
            BroadcastEndpoint::Public => "public",
            BroadcastEndpoint::Private { .. } => "private",
        }
    }
}

const DEFAULT_PRIVATE_RELAYS: &[(&str, &str)] = &[
    ("flashbots", "https://relay.flashbots.net"),
    ("beaverbuild", "https://rpc.beaverbuild.org"),
    ("builder0x69", "https://builder0x69.io"),
    ("rsync-builder", "https://rpc.rsync-builder.xyz"),
    (
        "alchemy",
        "https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_API_KEY}",
    ),
];

#[derive(Clone, Copy, Debug)]
enum BroadcastMode {
    Public,
    Private,
}

fn expected_univ3_edge_upper_bound(pool_count: usize) -> usize {
    pool_count.saturating_mul(2)
}

fn chain_hot_pool_base_cap(chain_name: &str) -> usize {
    match chain_name.to_ascii_lowercase().as_str() {
        "ethereum" => 420,
        "arbitrum" | "base" => 360,
        "optimism" => 320,
        "linea" | "ink" | "abstract" => 260,
        _ => 280,
    }
}

/// Chain-aware event sampling: L2s run hot at high sample rates; Ethereum throttles RPC load.
fn derive_chain_event_sampling_rate(chain_name: &str, configured_rate: f64) -> f64 {
    let configured = configured_rate.clamp(0.05, 1.0);
    match chain_name.to_ascii_lowercase().as_str() {
        // Mainnet blocks are slower but logs are denser; cap sampling to protect quote budget.
        "ethereum" => configured.min(0.55),
        "base" | "arbitrum" | "optimism" => configured.max(0.85),
        _ => configured,
    }
}

/// Ethereum needs wider execution windows; L2s stay tight for sub-block latency.
fn derive_chain_time_budget_ms(
    chain_name: &str,
    search_ms: u64,
    quoting_ms: u64,
    simulation_ms: u64,
) -> (u64, u64, u64) {
    match chain_name.to_ascii_lowercase().as_str() {
        "ethereum" => (
            search_ms.max(400),
            quoting_ms.max(350),
            simulation_ms.max(450),
        ),
        "base" | "arbitrum" | "optimism" => (
            search_ms.min(250),
            quoting_ms.min(150),
            simulation_ms.min(200),
        ),
        _ => (search_ms, quoting_ms, simulation_ms),
    }
}

fn univ3_quote_concurrency() -> usize {
    std::env::var("UNIV3_QUOTE_CONCURRENCY")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_UNIV3_QUOTE_CONCURRENCY)
}

fn derive_chain_hot_pool_cap(chain_name: &str, quote_budget_ms: u64) -> usize {
    let base = chain_hot_pool_base_cap(chain_name) as f64;
    let budget_scale = (quote_budget_ms as f64 / 250.0).clamp(0.6, 2.0);
    (base * budget_scale).round() as usize
}

fn derive_chain_max_edges_hot(chain_name: &str, quote_budget_ms: u64) -> usize {
    let hot_pool_cap = derive_chain_hot_pool_cap(chain_name, quote_budget_ms);
    let edge_headroom = match chain_name.to_ascii_lowercase().as_str() {
        "ethereum" => 10,
        "arbitrum" | "base" => 9,
        "optimism" => 8,
        _ => 7,
    };
    hot_pool_cap.saturating_mul(edge_headroom).max(1_200)
}

#[derive(Clone, Copy, Debug, Default)]
struct GraphDigest {
    edges: usize,
    active_edges: usize,
    weight_sum: i128,
    weight_abs_sum: i128,
}

fn graph_digest(graph: &Graph) -> GraphDigest {
    let mut digest = GraphDigest {
        edges: graph.edges.len(),
        ..Default::default()
    };
    for edge in graph.edges.iter() {
        if edge.active {
            digest.active_edges = digest.active_edges.saturating_add(1);
            let weight = i128::from(edge.weight);
            digest.weight_sum = digest.weight_sum.saturating_add(weight);
            let abs = if weight == i128::MIN {
                i128::MAX
            } else {
                weight.abs()
            };
            digest.weight_abs_sum = digest.weight_abs_sum.saturating_add(abs);
        }
    }
    digest
}

fn graph_changed_significantly(previous: Option<GraphDigest>, current: GraphDigest) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.edges != current.edges || previous.active_edges != current.active_edges {
        return true;
    }
    if previous.weight_abs_sum == 0 {
        return current.weight_abs_sum != 0;
    }
    let delta = (current.weight_abs_sum - previous.weight_abs_sum).abs();
    let threshold = previous.weight_abs_sum / 20;
    delta > threshold
}

fn build_univ3_price_path(
    token_in: Address,
    token_out: Address,
    fee: u32,
) -> Vec<(Address, Option<u32>)> {
    vec![(token_in, None), (token_out, Some(fee))]
}

fn monitored_pools_from_configs(pools: &[ResolvedUniV2PoolCfg]) -> Vec<MonitoredPool> {
    let mut unique: HashMap<Address, MonitoredPool> = HashMap::new();
    for pool in pools {
        unique.entry(pool.pair).or_insert(MonitoredPool {
            pair: pool.pair,
            token_in: pool.token_in,
            token_out: pool.token_out,
            fee_bps: pool.fee_bps,
        });
    }
    unique.into_values().collect()
}

fn build_hub_tokens(ops_inputs: &crate::ops_inputs::OpsInputs) -> HashSet<Address> {
    let mut hubs = HashSet::new();
    let default_symbols = ["WETH", "USDC", "USDT", "DAI"];
    for seed in ops_inputs.universe.token_seeds.iter() {
        if let Some(symbol) = seed.symbol.as_deref() {
            if default_symbols
                .iter()
                .any(|entry| entry.eq_ignore_ascii_case(symbol))
            {
                if let Ok(addr) = Address::from_str(&seed.address) {
                    hubs.insert(addr);
                }
            }
        }
    }
    for addr in ops_inputs.universe.hub_tokens.iter() {
        if let Ok(parsed) = Address::from_str(addr) {
            hubs.insert(parsed);
        }
    }
    hubs
}

fn build_dynamic_token_whitelist(
    hot_univ2: &[ResolvedUniV2PoolCfg],
    hot_univ3: &[PoolRecord],
    base: &HashSet<Address>,
    mandatory_tokens: &HashSet<Address>,
    dynamic_top_tokens_30d: usize,
    max_tokens: usize,
) -> HashSet<Address> {
    if max_tokens == 0 {
        return HashSet::new();
    }

    let mut whitelist = HashSet::with_capacity(max_tokens.max(8));
    for token in mandatory_tokens.iter().copied() {
        if whitelist.len() >= max_tokens {
            return whitelist;
        }
        whitelist.insert(token);
    }

    let mut token_scores: HashMap<Address, u64> = HashMap::new();
    let univ2_total = hot_univ2.len() as u64;
    for (idx, pool) in hot_univ2.iter().enumerate() {
        let weight = univ2_total.saturating_sub(idx as u64).max(1);
        token_scores
            .entry(pool.token_in)
            .and_modify(|score| *score = score.saturating_add(weight))
            .or_insert(weight);
        token_scores
            .entry(pool.token_out)
            .and_modify(|score| *score = score.saturating_add(weight))
            .or_insert(weight);
    }
    let univ3_total = hot_univ3.len() as u64;
    for (idx, pool) in hot_univ3.iter().enumerate() {
        let weight = univ3_total.saturating_sub(idx as u64).max(1);
        token_scores
            .entry(pool.token0)
            .and_modify(|score| *score = score.saturating_add(weight))
            .or_insert(weight);
        token_scores
            .entry(pool.token1)
            .and_modify(|score| *score = score.saturating_add(weight))
            .or_insert(weight);
    }

    let mut ranked_tokens: Vec<(Address, u64)> = token_scores.into_iter().collect();
    ranked_tokens.sort_unstable_by(|(token_a, score_a), (token_b, score_b)| {
        score_b.cmp(score_a).then_with(|| token_a.cmp(token_b))
    });

    let mut added_dynamic = 0usize;
    for (token, _) in ranked_tokens {
        if added_dynamic >= dynamic_top_tokens_30d || whitelist.len() >= max_tokens {
            break;
        }
        if whitelist.insert(token) {
            added_dynamic = added_dynamic.saturating_add(1);
        }
    }

    for token in base.iter().copied() {
        if whitelist.len() >= max_tokens {
            return whitelist;
        }
        whitelist.insert(token);
    }

    whitelist
}

fn sanitize_token_whitelist_cap(configured_cap: usize) -> usize {
    configured_cap.max(64)
}

fn sanitize_dynamic_top_tokens_30d(configured: usize) -> usize {
    configured.max(60)
}

fn is_usd_stable_symbol(symbol: &str) -> bool {
    matches!(
        symbol.to_ascii_uppercase().as_str(),
        "USDC"
            | "USDC.E"
            | "USDT"
            | "DAI"
            | "USDB"
            | "LUSD"
            | "FRAX"
            | "TUSD"
            | "USDE"
            | "FDUSD"
            | "PYUSD"
            | "CRVUSD"
            | "GHO"
            | "SUSD"
            | "USDL"
            | "USD0"
            | "USDY"
    )
}

fn build_mandatory_universe_tokens(ops_inputs: &crate::ops_inputs::OpsInputs) -> HashSet<Address> {
    let include_stables = ops_inputs.universe.include_usd_stablecoins.unwrap_or(true);
    let include_weth = ops_inputs.universe.include_weth.unwrap_or(true);
    let include_wbtc = ops_inputs.universe.include_wbtc.unwrap_or(true);

    let mut mandatory = HashSet::new();
    for seed in &ops_inputs.universe.token_seeds {
        let Ok(address) = Address::from_str(&seed.address) else {
            continue;
        };
        let Some(symbol) = seed.symbol.as_deref() else {
            continue;
        };
        let upper = symbol.to_ascii_uppercase();
        if (include_stables && is_usd_stable_symbol(upper.as_str()))
            || (include_weth && upper == "WETH")
            || (include_wbtc && upper == "WBTC")
        {
            mandatory.insert(address);
        }
    }
    mandatory
}

fn collect_univ3_fee_tiers(
    ops_inputs: &crate::ops_inputs::OpsInputs,
    chain_name: &str,
) -> HashSet<u32> {
    let mut tiers = HashSet::new();
    let Some(chain) = ops_inputs.chain_inputs(chain_name) else {
        return tiers;
    };
    for venue in &chain.venues {
        if !matches!(venue.kind, Some(crate::ops_inputs::VenueKind::Univ3Like)) {
            continue;
        }
        if let Some(venue_tiers) = venue.fee_tiers.as_ref() {
            tiers.extend(venue_tiers.iter().copied());
        }
    }
    tiers
}

fn filter_cycles_by_hubs(
    cycles: Vec<Vec<usize>>,
    graph: &Graph,
    hub_tokens: &HashSet<Address>,
) -> Vec<Vec<usize>> {
    let strict_intermediates = read_feature_flag("STRICT_HUB_INTERMEDIATES", false);
    let strict_start = read_feature_flag("STRICT_START_TOKEN_HUB_ONLY", false);
    if hub_tokens.is_empty() && !strict_start {
        return cycles;
    }
    cycles
        .into_iter()
        .filter(|cycle| {
            let Some(start_ix) = cycle.first().copied() else {
                return false;
            };
            let Some(start_token) = graph.nodes.get(start_ix) else {
                return false;
            };
            if strict_start && !hub_tokens.contains(start_token) {
                return false;
            }
            if !strict_intermediates || cycle.len() < 3 {
                return true;
            }
            let last = cycle.len().saturating_sub(1);
            for (idx, node) in cycle.iter().enumerate() {
                if idx == 0 || idx == last {
                    continue;
                }
                if let Some(token) = graph.nodes.get(*node) {
                    if !hub_tokens.contains(token) {
                        return false;
                    }
                }
            }
            true
        })
        .collect()
}

fn cycle_rejected_by_hub_filter(
    cycle: &[usize],
    graph: &Graph,
    hub_tokens: &HashSet<Address>,
) -> bool {
    let strict_intermediates = read_feature_flag("STRICT_HUB_INTERMEDIATES", false);
    let strict_start = read_feature_flag("STRICT_START_TOKEN_HUB_ONLY", false);
    if hub_tokens.is_empty() && !strict_start {
        return false;
    }
    let Some(start_ix) = cycle.first().copied() else {
        return true;
    };
    let Some(start_token) = graph.nodes.get(start_ix) else {
        return true;
    };
    if strict_start && !hub_tokens.contains(start_token) {
        return true;
    }
    if !strict_intermediates || cycle.len() < 3 {
        return false;
    }
    let last = cycle.len().saturating_sub(1);
    for (idx, node) in cycle.iter().enumerate() {
        if idx == 0 || idx == last {
            continue;
        }
        if let Some(token) = graph.nodes.get(*node) {
            if !hub_tokens.contains(token) {
                return true;
            }
        }
    }
    false
}

fn start_token_pricing_reliable(
    start_token: Address,
    wrapped_native: Address,
    native_price: NativePrice,
) -> bool {
    if start_token == wrapped_native {
        return true;
    }
    native_price.is_reliable()
}

fn should_reuse_cached_native_price(
    token: Address,
    wrapped_native: Address,
    native_price: NativePrice,
) -> bool {
    token == wrapped_native || native_price.is_reliable()
}

fn should_cache_native_price(
    token: Address,
    wrapped_native: Address,
    native_price: NativePrice,
) -> bool {
    token == wrapped_native || native_price.is_reliable()
}

fn cap_cycles_per_start(
    cycles: Vec<Vec<usize>>,
    graph: &Graph,
    topk_per_token: usize,
) -> Vec<Vec<usize>> {
    if topk_per_token == 0 {
        return Vec::new();
    }
    let mut per_start: HashMap<Address, Vec<(i64, Vec<usize>)>> = HashMap::new();
    for cycle in cycles {
        let Some(start_ix) = cycle.first() else {
            continue;
        };
        let Some(start_token) = graph.nodes.get(*start_ix) else {
            continue;
        };
        let weight = graph.cycle_weight(&cycle).unwrap_or(i64::MAX);
        per_start
            .entry(*start_token)
            .or_default()
            .push((weight, cycle));
    }
    let mut capped = Vec::new();
    for entries in per_start.values_mut() {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, cycle) in entries.drain(..).take(topk_per_token) {
            capped.push(cycle);
        }
    }
    capped
}

fn map_cycle_addresses_to_indices(graph: &Graph, cycle: &[Address]) -> Option<Vec<usize>> {
    if cycle.len() < 2 {
        return None;
    }
    let mut indices = Vec::with_capacity(cycle.len() + 1);
    for addr in cycle.iter() {
        let idx = graph.ix.get(addr)?;
        indices.push(*idx);
    }
    if indices.first() != indices.last() {
        if let Some(first) = indices.first().copied() {
            indices.push(first);
        }
    }
    Some(indices)
}

fn cycle_indices_to_addresses(graph: &Graph, cycle: &[usize]) -> Vec<Address> {
    cycle
        .iter()
        .filter_map(|idx| graph.nodes.get(*idx).copied())
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct EdgeScoreWeights {
    liquidity: f64,
    profitability: f64,
    slippage: f64,
}

#[derive(Clone, Debug)]
struct ScoredEdge {
    idx: usize,
    score: f64,
    liquidity: U256,
    slippage_bps: u32,
    weight: i64,
}

impl PartialEq for ScoredEdge {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx
    }
}

impl Eq for ScoredEdge {}

impl PartialOrd for ScoredEdge {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredEdge {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.liquidity.cmp(&other.liquidity))
            .then_with(|| other.slippage_bps.cmp(&self.slippage_bps))
            .then_with(|| other.weight.cmp(&self.weight))
            .then_with(|| self.idx.cmp(&other.idx))
    }
}

fn edge_fee_bps(edge: &Edge) -> Option<u32> {
    match &edge.venue {
        VenueEdge::UniV3 { path, .. } => {
            if path.len() < 2 {
                return None;
            }
            path.iter()
                .skip(1)
                .find_map(|(_, maybe_fee)| maybe_fee.as_ref().copied())
        }
        _ => None,
    }
}

fn edge_quality_score(edge: &Edge, profitability: f64, weights: EdgeScoreWeights) -> f64 {
    let liquidity = u256_to_f64(edge.max_input).max(0.0);
    let liquidity_score = (1.0 + liquidity).ln();
    let slippage = edge
        .observed_slippage_bps
        .max(edge.tolerance_bps)
        .min(10_000) as f64;
    (weights.liquidity * liquidity_score) + (weights.profitability * profitability)
        - (weights.slippage * slippage)
}

async fn prune_edges_by_quality(
    graph: &mut Graph,
    max_edges: usize,
    max_slippage_bps: u32,
    min_score: f64,
    weights: EdgeScoreWeights,
    profitability: &ProfitabilitySnapshot,
) {
    if max_edges == 0 || graph.edges.len() <= max_edges {
        return;
    }

    let mut heap: BinaryHeap<Reverse<ScoredEdge>> = BinaryHeap::with_capacity(max_edges + 1);
    for (idx, edge) in graph.edges.iter().enumerate() {
        if !edge.active {
            continue;
        }
        if edge.observed_slippage_bps > max_slippage_bps {
            continue;
        }
        let profit_score = edge_fee_bps(edge)
            .map(|fee| profitability.score(edge.from, edge.to, fee))
            .unwrap_or(0.0);
        let score = edge_quality_score(edge, profit_score, weights);
        if score < min_score {
            continue;
        }
        heap.push(Reverse(ScoredEdge {
            idx,
            score,
            liquidity: edge.max_input,
            slippage_bps: edge.observed_slippage_bps,
            weight: edge.weight,
        }));
        if heap.len() > max_edges {
            heap.pop();
        }
    }

    let mut keep = HashSet::new();
    for Reverse(entry) in heap.into_sorted_vec() {
        if keep.len() >= max_edges {
            break;
        }
        keep.insert(entry.idx);
    }

    if keep.is_empty() {
        return;
    }

    let mut changed = false;
    for (idx, edge) in graph.edges.iter_mut().enumerate() {
        if edge.active && !keep.contains(&idx) {
            edge.active = false;
            changed = true;
        }
    }

    if changed {
        graph.refresh_incremental_adjacency();
    }
}

async fn build_profitability_snapshot(hot_paths: &HotPathCache) -> ProfitabilitySnapshot {
    hot_paths.profitability_snapshot().await
}

struct ProfitThresholdParams<'a> {
    base_threshold: U256,
    competition_buffer: U256,
    flash_fee_amount: U256,
    slippage_floor: U256,
    has_bridge_step: bool,
    est_gross_after_fee: U256,
    cross_chain_profit_bps: u32,
    cross_chain_min_profit_wei: U256,
    backrun_hint: Option<&'a BackrunHint>,
}

struct DynamicProfitParams<'a> {
    fee: &'a FeeEstimate,
    est_gas: u64,
    est_gross: U256,
    congestion: f64,
    competition: &'a CompetitionSnapshot,
    latency_secs: f64,
    native_price: NativePrice,
}

fn finalize_profit_threshold(params: ProfitThresholdParams<'_>) -> U256 {
    let mut min_profit_requirement = params
        .base_threshold
        .saturating_add(params.competition_buffer)
        .saturating_add(params.flash_fee_amount);

    if params.has_bridge_step {
        let premium = mul_div(
            params.est_gross_after_fee,
            U256::from(params.cross_chain_profit_bps as u64),
            U256::from(10_000u64),
        );
        let bridge_buffer = premium.max(params.cross_chain_min_profit_wei);
        min_profit_requirement = min_profit_requirement.saturating_add(bridge_buffer);
    }

    if let Some(hint) = params.backrun_hint {
        let discounted = mul_div(min_profit_requirement, U256::from(9u64), U256::from(10u64));
        min_profit_requirement = min_profit_requirement.min(discounted);
        info!(
            target: "backrun",
            from = %format!("0x{}", hex::encode(hint.from)),
            to = %format!("0x{}", hex::encode(hint.to)),
            impact_bps = hint.price_impact_bps,
            source = hint.source,
            "Applying backrun hint discount to profit threshold",
        );
    }

    if min_profit_requirement < params.slippage_floor {
        min_profit_requirement = params.slippage_floor;
    }

    min_profit_requirement
}

fn resolve_slippage_and_profit_floors(
    plan_cycle_slippage_bps: u32,
    sizing_max_slippage_bps: u32,
    executor_profit_floor_bps: u32,
) -> (u32, u32) {
    (
        plan_cycle_slippage_bps.max(sizing_max_slippage_bps),
        executor_profit_floor_bps,
    )
}

#[derive(Clone, Copy, Debug)]
enum PrivateSubmissionMethod {
    Bundle,
    PrivateRaw,
}

impl PrivateSubmissionMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bundle => "bundle",
            Self::PrivateRaw => "private_raw",
        }
    }
}

#[derive(Clone)]
enum PrivateRelayProvider {
    Ws(Provider<Ws>),
    FlashbotsHttp(FlashbotsSignedHttp),
    RpcHttp(Provider<Http>),
}

#[derive(Clone)]
struct RelayEndpoint {
    label: String,
    provider: PrivateRelayProvider,
    chain_name: String,
    allow_private_raw_fallback: bool,
}

impl RelayEndpoint {
    fn label(&self) -> &str {
        &self.label
    }
}

impl PrivateRelayProvider {
    async fn connect(endpoint: &str, signer: &LocalWallet) -> Result<Self, ProviderError> {
        if endpoint.starts_with("ws") {
            Provider::<Ws>::connect(endpoint)
                .await
                .map(PrivateRelayProvider::Ws)
        } else if endpoint.starts_with("http") {
            let is_flashbots_style = DEFAULT_PRIVATE_RELAYS
                .iter()
                .any(|(_, url)| endpoint.starts_with(url));
            if is_flashbots_style {
                FlashbotsSignedHttp::new(endpoint, signer)
                    .await
                    .map(PrivateRelayProvider::FlashbotsHttp)
                    .map_err(|err| ProviderError::CustomError(err.to_string()))
            } else {
                Provider::<Http>::try_from(endpoint)
                    .map(PrivateRelayProvider::RpcHttp)
                    .map_err(|err| ProviderError::CustomError(err.to_string()))
            }
        } else {
            Err(ProviderError::CustomError(
                "unsupported private relay scheme".into(),
            ))
        }
    }

    async fn send_bundle_transaction(
        &self,
        raw: Bytes,
        target_block: U64,
        chain_name: &str,
        allow_private_raw_fallback: bool,
    ) -> Result<(TxHash, PrivateSubmissionMethod), ProviderError> {
        let tx_hash = H256::from(ethers::utils::keccak256(&raw));
        match self {
            PrivateRelayProvider::Ws(provider) => {
                let request = build_send_bundle_request(&raw, target_block);
                provider
                    .request::<serde_json::Value, serde_json::Value>(
                        "eth_sendBundle",
                        request["params"].clone(),
                    )
                    .await?;
                Ok((tx_hash, PrivateSubmissionMethod::Bundle))
            }
            PrivateRelayProvider::FlashbotsHttp(provider) => provider
                .send_bundle(raw, target_block)
                .await
                .map(|hash| (hash, PrivateSubmissionMethod::Bundle)),
            PrivateRelayProvider::RpcHttp(provider) => {
                send_private_rpc_bundle(
                    provider,
                    raw,
                    target_block,
                    chain_name,
                    allow_private_raw_fallback,
                )
                .await
            }
        }
    }
}

async fn send_private_rpc_bundle<P: JsonRpcClient>(
    provider: &Provider<P>,
    raw: Bytes,
    target_block: U64,
    chain_name: &str,
    allow_private_raw_fallback: bool,
) -> Result<(TxHash, PrivateSubmissionMethod), ProviderError> {
    let tx_hash = H256::from(ethers::utils::keccak256(&raw));
    let request = build_send_bundle_request(&raw, target_block);
    let bundle_attempt = provider
        .request::<serde_json::Value, serde_json::Value>(
            "eth_sendBundle",
            request["params"].clone(),
        )
        .await;
    match bundle_attempt {
        Ok(_) => Ok((tx_hash, PrivateSubmissionMethod::Bundle)),
        Err(err) if allow_private_raw_fallback => {
            warn!(
                target: "broadcast",
                chain = chain_name,
                error = %err,
                "eth_sendBundle failed on private RPC relay; trying eth_sendRawTransaction fallback"
            );
            provider
                .request::<serde_json::Value, serde_json::Value>(
                    "eth_sendRawTransaction",
                    serde_json::json!([format!("0x{}", hex::encode(&raw))]),
                )
                .await?;
            Ok((tx_hash, PrivateSubmissionMethod::PrivateRaw))
        }
        Err(err) => Err(err),
    }
}

#[derive(Clone)]
struct FlashbotsSignedHttp {
    endpoint: String,
    client: reqwest::Client,
    signer: LocalWallet,
}

#[derive(Debug, serde::Deserialize)]
struct JsonRpcErrorBody {
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct JsonRpcResponse {
    result: Option<String>,
    error: Option<JsonRpcErrorBody>,
}

impl FlashbotsSignedHttp {
    async fn new(endpoint: &str, signer: &LocalWallet) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|err| ProviderError::CustomError(err.to_string()))?;
        Ok(Self {
            endpoint: endpoint.to_string(),
            client,
            signer: signer.clone(),
        })
    }

    async fn send_bundle(&self, raw: Bytes, target_block: U64) -> Result<TxHash, ProviderError> {
        let tx_hash = H256::from(ethers::utils::keccak256(&raw));
        let body = build_send_bundle_request(&raw, target_block);
        self.send_signed_rpc(body).await?;
        Ok(tx_hash)
    }

    async fn send_signed_rpc(&self, body: serde_json::Value) -> Result<String, ProviderError> {
        let body_str = serde_json::to_string(&body)
            .map_err(|err| ProviderError::CustomError(err.to_string()))?;

        let payload_hash = ethers::utils::keccak256(body_str.as_bytes());
        let signature = self
            .signer
            .sign_message(payload_hash)
            .await
            .map_err(|err| ProviderError::CustomError(err.to_string()))?;
        let sig_hex = format!("0x{}", hex::encode(signature.to_vec()));
        let header_val = format!("{}:{}", self.signer.address(), sig_hex);

        let response = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("X-Flashbots-Signature", header_val)
            .body(body_str)
            .send()
            .await
            .map_err(|err| ProviderError::CustomError(err.to_string()))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| ProviderError::CustomError(err.to_string()))?;

        let parsed: JsonRpcResponse = serde_json::from_str(&text)
            .map_err(|err| ProviderError::CustomError(err.to_string()))?;

        if let Some(error) = parsed.error {
            return Err(ProviderError::CustomError(error.message));
        }

        let Some(result) = parsed.result else {
            return Err(ProviderError::CustomError(format!(
                "unexpected response status={} body={}",
                status, text
            )));
        };
        Ok(result)
    }
}

fn build_send_bundle_request(raw: &Bytes, target_block: U64) -> serde_json::Value {
    let raw_hex = format!("0x{}", hex::encode(raw));
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1u32,
        "method": "eth_sendBundle",
        "params": [{
            "txs": [raw_hex],
            "blockNumber": format!("{:#x}", target_block),
        }],
    })
}

#[derive(Clone)]
struct BroadcastConfig {
    endpoint: BroadcastEndpoint,
    role: MevRole,
    filler_priority_fee: Option<U256>,
    searcher_priority_fee: Option<U256>,
    public_jitter_bps: u32,
    private_inclusion_timeout: Duration,
    relay_health: Arc<StdMutex<HealthTracker>>,
}

impl BroadcastConfig {
    fn priority_fee(&self) -> Option<U256> {
        match self.role {
            MevRole::Searcher => self.searcher_priority_fee,
            MevRole::Filler => self.filler_priority_fee,
        }
    }
}

#[derive(Clone)]
struct ShadowConfig {
    enabled: bool,
    log_path: Option<PathBuf>,
    tag: Option<String>,
}

impl ShadowConfig {
    fn disabled() -> Self {
        Self {
            enabled: false,
            log_path: None,
            tag: None,
        }
    }
}

#[derive(Serialize)]
struct ShadowExecutionRecord {
    timestamp_ms: u128,
    chain_env: String,
    tag: Option<String>,
    tx_hash: String,
    to: Option<String>,
    gas_limit: Option<String>,
    max_fee_per_gas: Option<String>,
    max_priority_fee_per_gas: Option<String>,
    gas_price: Option<String>,
    value: Option<String>,
    data_len: usize,
    cycle_start: Option<String>,
    amount_in_wei: Option<String>,
    est_gross_after_fee_wei: Option<String>,
    net_profit_wei: Option<String>,
    gas_cost_wei: Option<String>,
    min_profit_wei: Option<String>,
    max_slippage_bps: Option<u32>,
    hops: Option<usize>,
}

#[derive(Clone)]
struct ShadowPlanMeta {
    start_token: Address,
    amount_in: U256,
    est_gross_after_fee: U256,
    #[allow(dead_code)]
    gas_cost: U256,
    gas_cost_native: U256,
    net_profit: U256,
    hops: usize,
    min_profit: U256,
    max_slippage_bps: u32,
}

#[derive(Clone)]
struct ChaosConfig {
    relay_reject_bps: u32,
    public_reject_bps: u32,
    broadcast_delay_ms: u64,
}

impl ChaosConfig {
    fn should_reject_relay(&self) -> bool {
        self.relay_reject_bps > 0
            && rand::Rng::gen_range(&mut rand::thread_rng(), 0..10_000) < self.relay_reject_bps
    }

    fn should_reject_public(&self) -> bool {
        self.public_reject_bps > 0
            && rand::Rng::gen_range(&mut rand::thread_rng(), 0..10_000) < self.public_reject_bps
    }

    fn broadcast_delay(&self) -> Option<Duration> {
        if self.broadcast_delay_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.broadcast_delay_ms))
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HealthSettings {
    ema_alpha: f64,
    thresholds: HealthThresholds,
}

fn apply_public_mempool_jitter(tx: &mut TypedTransaction, jitter_bps: u32) {
    if jitter_bps == 0 {
        return;
    }

    let spread = jitter_bps as i64;
    let mut rng = rand::thread_rng();
    let adjustment: i64 = rand::Rng::gen_range(&mut rng, -spread..=spread);

    let apply = |value: U256| -> U256 {
        if value.is_zero() {
            return value;
        }
        let scaled = mul_div(
            value,
            U256::from(10_000i64.saturating_add(adjustment) as u64),
            U256::from(10_000u64),
        );
        if scaled.is_zero() {
            value
        } else {
            scaled
        }
    };

    match tx {
        TypedTransaction::Eip1559(inner) => {
            if let Some(max_fee) = inner.max_fee_per_gas {
                inner.max_fee_per_gas = Some(apply(max_fee));
            }
            if let Some(max_priority) = inner.max_priority_fee_per_gas {
                inner.max_priority_fee_per_gas = Some(apply(max_priority));
            }
        }
        TypedTransaction::Eip2930(inner) => {
            if let Some(gp) = inner.tx.gas_price {
                inner.tx.gas_price = Some(apply(gp));
            }
        }
        TypedTransaction::Legacy(inner) => {
            if let Some(gp) = inner.gas_price {
                inner.gas_price = Some(apply(gp));
            }
        }
    }
}

fn apply_gas_parameters(tx: &mut TypedTransaction, gas: &FeeEstimate) {
    match tx {
        TypedTransaction::Eip1559(inner) => {
            if let Some(max_fee) = gas.max_fee_per_gas {
                inner.max_fee_per_gas = Some(max_fee);
            }
            if let Some(max_priority) = gas.max_priority_fee_per_gas {
                inner.max_priority_fee_per_gas = Some(max_priority);
            }

            if inner.max_fee_per_gas.is_none() && inner.max_priority_fee_per_gas.is_none() {
                tx.set_gas_price(gas.gas_price);
            }
        }
        _ => {
            tx.set_gas_price(gas.gas_price);
        }
    }
}

fn lock_unpoison<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!(target: "health", "Mutex poisoned; continuing with last known state");
            poisoned.into_inner()
        }
    }
}

async fn connect_http_provider_with_health(
    label: &str,
    endpoints: &[String],
    max_backoff: Duration,
    health: Arc<StdMutex<HealthTracker>>,
) -> Result<(Provider<Http>, String)> {
    if endpoints.is_empty() {
        return Err(anyhow!("no http endpoints configured for {label}"));
    }

    let mut attempt: u32 = 0;
    loop {
        let ordered: Vec<String> = {
            let tracker = lock_unpoison(health.as_ref());
            let mut sorted: Vec<String> = endpoints.to_vec();
            sorted.sort_by(|a, b| {
                let score_a = tracker.health_score(a);
                let score_b = tracker.health_score(b);
                score_b.partial_cmp(&score_a).unwrap_or(Ordering::Equal)
            });
            sorted
        };

        let mut any_healthy = false;
        for endpoint in ordered {
            let is_healthy = { lock_unpoison(health.as_ref()).is_healthy(&endpoint) };
            if !is_healthy {
                continue;
            }
            any_healthy = true;

            info!(target: "rpc", %label, endpoint = %endpoint, "connecting http endpoint");
            let start = Instant::now();
            match Provider::<Http>::try_from(endpoint.as_str()) {
                Ok(provider) => {
                    lock_unpoison(health.as_ref()).record_success(&endpoint, Some(start.elapsed()));
                    info!(
                        target: "rpc",
                        %label,
                        endpoint = %endpoint,
                        "http endpoint connected"
                    );
                    return Ok((provider, endpoint));
                }
                Err(err) => {
                    lock_unpoison(health.as_ref()).record_failure(
                        &endpoint,
                        true,
                        Some(start.elapsed()),
                    );
                    warn!(
                        target: "rpc",
                        %label,
                        endpoint = %endpoint,
                        error = ?err,
                        "http endpoint connection failed"
                    );
                }
            }
        }

        if !any_healthy {
            warn!(
                target: "rpc",
                %label,
                "all http endpoints marked unhealthy; retrying with backoff"
            );
        }

        attempt = attempt.saturating_add(1);
        let capped = attempt.min(5);
        let backoff_secs = 1u64 << capped;
        let delay = Duration::from_secs(backoff_secs).min(max_backoff);
        error!(
            target: "rpc",
            %label,
            ?delay,
            "all http endpoints failed, backing off before retry"
        );
        sleep(delay).await;
    }
}

async fn connect_private_relays(
    endpoints: &[String],
    chain_name: &str,
    allow_private_raw_fallback: bool,
    max_backoff: Duration,
    signer: &LocalWallet,
) -> Vec<RelayEndpoint> {
    let valid_endpoints: Vec<String> = endpoints
        .iter()
        .filter_map(|endpoint| {
            let trimmed = endpoint.trim();
            if trimmed.is_empty() {
                warn!(target: "broadcast", "Ignoring empty private relay endpoint");
                None
            } else {
                Some(trimmed.to_owned())
            }
        })
        .collect();

    if valid_endpoints.is_empty() {
        return Vec::new();
    }

    const MAX_ATTEMPTS: u32 = 6;
    const MAX_TOTAL_WAIT: Duration = Duration::from_secs(120);

    let started_at = Instant::now();
    let mut attempt: u32 = 0;

    loop {
        attempt = attempt.saturating_add(1);

        let mut providers = Vec::new();
        for endpoint in &valid_endpoints {
            let relay_label = DEFAULT_PRIVATE_RELAYS
                .iter()
                .find_map(|(label, url)| {
                    if endpoint.starts_with(url) {
                        Some((*label).to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| endpoint.to_string());
            info!(
                target: "broadcast",
                endpoint = %endpoint,
                relay = %relay_label,
                "Connecting private relay endpoint"
            );

            match PrivateRelayProvider::connect(endpoint, signer).await {
                Ok(provider) => {
                    info!(
                        target: "broadcast",
                        endpoint = %endpoint,
                        relay = %relay_label,
                        "Private relay endpoint connected"
                    );
                    providers.push(RelayEndpoint {
                        label: relay_label.clone(),
                        provider,
                        chain_name: chain_name.to_string(),
                        allow_private_raw_fallback,
                    });
                }
                Err(err) => {
                    warn!(
                        target: "broadcast",
                        endpoint = %endpoint,
                        error = %err,
                        "Failed to connect private relay endpoint"
                    );
                }
            }
        }

        if !providers.is_empty() {
            return providers;
        }

        if attempt >= MAX_ATTEMPTS || started_at.elapsed() >= MAX_TOTAL_WAIT {
            warn!(
                target: "broadcast",
                attempts = attempt,
                elapsed = ?started_at.elapsed(),
                "Failed to connect to any private relay endpoints after retries"
            );
            break;
        }

        let capped = attempt.saturating_sub(1).min(5);
        let backoff_secs = 1u64 << capped;
        let delay = Duration::from_secs(backoff_secs).min(max_backoff);
        warn!(
            target: "broadcast",
            attempt,
            ?delay,
            "Failed to connect to any private relay endpoints; retrying after backoff"
        );
        sleep(delay).await;
    }

    Vec::new()
}

#[derive(Clone)]
struct PendingSwap {
    amount_in: U256,
    last_seen: Instant,
}

#[derive(Debug)]
enum HealthStatus {
    Healthy { balance: U256, reserve_txs: U256 },
}

#[derive(Debug)]
struct NonceManager<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    provider: Arc<Provider<C>>,
    wallet: Address,
    current: Mutex<Option<U256>>,
    pending: Mutex<HashMap<U256, Instant>>,
}

impl<C> NonceManager<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    fn new(provider: Arc<Provider<C>>, wallet: Address) -> Self {
        Self {
            provider,
            wallet,
            current: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
        }
    }

    async fn get_next(&self) -> Result<U256> {
        let mut current = self.current.lock().await;
        let mut pending = self.pending.lock().await;

        let now = Instant::now();
        // Drop stale in-flight markers (beyond any realistic inclusion window).
        pending.retain(|_, time| now.duration_since(*time) < Duration::from_secs(120));

        // Authoritative starting point: the chain's PENDING nonce accounts for our
        // txs already seen by the node's mempool. Using `Latest` (confirmed) here
        // was the historical bug — it could hand out a nonce still occupied by an
        // unconfirmed tx, causing replacement wars or silently dropped txs.
        let chain_pending = self
            .provider
            .get_transaction_count(self.wallet, Some(BlockNumber::Pending.into()))
            .await
            .context("fetch pending nonce")?;

        // Local high-water mark guards against an RPC pending view that lags our
        // privately-submitted bundles (relay txs may not be in this node's mempool).
        let local_floor = current.unwrap_or(chain_pending);
        let next = std::cmp::max(chain_pending, local_floor);

        *current = Some(next.saturating_add(U256::one()));
        pending.insert(next, now);
        Ok(next)
    }

    async fn mark_confirmed(&self, nonce: U256) {
        let mut pending = self.pending.lock().await;
        pending.remove(&nonce);
    }

    async fn mark_failed(&self, nonce: U256) {
        let mut current = self.current.lock().await;
        let mut pending = self.pending.lock().await;
        pending.remove(&nonce);

        // Gap recovery: if the failed nonce was the highest we allocated and nothing
        // higher is still in flight, reclaim it so the next dispatch reuses it instead
        // of leaving a permanent mempool gap. If the tx actually landed despite the
        // local failure, the next get_next() reconciles via the chain pending nonce
        // (max(chain_pending, local_floor)), so we never reuse a nonce that confirmed.
        let highest_in_flight = pending.keys().copied().max();
        if highest_in_flight.map(|h| h < nonce).unwrap_or(true) {
            *current = Some(nonce);
        }
    }
}

#[derive(Clone, Debug)]
struct CircuitBreakerStatus {
    is_tripped: bool,
    reason: Option<String>,
    hourly_loss_wei: U256,
    daily_loss_wei: U256,
    consecutive_failures: u32,
}

impl CircuitBreakerStatus {
    fn active_reason(&self) -> String {
        self.reason
            .clone()
            .unwrap_or_else(|| "circuit breaker limits exceeded".to_string())
    }
}

#[derive(Debug)]
struct CircuitBreaker {
    hourly_loss_limit: U256,
    daily_loss_limit: U256,
    consecutive_fail_limit: u32,
    hourly_losses: Mutex<VecDeque<(Instant, U256)>>,
    daily_losses: Mutex<VecDeque<(Instant, U256)>>,
    consecutive_failures: Mutex<u32>,
}

impl CircuitBreaker {
    fn new(hourly_loss_limit: U256, daily_loss_limit: U256, consecutive_fail_limit: u32) -> Self {
        Self {
            hourly_loss_limit,
            daily_loss_limit,
            consecutive_fail_limit,
            hourly_losses: Mutex::new(VecDeque::new()),
            daily_losses: Mutex::new(VecDeque::new()),
            consecutive_failures: Mutex::new(0),
        }
    }

    async fn record_failure(&self, loss: U256) -> CircuitBreakerStatus {
        let now = Instant::now();
        {
            let mut hourly = self.hourly_losses.lock().await;
            hourly.push_back((now, loss));
            Self::prune_losses(&mut hourly, now, Duration::from_secs(3600));
        }
        {
            let mut daily = self.daily_losses.lock().await;
            daily.push_back((now, loss));
            Self::prune_losses(&mut daily, now, Duration::from_secs(86400));
        }

        let mut failures = self.consecutive_failures.lock().await;
        *failures = failures.saturating_add(1);
        drop(failures);

        let status = self.current_status().await;
        if status.is_tripped {
            warn!(
                reason = %status.active_reason(),
                hourly_loss = %status.hourly_loss_wei,
                daily_loss = %status.daily_loss_wei,
                consecutive = status.consecutive_failures,
                "Circuit breaker tripped"
            );
        }
        status
    }

    async fn record_success(&self) {
        let mut failures = self.consecutive_failures.lock().await;
        *failures = 0;
    }

    async fn reset(&self) -> CircuitBreakerStatus {
        {
            let mut hourly = self.hourly_losses.lock().await;
            hourly.clear();
        }
        {
            let mut daily = self.daily_losses.lock().await;
            daily.clear();
        }
        {
            let mut failures = self.consecutive_failures.lock().await;
            *failures = 0;
        }
        self.current_status().await
    }

    async fn current_status(&self) -> CircuitBreakerStatus {
        let now = Instant::now();
        let hourly_total = {
            let mut hourly = self.hourly_losses.lock().await;
            Self::prune_losses(&mut hourly, now, Duration::from_secs(3600));
            Self::sum_losses(&hourly)
        };
        let daily_total = {
            let mut daily = self.daily_losses.lock().await;
            Self::prune_losses(&mut daily, now, Duration::from_secs(86400));
            Self::sum_losses(&daily)
        };
        let consecutive_failures = *self.consecutive_failures.lock().await;

        let reason = self.evaluate_reason(hourly_total, daily_total, consecutive_failures);
        let is_tripped = reason.is_some();

        CircuitBreakerStatus {
            is_tripped,
            reason,
            hourly_loss_wei: hourly_total,
            daily_loss_wei: daily_total,
            consecutive_failures,
        }
    }

    fn evaluate_reason(
        &self,
        hourly_total: U256,
        daily_total: U256,
        consecutive_failures: u32,
    ) -> Option<String> {
        if !self.hourly_loss_limit.is_zero() && hourly_total > self.hourly_loss_limit {
            Some(format!(
                "hourly loss {} exceeds limit {}",
                hourly_total, self.hourly_loss_limit
            ))
        } else if !self.daily_loss_limit.is_zero() && daily_total > self.daily_loss_limit {
            Some(format!(
                "daily loss {} exceeds limit {}",
                daily_total, self.daily_loss_limit
            ))
        } else if self.consecutive_fail_limit > 0
            && consecutive_failures > self.consecutive_fail_limit
        {
            Some(format!(
                "consecutive failures {} exceeds limit {}",
                consecutive_failures, self.consecutive_fail_limit
            ))
        } else {
            None
        }
    }

    fn prune_losses(losses: &mut VecDeque<(Instant, U256)>, now: Instant, window: Duration) {
        while let Some((time, _)) = losses.front() {
            if now.duration_since(*time) > window {
                losses.pop_front();
            } else {
                break;
            }
        }
    }

    fn sum_losses(losses: &VecDeque<(Instant, U256)>) -> U256 {
        losses
            .iter()
            .fold(U256::zero(), |acc, (_, loss)| acc.saturating_add(*loss))
    }
}

#[derive(Clone, Debug)]
struct BackrunHint {
    from: Address,
    to: Address,
    amount_in: U256,
    price_impact_bps: u32,
    source: String,
    observed_at: Instant,
}

struct HintParams<'a> {
    from: Address,
    to: Address,
    amount_in: U256,
    estimated_impact: u32,
    source: &'a str,
    now: Instant,
}

struct BackrunMonitor {
    swaps: Arc<Mutex<HashMap<(Address, Address), PendingSwap>>>,
    hints: Arc<Mutex<VecDeque<BackrunHint>>>,
    min_amount: U256,
    min_price_impact_bps: u32,
    tokens: TokenList,
}

impl BackrunMonitor {
    fn new(min_amount: U256, min_price_impact_bps: u32, tokens: TokenList) -> Self {
        Self {
            swaps: Arc::new(Mutex::new(HashMap::new())),
            hints: Arc::new(Mutex::new(VecDeque::new())),
            min_amount,
            min_price_impact_bps,
            tokens,
        }
    }

    async fn record(&self, path: &[Address], amount_in: U256, source: &str) {
        if amount_in < self.min_amount || path.len() < 2 {
            return;
        }
        let tokens = self.tokens.current_set();
        if !path.iter().all(|token| tokens.contains(token)) {
            return;
        }
        let mut swaps = self.swaps.lock().await;
        let now = Instant::now();
        if let Some(&from) = path.first() {
            if let Some(&to) = path.get(1) {
                let estimated_impact = self.estimate_price_impact(amount_in);
                if estimated_impact < self.min_price_impact_bps {
                    return;
                }
                let params = HintParams {
                    from,
                    to,
                    amount_in,
                    estimated_impact,
                    source,
                    now,
                };
                self.push_hint(&mut swaps, params).await;
            }
        }
    }

    async fn record_liquidation(
        &self,
        debt_token: Address,
        collateral_token: Address,
        repay_amount: U256,
        protocol: &str,
    ) {
        let mut swaps = self.swaps.lock().await;
        let now = Instant::now();
        let estimated_impact = self.estimate_price_impact(repay_amount);
        if repay_amount < self.min_amount || estimated_impact < self.min_price_impact_bps {
            return;
        }
        let params = HintParams {
            from: debt_token,
            to: collateral_token,
            amount_in: repay_amount,
            estimated_impact,
            source: protocol,
            now,
        };
        self.push_hint(&mut swaps, params).await;
    }

    async fn push_hint(
        &self,
        swaps: &mut HashMap<(Address, Address), PendingSwap>,
        params: HintParams<'_>,
    ) {
        swaps.insert(
            (params.from, params.to),
            PendingSwap {
                amount_in: params.amount_in,
                last_seen: params.now,
            },
        );

        let mut hints = self.hints.lock().await;
        hints.push_back(BackrunHint {
            from: params.from,
            to: params.to,
            amount_in: params.amount_in,
            price_impact_bps: params.estimated_impact,
            source: params.source.to_string(),
            observed_at: params.now,
        });
        while hints.len() > 32 {
            hints.pop_front();
        }
    }

    fn estimate_price_impact(&self, amount_in: U256) -> u32 {
        if amount_in.is_zero() {
            return 0;
        }
        let rough = mul_div(
            amount_in,
            U256::from(10_000u64),
            amount_in.saturating_add(self.min_amount),
        );
        u32::try_from(rough.as_u64()).unwrap_or(u32::MAX)
    }

    async fn prune(&self, ttl: Duration) {
        let mut swaps = self.swaps.lock().await;
        let now = Instant::now();
        swaps.retain(|_, swap| now.duration_since(swap.last_seen) <= ttl);
        let mut hints = self.hints.lock().await;
        hints.retain(|hint| now.duration_since(hint.observed_at) <= ttl);
    }

    async fn active_hints(&self, ttl: Duration) -> Vec<BackrunHint> {
        self.prune(ttl).await;
        let now = Instant::now();
        let hints = self.hints.lock().await;
        hints
            .iter()
            .filter(|hint| now.duration_since(hint.observed_at) <= ttl)
            .cloned()
            .collect()
    }

    async fn best_amount_for(&self, from: Address, to: Address, ttl: Duration) -> Option<U256> {
        let swaps = self.swaps.lock().await;
        if let Some(swap) = swaps.get(&(from, to)) {
            if Instant::now().duration_since(swap.last_seen) <= ttl {
                return Some(swap.amount_in);
            }
        }
        None
    }

    async fn hint_for(&self, from: Address, to: Address, ttl: Duration) -> Option<BackrunHint> {
        let now = Instant::now();
        let mut hints = self.hints.lock().await;
        let mut found_idx = None;
        for (idx, hint) in hints.iter().enumerate() {
            if hint.from == from && hint.to == to && now.duration_since(hint.observed_at) <= ttl {
                found_idx = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_idx {
            if let Some(hint) = hints.remove(idx) {
                return Some(hint);
            }
            if let Some(fallback) = hints.pop_front() {
                warn!("Backrun hints cache missing expected index; using fallback");
                return Some(fallback);
            }
            warn!("Backrun hints cache empty when expecting entry");
        }
        None
    }

    async fn run<C>(self: Arc<Self>, provider: Arc<Provider<C>>, interval: Duration)
    where
        C: JsonRpcClient + Clone + Send + Sync + 'static,
    {
        loop {
            let pending_block = provider
                .get_block_with_txs(BlockNumber::Pending)
                .await
                .ok()
                .flatten();

            if let Some(block) = pending_block {
                for tx in block.transactions {
                    if let Some((amount_in, path)) = decode_univ2_swap(&tx) {
                        self.record(&path, amount_in, "pending").await;
                    }
                }
            }

            let latest_block = provider
                .get_block_with_txs(BlockNumber::Latest)
                .await
                .ok()
                .flatten();

            if let Some(block) = latest_block {
                for tx in block.transactions {
                    if let Some((amount_in, path)) = decode_univ2_swap(&tx) {
                        self.record(&path, amount_in, "latest").await;
                    }
                }
            }

            self.prune(Duration::from_secs(30)).await;
            sleep(interval).await;
        }
    }
}

fn decode_univ2_swap(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0[0..4].try_into().ok()?;
    const SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4] = [0x38, 0xed, 0x17, 0x39];
    const SWAP_EXACT_TOKENS_FOR_TOKENS_SUPPORTING_FEE_ON_TRANSFER: [u8; 4] =
        [0x5c, 0x11, 0xd7, 0x95];
    if selector != SWAP_EXACT_TOKENS_FOR_TOKENS
        && selector != SWAP_EXACT_TOKENS_FOR_TOKENS_SUPPORTING_FEE_ON_TRANSFER
    {
        return None;
    }

    let params = vec![
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Array(Box::new(ParamType::Address)),
        ParamType::Address,
        ParamType::Uint(256),
    ];
    let decoded = decode(&params, &tx.input.0[4..]).ok()?;
    let amount_in = decoded[0].clone().into_uint()?;
    if amount_in.is_zero() {
        return None;
    }
    let path_tokens = decoded[2].clone().into_array()?;
    if path_tokens.len() < 2 {
        return None;
    }
    let mut path = Vec::with_capacity(path_tokens.len());
    for token in path_tokens {
        if let Some(addr) = token.into_address() {
            path.push(addr);
        } else {
            return None;
        }
    }
    Some((amount_in, path))
}

#[derive(Debug)]
struct CongestionTracker {
    alpha: f64,
    ema_base_fee: f64,
    ema_interval: f64,
    baseline_fee: Option<f64>,
    last_update: Option<Instant>,
}

impl CongestionTracker {
    fn new(alpha: f64) -> Self {
        Self {
            alpha,
            ema_base_fee: 0.0,
            ema_interval: 0.0,
            baseline_fee: None,
            last_update: None,
        }
    }

    fn observe(&mut self, base_fee: Option<U256>) -> f64 {
        let now = Instant::now();
        if let Some(last) = self.last_update {
            let interval = now.duration_since(last).as_secs_f64();
            if self.ema_interval == 0.0 {
                self.ema_interval = interval;
            } else {
                self.ema_interval = self.alpha * interval + (1.0 - self.alpha) * self.ema_interval;
            }
        }
        self.last_update = Some(now);

        if let Some(base_fee) = base_fee {
            let base_fee_f = u256_to_f64(base_fee);
            if base_fee_f > 0.0 {
                if self.baseline_fee.is_none() {
                    self.baseline_fee = Some(base_fee_f);
                }
                if self.ema_base_fee == 0.0 {
                    self.ema_base_fee = base_fee_f;
                } else {
                    self.ema_base_fee =
                        self.alpha * base_fee_f + (1.0 - self.alpha) * self.ema_base_fee;
                }
            }
        }

        let baseline = self
            .baseline_fee
            .unwrap_or_else(|| self.ema_base_fee.max(1.0));

        let fee_ratio = if baseline > 0.0 {
            (self.ema_base_fee / baseline).clamp(0.5, 3.0)
        } else {
            1.0
        };

        let interval_ratio = if self.ema_interval > 0.0 {
            (self.ema_interval / 30.0).clamp(0.5, 3.0)
        } else {
            1.0
        };

        let congestion_component = 1.0 + (fee_ratio - 1.0) * 0.6 + (interval_ratio - 1.0) * 0.4;
        congestion_component.clamp(0.5, 3.0)
    }
}

#[derive(Clone, Debug)]
struct CompetitionSnapshot {
    pressure_multiplier: f64,
    extra_buffer_wei: U256,
    win_rate: f64,
    pending_competition: f64,
}

#[derive(Debug)]
struct CompetitionTracker {
    alpha: f64,
    ema_win_rate: Option<f64>,
    ema_rejection_rate: Option<f64>,
    ema_latency_ms: Option<f64>,
    ema_slack_wei: Option<U256>,
}

impl CompetitionTracker {
    fn new(alpha: f64) -> Self {
        Self {
            alpha: alpha.clamp(0.01, 1.0),
            ema_win_rate: None,
            ema_rejection_rate: None,
            ema_latency_ms: None,
            ema_slack_wei: None,
        }
    }

    fn snapshot(&self) -> CompetitionSnapshot {
        let win_rate = self.ema_win_rate.unwrap_or(0.85).clamp(0.0, 1.0);
        let rejection_rate = self.ema_rejection_rate.unwrap_or(0.0).clamp(0.0, 1.0);
        let latency_ms = self.ema_latency_ms.unwrap_or(450.0).max(1.0);

        let win_pressure = (1.0 - win_rate).clamp(0.0, 0.9);
        let latency_ratio = (latency_ms / 400.0).clamp(0.3, 4.0) - 1.0;
        let mut pressure = 1.0 + 0.6 * win_pressure + 0.5 * rejection_rate + 0.2 * latency_ratio;
        pressure = pressure.clamp(0.7, 4.0);

        let normalized_pressure = ((pressure - 1.0).max(0.0) / 3.0).clamp(0.0, 1.0);
        let pending_competition =
            (0.6 * normalized_pressure + 0.4 * rejection_rate).clamp(0.0, 1.0);

        let slack = self.ema_slack_wei.unwrap_or_else(U256::zero);
        let buffer_fraction = (0.35 + 0.35 * rejection_rate + 0.3 * win_pressure).clamp(0.05, 0.9);
        let scaled_fraction = (buffer_fraction * 1_000_000.0).round() as u64;
        let extra_buffer_wei = if slack.is_zero() {
            U256::zero()
        } else {
            mul_div(slack, U256::from(scaled_fraction), U256::from(1_000_000u64))
        };

        CompetitionSnapshot {
            pressure_multiplier: pressure,
            extra_buffer_wei,
            win_rate,
            pending_competition,
        }
    }

    fn record_success(
        &mut self,
        latency: std::time::Duration,
        private_relay_rejected: bool,
        min_profit: U256,
        net_profit: U256,
    ) {
        self.update_win_rate(1.0);
        self.update_rejection_rate(private_relay_rejected);
        self.update_latency(latency);
        if net_profit > min_profit {
            let slack = net_profit.saturating_sub(min_profit);
            self.update_slack(slack);
        }
    }

    fn record_failure(&mut self, private_relay_rejected: bool) {
        self.update_win_rate(0.0);
        self.update_rejection_rate(private_relay_rejected);
    }

    fn update_win_rate(&mut self, outcome: f64) {
        Self::update_float_ema(&mut self.ema_win_rate, self.alpha, outcome);
    }

    fn update_rejection_rate(&mut self, rejected: bool) {
        let value = if rejected { 1.0 } else { 0.0 };
        Self::update_float_ema(&mut self.ema_rejection_rate, self.alpha, value);
    }

    fn update_latency(&mut self, latency: std::time::Duration) {
        let millis = latency.as_secs_f64() * 1_000.0;
        Self::update_float_ema(&mut self.ema_latency_ms, self.alpha, millis);
    }

    fn update_slack(&mut self, slack: U256) {
        let alpha = (self.alpha * 1_000_000.0).round() as u64;
        let alpha = alpha.clamp(1, 1_000_000);
        let inv = 1_000_000u64.saturating_sub(alpha);
        match &mut self.ema_slack_wei {
            Some(current) => {
                let new_component = mul_div(slack, U256::from(alpha), U256::from(1_000_000u64));
                let old_component = mul_div(*current, U256::from(inv), U256::from(1_000_000u64));
                *current = new_component + old_component;
            }
            None => {
                self.ema_slack_wei = Some(slack);
            }
        }
    }

    fn update_float_ema(target: &mut Option<f64>, alpha: f64, value: f64) {
        if value.is_nan() {
            return;
        }
        match target {
            Some(current) => {
                *current = alpha * value + (1.0 - alpha) * *current;
            }
            None => {
                *target = Some(value);
            }
        }
    }
}

fn compute_start_priorities_inner(
    graph: &Graph,
    base_profiles: &HashMap<Address, TradeSizing>,
    backrun_hints: &[BackrunHint],
    min_flash_amount: U256,
) -> HashMap<Address, i128> {
    let mut priorities = HashMap::new();

    for token in &graph.nodes {
        let liquidity_score = base_profiles
            .get(token)
            .map(|profile| {
                profile
                    .base_amount
                    .min(U256::from(i128::MAX as u128))
                    .as_u128()
            })
            .unwrap_or(0);

        priorities.insert(*token, liquidity_score.min(i128::MAX as u128) as i128);
    }

    for edge in &graph.edges {
        let capacity_component =
            edge.max_input.min(U256::from(i128::MAX as u128)).as_u128() as i128;
        let swing_component = i128::from(edge.observed_slippage_bps.max(edge.tolerance_bps));
        let contribution = capacity_component.saturating_add(swing_component);
        let entry = priorities.entry(edge.from).or_insert(0);
        *entry = entry
            .saturating_add(contribution)
            .clamp(i128::MIN + 1, i128::MAX);
    }

    let min_flash = min_flash_amount.max(U256::one());
    for hint in backrun_hints {
        let scaled_amount = hint
            .amount_in
            .checked_div(min_flash)
            .unwrap_or_else(U256::zero)
            .min(U256::from(10_000u64))
            .as_u128() as i128;
        let impact = i128::from(hint.price_impact_bps.max(1));
        let boost = scaled_amount
            .saturating_mul(impact)
            .saturating_add(impact.saturating_mul(10));
        let entry = priorities.entry(hint.from).or_insert(0);
        *entry = entry.saturating_add(boost).clamp(i128::MIN + 1, i128::MAX);
    }

    priorities
}

#[derive(Debug, Clone)]
enum Command {
    Start,
    Stop,
    Quit,
    ResetCircuit,
}

#[derive(Debug)]
enum ScanOutcome {
    Executed(Box<ExecutionSummary>),
    NotProfitable {
        reason: String,
        edges: usize,
        expected_univ3_edges: usize,
    },
    NoOpportunity {
        edges: usize,
        expected_univ3_edges: usize,
    },
    Failed {
        reason: String,
        tx_hash: H256,
        edges: usize,
        expected_univ3_edges: usize,
        private_relay_rejected: bool,
        estimated_loss_wei: U256,
    },
}

#[derive(Debug)]
struct ReceiptFailure {
    reason: String,
    tx_hash: H256,
    edges: usize,
}

#[derive(Debug)]
struct DispatchResult {
    tx_hash: H256,
    receipt: Option<TransactionReceipt>,
    latency: std::time::Duration,
    private_relay_rejected: bool,
    private_submission_method: Option<PrivateSubmissionMethod>,
}

fn classify_receipt(
    receipt: Option<TransactionReceipt>,
    pending_tx_hash: H256,
    edges_scanned: usize,
) -> Result<H256, ReceiptFailure> {
    let tx_hash = receipt
        .as_ref()
        .map(|r| r.transaction_hash)
        .unwrap_or(pending_tx_hash);

    match receipt {
        Some(receipt) => match receipt.status {
            Some(status) if status == U64::one() => Ok(tx_hash),
            Some(status) => Err(ReceiptFailure {
                reason: format!("transaction reverted with status {status}"),
                tx_hash,
                edges: edges_scanned,
            }),
            None => Err(ReceiptFailure {
                reason: "transaction receipt missing status".into(),
                tx_hash,
                edges: edges_scanned,
            }),
        },
        None => Err(ReceiptFailure {
            reason: "transaction dropped before receipt was available".into(),
            tx_hash,
            edges: edges_scanned,
        }),
    }
}

struct RunnerConfig<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    feature_gate: FeatureGate,
    chain_name: String,
    provider: Arc<Provider<C>>,
    rpc_endpoint: String,
    rpc_health: Arc<StdMutex<HealthTracker>>,
    univ3_quoter: Address,
    univ3_factory: Address,
    univ3_validation: Option<UniV3ValidationConfig>,
    univ3_fee_tiers: Option<Arc<HashSet<u32>>>,
    bal_vault: Address,
    aave_pool: Option<Address>,
    erc3156_lender: Option<Address>,
    erc3156_fee_bps: u32,
    bal_flashloan_tokens: Option<HashSet<Address>>,
    aave_flashloan_tokens: Option<HashSet<Address>>,
    erc3156_flashloan_tokens: Option<HashSet<Address>>,
    univ2_flashloan_tokens: Option<HashSet<Address>>,
    univ3_flashloan_tokens: Option<HashSet<Address>>,
    chain_env_prefix: String,
    tokens: TokenList,
    wrapped_native: Address,
    capital: Arc<CapitalManager>,
    pool_depth_cache: Arc<PoolDepthCache>,
    pool_monitor: Option<Arc<ingestion::PoolMonitor<C>>>,
    hot_univ2_pools: Arc<tokio::sync::RwLock<Vec<ResolvedUniV2PoolCfg>>>,
    hot_univ3_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
    edge_slippage_bps: u32,
    executor_max_slippage_bps: u32,
    edge_prune_max_slippage_bps: u32,
    edge_prune_min_score: f64,
    edge_prune_liquidity_weight: f64,
    edge_prune_profit_weight: f64,
    edge_prune_slippage_weight: f64,
    cycle_limits: BellmanFordLimits,
    search_budget: Duration,
    quote_budget: Duration,
    simulation_budget: Duration,
    max_edges_hot: usize,
    topk_per_token: usize,
    dynamic_top_tokens_30d: usize,
    mandatory_universe_tokens: HashSet<Address>,
    hub_tokens: HashSet<Address>,
    max_gas_price_wei: U256,
    max_gas_price_congestion_bps: u32,
    profit_margin_bps: u32,
    opportunity_cost_wei: U256,
    cross_chain_profit_bps: u32,
    cross_chain_min_profit_wei: U256,
    max_candidate_paths: usize,
    min_flash_loan_wei: U256,
    min_edge_max_input: U256,
    min_liquidity_tokens: f64,
    max_quote_block_lag: U64,
    congestion_alpha: f64,
    competition_alpha: f64,
    jit_config: Option<JitConfig>,
    broadcast: BroadcastConfig,
    shadow: ShadowConfig,
    chaos: ChaosConfig,
    wallet: Option<LocalWallet>,
    backrun_monitor: Option<Arc<BackrunMonitor>>,
    sandwich_monitor: Option<Arc<SandwichMonitor<C>>>,
    bridge: Option<Arc<BridgePlanner>>,
    low_liquidity_scanner: Option<Arc<Mutex<LowLiquidityScanner<C>>>>,
    liquidation_monitor: Option<Arc<LiquidationMonitor<C>>>,
    circuit_breaker: Arc<CircuitBreaker>,
    metrics: Option<Arc<Metrics>>,
    accounting: Option<Arc<Accounting>>,
    fee_estimator: FeeEstimator<C>,
}

struct Runner<M, C>
where
    M: Middleware + 'static,
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    feature_gate: FeatureGate,
    provider: Arc<Provider<C>>,
    rpc_endpoint: String,
    rpc_health: Arc<StdMutex<HealthTracker>>,
    chain_name: String,
    #[allow(dead_code)]
    univ3_quoter: Address,
    #[allow(dead_code)]
    univ3_factory: Address,
    univ3_validation: Option<UniV3ValidationConfig>,
    univ3_fee_tiers: Option<Arc<HashSet<u32>>>,
    bal_vault: Address,
    aave_pool: Option<Address>,
    erc3156_lender: Option<Address>,
    erc3156_fee_bps: u32,
    bal_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    aave_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    erc3156_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    univ2_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    univ3_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    chain_env_prefix: String,
    tokens: TokenList,
    wrapped_native: Address,
    capital: Arc<CapitalManager>,
    pool_depth_cache: Arc<PoolDepthCache>,
    pool_monitor: Option<Arc<ingestion::PoolMonitor<C>>>,
    hot_univ2_pools: Arc<tokio::sync::RwLock<Vec<ResolvedUniV2PoolCfg>>>,
    hot_univ3_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
    edge_slippage_bps: u32,
    executor_max_slippage_bps: u32,
    edge_prune_max_slippage_bps: u32,
    edge_prune_min_score: f64,
    edge_prune_liquidity_weight: f64,
    edge_prune_profit_weight: f64,
    edge_prune_slippage_weight: f64,
    cycle_limits: BellmanFordLimits,
    search_budget: Duration,
    quote_budget: Duration,
    simulation_budget: Duration,
    max_edges_hot: usize,
    topk_per_token: usize,
    dynamic_top_tokens_30d: usize,
    mandatory_universe_tokens: HashSet<Address>,
    hub_tokens: HashSet<Address>,
    max_gas_price_wei: U256,
    max_gas_price_congestion_bps: u32,
    profit_margin_bps: u32,
    opportunity_cost_wei: U256,
    cross_chain_profit_bps: u32,
    cross_chain_min_profit_wei: U256,
    max_candidate_paths: usize,
    min_flash_loan_wei: U256,
    min_edge_max_input: U256,
    min_liquidity_tokens: f64,
    max_quote_block_lag: U64,
    congestion: Arc<Mutex<CongestionTracker>>,
    competition: Arc<Mutex<CompetitionTracker>>,
    jit_config: Option<JitConfig>,
    executor: MultiVenueArbExecutor<M>,
    broadcast: BroadcastConfig,
    shadow: ShadowConfig,
    chaos: ChaosConfig,
    wallet: Option<LocalWallet>,
    nonce_manager: Option<Arc<NonceManager<C>>>,
    backrun: Option<Arc<BackrunMonitor>>,
    sandwich: Option<Arc<SandwichMonitor<C>>>,
    bridge: Option<Arc<BridgePlanner>>,
    low_liquidity: Option<Arc<Mutex<LowLiquidityScanner<C>>>>,
    liquidations: Option<Arc<LiquidationMonitor<C>>>,
    hot_paths: Arc<HotPathCache>,
    quoter: Arc<UniQuoter<C>>,
    bal_quote: Arc<BalQuote<C>>,
    curve_quote: Arc<CurveQuote<C>>,
    quote_semaphore: Arc<Semaphore>,
    univ3_validation_once: Arc<OnceCell<()>>,
    circuit_breaker: Arc<CircuitBreaker>,
    metrics: Option<Arc<Metrics>>,
    accounting: Option<Arc<Accounting>>,
    token_decimals: Arc<Mutex<HashMap<Address, u8>>>,
    native_price_cache: Arc<Mutex<HashMap<Address, NativePrice>>>,
    fee_estimator: FeeEstimator<C>,
    last_graph_digest: Arc<Mutex<Option<GraphDigest>>>,
    previous_cycle_seeds: Arc<Mutex<Vec<Vec<Address>>>>,
    candidate_logger: Arc<CandidateDecisionLogger>,
}

impl<M, C> Runner<M, C>
where
    M: Middleware + 'static,
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    fn build_executor_call(&self, plan_args: &ExecutorPlan) -> Option<ContractCall<M, U256>> {
        if plan_args.loans.len() != 1 {
            warn!(
                loan_count = plan_args.loans.len(),
                "skipping plan with unsupported loan count"
            );
            return None;
        }
        Some(self.executor.start_v2(plan_args.clone()))
    }

    fn new(config: RunnerConfig<C>, executor: MultiVenueArbExecutor<M>) -> Self {
        let RunnerConfig {
            feature_gate,
            chain_name,
            provider,
            rpc_endpoint,
            rpc_health,
            univ3_quoter,
            univ3_factory,
            univ3_validation,
            univ3_fee_tiers,
            bal_vault,
            aave_pool,
            erc3156_lender,
            erc3156_fee_bps,
            bal_flashloan_tokens,
            aave_flashloan_tokens,
            erc3156_flashloan_tokens,
            univ2_flashloan_tokens,
            univ3_flashloan_tokens,
            chain_env_prefix,
            tokens,
            wrapped_native,
            capital,
            pool_depth_cache,
            pool_monitor,
            hot_univ2_pools,
            hot_univ3_pools,
            edge_slippage_bps,
            executor_max_slippage_bps,
            edge_prune_max_slippage_bps,
            edge_prune_min_score,
            edge_prune_liquidity_weight,
            edge_prune_profit_weight,
            edge_prune_slippage_weight,
            cycle_limits,
            search_budget,
            quote_budget,
            simulation_budget,
            max_edges_hot,
            topk_per_token,
            dynamic_top_tokens_30d,
            mandatory_universe_tokens,
            hub_tokens,
            max_gas_price_wei,
            max_gas_price_congestion_bps,
            profit_margin_bps,
            opportunity_cost_wei,
            cross_chain_profit_bps,
            cross_chain_min_profit_wei,
            max_candidate_paths,
            min_flash_loan_wei,
            min_edge_max_input,
            min_liquidity_tokens,
            max_quote_block_lag,
            congestion_alpha,
            competition_alpha,
            jit_config,
            broadcast,
            shadow,
            chaos,
            wallet,
            backrun_monitor,
            sandwich_monitor,
            bridge,
            low_liquidity_scanner,
            liquidation_monitor,
            circuit_breaker,
            metrics,
            accounting,
            fee_estimator,
        } = config;
        let bal_flashloan_tokens = bal_flashloan_tokens.map(Arc::new);
        let aave_flashloan_tokens = aave_flashloan_tokens.map(Arc::new);
        let erc3156_flashloan_tokens = erc3156_flashloan_tokens.map(Arc::new);
        let univ2_flashloan_tokens = univ2_flashloan_tokens.map(Arc::new);
        let univ3_flashloan_tokens = univ3_flashloan_tokens.map(Arc::new);
        let nonce_manager = wallet
            .as_ref()
            .map(|wallet| Arc::new(NonceManager::new(provider.clone(), wallet.address())));
        let provider_for_quoter = provider.clone();
        let bal_quote = Arc::new(BalQuote::new(provider.clone(), bal_vault));
        let curve_quote = Arc::new(CurveQuote::new(provider.clone()));
        Self {
            feature_gate,
            provider,
            rpc_endpoint,
            rpc_health,
            univ3_quoter,
            univ3_factory,
            univ3_validation,
            univ3_fee_tiers,
            bal_vault,
            aave_pool,
            erc3156_lender,
            erc3156_fee_bps,
            bal_flashloan_tokens,
            aave_flashloan_tokens,
            erc3156_flashloan_tokens,
            univ2_flashloan_tokens,
            univ3_flashloan_tokens,
            chain_env_prefix,
            tokens,
            wrapped_native,
            capital,
            pool_depth_cache,
            pool_monitor,
            hot_univ2_pools,
            hot_univ3_pools,
            edge_slippage_bps,
            executor_max_slippage_bps,
            edge_prune_max_slippage_bps,
            edge_prune_min_score,
            edge_prune_liquidity_weight,
            edge_prune_profit_weight,
            edge_prune_slippage_weight,
            cycle_limits,
            search_budget,
            quote_budget,
            simulation_budget,
            max_edges_hot,
            topk_per_token,
            dynamic_top_tokens_30d,
            mandatory_universe_tokens,
            hub_tokens,
            max_gas_price_wei,
            max_gas_price_congestion_bps,
            profit_margin_bps,
            opportunity_cost_wei,
            cross_chain_profit_bps,
            cross_chain_min_profit_wei,
            max_candidate_paths,
            min_flash_loan_wei,
            min_edge_max_input,
            min_liquidity_tokens,
            max_quote_block_lag,
            chain_name,
            congestion: Arc::new(Mutex::new(CongestionTracker::new(congestion_alpha))),
            competition: Arc::new(Mutex::new(CompetitionTracker::new(competition_alpha))),
            jit_config,
            executor,
            broadcast,
            shadow,
            chaos,
            wallet,
            nonce_manager,
            backrun: backrun_monitor,
            sandwich: sandwich_monitor,
            bridge,
            low_liquidity: low_liquidity_scanner,
            liquidations: liquidation_monitor,
            hot_paths: HotPathCache::with_failure_backoff(
                0.1,
                Duration::from_secs(300),
                20,
                Duration::from_secs(60),
            ),
            quoter: Arc::new(UniQuoter::new(
                provider_for_quoter,
                univ3_quoter,
                univ3_factory,
            )),
            bal_quote,
            curve_quote,
            quote_semaphore: Arc::new(Semaphore::new(univ3_quote_concurrency())),
            univ3_validation_once: Arc::new(OnceCell::new()),
            circuit_breaker,
            metrics,
            accounting,
            token_decimals: Arc::new(Mutex::new(HashMap::new())),
            native_price_cache: Arc::new(Mutex::new(HashMap::new())),
            fee_estimator,
            last_graph_digest: Arc::new(Mutex::new(None)),
            previous_cycle_seeds: Arc::new(Mutex::new(Vec::new())),
            candidate_logger: Arc::new(CandidateDecisionLogger::from_env()),
        }
    }

    fn stage_candidate_id(
        &self,
        cycle_start: Address,
        cycle_ix: &[usize],
        graph: &Graph,
        block_number: U64,
        fee_tiers: &[u32],
    ) -> String {
        let mut material = format!("{}:{}:{}:", self.chain_name, cycle_start, block_number);
        for ix in cycle_ix {
            if let Some(token) = graph.nodes.get(*ix) {
                material.push_str(&format!("{token}:"));
            }
        }
        for fee in fee_tiers {
            material.push_str(&format!("{fee}:"));
        }
        format!(
            "0x{}",
            hex::encode(ethers::utils::keccak256(material.as_bytes()))
        )
    }

    fn log_candidate_stage(
        &self,
        stage: &str,
        chain: &str,
        candidate_id: Option<String>,
        cycle_start_token: Option<Address>,
        hops: Option<usize>,
        edges_scanned: usize,
        venue_path: Option<Vec<String>>,
        gross_after_fee: Option<U256>,
        gas_cost_native: Option<U256>,
        gas_cost_start_token: Option<U256>,
        pricing_reliable: bool,
        min_profit_threshold: Option<U256>,
        rejection_reason: Option<&str>,
        simulation_status: Option<&str>,
        bridge: bool,
        liquidation: bool,
    ) {
        let record = CandidateDecisionRecord {
            timestamp_ms: CandidateDecisionRecord::now_ms(),
            stage: stage.to_string(),
            chain: chain.to_string(),
            candidate_id,
            cycle_start_token: cycle_start_token.map(|addr| format!("0x{}", hex::encode(addr))),
            hops,
            edges_scanned,
            venue_path,
            gross_after_fee: gross_after_fee.map(|v| v.to_string()),
            gas_cost_native: gas_cost_native.map(|v| v.to_string()),
            gas_cost_start_token: gas_cost_start_token.map(|v| v.to_string()),
            pricing_reliable,
            min_profit_threshold: min_profit_threshold.map(|v| v.to_string()),
            rejection_reason: rejection_reason.map(ToString::to_string),
            simulation_status: simulation_status.map(ToString::to_string),
            bridge,
            liquidation,
            block_number: None,
            path_tokens: None,
            fee_tiers: None,
            quote_block_number: None,
            quote_block_lag: None,
            gas_estimate_wei: None,
            profit_after_gas_wei: None,
            pricing_source: None,
            error: None,
        };
        if let Err(err) = self.candidate_logger.write(record) {
            warn!(error = %err, path = %self.candidate_logger.path().display(), "Failed writing candidate decision log");
        }
    }

    async fn handle_command(
        &self,
        cmd: Command,
        running: &mut bool,
        shutdown: &mut bool,
        status_tx: &watch::Sender<StatusSnapshot>,
        last_execution: &Option<ExecutionSummary>,
    ) {
        match cmd {
            Command::Start => {
                if !*running {
                    let status = self.circuit_breaker.current_status().await;
                    if status.is_tripped {
                        status_tx
                            .send(StatusSnapshot::new(
                                RunnerState::Error,
                                format!("Circuit breaker active: {}", status.active_reason()),
                                last_execution.clone(),
                            ))
                            .ok();
                    } else {
                        *running = true;
                        status_tx
                            .send(StatusSnapshot::new(
                                RunnerState::Running,
                                "Arbitrage loop started",
                                last_execution.clone(),
                            ))
                            .ok();
                    }
                }
            }
            Command::Stop => {
                if *running {
                    *running = false;
                    status_tx
                        .send(StatusSnapshot::new(
                            RunnerState::Idle,
                            "Arbitrage loop paused",
                            last_execution.clone(),
                        ))
                        .ok();
                }
            }
            Command::Quit => {
                *running = false;
                *shutdown = true;
                status_tx
                    .send(StatusSnapshot::new(
                        RunnerState::Stopped,
                        "Shutdown requested",
                        last_execution.clone(),
                    ))
                    .ok();
            }
            Command::ResetCircuit => {
                let status = self.circuit_breaker.reset().await;
                let detail = if status.is_tripped {
                    format!(
                        "Circuit breaker reset but still active: {}",
                        status.active_reason()
                    )
                } else {
                    "Circuit breaker reset; limits cleared".to_string()
                };
                let state = if *running {
                    RunnerState::Running
                } else {
                    RunnerState::Idle
                };
                status_tx
                    .send(StatusSnapshot::new(state, detail, last_execution.clone()))
                    .ok();
            }
        }
    }

    async fn check_wallet_health(&self) -> Result<HealthStatus> {
        let Some(wallet) = self.wallet.as_ref() else {
            return Ok(HealthStatus::Healthy {
                balance: U256::zero(),
                reserve_txs: U256::zero(),
            });
        };

        let balance = self.provider.get_balance(wallet.address(), None).await?;

        let gas_per_tx = self
            .max_gas_price_wei
            .saturating_mul(U256::from(500_000u64));
        let min_balance = if self.shadow.enabled {
            U256::zero()
        } else {
            gas_per_tx.saturating_mul(U256::from(20u64))
        };

        if !self.shadow.enabled && balance < min_balance {
            return Err(anyhow!(
                "Wallet balance {} below minimum {}. Deposit ETH immediately!",
                balance,
                min_balance
            ));
        }

        let warning_threshold = min_balance.saturating_mul(U256::from(2u64));
        if balance < warning_threshold {
            warn!(
                balance = %balance,
                threshold = %warning_threshold,
                "Wallet balance running low - deposit recommended"
            );
        }

        if self.shadow.enabled && balance < gas_per_tx {
            warn!(
                balance = %balance,
                needed_for_live = %gas_per_tx,
                "Shadow mode enabled: live broadcasts will need funding"
            );
        }

        let reserve_txs = if gas_per_tx.is_zero() {
            U256::zero()
        } else {
            balance / gas_per_tx
        };

        Ok(HealthStatus::Healthy {
            balance,
            reserve_txs,
        })
    }

    async fn scan_once(&self) -> Result<ScanOutcome> {
        let scan_started = Instant::now();
        if !self.feature_gate.cycle_arb {
            info!("Cycle arbitrage disabled via FEATURE_CYCLE_ARB");
            return Ok(ScanOutcome::NoOpportunity {
                edges: 0,
                expected_univ3_edges: 0,
            });
        }
        let univ3_validation = self.univ3_validation.clone();
        let univ3_fee_tiers = self.univ3_fee_tiers.clone();
        let chain_name = self.chain_name.clone();
        let validation_once = Arc::clone(&self.univ3_validation_once);
        let max_quote_block_lag = self.max_quote_block_lag;
        let hot_univ2 = { self.hot_univ2_pools.read().await.clone() };
        let hot_univ3 = { self.hot_univ3_pools.read().await.clone() };
        let outcome = self
            .scan_once_with(
                |graph,
                 provider,
                 pool_monitor,
                 quoter,
                 bal_vault,
                 chain_env_prefix: String,
                 token_whitelist,
                 base_amount_wei,
                 base_profiles,
                 edge_slippage_bps,
                 gas_price,
                 token_decimals,
                 native_prices,
                 min_liquidity_tokens,
                 min_edge_max_input,
                 low_liquidity,
                 hot_paths,
                 quote_semaphore,
                 block_number| {
                    let hot_univ2 = hot_univ2.clone();
                    let hot_univ3 = hot_univ3.clone();
                    let validation = univ3_validation.clone();
                    let fee_tiers = univ3_fee_tiers.clone();
                    let chain = chain_name.clone();
                    let once = validation_once.clone();
                    Box::pin(async move {
                        populate_edges(
                            graph,
                            provider,
                            pool_monitor,
                            quoter,
                            validation,
                            fee_tiers,
                            bal_vault,
                            chain_env_prefix.as_str(),
                            chain,
                            token_whitelist.as_ref(),
                            base_amount_wei,
                            base_profiles.clone(),
                            edge_slippage_bps,
                            gas_price,
                            token_decimals.clone(),
                            native_prices.clone(),
                            min_liquidity_tokens,
                            min_edge_max_input,
                            low_liquidity,
                            &hot_univ2,
                            &hot_univ3,
                            hot_paths,
                            quote_semaphore,
                            block_number,
                            max_quote_block_lag,
                            once,
                        )
                        .await
                    })
                },
            )
            .await;

        if let Some(metrics) = &self.metrics {
            metrics
                .record_scan_latency(&self.chain_name, scan_started.elapsed().as_millis() as u64);
        }

        outcome
    }

    async fn load_token_decimals(&self) -> HashMap<Address, u8> {
        let tokens = self.tokens.current();
        let mut cache = self.token_decimals.lock().await;

        for &token in tokens.iter() {
            if cache.contains_key(&token) {
                continue;
            }

            let mut last_err = None;
            for attempt in 0..3 {
                match erc20_decimals(self.provider.clone(), token).await {
                    Ok(decimals) => {
                        cache.insert(token, decimals);
                        last_err = None;
                        break;
                    }
                    Err(err) => {
                        last_err = Some(err);
                        if attempt < 2 {
                            sleep(Duration::from_millis(200 << attempt)).await;
                        }
                    }
                }
            }

            if let Some(err) = last_err {
                warn!(
                    error = %err,
                    token = %format!("0x{}", hex::encode(token)),
                    retries = 3,
                    "Failed to fetch token decimals after retries; defaulting to 18 for now",
                );
            }
        }

        cache.clone()
    }

    fn native_price_for(
        &self,
        token: Address,
        prices: &HashMap<Address, NativePrice>,
    ) -> NativePrice {
        if token == self.wrapped_native {
            let amount = U256::exp10(18);
            return NativePrice::new(amount, amount, true);
        }
        prices
            .get(&token)
            .copied()
            .unwrap_or_else(|| NativePrice::new(U256::zero(), U256::zero(), false))
    }

    async fn fetch_native_price(
        &self,
        token: Address,
        decimals: u8,
        block_number: U64,
    ) -> Option<NativePrice> {
        if token == self.wrapped_native || self.wrapped_native.is_zero() {
            let amount = U256::exp10(decimals.min(18) as usize);
            return Some(NativePrice::new(amount, amount, true));
        }

        let amount_in = U256::exp10(decimals.min(18) as usize);
        for fee in FEE_TIERS {
            let path = build_univ3_price_path(token, self.wrapped_native, fee);
            if let Ok(out) = self.quoter.quote_path(path, amount_in, block_number).await {
                if !out.is_zero() {
                    return Some(NativePrice::new(amount_in, out, true));
                }
            }
        }

        None
    }

    async fn load_native_prices(
        &self,
        tokens: &[Address],
        token_decimals: &HashMap<Address, u8>,
        block_number: U64,
    ) -> HashMap<Address, NativePrice> {
        let mut cache = self.native_price_cache.lock().await;
        for &token in tokens.iter() {
            if let Some(cached) = cache.get(&token) {
                if should_reuse_cached_native_price(token, self.wrapped_native, *cached) {
                    continue;
                }
            }

            let decimals = token_decimals.get(&token).copied().unwrap_or(18);
            let price = self
                .fetch_native_price(token, decimals, block_number)
                .await
                .unwrap_or_else(|| {
                    if token == self.wrapped_native {
                        let amount = U256::exp10(decimals.min(18) as usize);
                        NativePrice::new(amount, amount, true)
                    } else {
                        NativePrice::new(U256::zero(), U256::zero(), false)
                    }
                });
            if should_cache_native_price(token, self.wrapped_native, price) {
                cache.insert(token, price);
            } else {
                cache.remove(&token);
            }
        }

        cache
            .iter()
            .filter_map(|(token, price)| {
                if tokens.contains(token) {
                    Some((*token, *price))
                } else {
                    None
                }
            })
            .collect()
    }

    async fn compute_base_amounts(
        &self,
        token_decimals: &HashMap<Address, u8>,
        capital: &CapitalSnapshot,
    ) -> HashMap<Address, TradeSizing> {
        let tokens = self.tokens.current();
        let min_flash = capital.min_flash_loan;
        let max_flash = capital.max_flash_loan.max(min_flash);
        let divisor = U256::from(5u64);
        let mut map = HashMap::new();
        for &token in tokens.iter() {
            let decimals = token_decimals.get(&token).copied().unwrap_or(18);
            let depth = self
                .pool_depth_cache
                .estimate_liquidity(token, decimals)
                .await;
            let mut sized = if depth.is_zero() {
                U256::zero()
            } else {
                depth / divisor
            };
            if sized < min_flash {
                sized = min_flash;
            }
            if sized > max_flash {
                sized = max_flash;
            }
            let tolerance = if depth.is_zero() {
                self.edge_slippage_bps
            } else {
                let scale = 10f64.powi(decimals as i32);
                let depth_tokens = if scale > 0.0 {
                    u256_to_f64(depth) / scale
                } else {
                    u256_to_f64(depth)
                };
                let amount_tokens = if scale > 0.0 {
                    u256_to_f64(sized) / scale
                } else {
                    u256_to_f64(sized)
                };
                let usage = if depth_tokens > 0.0 {
                    (amount_tokens / depth_tokens).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let scaled = (usage * 100.0).ceil();
                let min_tolerance = 5.0f64.min(self.edge_slippage_bps as f64);
                let clamped = scaled.max(min_tolerance).min(self.edge_slippage_bps as f64);
                clamped.round() as u32
            };
            map.insert(token, TradeSizing::new(sized, tolerance));
        }
        map
    }

    fn gas_price_for_weights(&self, gas: &FeeEstimate) -> U256 {
        if gas.l1_data_fee.is_zero() {
            return gas.gas_price;
        }

        const AVG_EDGE_GAS: u64 = 210_000;
        let hops = self.cycle_limits.max_hops.max(1) as u64;
        let denom = U256::from(AVG_EDGE_GAS).saturating_mul(U256::from(hops));
        let per_unit_overhead = gas
            .l1_data_fee
            .checked_div(denom)
            .unwrap_or_else(U256::zero);
        gas.gas_price.saturating_add(per_unit_overhead)
    }

    fn dynamic_min_profit(&self, params: DynamicProfitParams<'_>) -> U256 {
        let gas_cost_native = params
            .fee
            .gas_price
            .saturating_mul(U256::from(params.est_gas))
            .saturating_add(params.fee.l1_data_fee);
        let Some(gas_cost) = params
            .native_price
            .tokens_for_native_strict(gas_cost_native)
        else {
            return U256::MAX;
        };
        let margin = mul_div(
            gas_cost,
            U256::from(self.profit_margin_bps as u64),
            U256::from(10_000u64),
        );
        let mut threshold = gas_cost.saturating_add(margin).saturating_add(
            params
                .native_price
                .tokens_for_native_strict(self.opportunity_cost_wei)
                .unwrap_or(U256::MAX),
        );

        let congestion_scaled =
            ((params.congestion * 1_000_000.0).round()).clamp(500_000.0, 5_000_000.0) as u64;
        threshold = mul_div(
            threshold,
            U256::from(congestion_scaled),
            U256::from(1_000_000u64),
        );

        if params.est_gross > gas_cost {
            let net_headroom = params.est_gross.saturating_sub(gas_cost);
            let win_rate = params.competition.win_rate.clamp(0.0, 1.0);
            let pending_comp = params.competition.pending_competition.clamp(0.0, 1.0);
            let gamma = (0.5 + 0.4 * (1.0 - win_rate) * pending_comp).min(0.9);
            let gamma_scaled = ((gamma * 1_000_000.0).round()) as u64;
            let buffer = mul_div(
                net_headroom,
                U256::from(gamma_scaled),
                U256::from(1_000_000u64),
            );
            threshold = threshold.saturating_add(buffer);
        }

        let pending_comp_level = params.competition.pending_competition;
        let base_vol_multiplier = if pending_comp_level > 0.7 { 1.25 } else { 1.0 };
        const INVENTORY_LATENCY_SECS: f64 = 9.0;
        let latency_factor = if params.latency_secs <= 0.0 {
            1.0
        } else {
            (params.latency_secs / INVENTORY_LATENCY_SECS).max(1.0)
        };
        let latency_multiplier = latency_factor.powf(0.25);
        let combined_vol_multiplier = base_vol_multiplier * latency_multiplier;
        let vol_bps = (combined_vol_multiplier * 100.0)
            .clamp(100.0, 500.0)
            .round() as u64;
        threshold = mul_div(threshold, U256::from(vol_bps), U256::from(100u64));

        let pressure_scaled = ((params.competition.pressure_multiplier * 1_000_000.0).round())
            .clamp(500_000.0, 6_000_000.0) as u64;
        mul_div(
            threshold,
            U256::from(pressure_scaled),
            U256::from(1_000_000u64),
        )
    }

    async fn process_sandwich_opportunities(
        &self,
        fee: &FeeEstimate,
        native_prices: &HashMap<Address, NativePrice>,
    ) {
        if let Some(monitor) = &self.sandwich {
            let mut processed = 0usize;
            while processed < 4 {
                if let Some(opportunity) = monitor.next_opportunity().await {
                    processed += 1;
                    let native_price = self.native_price_for(opportunity.token_in, native_prices);
                    self.report_sandwich_opportunity(&opportunity, fee, native_price);
                } else {
                    break;
                }
            }
        }
    }

    fn report_sandwich_opportunity(
        &self,
        opportunity: &SandwichOpportunity,
        fee: &FeeEstimate,
        native_price: NativePrice,
    ) {
        let gas_cost_native = fee
            .gas_price
            .saturating_mul(U256::from(opportunity.estimated_gas))
            .saturating_add(fee.l1_data_fee);
        let Some(gas_cost) = native_price.tokens_for_native_strict(gas_cost_native) else {
            return;
        };
        let net = opportunity.expected_profit.saturating_sub(gas_cost);
        let age_ms = opportunity
            .discovered_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let victim_sender = opportunity
            .victim_sender
            .map(|addr| format!("0x{}", hex::encode(addr.as_bytes())))
            .unwrap_or_else(|| "unknown".to_string());
        info!(
            strategy = "sandwich",
            victim = %format!("{:#x}", opportunity.victim_hash),
            token_in = %format!("0x{}", hex::encode(opportunity.token_in.as_bytes())),
            token_out = %format!("0x{}", hex::encode(opportunity.token_out.as_bytes())),
            victim_amount = %opportunity.victim_amount_in,
            front_amount = %opportunity.frontrun_amount_in,
            gross_profit = %opportunity.expected_profit,
            est_net = %net,
            est_gas = opportunity.estimated_gas,
            age_ms = age_ms,
            fee_bps = opportunity.fee_bps,
            sender = %victim_sender,
            "Sandwich opportunity detected"
        );
    }

    fn flash_loan_quotes(&self, token: Address, max_cycle_input: U256) -> Vec<FlashLoanQuote> {
        let capital = self.capital.snapshot();
        let mut quotes = Vec::new();
        let capped_amount = max_cycle_input
            .min(capital.max_flash_loan)
            .max(capital.min_flash_loan);

        if capped_amount < capital.min_flash_loan {
            return quotes;
        }

        let balancer_supported = self
            .bal_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(true);

        if balancer_supported {
            quotes.push(FlashLoanQuote {
                provider: FlashLoanProvider::Balancer,
                max_amount: capped_amount,
                fee_bps: 0,
                provider_addr: Some(self.bal_vault),
            });
        }

        let aave_supported = self
            .aave_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(false);
        if aave_supported && self.aave_pool.is_some() {
            quotes.push(FlashLoanQuote {
                provider: FlashLoanProvider::AaveV3,
                max_amount: capped_amount,
                fee_bps: 9,
                provider_addr: self.aave_pool,
            });
        }

        let erc3156_supported = self
            .erc3156_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(false);
        if erc3156_supported {
            if let Some(lender) = self.erc3156_lender {
                quotes.push(FlashLoanQuote {
                    provider: FlashLoanProvider::Erc3156,
                    max_amount: capped_amount,
                    fee_bps: self.erc3156_fee_bps,
                    provider_addr: Some(lender),
                });
            }
        }

        let univ2_supported = self
            .univ2_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(false);
        if univ2_supported {
            quotes.push(FlashLoanQuote {
                provider: FlashLoanProvider::Univ2Flashswap,
                max_amount: capped_amount,
                fee_bps: 0,
                provider_addr: None,
            });
        }

        let univ3_supported = self
            .univ3_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(false);
        if univ3_supported {
            quotes.push(FlashLoanQuote {
                provider: FlashLoanProvider::Univ3Flash,
                max_amount: capped_amount,
                fee_bps: 0,
                provider_addr: None,
            });
        }

        quotes
    }

    fn estimate_cycle_latency(&self, graph: &Graph, cycle: &[usize]) -> f64 {
        const INVENTORY_LATENCY_SECS: f64 = 9.0;
        const BRIDGE_LATENCY_BASELINE_SECS: f64 = 242.0;
        let mut total_bridge_latency: f64 = 0.0;
        let mut has_bridge = false;

        for window in cycle.windows(2) {
            let from_idx = window[0];
            let to_idx = window[1];
            let Some(&from_addr) = graph.nodes.get(from_idx) else {
                continue;
            };
            let Some(&to_addr) = graph.nodes.get(to_idx) else {
                continue;
            };
            let Some(edge) = graph.edge_between(from_addr, to_addr) else {
                continue;
            };
            if let VenueEdge::Bridge {
                estimated_time_secs,
                ..
            } = &edge.venue
            {
                has_bridge = true;
                if *estimated_time_secs > 0 {
                    total_bridge_latency += *estimated_time_secs as f64;
                }
            }
        }

        if has_bridge {
            let baseline = total_bridge_latency.max(BRIDGE_LATENCY_BASELINE_SECS);
            baseline.max(INVENTORY_LATENCY_SECS)
        } else {
            INVENTORY_LATENCY_SECS
        }
    }

    fn compute_start_priorities(
        &self,
        graph: &Graph,
        base_profiles: &HashMap<Address, TradeSizing>,
        backrun_hints: &[BackrunHint],
    ) -> HashMap<Address, i128> {
        compute_start_priorities_inner(graph, base_profiles, backrun_hints, self.min_flash_loan_wei)
    }

    async fn scan_once_with<F>(&self, mut populate: F) -> Result<ScanOutcome>
    where
        F: for<'a> FnMut(
                &'a mut Graph,
                Arc<Provider<C>>,
                Option<Arc<PoolMonitor<C>>>,
                Arc<UniQuoter<C>>,
                Address,
                String,
                Arc<HashSet<Address>>,
                U256,
                Arc<HashMap<Address, TradeSizing>>,
                u32,
                U256,
                Arc<HashMap<Address, u8>>,
                Arc<HashMap<Address, NativePrice>>,
                f64,
                U256,
                &'a [LowLiquidityPool],
                Arc<HotPathCache>,
                Arc<Semaphore>,
                U64,
            )
                -> Pin<Box<dyn Future<Output = Result<Vec<Edge>>> + Send + 'a>>
            + Send,
    {
        if self.wallet.is_some() {
            let HealthStatus::Healthy {
                balance,
                reserve_txs,
            } = self.check_wallet_health().await?;
            debug!(
                balance = %balance,
                reserve_txs = %reserve_txs,
                "Wallet health check passed"
            );
        }

        let mut graph = Graph::default();
        let search_start = Instant::now();
        let expected_univ3_edges =
            expected_univ3_edge_upper_bound(self.hot_univ3_pools.read().await.len());

        // FAIL CLOSED: the latest block number is a required freshness signal. It
        // drives stale-edge quarantine (max_quote_block_lag) and congestion limits.
        // If we cannot obtain a non-zero head, we must NOT scan or broadcast on
        // unknown/stale state. Returning an rpc-classified error aborts this cycle
        // and lets the run loop back off and retry instead of trading blind.
        let (base_fee, block_number) = match self.provider.get_block(BlockNumber::Latest).await {
            Ok(Some(block)) => {
                let number = block.number.unwrap_or_default();
                if number.is_zero() {
                    return Err(anyhow!(
                        "rpc returned latest block with no number; refusing to scan on stale state (fail-closed)"
                    ));
                }
                (block.base_fee_per_gas, number)
            }
            Ok(None) => {
                return Err(anyhow!(
                    "rpc returned no latest block; refusing to scan on stale state (fail-closed)"
                ));
            }
            Err(err) => {
                warn!(error = %err, "Failed to fetch latest block; failing closed");
                return Err(anyhow::Error::new(err)
                    .context("fetch latest block (rpc); refusing to scan on stale state"));
            }
        };

        let priority_fee = self.broadcast.priority_fee();
        let baseline_calldata = vec![0u8; 120];
        let baseline_tx: TypedTransaction = TransactionRequest::new()
            .data(Bytes::from(baseline_calldata))
            .into();
        let gas_parameters = match self
            .fee_estimator
            .estimate_for_tx(&baseline_tx, Some(U256::from(210_000u64)), priority_fee)
            .await
        {
            Ok(estimate) => estimate,
            Err(err) => {
                warn!(error = %err, chain = %self.chain_name, "Failed to estimate baseline fee");
                FeeEstimate {
                    gas_limit: U256::from(210_000u64),
                    gas_price: U256::zero(),
                    base_fee_per_gas: base_fee,
                    priority_fee_per_gas: priority_fee,
                    max_fee_per_gas: base_fee.and_then(|base| {
                        priority_fee.map(|priority| base.saturating_add(priority))
                    }),
                    max_priority_fee_per_gas: priority_fee,
                    l1_data_fee: U256::zero(),
                    total_fee_native: U256::zero(),
                }
            }
        };

        let gas_price_for_weights = self.gas_price_for_weights(&gas_parameters);

        let mut max_gas_threshold = if self.max_gas_price_wei.is_zero() {
            U256::MAX
        } else {
            self.max_gas_price_wei
        };
        if max_gas_threshold != U256::MAX && self.max_gas_price_congestion_bps > 0 {
            if let Some(base) = base_fee {
                let multiplier_bps = self.max_gas_price_congestion_bps.max(10_000);
                let dynamic_cap = base
                    .checked_mul(U256::from(multiplier_bps))
                    .unwrap_or(U256::MAX)
                    .checked_div(U256::from(10_000u64))
                    .unwrap_or(U256::MAX);
                if dynamic_cap > max_gas_threshold {
                    max_gas_threshold = dynamic_cap;
                }
            }
        }

        if max_gas_threshold != U256::MAX && gas_parameters.gas_price > max_gas_threshold {
            return Ok(ScanOutcome::NotProfitable {
                reason: format!(
                    "gas price {} above max threshold {}",
                    gas_parameters.gas_price, max_gas_threshold
                ),
                edges: 0,
                expected_univ3_edges,
            });
        }

        let congestion_multiplier = {
            let mut tracker = self.congestion.lock().await;
            tracker.observe(base_fee)
        };

        let competition_snapshot = {
            let tracker = self.competition.lock().await;
            tracker.snapshot()
        };
        let competition_multiplier = competition_snapshot.pressure_multiplier;
        let capital_snapshot = self.capital.snapshot();

        let token_decimals_map = Arc::new(self.load_token_decimals().await);
        let tokens = self.tokens.current();
        let native_prices_map = Arc::new(
            self.load_native_prices(tokens.as_ref(), token_decimals_map.as_ref(), block_number)
                .await,
        );
        let base_profiles_map = Arc::new(
            self.compute_base_amounts(token_decimals_map.as_ref(), &capital_snapshot)
                .await,
        );
        self.hot_paths
            .update_top_tokens(base_profiles_map.as_ref())
            .await;
        if self.feature_gate.sandwich {
            self.process_sandwich_opportunities(&gas_parameters, native_prices_map.as_ref())
                .await;
        } else if self.sandwich.is_some() {
            warn!("Sandwich monitor present but FEATURE_SANDWICH=0; ignoring opportunities");
        }
        let low_liquidity_pools = if let Some(scanner) = &self.low_liquidity {
            let mut guard = scanner.lock().await;
            guard.poll(token_decimals_map.as_ref()).await?
        } else {
            Vec::new()
        };
        let base_token_whitelist = self.tokens.current_set();
        let hot_univ2_tokens = self.hot_univ2_pools.read().await.clone();
        let hot_univ3_tokens = self.hot_univ3_pools.read().await.clone();
        let raw_token_whitelist_cap = std::env::var("TOKEN_WHITELIST_MAX")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(512);
        let token_whitelist_cap = sanitize_token_whitelist_cap(raw_token_whitelist_cap);
        if raw_token_whitelist_cap < 64 {
            warn!(
                chain = %self.chain_name,
                configured = raw_token_whitelist_cap,
                effective = token_whitelist_cap,
                "TOKEN_WHITELIST_MAX < 64 can collapse scan coverage; clamping to preserve a production-safe token universe"
            );
        }
        let token_whitelist = Arc::new(build_dynamic_token_whitelist(
            &hot_univ2_tokens,
            &hot_univ3_tokens,
            base_token_whitelist.as_ref(),
            &self.mandatory_universe_tokens,
            self.dynamic_top_tokens_30d,
            token_whitelist_cap,
        ));
        let mut edges = populate(
            &mut graph,
            self.provider.clone(),
            self.pool_monitor.clone(),
            Arc::clone(&self.quoter),
            self.bal_vault,
            self.chain_env_prefix.clone(),
            token_whitelist.clone(),
            capital_snapshot.base_amount,
            base_profiles_map.clone(),
            self.edge_slippage_bps,
            gas_price_for_weights,
            token_decimals_map.clone(),
            native_prices_map.clone(),
            self.min_liquidity_tokens,
            self.min_edge_max_input,
            &low_liquidity_pools,
            self.hot_paths.clone(),
            self.quote_semaphore.clone(),
            block_number,
        )
        .await?;

        if self.feature_gate.bridge {
            if let Some(bridge) = &self.bridge {
                let mut bridge_edges = bridge.add_edges(
                    &mut graph,
                    base_profiles_map.as_ref(),
                    capital_snapshot.base_amount,
                    gas_price_for_weights,
                    native_prices_map.as_ref(),
                )?;
                edges.append(&mut bridge_edges);
            }
        } else if self.bridge.is_some() {
            warn!("Bridge planner present but FEATURE_BRIDGE=0; skipping bridge edges");
        }

        if self.feature_gate.liquidations {
            if let Some(liquidations) = &self.liquidations {
                for opportunity in liquidations.poll().await? {
                    if let Some(monitor) = &self.backrun {
                        monitor
                            .record_liquidation(
                                opportunity.debt_token,
                                opportunity.collateral_token,
                                opportunity.repay_amount,
                                opportunity.protocol.as_str(),
                            )
                            .await;
                    }

                    let base_amount = base_profiles_map
                        .get(&opportunity.debt_token)
                        .map(|profile| profile.base_amount)
                        .unwrap_or(capital_snapshot.base_amount);
                    let edge = opportunity.to_edge(
                        gas_price_for_weights,
                        base_amount,
                        self.native_price_for(opportunity.debt_token, native_prices_map.as_ref()),
                    );
                    graph.add_edge(edge.clone());
                    edges.push(edge);
                }
            }
        } else if self.liquidations.is_some() {
            warn!("Liquidation monitor present but FEATURE_LIQUIDATIONS=0; skipping liquidation edges");
        }

        let profitability = build_profitability_snapshot(&self.hot_paths).await;
        prune_edges_by_quality(
            &mut graph,
            self.max_edges_hot,
            self.edge_prune_max_slippage_bps,
            self.edge_prune_min_score,
            EdgeScoreWeights {
                liquidity: self.edge_prune_liquidity_weight,
                profitability: self.edge_prune_profit_weight,
                slippage: self.edge_prune_slippage_weight,
            },
            &profitability,
        )
        .await;

        graph.refresh_incremental_adjacency_with_metrics(
            self.metrics.as_deref(),
            Some(&self.chain_name),
        );

        let edges_scanned = graph.edges.iter().filter(|edge| edge.active).count();
        let current_digest = graph_digest(&graph);
        let significant_change = {
            let mut guard = self.last_graph_digest.lock().await;
            let changed = graph_changed_significantly(*guard, current_digest);
            *guard = Some(current_digest);
            changed
        };
        let mut last_skip_reason: Option<String> = None;

        struct CandidatePlan {
            plan_args: ExecutorPlan,
            cycle_start: Address,
            amount_in: U256,
            est_gross_after_fee: U256,
            gas_cost: U256,
            gas_cost_native: U256,
            net_profit: U256,
            hops: usize,
            gas_limit: U256,
            #[allow(dead_code)]
            l1_data_fee: U256,
            fee_estimate: FeeEstimate,
            max_slippage_bps: u32,
            slippage_floor: U256,
            congestion_multiplier: f64,
            competition_snapshot: CompetitionSnapshot,
            competition_buffer: U256,
            cycle_latency_secs: f64,
            native_price: NativePrice,
            cycle_edges: Vec<(Address, Address, u32)>,
            has_bridge_step: bool,
            liquidation_markets: Vec<String>,
            strategy: Strategy,
            venue_path: Vec<String>,
            candidate_id: String,
        }

        let mut best_candidate: Option<CandidatePlan> = None;
        let executor_address = self.executor.address();
        let backrun_hints = if let Some(monitor) = &self.backrun {
            monitor.active_hints(Duration::from_secs(45)).await
        } else {
            Vec::new()
        };

        let start_priorities =
            self.compute_start_priorities(&graph, base_profiles_map.as_ref(), &backrun_hints);
        let seed_cycles: Vec<Vec<usize>> = {
            let guard = self.previous_cycle_seeds.lock().await;
            guard
                .iter()
                .filter_map(|cycle| map_cycle_addresses_to_indices(&graph, cycle))
                .collect()
        };
        let raw_cycles: Vec<Vec<usize>> = if significant_change {
            let mut cycles: Vec<Vec<usize>> = graph
                .bellman_ford(
                    &start_priorities,
                    &self.cycle_limits,
                    self.max_candidate_paths,
                    self.metrics.as_deref(),
                )
                .into_iter()
                .map(|candidate| candidate.cycle)
                .collect();
            if !seed_cycles.is_empty() {
                cycles.splice(0..0, seed_cycles.clone());
            }
            cycles
        } else {
            seed_cycles
        };

        let mut canonical_order: Vec<Vec<usize>> = Vec::new();
        let mut canonical_buckets: HashMap<Vec<usize>, Vec<(usize, Vec<usize>)>> = HashMap::new();

        for mut cycle in raw_cycles {
            if cycle.len() < 2 {
                continue;
            }

            let Some(&first) = cycle.first() else {
                continue;
            };

            if cycle.last() != Some(&first) {
                cycle.push(first);
            }

            let signature = canonicalize_cycle(cycle.clone());
            let start_ix = first;

            if let Some(entries) = canonical_buckets.get_mut(&signature) {
                if !entries.iter().any(|(ix, _)| *ix == start_ix) {
                    entries.push((start_ix, cycle));
                }
            } else {
                canonical_order.push(signature.clone());
                canonical_buckets.insert(signature, vec![(start_ix, cycle)]);
            }
        }

        let mut candidate_cycles: Vec<Vec<usize>> = Vec::new();
        for signature in canonical_order {
            if let Some(mut entries) = canonical_buckets.remove(&signature) {
                let mut added = false;
                let mut fallback: Option<Vec<usize>> = None;

                for (start_ix, cycle) in entries.drain(..) {
                    let start_token = graph.nodes[start_ix];
                    if !self
                        .flash_loan_quotes(start_token, capital_snapshot.max_flash_loan)
                        .is_empty()
                    {
                        candidate_cycles.push(cycle);
                        added = true;
                    } else if fallback.is_none() {
                        fallback = Some(cycle);
                    }
                }

                if !added {
                    if let Some(cycle) = fallback {
                        candidate_cycles.push(cycle);
                    }
                }
            }
        }

        for cycle in candidate_cycles.iter() {
            if cycle_rejected_by_hub_filter(cycle, &graph, &self.hub_tokens) {
                let start_token = cycle.first().and_then(|ix| graph.nodes.get(*ix)).copied();
                let candidate_id = start_token
                    .map(|token| self.stage_candidate_id(token, cycle, &graph, block_number, &[]));
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    candidate_id,
                    start_token,
                    Some(cycle.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    true,
                    None,
                    Some("hub_filter_rejected"),
                    None,
                    false,
                    false,
                );
            }
        }
        candidate_cycles = filter_cycles_by_hubs(candidate_cycles, &graph, &self.hub_tokens);
        candidate_cycles = cap_cycles_per_start(candidate_cycles, &graph, self.topk_per_token);
        if candidate_cycles.len() > self.max_candidate_paths {
            candidate_cycles.truncate(self.max_candidate_paths);
        }

        {
            let mut guard = self.previous_cycle_seeds.lock().await;
            *guard = candidate_cycles
                .iter()
                .map(|cycle| cycle_indices_to_addresses(&graph, cycle))
                .collect();
        }

        if search_start.elapsed() > self.search_budget {
            warn!(
                elapsed_ms = search_start.elapsed().as_millis(),
                budget_ms = self.search_budget.as_millis(),
                "search budget exceeded; proceeding with reduced candidates"
            );
        }

        if let Some(metrics) = &self.metrics {
            let latency_ms = search_start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            metrics.record_stage_latency(&self.chain_name, "search", latency_ms);
        }

        let quote_start = Instant::now();
        'cycle: for cycle_ix in candidate_cycles {
            if cycle_ix.len() < 2 {
                continue;
            }
            if quote_start.elapsed() > self.quote_budget {
                warn!(
                    elapsed_ms = quote_start.elapsed().as_millis(),
                    budget_ms = self.quote_budget.as_millis(),
                    "quote budget exceeded; skipping remaining candidates"
                );
                break;
            }
            let cycle_start_ix = match cycle_ix.first() {
                Some(ix) => *ix,
                None => continue,
            };
            let cycle_start = graph.nodes[cycle_start_ix];
            let mut native_price = self.native_price_for(cycle_start, native_prices_map.as_ref());
            if cycle_start == self.wrapped_native && !native_price.is_reliable() {
                let amount = U256::exp10(18);
                native_price = NativePrice::new(amount, amount, true);
            }
            let pricing_reliable =
                start_token_pricing_reliable(cycle_start, self.wrapped_native, native_price);
            let candidate_id =
                self.stage_candidate_id(cycle_start, &cycle_ix, &graph, block_number, &[]);
            if !pricing_reliable {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    Some("unreliable_native_price_for_start_token"),
                    None,
                    false,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} rejected: unreliable native price for start token",
                    hex::encode(cycle_start)
                ));
                continue;
            }
            let Some(competition_buffer) =
                native_price.tokens_for_native_strict(competition_snapshot.extra_buffer_wei)
            else {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    Some(competition_snapshot.extra_buffer_wei),
                    None,
                    false,
                    None,
                    Some("unreliable_native_price_for_start_token"),
                    None,
                    false,
                    false,
                );
                continue;
            };
            if let Some(&last_ix) = cycle_ix.last() {
                debug_assert_eq!(
                    graph.nodes[last_ix], cycle_start,
                    "cycle must terminate at the starting token"
                );
            }

            let cycle_base_amount = base_profiles_map
                .get(&cycle_start)
                .map(|profile| profile.base_amount)
                .unwrap_or(capital_snapshot.base_amount);
            let mut estimated_cycle_gas: u64 = 0;
            let mut cycle_max_input = cycle_base_amount;
            let cycle_latency_secs = self.estimate_cycle_latency(&graph, &cycle_ix);
            let mut cycle_edges_vec: Vec<Edge> =
                Vec::with_capacity(cycle_ix.len().saturating_sub(1));
            let mut backrun_hint: Option<BackrunHint> = None;
            let mut has_bridge_step = false;
            for window in cycle_ix.windows(2) {
                let u = graph.nodes[window[0]];
                let v = graph.nodes[window[1]];
                let Some(edge) = graph.edge_between(u, v) else {
                    warn!(from = %u, to = %v, "Skipping cycle due to missing edge");
                    self.log_candidate_stage(
                        "candidate_rejected_pre_sim",
                        &self.chain_name,
                        Some(candidate_id.clone()),
                        Some(cycle_start),
                        Some(cycle_ix.len().saturating_sub(1)),
                        edges_scanned,
                        None,
                        None,
                        None,
                        None,
                        pricing_reliable,
                        None,
                        Some("invalid_edge"),
                        None,
                        false,
                        false,
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.record_failure(U256::zero());
                    }
                    let _ = self.circuit_breaker.record_failure(U256::zero()).await;
                    continue 'cycle;
                };
                estimated_cycle_gas = estimated_cycle_gas.saturating_add(edge.estimated_gas);
                cycle_max_input = cycle_max_input.min(edge.max_input);
                if matches!(edge.venue, VenueEdge::Bridge { .. }) {
                    has_bridge_step = true;
                }
                cycle_edges_vec.push(edge.clone());
            }

            if has_bridge_step && !self.feature_gate.bridge {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("invalid_candidate_path"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason =
                    Some("cycle requires bridge step but FEATURE_BRIDGE=0".to_string());
                continue;
            }

            if cycle_max_input.is_zero() {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("no_liquidity"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} has zero capacity after slippage control",
                    hex::encode(cycle_start)
                ));
                continue;
            }

            let mut trade_cap = cycle_base_amount.min(cycle_max_input);

            if let Some(monitor) = &self.backrun {
                if self.broadcast.role == MevRole::Filler && cycle_ix.len() >= 2 {
                    let from = graph.nodes[cycle_ix[0]];
                    let next_idx = cycle_ix[1];
                    let to = graph.nodes[next_idx];
                    if let Some(pending_amount) = monitor
                        .best_amount_for(from, to, Duration::from_secs(30))
                        .await
                    {
                        if !pending_amount.is_zero() {
                            trade_cap = trade_cap.min(pending_amount);
                        }
                    }

                    if let Some(hint) = monitor.hint_for(from, to, Duration::from_secs(45)).await {
                        backrun_hint = Some(hint);
                        if let Some(amount) = backrun_hint.as_ref().map(|h| h.amount_in) {
                            trade_cap = trade_cap.min(amount);
                        }
                    }
                }
            }

            if trade_cap.is_zero() {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("no_quote_available"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} reduced to zero trade size",
                    hex::encode(cycle_start)
                ));
                continue;
            }

            let quotes = self.flash_loan_quotes(cycle_start, trade_cap);
            if quotes.is_empty() {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("no_flashloan_provider"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} unsupported by flash loan providers",
                    hex::encode(cycle_start)
                ));
                continue;
            }

            let preview_plan = match build_plan_for_cycle(
                &graph,
                &cycle_ix,
                cycle_base_amount,
                executor_address,
                self.jit_config.as_ref(),
                Some(self.quoter.as_ref()),
                block_number,
            )
            .await
            {
                Ok(plan) => plan,
                Err(err) => {
                    warn!(error = %err, "Skipping candidate due to plan construction failure");
                    self.log_candidate_stage(
                        "candidate_rejected_pre_sim",
                        &self.chain_name,
                        Some(candidate_id.clone()),
                        Some(cycle_start),
                        Some(cycle_ix.len().saturating_sub(1)),
                        edges_scanned,
                        None,
                        None,
                        None,
                        None,
                        pricing_reliable,
                        None,
                        Some("plan_build_failed"),
                        None,
                        has_bridge_step,
                        false,
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.record_failure(U256::zero());
                    }
                    let _ = self.circuit_breaker.record_failure(U256::zero()).await;
                    continue;
                }
            };

            let mut adjusted_cycle_gas = estimated_cycle_gas;
            if self
                .jit_config
                .as_ref()
                .map(|cfg| cfg.enabled)
                .unwrap_or(false)
            {
                let jit_add_steps = preview_plan
                    .steps
                    .iter()
                    .filter(|s| matches!(s, StepData::JitLiquidityAdd { .. }))
                    .count() as u64;
                let jit_remove_steps = preview_plan
                    .steps
                    .iter()
                    .filter(|s| matches!(s, StepData::JitLiquidityRemove { .. }))
                    .count() as u64;
                adjusted_cycle_gas = adjusted_cycle_gas
                    .saturating_add(jit_add_steps.saturating_mul(JIT_PRESWAP_ESTIMATED_GAS))
                    .saturating_add(jit_add_steps.saturating_mul(JIT_LP_ADD_ESTIMATED_GAS))
                    .saturating_add(jit_remove_steps.saturating_mul(JIT_LP_REMOVE_ESTIMATED_GAS));
            }

            let Some(sizing) = optimize_trade_size(OptimizeTradeParams {
                edges: &cycle_edges_vec,
                quotes: &quotes,
                min_amount: capital_snapshot.min_flash_loan,
                max_amount: trade_cap,
                gas_price: gas_parameters.gas_price,
                estimated_gas: adjusted_cycle_gas,
                l1_data_fee: gas_parameters.l1_data_fee,
                native_price,
                quoter: self.quoter.as_ref(),
                bal_quote: self.bal_quote.as_ref(),
                curve_quote: self.curve_quote.as_ref(),
                block_number,
            })
            .await
            else {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("no_quote_available"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} had no profitable sizing",
                    hex::encode(cycle_start)
                ));
                continue;
            };

            let trade_amount = sizing.amount_in;
            let plan = match build_plan_for_cycle(
                &graph,
                &cycle_ix,
                trade_amount,
                executor_address,
                self.jit_config.as_ref(),
                Some(self.quoter.as_ref()),
                block_number,
            )
            .await
            {
                Ok(plan) => plan,
                Err(err) => {
                    warn!(error = %err, "Skipping candidate due to plan reconstruction failure");
                    self.log_candidate_stage(
                        "candidate_rejected_pre_sim",
                        &self.chain_name,
                        Some(candidate_id.clone()),
                        Some(cycle_start),
                        Some(cycle_ix.len().saturating_sub(1)),
                        edges_scanned,
                        None,
                        None,
                        None,
                        None,
                        pricing_reliable,
                        None,
                        Some("plan_build_failed"),
                        None,
                        has_bridge_step,
                        false,
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.record_failure(U256::zero());
                    }
                    let _ = self.circuit_breaker.record_failure(U256::zero()).await;
                    continue;
                }
            };

            let est_gross = sizing.gross;
            let flash_fee_amount = sizing.flash_fee;
            let est_gross_after_fee = est_gross.saturating_sub(flash_fee_amount);
            if let Some(metrics) = &self.metrics {
                metrics.record_sizing_quotes(sizing.quote_count);
            }
            if let Err(err) = ensure_single_loan_allocation(&sizing.allocations) {
                if let Some(metrics) = &self.metrics {
                    metrics.record_multi_loan_rejection(&self.chain_name);
                }
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    Some(est_gross_after_fee),
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("unsupported_loan_count"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(err.to_string());
                continue;
            }

            let (swap_slippage_bps, profit_floor_bps) = resolve_slippage_and_profit_floors(
                plan.cycle_slippage_bps,
                sizing.max_slippage_bps,
                self.executor_max_slippage_bps,
            );
            let slippage_floor = mul_div(
                trade_amount,
                U256::from(profit_floor_bps),
                U256::from(10_000u64),
            );

            let mut min_profit_requirement = finalize_profit_threshold(ProfitThresholdParams {
                base_threshold: self.dynamic_min_profit(DynamicProfitParams {
                    fee: &gas_parameters,
                    est_gas: adjusted_cycle_gas,
                    est_gross: est_gross_after_fee,
                    congestion: congestion_multiplier,
                    competition: &competition_snapshot,
                    latency_secs: cycle_latency_secs,
                    native_price,
                }),
                competition_buffer,
                flash_fee_amount,
                slippage_floor,
                has_bridge_step,
                est_gross_after_fee,
                cross_chain_profit_bps: self.cross_chain_profit_bps,
                cross_chain_min_profit_wei: self.cross_chain_min_profit_wei,
                backrun_hint: backrun_hint.as_ref(),
            });

            if est_gross_after_fee <= min_profit_requirement {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    Some(est_gross_after_fee),
                    None,
                    None,
                    pricing_reliable,
                    Some(min_profit_requirement),
                    Some("below_min_profit_threshold"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} grossWei={} thresholdWei={}",
                    hex::encode(cycle_start),
                    est_gross_after_fee,
                    min_profit_requirement
                ));
                continue;
            }
            let mut ops: Vec<ExecutorStep> = Vec::with_capacity(plan.steps.len());
            for step in plan.steps {
                match step {
                    StepData::Uniswap {
                        path,
                        amount_in,
                        min_out,
                    } => {
                        let data = ethers::abi::encode(&[
                            Token::Bytes(path.to_vec()),
                            Token::Uint(amount_in),
                            Token::Uint(min_out),
                        ]);
                        ops.push(ExecutorStep {
                            op: EXECUTOR_OP_UNIV3,
                            data: Bytes::from(data),
                        });
                    }
                    StepData::JitLiquidityAdd {
                        pool,
                        token0,
                        token1,
                        amount0,
                        amount1,
                        tick_range,
                    } => {
                        let data = ethers::abi::encode(&[
                            Token::Address(pool),
                            Token::Address(token0),
                            Token::Address(token1),
                            Token::Uint(amount0),
                            Token::Uint(amount1),
                            Token::Uint(U256::from(tick_range)),
                        ]);
                        ops.push(ExecutorStep {
                            op: EXECUTOR_OP_JIT_LP_ADD,
                            data: Bytes::from(data),
                        });
                    }
                    StepData::JitLiquidityRemove {
                        pool,
                        target_token,
                        fee,
                        min_out,
                    } => {
                        let data = ethers::abi::encode(&[
                            Token::Address(pool),
                            Token::Address(target_token),
                            Token::Uint(U256::from(fee)),
                            Token::Uint(min_out),
                        ]);
                        ops.push(ExecutorStep {
                            op: EXECUTOR_OP_JIT_LP_REMOVE,
                            data: Bytes::from(data),
                        });
                    }
                    StepData::Balancer {
                        pool_id,
                        token_in,
                        token_out,
                        amount_in,
                        min_out,
                    } => {
                        let data = ethers::abi::encode(&[
                            Token::FixedBytes(pool_id.to_vec()),
                            Token::Address(token_in),
                            Token::Address(token_out),
                            Token::Uint(amount_in),
                            Token::Uint(min_out),
                        ]);
                        ops.push(ExecutorStep {
                            op: EXECUTOR_OP_BALANCER,
                            data: Bytes::from(data),
                        });
                    }
                    StepData::Bridge {
                        adapter,
                        token_in,
                        amount_in,
                        dst_chain_id,
                        max_bridge_time_secs,
                        call,
                    } => {
                        let data = ethers::abi::encode(&[
                            Token::Address(adapter),
                            Token::Address(token_in),
                            Token::Uint(amount_in),
                            Token::Uint(U256::from(dst_chain_id)),
                            Token::Uint(U256::from(max_bridge_time_secs)),
                            Token::Bytes(call.to_vec()),
                        ]);
                        ops.push(ExecutorStep {
                            op: EXECUTOR_OP_BRIDGE,
                            data: Bytes::from(data),
                        });
                    }
                    StepData::Generic {
                        target,
                        call,
                        pre_action,
                    } => {
                        let (action, token, amount) = match pre_action {
                            Some(GenericPreAction::Approve { token, amount }) => {
                                (U256::from(1u64), token, amount)
                            }
                            Some(GenericPreAction::Transfer { token, amount }) => {
                                (U256::from(2u64), token, amount)
                            }
                            None => (U256::zero(), Address::zero(), U256::zero()),
                        };
                        let data = ethers::abi::encode(&[
                            Token::Address(target),
                            Token::Bytes(call.to_vec()),
                            Token::Uint(action),
                            Token::Address(token),
                            Token::Uint(amount),
                        ]);
                        ops.push(ExecutorStep {
                            op: EXECUTOR_OP_GENERIC,
                            data: Bytes::from(data),
                        });
                    }
                }
            }

            let loans: Vec<ExecutorLoan> = sizing
                .allocations
                .iter()
                .map(|alloc| ExecutorLoan {
                    token: cycle_start,
                    amount: alloc.amount,
                    provider: alloc.provider.as_id(),
                    provider_addr: alloc.provider_addr.unwrap_or_else(|| match alloc.provider {
                        FlashLoanProvider::Balancer => self.bal_vault,
                        FlashLoanProvider::AaveV3 => self.aave_pool.unwrap_or_default(),
                        FlashLoanProvider::Erc3156 => Address::zero(),
                        FlashLoanProvider::Univ2Flashswap => Address::zero(),
                        FlashLoanProvider::Univ3Flash => Address::zero(),
                    }),
                })
                .collect();

            if loans.iter().any(|loan| {
                (loan.provider == FlashLoanProvider::Erc3156.as_id()
                    || loan.provider == FlashLoanProvider::Univ2Flashswap.as_id()
                    || loan.provider == FlashLoanProvider::Univ3Flash.as_id())
                    && loan.provider_addr.is_zero()
            }) {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    Some(est_gross_after_fee),
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("no_flashloan_provider"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some("flash loan provider address missing".to_string());
                continue;
            }

            let mut plan_args = ExecutorPlan {
                loans,
                cycle_slippage_bps: profit_floor_bps.min(u32::from(u16::MAX)) as u16,
                steps: ops,
                min_profit: min_profit_requirement,
            };

            let Some(call) = self.build_executor_call(&plan_args) else {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    Some(est_gross_after_fee),
                    None,
                    None,
                    pricing_reliable,
                    Some(min_profit_requirement),
                    Some("unsupported_loan_count"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some("plan has unsupported loan count".to_string());
                continue;
            };
            let gas_limit = match self
                .estimate_gas_with_fallback(&call, cycle_ix.len().saturating_sub(1))
                .await
            {
                Ok(limit) => limit,
                Err(err) => {
                    self.log_candidate_stage(
                        "candidate_rejected_pre_sim",
                        &self.chain_name,
                        Some(candidate_id.clone()),
                        Some(cycle_start),
                        Some(cycle_ix.len().saturating_sub(1)),
                        edges_scanned,
                        None,
                        Some(est_gross_after_fee),
                        None,
                        None,
                        pricing_reliable,
                        Some(min_profit_requirement),
                        Some("gas_estimation_failed"),
                        None,
                        has_bridge_step,
                        false,
                    );
                    let reason = format!(
                        "cycle start=0x{} gas estimation failed: {err}",
                        hex::encode(cycle_start)
                    );
                    warn!(
                        error = %err,
                        start = %format!("0x{}", hex::encode(cycle_start)),
                        "Skipping cycle due to gas estimation failure",
                    );
                    last_skip_reason = Some(reason);
                    continue;
                }
            };

            let calldata = call.calldata().unwrap_or_default();
            let mut tx_for_fee = call.tx.clone();
            tx_for_fee.set_data(calldata.clone());
            let fee_estimate = match self
                .fee_estimator
                .estimate_for_tx(&tx_for_fee, Some(gas_limit), priority_fee)
                .await
            {
                Ok(fee) => fee,
                Err(err) => {
                    warn!(error = %err, chain = %self.chain_name, "Falling back to baseline fee estimate");
                    FeeEstimate {
                        gas_limit,
                        gas_price: gas_parameters.gas_price,
                        base_fee_per_gas: gas_parameters.base_fee_per_gas,
                        priority_fee_per_gas: gas_parameters.priority_fee_per_gas,
                        max_fee_per_gas: gas_parameters.max_fee_per_gas,
                        max_priority_fee_per_gas: gas_parameters.max_priority_fee_per_gas,
                        l1_data_fee: gas_parameters.l1_data_fee,
                        total_fee_native: gas_parameters
                            .gas_price
                            .saturating_mul(gas_limit)
                            .saturating_add(gas_parameters.l1_data_fee),
                    }
                }
            };
            let gas_limit = gas_limit.max(fee_estimate.gas_limit);
            let mut fee_estimate = fee_estimate;
            if gas_limit != fee_estimate.gas_limit {
                fee_estimate.gas_limit = gas_limit;
                fee_estimate.total_fee_native = fee_estimate
                    .gas_price
                    .saturating_mul(gas_limit)
                    .saturating_add(fee_estimate.l1_data_fee);
            }
            let gas_units = if gas_limit > U256::from(u64::MAX) {
                u64::MAX
            } else {
                gas_limit.as_u64()
            };

            let final_threshold = self.dynamic_min_profit(DynamicProfitParams {
                fee: &fee_estimate,
                est_gas: gas_units,
                est_gross: est_gross_after_fee,
                congestion: congestion_multiplier,
                competition: &competition_snapshot,
                latency_secs: cycle_latency_secs,
                native_price,
            });
            min_profit_requirement = finalize_profit_threshold(ProfitThresholdParams {
                base_threshold: final_threshold,
                competition_buffer,
                flash_fee_amount,
                slippage_floor,
                has_bridge_step,
                est_gross_after_fee,
                cross_chain_profit_bps: self.cross_chain_profit_bps,
                cross_chain_min_profit_wei: self.cross_chain_min_profit_wei,
                backrun_hint: backrun_hint.as_ref(),
            });
            plan_args.min_profit = min_profit_requirement;

            if est_gross_after_fee <= min_profit_requirement {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    Some(est_gross_after_fee),
                    Some(fee_estimate.total_fee_native),
                    native_price.tokens_for_native_strict(fee_estimate.total_fee_native),
                    pricing_reliable,
                    Some(min_profit_requirement),
                    Some("below_min_profit_threshold"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} grossWei={} thresholdWei={}",
                    hex::encode(cycle_start),
                    est_gross_after_fee,
                    min_profit_requirement
                ));
                continue;
            }

            let gas_cost_native = fee_estimate.total_fee_native;
            let Some(gas_cost) = native_price.tokens_for_native_strict(gas_cost_native) else {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    Some(est_gross_after_fee),
                    Some(gas_cost_native),
                    None,
                    false,
                    Some(min_profit_requirement),
                    Some("unreliable_native_price_for_start_token"),
                    None,
                    has_bridge_step,
                    false,
                );
                continue;
            };
            let net_profit = est_gross_after_fee.saturating_sub(gas_cost);
            if net_profit.is_zero() {
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    None,
                    Some(est_gross_after_fee),
                    Some(gas_cost_native),
                    Some(gas_cost),
                    pricing_reliable,
                    Some(min_profit_requirement),
                    Some("below_min_profit_threshold"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} net profit zero after gas",
                    hex::encode(cycle_start)
                ));
                continue;
            }

            if best_candidate
                .as_ref()
                .map(|candidate| net_profit > candidate.net_profit)
                .unwrap_or(true)
            {
                let mut liquidation_markets: Vec<String> = cycle_edges_vec
                    .iter()
                    .filter_map(|edge| match &edge.venue {
                        VenueEdge::Liquidation { protocol, .. } => Some(protocol.clone()),
                        _ => None,
                    })
                    .collect();
                liquidation_markets.sort();
                liquidation_markets.dedup();
                let strategy = if !liquidation_markets.is_empty() {
                    Strategy::Liquidation
                } else if backrun_hint.is_some() {
                    Strategy::Backrun
                } else {
                    Strategy::Arb
                };
                let venue_path = build_venue_path(&cycle_edges_vec);
                let fee_tiers: Vec<u32> = cycle_edges_vec
                    .iter()
                    .map(|edge| match &edge.venue {
                        VenueEdge::UniV3 { fee, .. } => *fee,
                        _ => 0,
                    })
                    .collect();
                let candidate_id = self.stage_candidate_id(
                    cycle_start,
                    &cycle_ix,
                    &graph,
                    block_number,
                    &fee_tiers,
                );
                self.log_candidate_stage(
                    "candidate_selected",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(cycle_ix.len().saturating_sub(1)),
                    edges_scanned,
                    Some(venue_path.clone()),
                    Some(est_gross_after_fee),
                    Some(gas_cost_native),
                    Some(gas_cost),
                    pricing_reliable,
                    Some(min_profit_requirement),
                    None,
                    None,
                    has_bridge_step,
                    !liquidation_markets.is_empty(),
                );
                best_candidate = Some(CandidatePlan {
                    plan_args,
                    cycle_start,
                    amount_in: trade_amount,
                    est_gross_after_fee,
                    gas_cost,
                    gas_cost_native,
                    l1_data_fee: fee_estimate.l1_data_fee,
                    fee_estimate: fee_estimate.clone(),
                    net_profit,
                    hops: cycle_ix.len().saturating_sub(1),
                    gas_limit,
                    max_slippage_bps: swap_slippage_bps,
                    slippage_floor,
                    congestion_multiplier,
                    competition_snapshot: competition_snapshot.clone(),
                    competition_buffer,
                    cycle_latency_secs,
                    native_price,
                    cycle_edges: cycle_edges_vec
                        .iter()
                        .filter_map(|edge| match &edge.venue {
                            VenueEdge::UniV3 { path, .. } => {
                                if path.len() < 2 {
                                    return None;
                                }
                                let fee = path
                                    .iter()
                                    .skip(1)
                                    .find_map(|(_, maybe_fee)| maybe_fee.as_ref().copied())
                                    .unwrap_or_default();
                                Some((edge.from, edge.to, fee))
                            }
                            _ => None,
                        })
                        .collect(),
                    has_bridge_step,
                    liquidation_markets,
                    strategy,
                    venue_path,
                    candidate_id,
                });
            }
        }

        if let Some(metrics) = &self.metrics {
            let latency_ms = quote_start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            metrics.record_stage_latency(&self.chain_name, "quote", latency_ms);
        }

        if let Some(candidate) = best_candidate {
            let mut candidate = candidate;
            if let Some(metrics) = &self.metrics {
                metrics.record_opportunity_seen(&self.chain_name, candidate.strategy.as_str());
            }
            let simulate_start = Instant::now();
            self.log_candidate_stage(
                "candidate_sent_to_sim",
                &self.chain_name,
                Some(format!("{}", candidate.candidate_id)),
                Some(candidate.cycle_start),
                Some(candidate.hops),
                edges_scanned,
                Some(candidate.venue_path.clone()),
                Some(candidate.est_gross_after_fee),
                Some(candidate.gas_cost_native),
                Some(candidate.gas_cost),
                start_token_pricing_reliable(
                    candidate.cycle_start,
                    self.wrapped_native,
                    candidate.native_price,
                ),
                Some(candidate.plan_args.min_profit),
                None,
                Some("pending"),
                candidate.has_bridge_step,
                !candidate.liquidation_markets.is_empty(),
            );
            let (simulated_gas_used, simulated_profit) = match timeout(
                self.simulation_budget,
                self.simulate_plan_execution(&candidate.plan_args, &candidate.fee_estimate),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => {
                    self.log_candidate_stage(
                        "candidate_rejected_post_sim",
                        &self.chain_name,
                        Some(format!("{}", candidate.candidate_id)),
                        Some(candidate.cycle_start),
                        Some(candidate.hops),
                        edges_scanned,
                        Some(candidate.venue_path.clone()),
                        Some(candidate.est_gross_after_fee),
                        Some(candidate.gas_cost_native),
                        Some(candidate.gas_cost),
                        start_token_pricing_reliable(
                            candidate.cycle_start,
                            self.wrapped_native,
                            candidate.native_price,
                        ),
                        Some(candidate.plan_args.min_profit),
                        Some("simulation_failed"),
                        Some("failed"),
                        candidate.has_bridge_step,
                        !candidate.liquidation_markets.is_empty(),
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.record_simulation(
                            &self.chain_name,
                            candidate.strategy.as_str(),
                            false,
                        );
                        let latency_ms = simulate_start
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX))
                            as u64;
                        metrics.record_stage_latency(&self.chain_name, "simulate", latency_ms);
                    }
                    if let Some(liquidations) = &self.liquidations {
                        for market in &candidate.liquidation_markets {
                            liquidations.record_revert(market).await;
                        }
                    }
                    return Ok(ScanOutcome::NotProfitable {
                        reason: format!(
                            "cycle start=0x{} simulation failed: {}",
                            hex::encode(candidate.cycle_start),
                            err
                        ),
                        edges: edges_scanned,
                        expected_univ3_edges,
                    });
                }
                Err(_) => {
                    self.log_candidate_stage(
                        "candidate_rejected_post_sim",
                        &self.chain_name,
                        Some(format!("{}", candidate.candidate_id)),
                        Some(candidate.cycle_start),
                        Some(candidate.hops),
                        edges_scanned,
                        Some(candidate.venue_path.clone()),
                        Some(candidate.est_gross_after_fee),
                        Some(candidate.gas_cost_native),
                        Some(candidate.gas_cost),
                        start_token_pricing_reliable(
                            candidate.cycle_start,
                            self.wrapped_native,
                            candidate.native_price,
                        ),
                        Some(candidate.plan_args.min_profit),
                        Some("simulation_timeout"),
                        Some("timeout"),
                        candidate.has_bridge_step,
                        !candidate.liquidation_markets.is_empty(),
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.record_simulation(
                            &self.chain_name,
                            candidate.strategy.as_str(),
                            false,
                        );
                        let latency_ms = simulate_start
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX))
                            as u64;
                        metrics.record_stage_latency(&self.chain_name, "simulate", latency_ms);
                    }
                    return Ok(ScanOutcome::NotProfitable {
                        reason: format!(
                            "cycle start=0x{} simulation budget exceeded",
                            hex::encode(candidate.cycle_start)
                        ),
                        edges: edges_scanned,
                        expected_univ3_edges,
                    });
                }
            };
            if let Some(metrics) = &self.metrics {
                metrics.record_simulation(&self.chain_name, candidate.strategy.as_str(), true);
                let latency_ms = simulate_start
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                metrics.record_stage_latency(&self.chain_name, "simulate", latency_ms);
            }
            if simulated_profit.is_zero() {
                self.log_candidate_stage(
                    "candidate_rejected_post_sim",
                    &self.chain_name,
                    Some(format!("{}", candidate.candidate_id)),
                    Some(candidate.cycle_start),
                    Some(candidate.hops),
                    edges_scanned,
                    Some(candidate.venue_path.clone()),
                    Some(candidate.est_gross_after_fee),
                    Some(candidate.gas_cost_native),
                    Some(candidate.gas_cost),
                    start_token_pricing_reliable(
                        candidate.cycle_start,
                        self.wrapped_native,
                        candidate.native_price,
                    ),
                    Some(candidate.plan_args.min_profit),
                    Some("simulation_failed"),
                    Some("zero_profit"),
                    candidate.has_bridge_step,
                    !candidate.liquidation_markets.is_empty(),
                );
                return Ok(ScanOutcome::NotProfitable {
                    reason: format!(
                        "cycle start=0x{} simulation returned zero profit",
                        hex::encode(candidate.cycle_start)
                    ),
                    edges: edges_scanned,
                    expected_univ3_edges,
                });
            }

            let buffered_sim_limit = simulated_gas_used
                .saturating_mul(U256::from(12u64))
                .checked_div(U256::from(10u64))
                .unwrap_or(simulated_gas_used);
            candidate.gas_limit = candidate.gas_limit.max(buffered_sim_limit);
            if candidate.gas_limit != candidate.fee_estimate.gas_limit {
                candidate.fee_estimate.gas_limit = candidate.gas_limit;
                candidate.fee_estimate.total_fee_native = candidate
                    .fee_estimate
                    .gas_price
                    .saturating_mul(candidate.gas_limit)
                    .saturating_add(candidate.fee_estimate.l1_data_fee);
            }

            let gas_limit_u64 = if candidate.gas_limit > U256::from(u64::MAX) {
                u64::MAX
            } else {
                candidate.gas_limit.as_u64()
            };

            let recomputed_min_profit = self
                .dynamic_min_profit(DynamicProfitParams {
                    fee: &candidate.fee_estimate,
                    est_gas: gas_limit_u64,
                    est_gross: candidate.est_gross_after_fee,
                    congestion: candidate.congestion_multiplier,
                    competition: &candidate.competition_snapshot,
                    latency_secs: candidate.cycle_latency_secs,
                    native_price: candidate.native_price,
                })
                .saturating_add(candidate.competition_buffer)
                .max(candidate.slippage_floor);
            if recomputed_min_profit != candidate.plan_args.min_profit {
                candidate.plan_args.min_profit = recomputed_min_profit;
            }

            candidate.fee_estimate.total_fee_native = candidate
                .fee_estimate
                .gas_price
                .saturating_mul(candidate.gas_limit)
                .saturating_add(candidate.fee_estimate.l1_data_fee);
            candidate.gas_cost_native = candidate.fee_estimate.total_fee_native;
            let Some(gas_cost_tokens) = candidate
                .native_price
                .tokens_for_native_strict(candidate.gas_cost_native)
            else {
                self.log_candidate_stage(
                    "candidate_rejected_post_sim",
                    &self.chain_name,
                    Some(format!("{}", candidate.candidate_id)),
                    Some(candidate.cycle_start),
                    Some(candidate.hops),
                    edges_scanned,
                    Some(candidate.venue_path.clone()),
                    Some(candidate.est_gross_after_fee),
                    Some(candidate.gas_cost_native),
                    None,
                    false,
                    Some(candidate.plan_args.min_profit),
                    Some("unreliable_native_price_for_start_token"),
                    Some("profit_gate_failed"),
                    candidate.has_bridge_step,
                    !candidate.liquidation_markets.is_empty(),
                );
                return Ok(ScanOutcome::NotProfitable {
                    reason: format!(
                        "cycle start=0x{} failed: unreliable native pricing after simulation",
                        hex::encode(candidate.cycle_start)
                    ),
                    edges: edges_scanned,
                    expected_univ3_edges,
                });
            };
            candidate.gas_cost = gas_cost_tokens;
            candidate.net_profit = candidate
                .est_gross_after_fee
                .saturating_sub(candidate.gas_cost);
            if candidate.net_profit < candidate.plan_args.min_profit {
                self.log_candidate_stage(
                    "candidate_rejected_post_sim",
                    &self.chain_name,
                    Some(format!(
                        "{}:{}:{}",
                        self.chain_name,
                        hex::encode(candidate.cycle_start),
                        candidate.hops
                    )),
                    Some(candidate.cycle_start),
                    Some(candidate.hops),
                    edges_scanned,
                    Some(candidate.venue_path.clone()),
                    Some(candidate.est_gross_after_fee),
                    Some(candidate.gas_cost_native),
                    Some(candidate.gas_cost),
                    start_token_pricing_reliable(
                        candidate.cycle_start,
                        self.wrapped_native,
                        candidate.native_price,
                    ),
                    Some(candidate.plan_args.min_profit),
                    Some("below_min_profit_threshold"),
                    Some("profit_gate_failed"),
                    candidate.has_bridge_step,
                    !candidate.liquidation_markets.is_empty(),
                );
                return Ok(ScanOutcome::NotProfitable {
                    reason: format!(
                        "cycle start=0x{} failed simulation profit check grossWei={} gasWei={} thresholdWei={}",
                        hex::encode(candidate.cycle_start),
                        candidate.est_gross_after_fee,
                        candidate.gas_cost_native,
                        candidate.plan_args.min_profit
                    ),
                    edges: edges_scanned,
                    expected_univ3_edges,
                });
            }

            let min_profit_target = candidate.plan_args.min_profit;
            self.log_candidate_stage(
                "candidate_dispatch_eligible",
                &self.chain_name,
                Some(format!("{}", candidate.candidate_id)),
                Some(candidate.cycle_start),
                Some(candidate.hops),
                edges_scanned,
                Some(candidate.venue_path.clone()),
                Some(candidate.est_gross_after_fee),
                Some(candidate.gas_cost_native),
                Some(candidate.gas_cost),
                start_token_pricing_reliable(
                    candidate.cycle_start,
                    self.wrapped_native,
                    candidate.native_price,
                ),
                Some(min_profit_target),
                None,
                Some("ok"),
                candidate.has_bridge_step,
                !candidate.liquidation_markets.is_empty(),
            );
            let call = self.executor.start_v2(candidate.plan_args.clone());
            let shadow_meta = ShadowPlanMeta {
                start_token: candidate.cycle_start,
                amount_in: candidate.amount_in,
                est_gross_after_fee: candidate.est_gross_after_fee,
                gas_cost: candidate.gas_cost,
                gas_cost_native: candidate.gas_cost_native,
                net_profit: candidate.net_profit,
                hops: candidate.hops,
                min_profit: min_profit_target,
                max_slippage_bps: candidate.max_slippage_bps,
            };
            let broadcast_start = Instant::now();
            let dispatch = match self
                .dispatch_call(
                    call,
                    candidate.gas_limit,
                    &candidate.fee_estimate,
                    Some(shadow_meta),
                )
                .await
            {
                Ok(dispatch) => dispatch,
                Err(err) => {
                    self.log_candidate_stage(
                        "candidate_rejected_post_sim",
                        &self.chain_name,
                        Some(format!("{}", candidate.candidate_id)),
                        Some(candidate.cycle_start),
                        Some(candidate.hops),
                        edges_scanned,
                        Some(candidate.venue_path.clone()),
                        Some(candidate.est_gross_after_fee),
                        Some(candidate.gas_cost_native),
                        Some(candidate.gas_cost),
                        false,
                        Some(min_profit_target),
                        Some("dispatch_blocked"),
                        Some("dispatch_failed"),
                        candidate.has_bridge_step,
                        !candidate.liquidation_markets.is_empty(),
                    );
                    return Err(err.context("dispatch_call failed"));
                }
            };
            self.log_candidate_stage(
                "candidate_dispatched",
                &self.chain_name,
                Some(format!("{}", candidate.candidate_id)),
                Some(candidate.cycle_start),
                Some(candidate.hops),
                edges_scanned,
                Some(candidate.venue_path.clone()),
                Some(candidate.est_gross_after_fee),
                Some(candidate.gas_cost_native),
                Some(candidate.gas_cost),
                start_token_pricing_reliable(
                    candidate.cycle_start,
                    self.wrapped_native,
                    candidate.native_price,
                ),
                Some(min_profit_target),
                None,
                Some("dispatched"),
                candidate.has_bridge_step,
                !candidate.liquidation_markets.is_empty(),
            );
            if let Some(metrics) = &self.metrics {
                let latency_ms = broadcast_start
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                metrics.record_stage_latency(&self.chain_name, "broadcast", latency_ms);
                if let Some(method) = dispatch.private_submission_method {
                    let method_stage = format!("broadcast_{}", method.as_str());
                    metrics.record_stage_latency(&self.chain_name, &method_stage, latency_ms);
                }
                metrics.record_tx_sent(&self.chain_name, candidate.strategy.as_str());
            }
            if let Some(accounting) = &self.accounting {
                if let Err(err) = accounting
                    .record_event(&self.chain_name, candidate.strategy.as_str(), "tx_sent")
                    .await
                {
                    warn!(
                        chain = %self.chain_name,
                        error = %err,
                        "Failed to persist tx_sent accounting event",
                    );
                }
            }
            let DispatchResult {
                tx_hash: pending_tx_hash,
                receipt,
                latency,
                private_relay_rejected,
                private_submission_method: _,
            } = dispatch;
            let tx_hash = match classify_receipt(receipt, pending_tx_hash, edges_scanned) {
                Ok(hash) => {
                    {
                        let mut tracker = self.competition.lock().await;
                        tracker.record_success(
                            latency,
                            private_relay_rejected,
                            min_profit_target,
                            candidate.net_profit,
                        );
                    }
                    if let Some(metrics) = &self.metrics {
                        metrics.record_tx_confirmed(&self.chain_name, candidate.strategy.as_str());
                    }
                    if let Some(accounting) = &self.accounting {
                        if let Err(err) = accounting
                            .record_event(
                                &self.chain_name,
                                candidate.strategy.as_str(),
                                "tx_confirmed",
                            )
                            .await
                        {
                            warn!(
                                chain = %self.chain_name,
                                error = %err,
                                "Failed to persist tx_confirmed accounting event",
                            );
                        }
                    }
                    if let Some(liquidations) = &self.liquidations {
                        for market in &candidate.liquidation_markets {
                            liquidations.record_success(market).await;
                        }
                    }
                    self.hot_paths.mark_cycle(&candidate.cycle_edges).await;
                    hash
                }
                Err(failure) => {
                    {
                        let mut tracker = self.competition.lock().await;
                        tracker.record_failure(private_relay_rejected);
                    }
                    if let Some(metrics) = &self.metrics {
                        metrics.record_tx_reverted(&self.chain_name, candidate.strategy.as_str());
                    }
                    if let Some(accounting) = &self.accounting {
                        if let Err(err) = accounting
                            .record_event(
                                &self.chain_name,
                                candidate.strategy.as_str(),
                                "tx_reverted",
                            )
                            .await
                        {
                            warn!(
                                chain = %self.chain_name,
                                error = %err,
                                "Failed to persist tx_reverted accounting event",
                            );
                        }
                    }
                    if let Some(liquidations) = &self.liquidations {
                        for market in &candidate.liquidation_markets {
                            liquidations.record_revert(market).await;
                        }
                    }
                    return Ok(ScanOutcome::Failed {
                        reason: failure.reason,
                        tx_hash: failure.tx_hash,
                        edges: failure.edges,
                        expected_univ3_edges,
                        private_relay_rejected,
                        estimated_loss_wei: candidate.gas_cost_native,
                    });
                }
            };

            let summary = ExecutionSummary {
                start_token: candidate.cycle_start,
                amount_in: candidate.amount_in,
                hops: candidate.hops,
                gross: candidate.est_gross_after_fee,
                net: candidate.net_profit,
                gross_native: tokens_to_native(
                    candidate.native_price,
                    candidate.est_gross_after_fee,
                ),
                net_native: tokens_to_native(candidate.native_price, candidate.net_profit),
                gas_cost: candidate.gas_cost,
                gas_cost_native: candidate.gas_cost_native,
                gas_limit: candidate.gas_limit,
                gas_price: candidate.fee_estimate.gas_price,
                max_fee_per_gas: candidate.fee_estimate.max_fee_per_gas,
                max_priority_fee_per_gas: candidate.fee_estimate.max_priority_fee_per_gas,
                tx_hash,
                edges_scanned,
                max_slippage_bps: candidate.max_slippage_bps,
                inclusion_latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
                private_relay_rejected,
                competition_pressure: competition_multiplier,
                competition_buffer_wei: candidate.competition_buffer,
                strategy: candidate.strategy.as_str().to_string(),
                venue_path: candidate.venue_path.clone(),
            };

            println!(
                concat!(
                    "EXEC hops={} start=0x{} amountInWei={} grossWei={} netWei={} gasStart={}",
                    " gasWei={} minProfitWei={} maxSlipBps={} latencyMs={} relayReject={}",
                    " compPress={:.2} compBufferWei={} tx={:#x}",
                ),
                summary.hops,
                hex::encode(summary.start_token),
                summary.amount_in,
                summary.gross,
                summary.net,
                summary.gas_cost,
                summary.gas_cost_native,
                min_profit_target,
                summary.max_slippage_bps,
                summary.inclusion_latency_ms,
                summary.private_relay_rejected,
                summary.competition_pressure,
                summary.competition_buffer_wei,
                summary.tx_hash
            );

            return Ok(ScanOutcome::Executed(Box::new(summary)));
        }

        if let Some(reason) = last_skip_reason {
            debug!(
                edges = edges_scanned,
                expected_univ3_edges,
                %reason,
                "Opportunity scanner filtered all candidates"
            );
            Ok(ScanOutcome::NotProfitable {
                reason,
                edges: edges_scanned,
                expected_univ3_edges,
            })
        } else {
            debug!(
                edges = edges_scanned,
                expected_univ3_edges, "Opportunity scanner found no viable cycles"
            );
            Ok(ScanOutcome::NoOpportunity {
                edges: edges_scanned,
                expected_univ3_edges,
            })
        }
    }

    async fn dispatch_call(
        &self,
        mut call: ContractCall<M, U256>,
        gas_limit: U256,
        gas: &FeeEstimate,
        shadow_meta: Option<ShadowPlanMeta>,
    ) -> Result<DispatchResult> {
        apply_gas_parameters(&mut call.tx, gas);
        call.tx.set_gas(gas_limit);
        if let Some(wallet) = &self.wallet {
            call.tx.set_from(wallet.address());
        }

        self.log_fee_sanity(gas, gas_limit);

        let chaos_delay = self.chaos.broadcast_delay();
        if let Some(delay) = chaos_delay {
            sleep(delay).await;
        }

        if self.shadow.enabled {
            return self.shadow_dispatch(&call.tx, chaos_delay, shadow_meta);
        }

        let mut nonce_record = None;
        if let Some(manager) = &self.nonce_manager {
            let nonce = manager.get_next().await?;
            call.tx.set_nonce(nonce);
            nonce_record = Some((Arc::clone(manager), nonce));
        }
        match &self.broadcast.endpoint {
            BroadcastEndpoint::Public => {
                apply_public_mempool_jitter(&mut call.tx, self.broadcast.public_jitter_bps);
                let start = Instant::now();
                if self.chaos.should_reject_public() {
                    if let Some((manager, nonce)) = &nonce_record {
                        manager.mark_failed(*nonce).await;
                    }
                    return Err(anyhow!("chaos: public broadcast dropped"));
                }

                let pending = match call.send().await {
                    Ok(pending) => pending,
                    Err(err) => {
                        if let Some((manager, nonce)) = &nonce_record {
                            manager.mark_failed(*nonce).await;
                        }
                        return Err(err.into());
                    }
                };
                let tx_hash = pending.tx_hash();
                let receipt = match pending.await {
                    Ok(receipt) => {
                        if let Some((manager, nonce)) = &nonce_record {
                            manager.mark_confirmed(*nonce).await;
                        }
                        receipt
                    }
                    Err(err) => {
                        if let Some((manager, nonce)) = &nonce_record {
                            manager.mark_failed(*nonce).await;
                        }
                        return Err(err.into());
                    }
                };
                Ok(DispatchResult {
                    tx_hash,
                    receipt,
                    latency: start.elapsed(),
                    private_relay_rejected: false,
                    private_submission_method: None,
                })
            }
            BroadcastEndpoint::Private { providers } => {
                let Some(wallet) = self.wallet.clone() else {
                    if let Some((manager, nonce)) = &nonce_record {
                        manager.mark_failed(*nonce).await;
                    }
                    return Err(anyhow!(
                        "PRIVATE_RELAY_URL configured but no signing wallet available"
                    ));
                };
                let client = self.executor.client();
                let mut tx = call.tx.clone();
                tx.set_gas(gas_limit);
                if let Err(err) = client.fill_transaction(&mut tx, None).await {
                    if let Some((manager, nonce)) = &nonce_record {
                        manager.mark_failed(*nonce).await;
                    }
                    return Err(err.into());
                }

                let start = Instant::now();
                let private_relay_rejected = false;
                if let Some(priority_fee) = self.broadcast.priority_fee() {
                    let base_fee =
                        client
                            .get_block(BlockNumber::Latest)
                            .await
                            .ok()
                            .and_then(|maybe_block| {
                                maybe_block.and_then(|block| block.base_fee_per_gas)
                            });
                    match &mut tx {
                        TypedTransaction::Eip1559(inner) => {
                            let current_max_fee = inner.max_fee_per_gas.unwrap_or_default();
                            let suggested_max_fee = if current_max_fee >= priority_fee {
                                current_max_fee
                            } else {
                                base_fee.map(|base| base + priority_fee).unwrap_or_else(|| {
                                    priority_fee.saturating_mul(U256::from(2u64))
                                })
                            };
                            inner.max_priority_fee_per_gas = Some(priority_fee);
                            inner.max_fee_per_gas = Some(suggested_max_fee);
                        }
                        TypedTransaction::Eip2930(inner) => {
                            inner.tx.gas_price = Some(priority_fee);
                        }
                        TypedTransaction::Legacy(inner) => {
                            inner.gas_price = Some(priority_fee);
                        }
                    }
                }

                let signature = match wallet.sign_transaction(&tx).await {
                    Ok(sig) => sig,
                    Err(err) => {
                        if let Some((manager, nonce)) = &nonce_record {
                            manager.mark_failed(*nonce).await;
                        }
                        return Err(err.into());
                    }
                };

                let providers = providers.clone();
                let raw = tx.rlp_signed(&signature);

                let (mut relay_candidates, relay_scores) = {
                    let tracker = lock_unpoison(self.broadcast.relay_health.as_ref());
                    let mut candidates = Vec::new();
                    let mut scores = HashMap::new();
                    for (index, relay) in providers.iter().enumerate() {
                        let label = relay.label();
                        let healthy = tracker.is_healthy(label);
                        if !healthy {
                            let snapshot = tracker.snapshot(label);
                            warn!(
                                target: "broadcast",
                                relay_index = index,
                                relay = %label,
                                reject_rate_ema = ?snapshot.reject_rate_ema,
                                success_rate_ema = ?snapshot.success_rate_ema,
                                latency_ms_ema = ?snapshot.latency_ms_ema,
                                "Skipping unhealthy private relay endpoint"
                            );
                            continue;
                        }
                        let score = tracker.health_score(label);
                        scores.insert(label.to_string(), score);
                        candidates.push((index, relay));
                    }
                    (candidates, scores)
                };

                if relay_candidates.is_empty() {
                    if let Some((manager, nonce)) = &nonce_record {
                        manager.mark_failed(*nonce).await;
                    }
                    error!(
                        target: "broadcast",
                        "All private relay endpoints unhealthy; failing closed"
                    );
                    return Err(anyhow!("all private relay endpoints unhealthy"));
                }

                relay_candidates.sort_by(|a, b| {
                    let score_a = relay_scores.get(a.1.label()).copied().unwrap_or(0.0);
                    let score_b = relay_scores.get(b.1.label()).copied().unwrap_or(0.0);
                    score_b.partial_cmp(&score_a).unwrap_or(Ordering::Equal)
                });

                let mut relay_errors: Vec<String> = Vec::new();
                let mut receipt_result = None;
                let target_bundle_block = match client.get_block_number().await {
                    Ok(block) => block.saturating_add(U64::one()),
                    Err(err) => {
                        if let Some((manager, nonce)) = &nonce_record {
                            manager.mark_failed(*nonce).await;
                        }
                        return Err(anyhow!(
                            "failed to fetch latest block number for bundle target: {err}"
                        ));
                    }
                };

                for (index, relay) in relay_candidates {
                    if self.chaos.should_reject_relay() {
                        relay_errors.push("chaos: relay rejected tx".to_string());
                        continue;
                    }
                    let attempt_start = Instant::now();
                    let send_result = relay
                        .provider
                        .send_bundle_transaction(
                            raw.clone(),
                            target_bundle_block,
                            &relay.chain_name,
                            relay.allow_private_raw_fallback,
                        )
                        .await;
                    match send_result {
                        Ok((tx_hash, method)) => {
                            let receipt_poll = async {
                                loop {
                                    match client.get_transaction_receipt(tx_hash).await {
                                        Ok(Some(receipt)) => break Ok(receipt),
                                        Ok(None) => {
                                            sleep(Duration::from_millis(200)).await;
                                            continue;
                                        }
                                        Err(err) => break Err(err),
                                    }
                                }
                            };

                            match tokio::time::timeout(
                                self.broadcast.private_inclusion_timeout,
                                receipt_poll,
                            )
                            .await
                            {
                                Ok(Ok(receipt)) => {
                                    lock_unpoison(self.broadcast.relay_health.as_ref())
                                        .record_success(
                                            relay.label(),
                                            Some(attempt_start.elapsed()),
                                        );
                                    if let Some((manager, nonce)) = &nonce_record {
                                        manager.mark_confirmed(*nonce).await;
                                    }
                                    receipt_result = Some((tx_hash, receipt, index, method));
                                    break;
                                }
                                Ok(Err(err)) => {
                                    warn!(
                                        target: "broadcast",
                                        relay_index = index,
                                        relay = %relay.label(),
                                        error = %err,
                                        "Private relay receipt poll failed",
                                    );
                                    lock_unpoison(self.broadcast.relay_health.as_ref())
                                        .record_failure(
                                            relay.label(),
                                            true,
                                            Some(attempt_start.elapsed()),
                                        );
                                    relay_errors.push(err.to_string());
                                }
                                Err(_) => {
                                    warn!(
                                        target: "broadcast",
                                        relay_index = index,
                                        relay = %relay.label(),
                                        timeout_ms = self
                                            .broadcast
                                            .private_inclusion_timeout
                                            .as_millis(),
                                        "Private relay inclusion timeout",
                                    );
                                    lock_unpoison(self.broadcast.relay_health.as_ref())
                                        .record_failure(
                                            relay.label(),
                                            true,
                                            Some(attempt_start.elapsed()),
                                        );
                                    relay_errors.push("timeout".to_string());
                                }
                            }
                        }
                        Err(err) => {
                            warn!(
                                target: "broadcast",
                                relay_index = index,
                                relay = %relay.label(),
                                error = %err,
                                "Private relay broadcast attempt failed",
                            );
                            lock_unpoison(self.broadcast.relay_health.as_ref()).record_failure(
                                relay.label(),
                                true,
                                Some(attempt_start.elapsed()),
                            );
                            relay_errors.push(err.to_string());
                        }
                    }
                }

                if let Some((tx_hash, receipt, relay_index, method)) = receipt_result {
                    info!(
                        target: "broadcast",
                        relay_index,
                        latency_ms = start.elapsed().as_millis(),
                        method = method.as_str(),
                        "Private relay inclusion confirmed",
                    );
                    return Ok(DispatchResult {
                        tx_hash,
                        receipt: Some(receipt),
                        latency: start.elapsed(),
                        private_relay_rejected,
                        private_submission_method: Some(method),
                    });
                }

                let all_unhealthy = {
                    let tracker = lock_unpoison(self.broadcast.relay_health.as_ref());
                    providers
                        .iter()
                        .all(|relay| !tracker.is_healthy(relay.label()))
                };
                if all_unhealthy {
                    if let Some((manager, nonce)) = &nonce_record {
                        manager.mark_failed(*nonce).await;
                    }
                    error!(
                        target: "broadcast",
                        "All private relay endpoints unhealthy; halting broadcast"
                    );
                    return Err(anyhow!("all private relay endpoints unhealthy"));
                }
                if relay_errors.is_empty() {
                    warn!(
                        target: "broadcast",
                        "No private relay endpoints available; failing closed",
                    );
                } else {
                    warn!(
                        target: "broadcast",
                        errors = ?relay_errors,
                        "All private relay broadcasts failed; failing closed",
                    );
                }
                if let Some((manager, nonce)) = &nonce_record {
                    manager.mark_failed(*nonce).await;
                }
                Err(anyhow!("all private relay broadcasts failed"))
            }
        }
    }

    fn shadow_dispatch(
        &self,
        tx: &TypedTransaction,
        delay: Option<Duration>,
        shadow_meta: Option<ShadowPlanMeta>,
    ) -> Result<DispatchResult> {
        let tx_hash = H256::from(rand::random::<[u8; 32]>());
        let receipt: TransactionReceipt = TransactionReceipt {
            transaction_hash: tx_hash,
            status: Some(U64::one()),
            gas_used: tx.gas().copied(),
            ..Default::default()
        };

        self.write_shadow_record(tx_hash, tx, shadow_meta)?;

        Ok(DispatchResult {
            tx_hash,
            receipt: Some(receipt),
            latency: delay.unwrap_or_default(),
            private_relay_rejected: false,
            private_submission_method: None,
        })
    }

    fn log_fee_sanity(&self, gas: &FeeEstimate, gas_limit: U256) {
        let total_fee_native = gas
            .gas_price
            .saturating_mul(gas_limit)
            .saturating_add(gas.l1_data_fee);
        let usd_price = native_usd_price(&self.chain_env_prefix);
        let total_fee_usd = usd_price.map(|price| {
            let native = u256_to_f64(total_fee_native) / 1e18;
            native * price
        });

        info!(
            target: "fee_sanity",
            gas_limit = %gas_limit,
            base_fee = ?gas.base_fee_per_gas,
            priority_fee = ?gas.priority_fee_per_gas,
            l1_data_fee = %gas.l1_data_fee,
            total_fee_native = %total_fee_native,
            total_fee_usd = ?total_fee_usd,
            "fee sanity check"
        );
    }

    fn write_shadow_record(
        &self,
        tx_hash: H256,
        tx: &TypedTransaction,
        meta: Option<ShadowPlanMeta>,
    ) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let to = tx.to().cloned().map(|dest| match dest {
            NameOrAddress::Address(addr) => format!("{addr:#x}"),
            NameOrAddress::Name(name) => name,
        });
        let gas_limit = tx.gas().map(|v| v.to_string());
        let (max_fee_per_gas, max_priority_fee_per_gas, gas_price) = match tx {
            TypedTransaction::Eip1559(inner) => (
                inner.max_fee_per_gas.map(|v| v.to_string()),
                inner.max_priority_fee_per_gas.map(|v| v.to_string()),
                None,
            ),
            TypedTransaction::Eip2930(inner) => {
                (None, None, inner.tx.gas_price.map(|v| v.to_string()))
            }
            TypedTransaction::Legacy(inner) => (None, None, inner.gas_price.map(|v| v.to_string())),
        };

        let record = ShadowExecutionRecord {
            timestamp_ms: now,
            chain_env: self.chain_env_prefix.clone(),
            tag: self.shadow.tag.clone(),
            tx_hash: format!("{tx_hash:#x}"),
            to,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas_price,
            value: tx.value().map(|v| v.to_string()),
            data_len: tx.data().map(|d| d.len()).unwrap_or(0),
            cycle_start: None,
            amount_in_wei: None,
            est_gross_after_fee_wei: None,
            net_profit_wei: None,
            gas_cost_wei: None,
            min_profit_wei: None,
            max_slippage_bps: None,
            hops: None,
        };

        let mut record = record;
        if let Some(meta) = meta {
            record.cycle_start = Some(format!("{:#x}", meta.start_token));
            record.amount_in_wei = Some(meta.amount_in.to_string());
            record.est_gross_after_fee_wei = Some(meta.est_gross_after_fee.to_string());
            record.net_profit_wei = Some(meta.net_profit.to_string());
            record.gas_cost_wei = Some(meta.gas_cost_native.to_string());
            record.min_profit_wei = Some(meta.min_profit.to_string());
            record.max_slippage_bps = Some(meta.max_slippage_bps);
            record.hops = Some(meta.hops);
        }

        if let Some(path) = &self.shadow.log_path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create shadow log directory {}", parent.display()))?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open shadow log {}", path.display()))?;
            serde_json::to_writer(&mut file, &record)?;
            file.write_all(b"\n")?;
        }

        Ok(())
    }

    async fn estimate_gas_with_fallback(
        &self,
        call: &ContractCall<M, U256>,
        hops: usize,
    ) -> Result<U256> {
        match call.estimate_gas().await {
            Ok(limit) => return Ok(limit),
            Err(err) => {
                warn!(error = %err, "Gas estimation failed, applying fallbacks");
            }
        }

        let base_gas = U256::from(200_000u64);
        let per_hop_gas = U256::from(50_000u64);
        let hop_count = U256::from(hops as u64);
        let heuristic = base_gas.saturating_add(per_hop_gas.saturating_mul(hop_count));

        match self.estimate_gas_pending(call).await {
            Ok(sim_gas) => {
                let buffered = sim_gas.saturating_mul(U256::from(12u64));
                Ok(buffered.checked_div(U256::from(10u64)).unwrap_or(sim_gas))
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "Pending-block gas estimate unavailable; using heuristic fallback"
                );
                let buffered = heuristic.saturating_mul(U256::from(15u64));
                Ok(buffered.checked_div(U256::from(10u64)).unwrap_or(heuristic))
            }
        }
    }

    async fn estimate_gas_pending(&self, call: &ContractCall<M, U256>) -> Result<U256> {
        let client = self.executor.client();
        let mut tx = call.tx.clone();
        if let Some(wallet) = &self.wallet {
            tx.set_from(wallet.address());
        }
        client
            .estimate_gas(&tx, Some(BlockId::Number(BlockNumber::Pending)))
            .await
            .map_err(Into::into)
    }

    async fn simulate_plan_execution(
        &self,
        plan: &ExecutorPlan,
        gas: &FeeEstimate,
    ) -> Result<(U256, U256)> {
        let mut call = self.executor.start_v2(plan.clone());
        if let Some(wallet) = &self.wallet {
            call.tx.set_from(wallet.address());
        }
        apply_gas_parameters(&mut call.tx, gas);
        let tx = call.tx.clone();
        let client = self.executor.client();

        let raw = client
            .call(&tx, Some(BlockId::Number(BlockNumber::Pending)))
            .await
            .context("pre-broadcast simulation reverted")?;
        if raw.len() < 32 {
            return Err(anyhow!(
                "simulation did not return profit; require 32-byte return"
            ));
        }
        let profit = U256::from_big_endian(&raw[..32]);
        if profit < plan.min_profit {
            return Err(anyhow!("simulation profit below min_profit"));
        }

        let gas_used = client
            .estimate_gas(&tx, Some(BlockId::Number(BlockNumber::Pending)))
            .await
            .context("simulation gas estimate failed")?;
        Ok((gas_used, profit))
    }

    async fn run(
        self,
        mut cmd_rx: mpsc::Receiver<Command>,
        status_tx: watch::Sender<StatusSnapshot>,
    ) -> Result<()> {
        let mut running = false;
        let mut shutdown = false;
        let mut last_execution: Option<ExecutionSummary> = None;

        status_tx
            .send(StatusSnapshot::new(
                RunnerState::Idle,
                "Awaiting start command",
                last_execution.clone(),
            ))
            .ok();

        while !shutdown {
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        self.handle_command(
                            cmd,
                            &mut running,
                            &mut shutdown,
                            &status_tx,
                            &last_execution,
                        )
                        .await;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        shutdown = true;
                        break;
                    }
                }
            }

            if shutdown {
                break;
            }

            if running {
                let breaker_status = self.circuit_breaker.current_status().await;
                if breaker_status.is_tripped {
                    running = false;
                    status_tx
                        .send(StatusSnapshot::new(
                            RunnerState::Error,
                            format!("Circuit breaker active: {}", breaker_status.active_reason()),
                            last_execution.clone(),
                        ))
                        .ok();
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }

                status_tx
                    .send(StatusSnapshot::new(
                        RunnerState::Running,
                        "Scanning for opportunities",
                        last_execution.clone(),
                    ))
                    .ok();

                match self.scan_once().await {
                    Ok(ScanOutcome::Executed(summary)) => {
                        let summary = summary.as_ref();
                        let relay_label = self.broadcast.endpoint.label();
                        emit_trade_json(&self.chain_name, relay_label, summary);
                        if let Some(metrics) = &self.metrics {
                            metrics.record_detection();
                            metrics.record_execution(
                                summary.gross,
                                summary.net,
                                summary.gas_cost,
                                summary.inclusion_latency_ms,
                            );
                            let net_usd = native_usd_price(&self.chain_env_prefix).map(|price| {
                                let net_native = u256_to_f64(summary.net_native);
                                (net_native / 1e18f64) * price
                            });
                            metrics.record_profit(
                                &self.chain_name,
                                summary.strategy.as_str(),
                                summary.gross_native,
                                summary.gas_cost_native,
                                summary.net_native,
                                net_usd,
                            );
                        }
                        if let Some(update) = self.capital.apply_profit(summary.net) {
                            if let Some(metrics) = &self.metrics {
                                metrics.record_capital(&update.snapshot);
                            }
                            if update.grew {
                                info!(
                                    base = %update.snapshot.base_amount,
                                    min_flash = %update.snapshot.min_flash_loan,
                                    max_flash = %update.snapshot.max_flash_loan,
                                    "Compounded capital expanded after profit",
                                );
                            }
                            if let Some((target, amount)) = update.siphon_ready {
                                info!(
                                    target = %format!("0x{}", hex::encode(target)),
                                    amount = %amount,
                                    "Siphon threshold reached; earmark transfer to secure wallet",
                                );
                            }
                        }
                        if let Some(accounting) = &self.accounting {
                            if let Err(err) = accounting
                                .record_execution(&self.chain_name, relay_label, summary)
                                .await
                            {
                                warn!(
                                    chain = %self.chain_name,
                                    relay = relay_label,
                                    error = %err,
                                    "Failed to persist execution to accounting",
                                );
                            }
                        }
                        self.circuit_breaker.record_success().await;
                        last_execution = Some(summary.clone());
                        status_tx
                            .send(StatusSnapshot::new(
                                RunnerState::Running,
                                format!(
                                    concat!(
                                        "Executed {}-hop cycle netWei={} (chain={}, relay={},",
                                        " edges scanned: {}, latencyMs={}, relayReject={},",
                                        " compPress={:.2})",
                                    ),
                                    summary.hops,
                                    summary.net,
                                    self.chain_name,
                                    relay_label,
                                    summary.edges_scanned,
                                    summary.inclusion_latency_ms,
                                    summary.private_relay_rejected,
                                    summary.competition_pressure
                                ),
                                last_execution.clone(),
                            ))
                            .ok();
                    }
                    Ok(ScanOutcome::NotProfitable {
                        reason,
                        edges,
                        expected_univ3_edges,
                    }) => {
                        if let Some(metrics) = &self.metrics {
                            metrics.record_detection();
                        }
                        status_tx
                            .send(StatusSnapshot::new(
                                RunnerState::Running,
                                format!(
                                    "No executable cycle: {reason} (edges scanned: {edges}, expected uniV3 max: {expected_univ3_edges})"
                                ),
                                last_execution.clone(),
                            ))
                            .ok();
                        sleep(Duration::from_millis(500)).await;
                    }
                    Ok(ScanOutcome::NoOpportunity {
                        edges,
                        expected_univ3_edges,
                    }) => {
                        status_tx
                            .send(StatusSnapshot::new(
                                RunnerState::Running,
                                format!(
                                    "No cycles found (edges scanned: {edges}, expected uniV3 max: {expected_univ3_edges})"
                                ),
                                last_execution.clone(),
                            ))
                            .ok();
                        sleep(Duration::from_millis(500)).await;
                    }
                    Ok(ScanOutcome::Failed {
                        reason,
                        tx_hash,
                        edges,
                        expected_univ3_edges,
                        private_relay_rejected,
                        estimated_loss_wei,
                    }) => {
                        if let Some(metrics) = &self.metrics {
                            metrics.record_detection();
                            metrics.record_failure(estimated_loss_wei);
                        }
                        println!(
                            "EXEC FAILED tx={:#x} reason={} relayReject={} edges_scanned={} expected_uniV3_max={} estLossWei={}",
                            tx_hash,
                            reason,
                            private_relay_rejected,
                            edges,
                            expected_univ3_edges,
                            estimated_loss_wei
                        );
                        let breaker_status = self
                            .circuit_breaker
                            .record_failure(estimated_loss_wei)
                            .await;
                        let mut detail = format!(
                            "Execution failed for {tx_hash:#x}: {reason} (relay rejected: {private_relay_rejected}, edges scanned: {edges}, expected uniV3 max: {expected_univ3_edges}, est loss wei={estimated_loss_wei})",
                            tx_hash = tx_hash,
                            reason = reason,
                            private_relay_rejected = private_relay_rejected,
                            edges = edges,
                            expected_univ3_edges = expected_univ3_edges,
                            estimated_loss_wei = estimated_loss_wei
                        );
                        if breaker_status.is_tripped {
                            running = false;
                            detail = format!(
                                "{detail}. Circuit breaker active: {}",
                                breaker_status.active_reason()
                            );
                        }
                        status_tx
                            .send(StatusSnapshot::new(
                                if running {
                                    RunnerState::Running
                                } else {
                                    RunnerState::Error
                                },
                                detail,
                                last_execution.clone(),
                            ))
                            .ok();
                        sleep(Duration::from_millis(500)).await;
                    }
                    Err(err) => {
                        if is_rpc_error(&err) {
                            if let Some(metrics) = &self.metrics {
                                metrics.record_rpc_error(&self.chain_name);
                            }
                            lock_unpoison(self.rpc_health.as_ref()).record_failure(
                                &self.rpc_endpoint,
                                true,
                                None,
                            );
                            if let Some(accounting) = &self.accounting {
                                if let Err(err) = accounting
                                    .record_event(&self.chain_name, "arb", "rpc_error")
                                    .await
                                {
                                    warn!(
                                        chain = %self.chain_name,
                                        error = %err,
                                        "Failed to persist rpc_error accounting event",
                                    );
                                }
                            }
                        }
                        status_tx
                            .send(StatusSnapshot::new(
                                RunnerState::Error,
                                format!("Error during scan: {:#}", err),
                                last_execution.clone(),
                            ))
                            .ok();
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            } else {
                tokio::select! {
                    maybe_cmd = cmd_rx.recv() => {
                        if let Some(cmd) = maybe_cmd {
                            self
                                .handle_command(
                                    cmd,
                                    &mut running,
                                    &mut shutdown,
                                    &status_tx,
                                    &last_execution,
                                )
                                .await;
                        } else {
                            shutdown = true;
                        }
                    }
                    _ = sleep(Duration::from_millis(200)) => {}
                }
            }
        }

        status_tx
            .send(StatusSnapshot::new(
                RunnerState::Stopped,
                "Runner stopped",
                last_execution,
            ))
            .ok();

        Ok(())
    }
}

struct ChainRuntimeHandle {
    name: String,
    command_tx: mpsc::Sender<Command>,
    status_rx: watch::Receiver<StatusSnapshot>,
    join_handle: tokio::task::JoinHandle<()>,
}

async fn broadcast_command(
    senders: &[mpsc::Sender<Command>],
    cmd: Command,
) -> Result<(), mpsc::error::SendError<Command>> {
    for tx in senders {
        tx.send(cmd.clone()).await?;
    }
    Ok(())
}

async fn command_listener(txs: Vec<mpsc::Sender<Command>>) {
    println!("Commands: start | stop | reset | quit");
    let stdin = io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let cmd = line.trim().to_lowercase();
                let send_result = match cmd.as_str() {
                    "start" => broadcast_command(&txs, Command::Start).await,
                    "stop" => broadcast_command(&txs, Command::Stop).await,
                    "reset" => broadcast_command(&txs, Command::ResetCircuit).await,
                    "quit" | "exit" => broadcast_command(&txs, Command::Quit).await,
                    "" => Ok(()),
                    other => {
                        println!("Unknown command `{other}`. Use start | stop | reset | quit");
                        Ok(())
                    }
                };

                if send_result.is_err() {
                    break;
                }

                if cmd == "quit" || cmd == "exit" {
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                eprintln!("Failed to read command: {err}");
                break;
            }
        }
    }
}

async fn monitor_status(chain: String, mut rx: watch::Receiver<StatusSnapshot>) {
    display_status(&chain, rx.borrow().clone());
    while rx.changed().await.is_ok() {
        let snapshot = rx.borrow().clone();
        display_status(&chain, snapshot);
    }
    println!("Status monitor terminated for {chain}");
}

fn display_status(chain: &str, snapshot: StatusSnapshot) {
    println!("\n================ STATUS ({chain}) ================");
    println!("State: {:?}", snapshot.state);
    println!("Detail: {}", snapshot.detail);
    match snapshot.last_execution {
        Some(ref exec) => {
            println!(
                concat!(
                    "Last execution -> start=0x{} hops={} grossWei={} netWei={}",
                    " gasWei={} tx={:#x} edgesScanned={} latencyMs={} relayReject={}",
                    " compPress={:.2} compBufferWei={}",
                ),
                hex::encode(exec.start_token),
                exec.hops,
                exec.gross,
                exec.net,
                exec.gas_cost,
                exec.tx_hash,
                exec.edges_scanned,
                exec.inclusion_latency_ms,
                exec.private_relay_rejected,
                exec.competition_pressure,
                exec.competition_buffer_wei
            );
        }
        None => println!("Last execution -> none"),
    }
    println!("========================================\n");
}

#[cfg(test)]
mod runner_tests {
    use super::*;
    use async_trait::async_trait;
    use ethers::providers::{Middleware, MockProvider, Provider, ProviderError};
    use ethers::types::{transaction::eip2718::TypedTransaction, Block, Bytes, H256, U64};
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::time::{sleep, Duration};

    #[derive(Clone, Debug)]
    struct TestMiddleware {
        inner: Provider<MockProvider>,
        send_called: Arc<AtomicBool>,
    }

    impl TestMiddleware {
        fn new(inner: Provider<MockProvider>) -> (Self, Arc<AtomicBool>) {
            let send_called = Arc::new(AtomicBool::new(false));
            (
                Self {
                    inner,
                    send_called: send_called.clone(),
                },
                send_called,
            )
        }
    }

    #[async_trait]
    impl Middleware for TestMiddleware {
        type Error = ProviderError;
        type Provider = MockProvider;
        type Inner = Provider<MockProvider>;

        fn inner(&self) -> &Self::Inner {
            &self.inner
        }

        async fn estimate_gas(
            &self,
            _tx: &TypedTransaction,
            _block: Option<BlockId>,
        ) -> Result<U256, Self::Error> {
            Err(ProviderError::CustomError(
                "forced estimation failure".to_string(),
            ))
        }

        async fn send_transaction<T: Into<TypedTransaction> + Send + Sync>(
            &self,
            _tx: T,
            _block: Option<BlockId>,
        ) -> Result<PendingTransaction<'_, Self::Provider>, Self::Error> {
            self.send_called.store(true, Ordering::SeqCst);
            Err(ProviderError::CustomError(
                "send should not be called".to_string(),
            ))
        }

        async fn get_gas_price(&self) -> Result<U256, Self::Error> {
            Ok(U256::from(1u64))
        }
    }

    #[tokio::test]
    async fn falls_back_to_heuristic_when_estimate_gas_fails() {
        let base_amount = U256::from(100u64);

        let (provider, mock) = Provider::mocked();
        let provider_arc = Arc::new(provider.clone());
        let (middleware, send_called) = TestMiddleware::new(provider);
        let executor = MultiVenueArbExecutor::new(Address::zero(), Arc::new(middleware));

        let block: Block<H256> = Block {
            base_fee_per_gas: Some(U256::from(1u64)),
            number: Some(U64::one()),
            ..Default::default()
        };
        mock.push(Some(block)).unwrap();

        let tokens = TokenList::new(Vec::new());
        let pool_cache = Arc::new(
            PoolDepthCache::new("test".into(), tokens.clone(), Duration::from_secs(60))
                .expect("pool depth cache"),
        );
        let capital = Arc::new(
            CapitalManager::new(
                base_amount,
                base_amount,
                base_amount,
                10_000,
                0,
                base_amount,
                base_amount,
                base_amount,
                None,
            )
            .expect("capital manager"),
        );
        let relay_health = Arc::new(StdMutex::new(HealthTracker::new(
            0.5,
            HealthThresholds::default(),
        )));
        let rpc_health = Arc::new(StdMutex::new(HealthTracker::new(
            0.5,
            HealthThresholds::default(),
        )));

        let runner = Runner::new(
            RunnerConfig {
                feature_gate: FeatureGate {
                    cycle_arb: true,
                    backrun: false,
                    sandwich: false,
                    liquidations: false,
                    bridge: false,
                },
                chain_name: "test".into(),
                provider: provider_arc.clone(),
                rpc_endpoint: "http://test".into(),
                rpc_health,
                univ3_quoter: Address::zero(),
                univ3_factory: Address::zero(),
                univ3_validation: None,
                univ3_fee_tiers: None,
                bal_vault: Address::zero(),
                aave_pool: None,
                erc3156_lender: None,
                erc3156_fee_bps: 9,
                bal_flashloan_tokens: None,
                aave_flashloan_tokens: None,
                erc3156_flashloan_tokens: None,
                univ2_flashloan_tokens: None,
                univ3_flashloan_tokens: None,
                chain_env_prefix: "TEST".into(),
                tokens,
                wrapped_native: Address::zero(),
                capital,
                pool_depth_cache: pool_cache,
                pool_monitor: None,
                hot_univ2_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_univ3_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                edge_slippage_bps: 0,
                executor_max_slippage_bps: 0,
                edge_prune_max_slippage_bps: 0,
                edge_prune_min_score: 0.0,
                edge_prune_liquidity_weight: 1.0,
                edge_prune_profit_weight: 1.0,
                edge_prune_slippage_weight: 0.0,
                cycle_limits: BellmanFordLimits {
                    min_hops: 2,
                    max_hops: 3,
                    max_relaxations: 3,
                    max_cycles: 8,
                    timeout: Duration::from_millis(250),
                },
                search_budget: Duration::from_millis(250),
                quote_budget: Duration::from_millis(250),
                simulation_budget: Duration::from_millis(250),
                max_edges_hot: 0,
                topk_per_token: 0,
                dynamic_top_tokens_30d: 60,
                mandatory_universe_tokens: HashSet::new(),
                hub_tokens: HashSet::new(),
                max_gas_price_wei: U256::from(u64::MAX),
                max_gas_price_congestion_bps: 0,
                profit_margin_bps: 0,
                opportunity_cost_wei: U256::zero(),
                cross_chain_profit_bps: 0,
                cross_chain_min_profit_wei: U256::zero(),
                max_candidate_paths: 4,
                min_flash_loan_wei: base_amount,
                min_edge_max_input: base_amount,
                min_liquidity_tokens: 0.0,
                max_quote_block_lag: U64::from(2u64),
                congestion_alpha: 0.3,
                competition_alpha: 0.5,
                jit_config: None,
                broadcast: BroadcastConfig {
                    endpoint: BroadcastEndpoint::Public,
                    role: MevRole::Searcher,
                    filler_priority_fee: None,
                    searcher_priority_fee: None,
                    public_jitter_bps: 0,
                    private_inclusion_timeout: Duration::from_millis(4500),
                    relay_health,
                },
                shadow: ShadowConfig::disabled(),
                chaos: ChaosConfig {
                    relay_reject_bps: 0,
                    public_reject_bps: 0,
                    broadcast_delay_ms: 0,
                },
                wallet: None,
                backrun_monitor: None,
                sandwich_monitor: None,
                bridge: None,
                low_liquidity_scanner: None,
                liquidation_monitor: None,
                circuit_breaker: Arc::new(CircuitBreaker::new(U256::zero(), U256::zero(), 3)),
                metrics: None,
                accounting: None,
                fee_estimator: FeeEstimator::new(
                    "test".into(),
                    crate::ops_inputs::GasModel::Eip1559,
                    provider_arc.as_ref().clone(),
                    ArbitrumFeeConfig::default(),
                    None,
                ),
            },
            executor,
        );

        let plan_args = ExecutorPlan {
            loans: vec![ExecutorLoan {
                token: Address::zero(),
                amount: base_amount,
                provider: FlashLoanProvider::Balancer.as_id(),
                provider_addr: runner.bal_vault,
            }],
            cycle_slippage_bps: 0,
            steps: Vec::new(),
            min_profit: U256::zero(),
        };
        let call = runner.executor.start_v2(plan_args);
        let gas_limit = runner
            .estimate_gas_with_fallback(&call, 2)
            .await
            .expect("fallback gas limit");

        assert_eq!(gas_limit, U256::from(450_000u64));
        assert!(
            !send_called.load(Ordering::SeqCst),
            "gas estimation should not broadcast transactions"
        );
    }

    #[tokio::test]
    async fn ensure_contract_deployed_accepts_code() {
        let (provider, mock) = Provider::mocked();
        mock.push::<Bytes, _>(Bytes::from(vec![1u8, 2u8])).unwrap();

        let result = ensure_contract_deployed(
            &provider,
            ContractDeploymentCheck {
                chain: "test",
                env_prefix: "TEST",
                label: "executor",
                suffix: "EXECUTOR_ADDRESS",
                address: Address::random(),
                expected_chain_id: 1,
                rpc_endpoint: "http://test-rpc",
            },
        )
        .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ensure_contract_deployed_rejects_empty_code() {
        let (provider, mock) = Provider::mocked();
        mock.push::<Bytes, _>(Bytes::default()).unwrap();

        let err = ensure_contract_deployed(
            &provider,
            ContractDeploymentCheck {
                chain: "test",
                env_prefix: "TEST",
                label: "executor",
                suffix: "EXECUTOR_ADDRESS",
                address: Address::random(),
                expected_chain_id: 1,
                rpc_endpoint: "http://test-rpc",
            },
        )
        .await
        .expect_err("expected missing contract code error");

        let message = err.to_string();
        assert!(message.contains("EXECUTOR_ADDRESS"));
        assert!(message.contains("no contract code"));
        assert!(message.contains("http://test-rpc"));
    }

    fn profit_bytes(amount: U256) -> String {
        format!("0x{:064x}", amount)
    }

    #[tokio::test]
    async fn simulate_plan_execution_returns_profit_bytes() {
        let base_amount = U256::from(100u64);
        let expected_profit = U256::from(555u64);

        let (provider, mock) = Provider::mocked();
        let provider_arc = Arc::new(provider.clone());
        let executor = MultiVenueArbExecutor::new(Address::zero(), provider_arc.clone());

        mock.push::<U256, _>(U256::from(210_000u64)).unwrap();
        mock.push::<String, _>(profit_bytes(expected_profit))
            .unwrap();

        let tokens = TokenList::new(Vec::new());
        let pool_cache = Arc::new(
            PoolDepthCache::new("test".into(), tokens.clone(), Duration::from_secs(60))
                .expect("pool depth cache"),
        );
        let capital = Arc::new(
            CapitalManager::new(
                base_amount,
                base_amount,
                base_amount,
                10_000,
                0,
                base_amount,
                base_amount,
                base_amount,
                None,
            )
            .expect("capital manager"),
        );
        let relay_health = Arc::new(StdMutex::new(HealthTracker::new(
            0.5,
            HealthThresholds::default(),
        )));

        let runner = Runner::new(
            RunnerConfig {
                feature_gate: FeatureGate {
                    cycle_arb: true,
                    backrun: false,
                    sandwich: false,
                    liquidations: false,
                    bridge: false,
                },
                chain_name: "test".into(),
                provider: provider_arc.clone(),
                rpc_endpoint: "http://test".into(),
                rpc_health: Arc::new(StdMutex::new(HealthTracker::new(
                    0.5,
                    HealthThresholds::default(),
                ))),
                univ3_quoter: Address::zero(),
                univ3_factory: Address::zero(),
                univ3_validation: None,
                univ3_fee_tiers: None,
                bal_vault: Address::zero(),
                aave_pool: None,
                erc3156_lender: None,
                erc3156_fee_bps: 9,
                bal_flashloan_tokens: None,
                aave_flashloan_tokens: None,
                erc3156_flashloan_tokens: None,
                univ2_flashloan_tokens: None,
                univ3_flashloan_tokens: None,
                chain_env_prefix: "TEST".into(),
                tokens: tokens.clone(),
                wrapped_native: Address::zero(),
                capital,
                pool_depth_cache: pool_cache,
                pool_monitor: None,
                hot_univ2_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_univ3_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                edge_slippage_bps: 0,
                executor_max_slippage_bps: 0,
                edge_prune_max_slippage_bps: 0,
                edge_prune_min_score: 0.0,
                edge_prune_liquidity_weight: 1.0,
                edge_prune_profit_weight: 1.0,
                edge_prune_slippage_weight: 0.0,
                cycle_limits: BellmanFordLimits {
                    min_hops: 2,
                    max_hops: 3,
                    max_relaxations: 3,
                    max_cycles: 8,
                    timeout: Duration::from_millis(250),
                },
                search_budget: Duration::from_millis(250),
                quote_budget: Duration::from_millis(250),
                simulation_budget: Duration::from_millis(250),
                max_edges_hot: 0,
                topk_per_token: 0,
                dynamic_top_tokens_30d: 60,
                mandatory_universe_tokens: HashSet::new(),
                hub_tokens: HashSet::new(),
                max_gas_price_wei: U256::from(u64::MAX),
                max_gas_price_congestion_bps: 0,
                profit_margin_bps: 0,
                opportunity_cost_wei: U256::zero(),
                cross_chain_profit_bps: 0,
                cross_chain_min_profit_wei: U256::zero(),
                max_candidate_paths: 4,
                min_flash_loan_wei: base_amount,
                min_edge_max_input: base_amount,
                min_liquidity_tokens: 0.0,
                max_quote_block_lag: U64::from(2u64),
                congestion_alpha: 0.3,
                competition_alpha: 0.5,
                jit_config: None,
                broadcast: BroadcastConfig {
                    endpoint: BroadcastEndpoint::Public,
                    role: MevRole::Searcher,
                    filler_priority_fee: None,
                    searcher_priority_fee: None,
                    public_jitter_bps: 0,
                    private_inclusion_timeout: Duration::from_millis(4500),
                    relay_health: relay_health.clone(),
                },
                shadow: ShadowConfig::disabled(),
                chaos: ChaosConfig {
                    relay_reject_bps: 0,
                    public_reject_bps: 0,
                    broadcast_delay_ms: 0,
                },
                wallet: None,
                backrun_monitor: None,
                sandwich_monitor: None,
                bridge: None,
                low_liquidity_scanner: None,
                liquidation_monitor: None,
                circuit_breaker: Arc::new(CircuitBreaker::new(U256::zero(), U256::zero(), 3)),
                metrics: None,
                accounting: None,
                fee_estimator: FeeEstimator::new(
                    "test".into(),
                    crate::ops_inputs::GasModel::Eip1559,
                    provider_arc.as_ref().clone(),
                    ArbitrumFeeConfig::default(),
                    None,
                ),
            },
            executor,
        );

        let plan_args = ExecutorPlan {
            loans: vec![ExecutorLoan {
                token: Address::zero(),
                amount: base_amount,
                provider: FlashLoanProvider::Balancer.as_id(),
                provider_addr: runner.bal_vault,
            }],
            cycle_slippage_bps: 0,
            steps: Vec::new(),
            min_profit: U256::zero(),
        };
        let fee = FeeEstimate {
            gas_limit: U256::zero(),
            gas_price: U256::zero(),
            base_fee_per_gas: None,
            priority_fee_per_gas: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            l1_data_fee: U256::zero(),
            total_fee_native: U256::zero(),
        };

        let (gas_used, profit) = runner
            .simulate_plan_execution(&plan_args, &fee)
            .await
            .expect("simulate plan execution");

        assert_eq!(profit, expected_profit);
        assert_eq!(gas_used, U256::from(210_000u64));
    }

    #[test]
    fn build_executor_call_skips_multi_loan_plans() {
        let (provider, _) = Provider::mocked();
        let provider_arc = Arc::new(provider);
        let executor = MultiVenueArbExecutor::new(Address::zero(), provider_arc.clone());
        let tokens = TokenList::new(Vec::new());
        let pool_cache = Arc::new(
            PoolDepthCache::new("test".into(), tokens.clone(), Duration::from_secs(60))
                .expect("pool depth cache"),
        );
        let capital = Arc::new(
            CapitalManager::new(
                U256::from(1u64),
                U256::from(1u64),
                U256::from(1u64),
                10_000,
                0,
                U256::from(1u64),
                U256::from(1u64),
                U256::from(1u64),
                None,
            )
            .expect("capital manager"),
        );
        let runner = Runner::new(
            RunnerConfig {
                feature_gate: FeatureGate {
                    cycle_arb: true,
                    backrun: false,
                    sandwich: false,
                    liquidations: false,
                    bridge: false,
                },
                chain_name: "test".into(),
                provider: provider_arc.clone(),
                rpc_endpoint: "http://test".into(),
                rpc_health: Arc::new(StdMutex::new(HealthTracker::new(
                    0.5,
                    HealthThresholds::default(),
                ))),
                univ3_quoter: Address::zero(),
                univ3_factory: Address::zero(),
                univ3_validation: None,
                univ3_fee_tiers: None,
                bal_vault: Address::zero(),
                aave_pool: None,
                erc3156_lender: None,
                erc3156_fee_bps: 9,
                bal_flashloan_tokens: None,
                aave_flashloan_tokens: None,
                erc3156_flashloan_tokens: None,
                univ2_flashloan_tokens: None,
                univ3_flashloan_tokens: None,
                chain_env_prefix: "TEST".into(),
                tokens,
                wrapped_native: Address::zero(),
                capital,
                pool_depth_cache: pool_cache,
                pool_monitor: None,
                hot_univ2_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_univ3_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                edge_slippage_bps: 0,
                executor_max_slippage_bps: 0,
                edge_prune_max_slippage_bps: 0,
                edge_prune_min_score: 0.0,
                edge_prune_liquidity_weight: 1.0,
                edge_prune_profit_weight: 1.0,
                edge_prune_slippage_weight: 0.0,
                cycle_limits: BellmanFordLimits {
                    min_hops: 2,
                    max_hops: 3,
                    max_relaxations: 3,
                    max_cycles: 8,
                    timeout: Duration::from_millis(250),
                },
                search_budget: Duration::from_millis(250),
                quote_budget: Duration::from_millis(250),
                simulation_budget: Duration::from_millis(250),
                max_edges_hot: 0,
                topk_per_token: 0,
                dynamic_top_tokens_30d: 60,
                mandatory_universe_tokens: HashSet::new(),
                hub_tokens: HashSet::new(),
                max_gas_price_wei: U256::from(u64::MAX),
                max_gas_price_congestion_bps: 0,
                profit_margin_bps: 0,
                opportunity_cost_wei: U256::zero(),
                cross_chain_profit_bps: 0,
                cross_chain_min_profit_wei: U256::zero(),
                max_candidate_paths: 4,
                min_flash_loan_wei: U256::from(1u64),
                min_edge_max_input: U256::from(1u64),
                min_liquidity_tokens: 0.0,
                max_quote_block_lag: U64::from(2u64),
                congestion_alpha: 0.3,
                competition_alpha: 0.5,
                jit_config: None,
                broadcast: BroadcastConfig {
                    endpoint: BroadcastEndpoint::Public,
                    role: MevRole::Searcher,
                    filler_priority_fee: None,
                    searcher_priority_fee: None,
                    public_jitter_bps: 0,
                    private_inclusion_timeout: Duration::from_millis(4500),
                    relay_health: Arc::new(StdMutex::new(HealthTracker::new(
                        0.5,
                        HealthThresholds::default(),
                    ))),
                },
                shadow: ShadowConfig::disabled(),
                chaos: ChaosConfig {
                    relay_reject_bps: 0,
                    public_reject_bps: 0,
                    broadcast_delay_ms: 0,
                },
                wallet: None,
                backrun_monitor: None,
                sandwich_monitor: None,
                bridge: None,
                low_liquidity_scanner: None,
                liquidation_monitor: None,
                circuit_breaker: Arc::new(CircuitBreaker::new(U256::zero(), U256::zero(), 3)),
                metrics: None,
                accounting: None,
                fee_estimator: FeeEstimator::new(
                    "test".into(),
                    crate::ops_inputs::GasModel::Eip1559,
                    provider_arc.as_ref().clone(),
                    ArbitrumFeeConfig::default(),
                    None,
                ),
            },
            executor,
        );

        let plan_args = ExecutorPlan {
            loans: vec![
                ExecutorLoan {
                    token: Address::zero(),
                    amount: U256::from(1u64),
                    provider: FlashLoanProvider::Balancer.as_id(),
                    provider_addr: Address::zero(),
                },
                ExecutorLoan {
                    token: Address::zero(),
                    amount: U256::from(2u64),
                    provider: FlashLoanProvider::Balancer.as_id(),
                    provider_addr: Address::zero(),
                },
            ],
            cycle_slippage_bps: 0,
            steps: Vec::new(),
            min_profit: U256::zero(),
        };

        assert!(
            runner.build_executor_call(&plan_args).is_none(),
            "multi-loan plan should not produce executor call"
        );
    }

    #[tokio::test]
    async fn backrun_monitor_exposes_recent_hints() {
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let tokens = TokenList::new(vec![a, b]);
        let monitor = BackrunMonitor::new(U256::from(1_000u64), 10, tokens);

        monitor.record(&[a, b], U256::from(500u64), "small").await;
        monitor.record(&[a, b], U256::from(5_000u64), "big").await;

        let hints = monitor.active_hints(Duration::from_secs(1)).await;
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].from, a);
        assert_eq!(hints[0].to, b);

        sleep(Duration::from_millis(1100)).await;
        let expired = monitor.active_hints(Duration::from_millis(500)).await;
        assert!(expired.is_empty());
    }

    #[test]
    fn start_priorities_are_boosted_by_backrun_hints() {
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let mut graph = Graph::default();
        let edge = Edge {
            from: a,
            to: b,
            rate_num: U256::one(),
            rate_den: U256::one(),
            venue: VenueEdge::UniV2 {
                pair: Address::zero(),
                token_out: b,
                token0: a,
                token1: b,
                reserve_in: U256::from(1_000u64),
                reserve_out: U256::from(1_000u64),
                fee_bps: 30,
            },
            estimated_gas: 100_000,
            weight: 0,
            max_input: U256::from(10_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        };
        graph.add_edge(edge.clone());
        graph.add_edge(Edge { to: a, ..edge });

        let base_profiles: HashMap<Address, TradeSizing> = HashMap::from([
            (a, TradeSizing::new(U256::from(1_000u64), 50)),
            (b, TradeSizing::new(U256::from(1_000u64), 50)),
        ]);

        let hint = BackrunHint {
            from: a,
            to: b,
            amount_in: U256::from(5_000u64),
            price_impact_bps: 120,
            source: "test".to_string(),
            observed_at: Instant::now(),
        };

        let baseline =
            compute_start_priorities_inner(&graph, &base_profiles, &[], U256::from(1_000u64));
        let boosted =
            compute_start_priorities_inner(&graph, &base_profiles, &[hint], U256::from(1_000u64));

        let baseline_a = baseline.get(&a).copied().unwrap_or_default();
        let boosted_a = boosted.get(&a).copied().unwrap_or_default();

        assert!(boosted_a > baseline_a, "backrun hint should raise priority");
    }
}

fn parse_chain_targets_from_env(chain_list: Option<String>, chain: Option<String>) -> Vec<String> {
    let mut targets: Vec<String> = chain_list
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .map(|chain| chain.trim().to_ascii_lowercase())
                .filter(|chain| !chain.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if targets.is_empty() {
        if let Some(chain) = chain
            .as_deref()
            .map(|chain| chain.trim().to_ascii_lowercase())
            .filter(|chain| !chain.is_empty())
        {
            targets.push(chain.to_string());
        }
    }

    if targets.is_empty() {
        // Prefer explicit CHAIN env over a hard-coded default; Base is the production primary.
        targets.push("base".to_string());
    }

    targets
}

fn parse_chain_targets() -> Vec<String> {
    parse_chain_targets_from_env(
        std::env::var("CHAIN_LIST").ok(),
        std::env::var("CHAIN").ok(),
    )
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum InputSource {
    Ops,
    Env,
}

fn resolve_required_address(
    chain: &str,
    env_prefix: &str,
    suffix: &str,
    legacy_suffixes: &[&str],
    ops_value: Option<String>,
) -> Result<Address> {
    let env_key = format!("{env_prefix}_{suffix}");
    let mut env_keys = Vec::with_capacity(1 + legacy_suffixes.len());
    env_keys.push(env_key.clone());
    env_keys.extend(
        legacy_suffixes
            .iter()
            .map(|legacy| format!("{env_prefix}_{legacy}")),
    );
    let env_value = env_keys.iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    });
    if ops_value.is_some() && env_value.is_some() {
        warn!(
            chain = chain,
            env_prefix = env_prefix,
            field = suffix,
            chosen = ?InputSource::Ops,
            "config source conflict; using ops inputs value",
        );
    }
    if let Some(value) = ops_value {
        return parse_address(&value, &format!("{chain}.{suffix}"));
    }
    if let Some(value) = env_value {
        return parse_address(&value, &env_key);
    }
    Err(anyhow!(
        "missing required {} for {} (expected ops inputs or env var {})",
        suffix,
        chain,
        env_key
    ))
}

fn resolve_optional_address(
    chain: &str,
    env_prefix: &str,
    suffix: &str,
    ops_value: Option<String>,
) -> Result<Option<Address>> {
    let env_key = format!("{env_prefix}_{suffix}");
    let env_value = std::env::var(&env_key)
        .ok()
        .filter(|value| !value.trim().is_empty());
    if ops_value.is_some() && env_value.is_some() {
        warn!(
            chain = chain,
            env_prefix = env_prefix,
            field = suffix,
            chosen = ?InputSource::Ops,
            "config source conflict; using ops inputs value",
        );
    }
    if let Some(value) = ops_value {
        return parse_address(&value, &format!("{chain}.{suffix}")).map(Some);
    }
    if let Some(value) = env_value {
        return parse_address(&value, &env_key).map(Some);
    }
    Ok(None)
}

fn resolve_erc3156_lender(
    chain_name: &str,
    ops_inputs: &crate::ops_inputs::OpsInputs,
) -> Result<Option<Address>> {
    let mut resolved: Option<Address> = None;
    if let Some(chain_inputs) = ops_inputs.chain_inputs(chain_name) {
        for (idx, flashloan) in chain_inputs.flashloans.iter().enumerate() {
            if matches!(
                flashloan.kind,
                Some(crate::ops_inputs::FlashloanKind::Erc3156Like)
            ) {
                if let Some(raw) = flashloan
                    .lender
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                {
                    let label = format!("{chain_name}.flashloans[{idx}].lender");
                    let addr = parse_address(raw, &label)?;
                    if addr.is_zero() {
                        return Err(anyhow!("{label} must not be the zero address"));
                    }
                    if let Some(existing) = resolved {
                        if existing != addr {
                            warn!(
                                chain = chain_name,
                                chosen = ?existing,
                                ignored = ?addr,
                                "multiple ERC3156 lenders configured; using first"
                            );
                        }
                    } else {
                        resolved = Some(addr);
                    }
                }
            }
        }
    }

    if resolved.is_some() {
        return Ok(resolved);
    }

    if let Ok(raw) = std::env::var("ERC3156_LENDER") {
        if !raw.trim().is_empty() {
            let addr = parse_address(&raw, "ERC3156_LENDER")?;
            if addr.is_zero() {
                return Err(anyhow!("ERC3156_LENDER must not be the zero address"));
            }
            return Ok(Some(addr));
        }
    }

    Ok(None)
}

fn resolve_erc3156_fee_bps(
    chain_name: &str,
    ops_inputs: &crate::ops_inputs::OpsInputs,
) -> Result<u32> {
    if let Some(chain) = ops_inputs
        .chains
        .iter()
        .find(|chain| chain.chain_name.eq_ignore_ascii_case(chain_name))
    {
        if let Some(fee_bps) = chain.flashloans.iter().find_map(|fl| {
            if matches!(fl.kind, Some(crate::ops_inputs::FlashloanKind::Erc3156Like)) {
                fl.fee_bps
            } else {
                None
            }
        }) {
            return Ok(fee_bps);
        }
    }

    match std::env::var("ERC3156_FEE_BPS") {
        Ok(raw) => {
            let fee_bps = raw.trim().parse::<u32>().map_err(|_| {
                anyhow!("ERC3156_FEE_BPS must be an integer in range 0..=10000; got `{raw}`")
            })?;
            if fee_bps > 10_000 {
                anyhow::bail!("ERC3156_FEE_BPS must be in range 0..=10000; got {fee_bps}");
            }
            Ok(fee_bps)
        }
        Err(std::env::VarError::NotPresent) => anyhow::bail!(
            "missing ERC3156 fee for chain {}; set ops_inputs flashloans[].fee_bps or ERC3156_FEE_BPS",
            chain_name
        ),
        Err(err) => Err(anyhow!("failed to read ERC3156_FEE_BPS: {err}")),
    }
}

struct ContractDeploymentCheck<'a> {
    chain: &'a str,
    env_prefix: &'a str,
    label: &'a str,
    suffix: &'a str,
    address: Address,
    expected_chain_id: u64,
    rpc_endpoint: &'a str,
}

async fn ensure_contract_deployed<M: Middleware>(
    provider: &M,
    check: ContractDeploymentCheck<'_>,
) -> Result<()>
where
    M::Error: 'static,
{
    let code = provider
        .get_code(check.address, None)
        .await
        .with_context(|| format!("fetch {} contract code", check.label))?;
    if code.0.is_empty() {
        let env_key = format!("{}_{}", check.env_prefix, check.suffix);
        let rpc_chain_id = provider
            .get_chainid()
            .await
            .ok()
            .map(|value| value.as_u64());
        let chain_hint = match rpc_chain_id {
            Some(rpc_chain_id) if rpc_chain_id != check.expected_chain_id => format!(
                "rpc chain_id {rpc_chain_id} does not match configured chain_id {}",
                check.expected_chain_id
            ),
            Some(rpc_chain_id) => format!("rpc chain_id {rpc_chain_id}"),
            None => "rpc chain_id unavailable".to_string(),
        };
        anyhow::bail!(
            "no contract code found for {} address {:#x} on {}; \
check {} or ops inputs {}.{} (rpc {}, {})",
            check.label,
            check.address,
            check.chain,
            env_key,
            check.chain,
            check.suffix,
            check.rpc_endpoint,
            chain_hint,
        );
    }
    Ok(())
}

fn resolve_public_jitter_bps(env_prefix: &str) -> Option<u32> {
    let prefixed = format!("{env_prefix}_PUBLIC_MEMPOOL_JITTER_BPS");
    std::env::var(&prefixed)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .or_else(|| {
            std::env::var("PUBLIC_MEMPOOL_JITTER_BPS")
                .ok()
                .and_then(|raw| raw.parse::<u32>().ok())
        })
}

fn resolve_health_settings(
    env_prefix: &str,
    ops_chain: Option<&crate::ops_inputs::OpsChainOverrides>,
) -> HealthSettings {
    let ops_health = ops_chain.and_then(|chain| chain.health.as_ref());
    let prefixed = |suffix: &str| format!("{env_prefix}_{suffix}");
    let env_f64 = |suffix: &str| {
        std::env::var(prefixed(suffix))
            .ok()
            .and_then(|raw| raw.parse::<f64>().ok())
            .or_else(|| {
                std::env::var(suffix)
                    .ok()
                    .and_then(|raw| raw.parse::<f64>().ok())
            })
    };

    let default_thresholds = HealthThresholds::default();
    let ema_alpha = ops_health
        .and_then(|health| health.ema_alpha)
        .or_else(|| env_f64("HEALTH_EMA_ALPHA"))
        .unwrap_or(0.35)
        .clamp(0.01, 1.0);
    let max_reject_rate = ops_health
        .and_then(|health| health.max_reject_rate_ema)
        .or_else(|| env_f64("HEALTH_MAX_REJECT_RATE_EMA"))
        .unwrap_or(default_thresholds.max_reject_rate_ema)
        .clamp(0.0, 1.0);
    let max_latency_ms = ops_health
        .and_then(|health| health.max_latency_ms_ema)
        .or_else(|| env_f64("HEALTH_MAX_LATENCY_MS_EMA"))
        .unwrap_or(default_thresholds.max_latency_ms_ema)
        .max(1.0);
    let min_success_rate = ops_health
        .and_then(|health| health.min_success_rate_ema)
        .or_else(|| env_f64("HEALTH_MIN_SUCCESS_RATE_EMA"))
        .unwrap_or(default_thresholds.min_success_rate_ema)
        .clamp(0.0, 1.0);

    HealthSettings {
        ema_alpha,
        thresholds: HealthThresholds {
            max_reject_rate_ema: max_reject_rate,
            max_latency_ms_ema: max_latency_ms,
            min_success_rate_ema: min_success_rate,
        },
    }
}

fn native_usd_price(env_prefix: &str) -> Option<f64> {
    let prefixed = format!("{env_prefix}_NATIVE_USD_PRICE");
    std::env::var(&prefixed)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .or_else(|| {
            std::env::var("NATIVE_USD_PRICE")
                .ok()
                .and_then(|raw| raw.parse::<f64>().ok())
        })
}

fn tokens_to_native(price: NativePrice, amount: U256) -> U256 {
    if price.token_amount.is_zero() || price.native_amount.is_zero() {
        return amount;
    }
    mul_div(
        amount,
        price.native_amount.max(U256::one()),
        price.token_amount.max(U256::one()),
    )
}

fn venue_label(edge: &Edge) -> String {
    match &edge.venue {
        VenueEdge::UniV3 { .. } => "univ3".to_string(),
        VenueEdge::UniV2 { .. } => "univ2".to_string(),
        VenueEdge::SolidlyV2 { .. } => "solidly_v2".to_string(),
        VenueEdge::Univ4 { .. } => "univ4".to_string(),
        VenueEdge::Curve { .. } => "curve".to_string(),
        VenueEdge::Balancer { .. } => "balancer".to_string(),
        VenueEdge::Bridge { bridge_name, .. } => format!("bridge:{bridge_name}"),
        VenueEdge::Liquidation { protocol, .. } => format!("liquidation:{protocol}"),
    }
}

fn build_venue_path(edges: &[Edge]) -> Vec<String> {
    edges.iter().map(venue_label).collect()
}

fn is_rpc_error(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}").to_ascii_lowercase();
    message.contains("rpc")
        || message.contains("transport")
        || message.contains("connection")
        || message.contains("timeout")
        || message.contains("http")
}

fn relay_env_endpoints(env_prefix: &str) -> Option<Vec<String>> {
    let prefixed_urls = format!("{env_prefix}_PRIVATE_RELAY_URLS");
    if let Ok(urls) = std::env::var(&prefixed_urls) {
        return Some(parse_endpoint_list(&urls));
    }
    let prefixed_url = format!("{env_prefix}_PRIVATE_RELAY_URL");
    if let Ok(url) = std::env::var(&prefixed_url) {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Some(vec![trimmed.to_string()]);
        }
    }
    if let Ok(urls) = std::env::var("PRIVATE_RELAY_URLS") {
        return Some(parse_endpoint_list(&urls));
    }
    if let Ok(url) = std::env::var("PRIVATE_RELAY_URL") {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Some(vec![trimmed.to_string()]);
        }
    }
    None
}

fn require_pool_inventory(
    chain: &str,
    venue: &str,
    path: &std::path::Path,
    records: &[PoolRecord],
) -> Result<()> {
    if records.is_empty() {
        return Err(anyhow!(
            "no pool inventory loaded for chain={} venue={} path={}; run pool ingestion/discovery before startup",
            chain,
            venue,
            path.display()
        ));
    }
    Ok(())
}

async fn select_broadcast_endpoint(
    cfg: &ChainCfg,
    ops_chain: Option<&crate::ops_inputs::OpsChainOverrides>,
    wallet: &LocalWallet,
    ws_backoff: Duration,
) -> Result<(BroadcastEndpoint, Option<u32>)> {
    let ops_mode = ops_chain.and_then(|chain| chain.broadcast_mode.clone());
    let mode = match ops_mode {
        Some(crate::ops_inputs::BroadcastMode::Private) => BroadcastMode::Private,
        Some(crate::ops_inputs::BroadcastMode::Public) => BroadcastMode::Public,
        None => BroadcastMode::Private,
    };

    let ops_relays = ops_chain
        .map(|chain| chain.broadcast_private_relays.clone())
        .filter(|relays| !relays.is_empty());
    let env_relays = relay_env_endpoints(&cfg.env_prefix).filter(|relays| !relays.is_empty());
    let relays = ops_relays.or(env_relays);
    let public_jitter_override = ops_chain.and_then(|chain| chain.broadcast_public_jitter_bps);
    let allow_private_raw_fallback = ops_chain
        .and_then(|chain| chain.broadcast_private_method_policy.clone())
        .map(|policy| policy.allows_private_raw_fallback())
        .unwrap_or(false);

    if let BroadcastMode::Public = mode {
        return Err(anyhow!(
            "public broadcast mode is disabled; configure private relays and use bundle-only execution"
        ));
    }

    if let Some(endpoints) = relays {
        let providers = connect_private_relays(
            &endpoints,
            &cfg.name,
            allow_private_raw_fallback,
            ws_backoff,
            wallet,
        )
        .await;
        if providers.is_empty() {
            return Err(anyhow!(
                "private relay mode requested but no relay endpoints connected"
            ));
        }
        return Ok((
            BroadcastEndpoint::Private { providers },
            public_jitter_override,
        ));
    }

    let allow_default = std::env::var("ENABLE_DEFAULT_PRIVATE_RELAYS")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(true);
    if cfg.name == "ethereum" && allow_default {
        let default_relays: Vec<String> = DEFAULT_PRIVATE_RELAYS
            .iter()
            .map(|(_, url)| url.to_string())
            .collect();
        let providers =
            connect_private_relays(&default_relays, &cfg.name, false, ws_backoff, wallet).await;
        if providers.is_empty() {
            return Err(anyhow!(
                "default private relay pipeline enabled but no relay endpoints connected"
            ));
        }
        info!(
            target: "broadcast",
            relays = %DEFAULT_PRIVATE_RELAYS
                .iter()
                .map(|(name, _)| name.to_string())
                .collect::<Vec<String>>()
                .join(","),
            "Default private relay pipeline enabled",
        );
        return Ok((
            BroadcastEndpoint::Private { providers },
            public_jitter_override,
        ));
    }

    Err(anyhow!(
        "private relay mode requested but no relays configured"
    ))
}

async fn launch_chain_runtime(
    cfg: ChainCfg,
    registry_chain: Option<RegistryChain>,
    ops_chain: Option<&crate::ops_inputs::OpsChainOverrides>,
    accounting: Option<Arc<Accounting>>,
    ops_inputs: &crate::ops_inputs::OpsInputs,
) -> Result<ChainRuntimeHandle> {
    if let Some(chain_entry) = registry_chain.as_ref() {
        apply_pool_env_overrides(&cfg.env_prefix, chain_entry)?;
    }

    let token_list = TokenList::new(cfg.tokens.clone());
    let wrapped_native = token_list.current().first().copied().unwrap_or_default();
    let base_amount_wei = load_base_amount_wei()?;

    let default_min_flash = if base_amount_wei.is_zero() {
        U256::zero()
    } else {
        base_amount_wei / U256::from(5u64)
    };
    let min_flash_loan_wei = std::env::var("MIN_FLASH_LOAN_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(|| default_min_flash.max(U256::one()));
    let default_max_flash = base_amount_wei
        .checked_mul(U256::from(5u64))
        .unwrap_or(U256::MAX);
    let mut max_flash_loan_wei = std::env::var("MAX_FLASH_LOAN_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or(default_max_flash);
    if max_flash_loan_wei < min_flash_loan_wei {
        max_flash_loan_wei = min_flash_loan_wei;
    }

    let reinvest_bps: u32 = std::env::var("REINVEST_BPS")
        .unwrap_or_else(|_| "10000".into())
        .parse()
        .context("parse REINVEST_BPS")?;
    let siphon_bps: u32 = std::env::var("SIPHON_BPS")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .context("parse SIPHON_BPS")?;
    let growth_unit = std::env::var("COMPOUND_GROWTH_UNIT_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(|| base_amount_wei.max(U256::one()));
    let max_base_cap = std::env::var("COMPOUND_MAX_BASE_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(|| {
            max_flash_loan_wei
                .checked_mul(U256::from(10u64))
                .unwrap_or(U256::MAX)
        });
    let siphon_threshold = std::env::var("SIPHON_THRESHOLD_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(|| default_min_flash.max(U256::one()));
    let siphon_target = std::env::var("SIPHON_TARGET_ADDRESS")
        .ok()
        .and_then(|raw| raw.parse::<Address>().ok());
    let capital_manager = Arc::new(CapitalManager::new(
        base_amount_wei,
        min_flash_loan_wei,
        max_flash_loan_wei,
        reinvest_bps,
        siphon_bps,
        growth_unit,
        max_base_cap,
        siphon_threshold,
        siphon_target,
    )?);

    let pool_depth_refresh_secs = std::env::var("POOL_DEPTH_REFRESH_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(300);
    let pool_depth_refresh = Duration::from_secs(pool_depth_refresh_secs.max(60));
    let pool_depth_cache = Arc::new(PoolDepthCache::new(
        cfg.name.clone(),
        token_list.clone(),
        pool_depth_refresh,
    )?);
    {
        let cache = pool_depth_cache.clone();
        let interval = pool_depth_refresh;
        tokio::spawn(async move {
            cache.refresh_all().await;
            loop {
                sleep(interval).await;
                cache.refresh_all().await;
            }
        });
    }

    let universe_cfg = &ops_inputs.universe;
    let max_hops: usize = universe_cfg
        .max_hops
        .or_else(|| {
            std::env::var("MAX_HOPS")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(6);
    let max_hops_cap: usize = std::env::var("MAX_HOPS_CAP")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(8)
        .max(1);
    let bounded_max_hops = max_hops.min(max_hops_cap);
    let max_relaxations: usize = std::env::var("BELLMAN_MAX_RELAXATIONS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(bounded_max_hops)
        .max(1)
        .min(bounded_max_hops);
    let max_hops = bounded_max_hops;
    let edge_slippage_bps: u32 = std::env::var("EDGE_SLIPPAGE_BPS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .context("parse EDGE_SLIPPAGE_BPS")?;
    let edge_prune_max_slippage_bps: u32 = universe_cfg
        .edge_prune_max_slippage_bps
        .or_else(|| {
            std::env::var("EDGE_PRUNE_MAX_SLIPPAGE_BPS")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(edge_slippage_bps);
    let edge_prune_min_score: f64 = universe_cfg
        .edge_prune_min_score
        .or_else(|| {
            std::env::var("EDGE_PRUNE_MIN_SCORE")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(0.0);
    let edge_prune_liquidity_weight: f64 = universe_cfg
        .edge_prune_liquidity_weight
        .or_else(|| {
            std::env::var("EDGE_PRUNE_LIQUIDITY_WEIGHT")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(1.0);
    let edge_prune_profit_weight: f64 = universe_cfg
        .edge_prune_profit_weight
        .or_else(|| {
            std::env::var("EDGE_PRUNE_PROFIT_WEIGHT")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(1.5);
    let edge_prune_slippage_weight: f64 = universe_cfg
        .edge_prune_slippage_weight
        .or_else(|| {
            std::env::var("EDGE_PRUNE_SLIPPAGE_WEIGHT")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(0.05);
    let max_gas_price_wei = std::env::var("MAX_GAS_PRICE_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(|| U256::from(150_000_000_000u64));
    let max_gas_price_congestion_bps: u32 = std::env::var("MAX_GAS_PRICE_CONGESTION_BPS")
        .unwrap_or_else(|_| "12000".into())
        .parse()
        .context("parse MAX_GAS_PRICE_CONGESTION_BPS")?;
    let profit_margin_bps: u32 = std::env::var("PROFIT_MARGIN_BPS")
        .unwrap_or_else(|_| "200".into())
        .parse()
        .context("parse PROFIT_MARGIN_BPS")?;
    let opportunity_cost_wei = std::env::var("OPPORTUNITY_COST_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(U256::zero);
    let cross_chain_profit_bps: u32 = std::env::var("CROSS_CHAIN_PROFIT_BPS")
        .unwrap_or_else(|_| "175".into())
        .parse()
        .context("parse CROSS_CHAIN_PROFIT_BPS")?;
    let cross_chain_min_profit_wei = std::env::var("CROSS_CHAIN_MIN_PROFIT_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(U256::zero);
    let max_candidate_paths: usize = universe_cfg
        .cycle_candidate_cap_per_block
        .or_else(|| {
            std::env::var("MAX_CANDIDATE_PATHS")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(8)
        .max(1);
    let cycle_search_timeout_ms: u64 = universe_cfg
        .time_budget_ms
        .search
        .or_else(|| {
            std::env::var("CYCLE_SEARCH_TIMEOUT_MS")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(250);
    let quote_budget_ms: u64 = universe_cfg
        .time_budget_ms
        .quoting
        .or_else(|| {
            std::env::var("QUOTE_BUDGET_MS")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(250);
    let simulation_budget_ms: u64 = universe_cfg
        .time_budget_ms
        .simulation
        .or_else(|| {
            std::env::var("SIMULATION_BUDGET_MS")
                .ok()
                .and_then(|raw| raw.parse().ok())
        })
        .unwrap_or(400);
    let (cycle_search_timeout_ms, quote_budget_ms, simulation_budget_ms) =
        derive_chain_time_budget_ms(
            &cfg.name,
            cycle_search_timeout_ms,
            quote_budget_ms,
            simulation_budget_ms,
        );
    let cycle_search_timeout = Duration::from_millis(cycle_search_timeout_ms.max(1));
    let max_bellman_cycles: usize = std::env::var("MAX_BELLMAN_CYCLES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or_else(|| {
            max_candidate_paths
                .saturating_mul(4)
                .max(max_candidate_paths)
                .max(1)
        });
    let min_edge_max_input = std::env::var("MIN_EDGE_MAX_INPUT_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(U256::zero);
    let mut min_liquidity_tokens: f64 = std::env::var("MIN_LIQUIDITY_TOKENS")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .unwrap_or(0.0);
    if let Some(value) = ops_inputs.universe.min_pool_liquidity_tokens {
        min_liquidity_tokens = value;
    }
    let max_quote_block_lag = std::env::var("MAX_QUOTE_BLOCK_LAG")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(U64::from)
        .unwrap_or_else(|| U64::from(2u64));
    let jit_enabled = std::env::var("JIT_LP_ENABLED")
        .unwrap_or_else(|_| "false".to_string())
        .to_lowercase()
        == "true";
    let jit_min_amount_in = std::env::var("JIT_MIN_AMOUNT_WEI")
        .ok()
        .and_then(|v| U256::from_dec_str(&v).ok())
        .unwrap_or_else(U256::zero);
    let jit_seed_bps: u32 = std::env::var("JIT_SEED_BPS")
        .unwrap_or_else(|_| "750".to_string())
        .parse()
        .unwrap_or(750);
    let jit_tick_range: u16 = std::env::var("JIT_TICK_RANGE")
        .unwrap_or_else(|_| "2".to_string())
        .parse()
        .unwrap_or(2);
    let jit_disable_on_quote_failure = std::env::var("JIT_DISABLE_ON_MIN_OUT_FAIL")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(true);
    let congestion_alpha: f64 = std::env::var("CONGESTION_EMA_ALPHA")
        .unwrap_or_else(|_| "0.3".into())
        .parse()
        .unwrap_or(0.3);
    let competition_alpha: f64 = std::env::var("COMPETITION_EMA_ALPHA")
        .unwrap_or_else(|_| "0.45".into())
        .parse()
        .unwrap_or(0.45);
    let min_hops: usize = std::env::var("MIN_HOPS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(3);
    let cycle_limits = BellmanFordLimits {
        min_hops: min_hops.min(max_hops.max(1)),
        max_hops,
        max_relaxations,
        max_cycles: max_bellman_cycles,
        timeout: cycle_search_timeout,
    };
    let search_budget = cycle_search_timeout;
    let quote_budget = Duration::from_millis(quote_budget_ms.max(1));
    let simulation_budget = Duration::from_millis(simulation_budget_ms.max(1));
    let auto_hot_pool_cap = derive_chain_hot_pool_cap(&cfg.name, quote_budget_ms);
    let auto_max_edges_hot = derive_chain_max_edges_hot(&cfg.name, quote_budget_ms);
    let max_edges_hot = universe_cfg
        .max_edges_hot
        .unwrap_or(auto_max_edges_hot)
        .max(1);
    let topk_per_token = universe_cfg.topk_per_token.unwrap_or(3).max(1);
    let raw_dynamic_top_tokens_30d = universe_cfg.dynamic_top_tokens_30d.unwrap_or(60);
    let dynamic_top_tokens_30d = sanitize_dynamic_top_tokens_30d(raw_dynamic_top_tokens_30d);
    if raw_dynamic_top_tokens_30d < 60 {
        warn!(
            configured = raw_dynamic_top_tokens_30d,
            effective = dynamic_top_tokens_30d,
            "universe.dynamic_top_tokens_30d below 60 reduces search breadth; clamping to top-60"
        );
    }
    let mandatory_universe_tokens = build_mandatory_universe_tokens(ops_inputs);
    let hub_tokens = build_hub_tokens(ops_inputs);
    let jit_config = jit_enabled.then_some(JitConfig {
        enabled: true,
        min_amount_in: jit_min_amount_in,
        seed_bps: jit_seed_bps,
        tick_range: jit_tick_range,
        disable_on_quote_failure: jit_disable_on_quote_failure,
    });

    let cb_hourly_loss_limit = std::env::var("CB_HOURLY_LOSS_LIMIT_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(U256::zero);
    let cb_daily_loss_limit = std::env::var("CB_DAILY_LOSS_LIMIT_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(U256::zero);
    let cb_max_consecutive_failures = std::env::var("CB_MAX_CONSECUTIVE_FAILURES")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(3);

    let rpc_backoff_max_secs: u64 = std::env::var("RPC_MAX_BACKOFF_SECS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .context("parse RPC_MAX_BACKOFF_SECS")?;
    let ws_backoff = Duration::from_secs(rpc_backoff_max_secs.max(1));
    let ws_connect_timeout_secs: u64 = std::env::var("WS_CONNECT_TIMEOUT_SECS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .context("parse WS_CONNECT_TIMEOUT_SECS")?;
    let health_settings = resolve_health_settings(&cfg.env_prefix, ops_chain);
    let rpc_health = Arc::new(StdMutex::new(HealthTracker::new(
        health_settings.ema_alpha,
        health_settings.thresholds,
    )));
    let relay_health = Arc::new(StdMutex::new(HealthTracker::new(
        health_settings.ema_alpha,
        health_settings.thresholds,
    )));
    let chaos_disable_ws = std::env::var("CHAOS_DISABLE_WS")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let rpc_endpoints = cfg.rpc_endpoints();
    let http_endpoints: Vec<String> = rpc_endpoints
        .iter()
        .filter_map(|url| coerce_http_url(url))
        .collect();
    if http_endpoints.is_empty() {
        return Err(anyhow!(
            "no valid http endpoints configured for execution-rpc; supply at least one http(s) url",
        ));
    }
    let (raw_provider, rpc_endpoint) = connect_http_provider_with_health(
        "execution-rpc",
        &http_endpoints,
        ws_backoff,
        rpc_health.clone(),
    )
    .await?;
    let provider = Arc::new(raw_provider);
    validate_aave_pool_probes(&cfg, provider.as_ref(), ops_inputs).await?;
    let ws_endpoint_env_key = format!("{}_WS_RPC_URLS", cfg.env_prefix);
    let ws_endpoints = if !cfg.ws_endpoints.is_empty() {
        cfg.ws_endpoints.clone()
    } else if let Ok(raw) = std::env::var(&ws_endpoint_env_key) {
        parse_endpoint_list(&raw)
    } else {
        rpc_endpoints
            .iter()
            .filter_map(|url| coerce_ws_url(url))
            .collect()
    };

    let ws_provider: Option<Arc<Provider<Ws>>> = if ws_endpoints.is_empty() || chaos_disable_ws {
        None
    } else {
        let connect =
            connect_ws_provider_with_fallbacks("subscription-rpc", &ws_endpoints, ws_backoff);
        let provider = if ws_connect_timeout_secs == 0 {
            match connect.await {
                Ok(provider) => Some(provider),
                Err(err) => {
                    warn!(
                        error = %err,
                        "websocket connection failed; falling back to RPC polling"
                    );
                    None
                }
            }
        } else {
            match timeout(Duration::from_secs(ws_connect_timeout_secs), connect).await {
                Ok(Ok(provider)) => Some(provider),
                Ok(Err(err)) => {
                    warn!(
                        error = %err,
                        "websocket connection failed; falling back to RPC polling"
                    );
                    None
                }
                Err(_) => {
                    warn!(
                        timeout_secs = ws_connect_timeout_secs,
                        "websocket connection timed out; falling back to RPC polling"
                    );
                    None
                }
            }
        };
        provider.map(Arc::new)
    };
    let raw_private_key = std::env::var("PRIVATE_KEY").context("PRIVATE_KEY")?;
    let wallet: LocalWallet = raw_private_key
        .parse::<LocalWallet>()
        .context("parse PRIVATE_KEY")?
        .with_chain_id(cfg.chain_id);
    let client = Arc::new(SignerMiddleware::new(
        provider.as_ref().clone(),
        wallet.clone(),
    ));

    let executor_address = resolve_required_address(
        &cfg.name,
        &cfg.env_prefix,
        "EXECUTOR_ADDRESS",
        &[],
        ops_chain.and_then(|chain| chain.executor_address.clone()),
    )?;
    let executor_owner = resolve_optional_address(
        &cfg.name,
        &cfg.env_prefix,
        "EXECUTOR_OWNER",
        ops_chain.and_then(|chain| chain.executor_owner.clone()),
    )?;
    let permit2_address = resolve_required_address(
        &cfg.name,
        &cfg.env_prefix,
        "PERMIT2_ADDRESS",
        &["PERMIT2"],
        ops_chain.and_then(|chain| chain.permit2_address.clone()),
    )?;
    let executor = MultiVenueArbExecutor::new(executor_address, client.clone());
    ensure_contract_deployed(
        provider.as_ref(),
        ContractDeploymentCheck {
            chain: &cfg.name,
            env_prefix: &cfg.env_prefix,
            label: "executor",
            suffix: "EXECUTOR_ADDRESS",
            address: executor_address,
            expected_chain_id: cfg.chain_id,
            rpc_endpoint: &rpc_endpoint,
        },
    )
    .await?;
    let (_, executor_max_slippage_bps_raw, _) = executor
        .get_config()
        .call()
        .await
        .context("fetch executor config")?;
    let executor_max_slippage_bps = u32::from(executor_max_slippage_bps_raw);

    if let Ok(onchain_owner) = executor.owner().call().await {
        if let Some(expected_owner) = executor_owner {
            if onchain_owner != expected_owner {
                warn!(
                    chain = %cfg.name,
                    env_prefix = %cfg.env_prefix,
                    onchain = %format!("{:#x}", onchain_owner),
                    expected = %format!("{:#x}", expected_owner),
                    "executor owner mismatch; using on-chain owner"
                );
            }
        }
    }

    if let Ok(onchain_permit2) = executor.permit_2().call().await {
        if onchain_permit2 != permit2_address {
            warn!(
                chain = %cfg.name,
                env_prefix = %cfg.env_prefix,
                onchain = %format!("{:#x}", onchain_permit2),
                expected = %format!("{:#x}", permit2_address),
                "executor permit2 mismatch; ensure deployment matches configuration"
            );
        }
    }

    let (broadcast_endpoint, public_jitter_override) =
        select_broadcast_endpoint(&cfg, ops_chain, &wallet, ws_backoff).await?;

    let mev_role = std::env::var("MEV_ROLE")
        .ok()
        .map(|raw| raw.parse())
        .transpose()?
        .unwrap_or_default();

    let filler_priority_fee = std::env::var("FILLER_PRIORITY_FEE_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .or_else(|| Some(U256::from(2_000_000_000u64)));

    let searcher_priority_fee = std::env::var("SEARCHER_PRIORITY_FEE_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok());

    let public_jitter_bps = public_jitter_override
        .or_else(|| resolve_public_jitter_bps(&cfg.env_prefix))
        .unwrap_or(75);

    let broadcast = BroadcastConfig {
        endpoint: broadcast_endpoint,
        role: mev_role,
        filler_priority_fee,
        searcher_priority_fee,
        public_jitter_bps,
        private_inclusion_timeout: std::env::var("PRIVATE_RELAY_TIMEOUT_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(4500)),
        relay_health: relay_health.clone(),
    };

    let shadow_enabled = std::env::var("SHADOW_MODE")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let shadow_log_path = std::env::var("SHADOW_LOG_PATH").ok().map(PathBuf::from);
    let shadow_tag = std::env::var("SHADOW_TAG").ok();
    let shadow = if shadow_enabled {
        ShadowConfig {
            enabled: true,
            log_path: shadow_log_path,
            tag: shadow_tag,
        }
    } else {
        ShadowConfig::disabled()
    };

    let chaos_relay_reject_bps = std::env::var("CHAOS_RELAY_REJECT_BPS")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(0);
    let chaos_public_reject_bps = std::env::var("CHAOS_PUBLIC_REJECT_BPS")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(0);
    let chaos_broadcast_delay_ms = std::env::var("CHAOS_BROADCAST_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(0);
    let chaos = ChaosConfig {
        relay_reject_bps: chaos_relay_reject_bps,
        public_reject_bps: chaos_public_reject_bps,
        broadcast_delay_ms: chaos_broadcast_delay_ms,
    };

    let feature_gate = FeatureGate::from_env();
    info!(
        cycle_arb = feature_gate.cycle_arb,
        backrun = feature_gate.backrun,
        sandwich = feature_gate.sandwich,
        liquidations = feature_gate.liquidations,
        bridge = feature_gate.bridge,
        "Feature gates loaded"
    );

    if !feature_gate.cycle_arb
        && !feature_gate.backrun
        && !feature_gate.sandwich
        && !feature_gate.liquidations
        && !feature_gate.bridge
    {
        return Err(anyhow!(
            "all feature gates disabled; enable FEATURE_CYCLE_ARB or another feature"
        ));
    }

    let backrun_requested = std::env::var("BACKRUN_MONITOR")
        .or_else(|_| std::env::var("BACKRUN_MONITOR_ENABLED"))
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let backrun_enabled = if backrun_requested && !feature_gate.backrun {
        warn!("Backrun monitor requested but FEATURE_BACKRUN=0; forcing disabled");
        false
    } else {
        backrun_requested && feature_gate.backrun
    };

    let backrun_monitor = if backrun_enabled {
        let min_amount = std::env::var("BACKRUN_MIN_AMOUNT_WEI")
            .ok()
            .and_then(|raw| U256::from_dec_str(&raw).ok())
            .unwrap_or(base_amount_wei);
        let min_price_impact_bps = std::env::var("BACKRUN_MIN_PRICE_IMPACT_BPS")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(75);
        let poll_interval = std::env::var("BACKRUN_POLL_INTERVAL_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(400));
        let monitor = Arc::new(BackrunMonitor::new(
            min_amount,
            min_price_impact_bps,
            token_list.clone(),
        ));
        tokio::spawn(monitor.clone().run(provider.clone(), poll_interval));
        Some(monitor)
    } else {
        None
    };

    let sandwich_requested = std::env::var("SANDWICH_MONITOR")
        .or_else(|_| std::env::var("SANDWICH_MONITOR_ENABLED"))
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let sandwich_enabled = if sandwich_requested && !feature_gate.sandwich {
        warn!("Sandwich monitor requested but FEATURE_SANDWICH=0; forcing disabled");
        false
    } else {
        sandwich_requested && feature_gate.sandwich
    };
    let sandwich_min_profit = std::env::var("SANDWICH_MIN_PROFIT_WEI")
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
        .unwrap_or_else(|| U256::from(100_000_000_000_000u64));
    let sandwich_monitor = if sandwich_enabled {
        match SandwichMonitor::new(provider.clone(), &cfg.env_prefix, sandwich_min_profit) {
            Ok(monitor) => {
                let monitor = Arc::new(monitor);
                let task_monitor = monitor.clone();
                tokio::spawn(async move {
                    task_monitor.run(Duration::from_millis(300)).await;
                });
                Some(monitor)
            }
            Err(err) => {
                warn!(error = %err, "Failed to initialize sandwich monitor");
                None
            }
        }
    } else {
        None
    };

    let bridge_planner = if feature_gate.bridge {
        let bridge_max_time_secs = std::env::var("BRIDGE_MAX_TIME_SECS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(300);
        BridgePlanner::from_env(bridge_max_time_secs)?.map(Arc::new)
    } else {
        if std::env::var_os("BRIDGE_ROUTES").is_some()
            || std::env::var_os("BRIDGE_ROUTES_FILE").is_some()
        {
            warn!("Bridge routes configured but FEATURE_BRIDGE=0; skipping bridge planner");
        }
        None
    };

    let low_liquidity_scanner = LowLiquidityScanner::from_env(provider.clone())?
        .map(|scanner| Arc::new(Mutex::new(scanner)));

    let liquidations_enabled =
        feature_gate.liquidations && ops_inputs.liquidations_enabled_for(&cfg.name);
    let liquidation_monitor = if liquidations_enabled {
        let ws_provider = if let Some(provider) = ws_provider.clone() {
            provider
        } else {
            return Err(anyhow!(
                "Liquidations enabled for {} but no websocket endpoints provided; set {} or supply websocket URLs in RPC configuration",
                cfg.name,
                ws_endpoint_env_key
            ));
        };
        match LiquidationMonitor::from_ops_inputs(
            provider.clone(),
            ws_provider,
            &cfg.name,
            ops_inputs,
        )? {
            Some(monitor) => Some(Arc::new(monitor)),
            None => {
                warn!(
                    chain = %cfg.name,
                    "Liquidations enabled but no markets configured; skipping liquidation monitor"
                );
                None
            }
        }
    } else {
        if ops_inputs.liquidations_enabled_for(&cfg.name) && !feature_gate.liquidations {
            warn!(
                chain = %cfg.name,
                "Liquidations configured but FEATURE_LIQUIDATIONS=0; skipping liquidation monitor"
            );
        }
        None
    };

    let circuit_breaker = Arc::new(CircuitBreaker::new(
        cb_hourly_loss_limit,
        cb_daily_loss_limit,
        cb_max_consecutive_failures,
    ));

    let metrics_port = std::env::var("PROMETHEUS_PORT")
        .ok()
        .and_then(|raw| raw.parse::<u16>().ok());

    let metrics: Option<Arc<Metrics>> = if let Some(port) = metrics_port {
        let metrics = Arc::new(Metrics::new()?);
        let exporter = metrics.clone();
        tokio::spawn(async move {
            if let Err(err) = exporter.export_to_prometheus(port).await {
                warn!(port = port, error = %err, "Prometheus exporter terminated");
            }
        });
        Some(metrics)
    } else {
        None
    };

    if let Some(metrics) = &metrics {
        metrics.record_capital(&capital_manager.snapshot());
    }

    let hot_pool_config = HotPoolConfig {
        min_liquidity_tokens,
        max_hot_pools: universe_cfg
            .max_hot_pools_per_chain_per_venue
            .unwrap_or(auto_hot_pool_cap)
            .max(1),
        max_cold_pools: universe_cfg.max_cold_pools_stored.unwrap_or(20_000),
        event_sampling_rate: derive_chain_event_sampling_rate(
            &cfg.name,
            universe_cfg.event_sampling_rate.unwrap_or(0.85),
        ),
        event_sampling_blocks: universe_cfg.event_sampling_block_window.unwrap_or(120),
        refresh_interval: Duration::from_secs(universe_cfg.hot_pool_refresh_secs.unwrap_or(30)),
    };

    info!(
        chain = %cfg.name,
        max_hot_pools_per_venue = hot_pool_config.max_hot_pools,
        max_edges_hot,
        quote_budget_ms,
        "Derived chain universe scan capacity"
    );

    let venues = ops_inputs
        .chain_inputs(&cfg.name)
        .map(|chain| chain.venues.clone())
        .unwrap_or_default();
    if venues.is_empty() {
        warn!(
            chain = %cfg.name,
            "no venue config found in ops inputs; hot pool ingestion disabled"
        );
    }

    let mut cold_univ2_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut cold_univ3_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut hot_univ2_by_venue: HashMap<String, Vec<ResolvedUniV2PoolCfg>> = HashMap::new();
    let mut hot_univ3_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut combined_univ2: Vec<ResolvedUniV2PoolCfg> = Vec::new();
    let mut combined_univ3: Vec<PoolRecord> = Vec::new();
    let token_decimals_hint: HashMap<Address, u8> = HashMap::new();

    for venue in venues.iter() {
        let Some(kind) = venue.kind.as_ref() else {
            continue;
        };
        match kind {
            crate::ops_inputs::VenueKind::Univ2Like => {
                let path = pool_data_path(&cfg.name, &venue.name);
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "univ2_like",
                    pool_inventory_path = %path.display(),
                    "loading cold pool inventory"
                );
                let mut cold = load_pool_records(&path).with_context(|| {
                    format!(
                        "failed to load cold univ2 pools for chain={} venue={} path={}",
                        cfg.name,
                        venue.name,
                        path.display()
                    )
                })?;
                if feature_gate.cycle_arb {
                    require_pool_inventory(&cfg.name, &venue.name, &path, &cold)?;
                }
                if cold.len() > hot_pool_config.max_cold_pools {
                    cold.truncate(hot_pool_config.max_cold_pools);
                }
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "univ2_like",
                    cold_pool_records = cold.len(),
                    max_cold_pools = hot_pool_config.max_cold_pools,
                    "loaded cold pool inventory"
                );
                let hot = rank_univ2_pools(
                    provider.clone(),
                    cold.as_slice(),
                    &token_decimals_hint,
                    &hot_pool_config,
                )
                .await
                .unwrap_or_default();
                log_hot_pool_refresh(&cfg.name, &venue.name, "univ2_like", hot.len());
                let configs = univ2_configs_from_records(&hot);
                combined_univ2.extend(configs.clone());
                hot_univ2_by_venue.insert(venue.name.clone(), configs);
                cold_univ2_by_venue.insert(venue.name.clone(), cold);
            }
            crate::ops_inputs::VenueKind::Univ3Like => {
                let path = pool_data_path(&cfg.name, &venue.name);
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "univ3_like",
                    pool_inventory_path = %path.display(),
                    "loading cold pool inventory"
                );
                let mut cold = load_pool_records(&path).with_context(|| {
                    format!(
                        "failed to load cold univ3 pools for chain={} venue={} path={}",
                        cfg.name,
                        venue.name,
                        path.display()
                    )
                })?;

                if feature_gate.cycle_arb {
                    require_pool_inventory(&cfg.name, &venue.name, &path, &cold)?;
                }

                if cold.len() > hot_pool_config.max_cold_pools {
                    cold.truncate(hot_pool_config.max_cold_pools);
                }
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "univ3_like",
                    cold_pool_records = cold.len(),
                    max_cold_pools = hot_pool_config.max_cold_pools,
                    "loaded cold pool inventory"
                );

                let hot =
                    match rank_univ3_pools(provider.clone(), cold.as_slice(), &hot_pool_config)
                        .await
                    {
                        Ok(hot) => hot,
                        Err(err) => {
                            warn!(
                                error = %err,
                                chain = %cfg.name,
                                venue = %venue.name,
                                "failed to rank initial univ3 hot pools; continuing with empty set"
                            );
                            Vec::new()
                        }
                    };

                log_hot_pool_refresh(&cfg.name, &venue.name, "univ3_like", hot.len());
                combined_univ3.extend(hot.clone());
                hot_univ3_by_venue.insert(venue.name.clone(), hot);
                cold_univ3_by_venue.insert(venue.name.clone(), cold);
            }
            _ => {}
        }
    }

    let hot_univ2_pools = Arc::new(tokio::sync::RwLock::new(combined_univ2));
    let hot_univ3_pools = Arc::new(tokio::sync::RwLock::new(combined_univ3));
    let hot_univ2_by_venue = Arc::new(tokio::sync::RwLock::new(hot_univ2_by_venue));
    let hot_univ3_by_venue = Arc::new(tokio::sync::RwLock::new(hot_univ3_by_venue));

    let pool_monitor = {
        let poll_ms = std::env::var("POOL_MONITOR_POLL_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(1_200);
        let stale_ms = std::env::var("POOL_MONITOR_STALE_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or_else(|| poll_ms.saturating_mul(3));
        let pools = hot_univ2_pools.read().await.clone();

        if pools.is_empty() {
            None
        } else {
            let monitored = monitored_pools_from_configs(&pools);

            match PoolMonitor::new(
                provider.clone(),
                ws_provider.clone(),
                monitored,
                Duration::from_millis(poll_ms.max(250)),
                Duration::from_millis(stale_ms.max(poll_ms)),
                None,
            ) {
                Ok(monitor) => {
                    let monitor = Arc::new(monitor);
                    monitor.clone().spawn();
                    Some(monitor)
                }
                Err(err) => {
                    warn!(error = %err, "failed to initialize pool monitor");
                    None
                }
            }
        }
    };

    for (venue_name, cold) in cold_univ2_by_venue.clone() {
        let provider = provider.clone();
        let venue = venue_name.clone();
        let chain = cfg.name.clone();
        let hot_config = hot_pool_config.clone();
        let hot_univ2_pools = Arc::clone(&hot_univ2_pools);
        let hot_univ2_by_venue = Arc::clone(&hot_univ2_by_venue);
        let token_decimals_hint = token_decimals_hint.clone();
        let pool_monitor = pool_monitor.clone();
        tokio::spawn(async move {
            info!(
                chain = %chain,
                venue = %venue,
                kind = "univ2_like",
                cold_pool_records = cold.len(),
                refresh_secs = hot_config.refresh_interval.as_secs(),
                "spawned hot pool refresh worker"
            );
            loop {
                match rank_univ2_pools(
                    provider.clone(),
                    cold.as_slice(),
                    &token_decimals_hint,
                    &hot_config,
                )
                .await
                {
                    Ok(hot) => {
                        log_hot_pool_refresh(&chain, &venue, "univ2_like", hot.len());
                        let configs = univ2_configs_from_records(&hot);
                        {
                            let mut guard = hot_univ2_by_venue.write().await;
                            guard.insert(venue.clone(), configs.clone());
                        }
                        let combined = {
                            let guard = hot_univ2_by_venue.read().await;
                            guard.values().flat_map(|v| v.clone()).collect::<Vec<_>>()
                        };
                        {
                            let mut guard = hot_univ2_pools.write().await;
                            *guard = combined.clone();
                        }
                        if let Some(monitor) = pool_monitor.as_ref() {
                            let monitored = monitored_pools_from_configs(&combined);
                            monitor.set_pools(monitored).await;
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, chain = %chain, venue = %venue, "failed to refresh univ2 hot pools");
                    }
                }
                sleep(hot_config.refresh_interval).await;
            }
        });
    }

    for (venue_name, cold) in cold_univ3_by_venue.clone() {
        let provider = provider.clone();
        let venue = venue_name.clone();
        let chain = cfg.name.clone();
        let hot_config = hot_pool_config.clone();
        let hot_univ3_pools = Arc::clone(&hot_univ3_pools);
        let hot_univ3_by_venue = Arc::clone(&hot_univ3_by_venue);
        tokio::spawn(async move {
            info!(
                chain = %chain,
                venue = %venue,
                kind = "univ3_like",
                cold_pool_records = cold.len(),
                refresh_secs = hot_config.refresh_interval.as_secs(),
                "spawned hot pool refresh worker"
            );
            loop {
                match rank_univ3_pools(provider.clone(), cold.as_slice(), &hot_config).await {
                    Ok(hot) => {
                        log_hot_pool_refresh(&chain, &venue, "univ3_like", hot.len());
                        {
                            let mut guard = hot_univ3_by_venue.write().await;
                            guard.insert(venue.clone(), hot.clone());
                        }
                        let combined = {
                            let guard = hot_univ3_by_venue.read().await;
                            guard.values().flat_map(|v| v.clone()).collect::<Vec<_>>()
                        };
                        let mut guard = hot_univ3_pools.write().await;
                        *guard = combined;
                    }
                    Err(err) => {
                        warn!(error = %err, chain = %chain, venue = %venue, "failed to refresh univ3 hot pools");
                    }
                }
                sleep(hot_config.refresh_interval).await;
            }
        });
    }

    if ws_provider.is_some() || !ws_endpoints.is_empty() {
        tokio::spawn(spawn_pending_tx_monitor(
            provider.clone(),
            ws_provider.clone(),
            ws_endpoints.clone(),
            ws_backoff,
            None,
        ));
    }

    let fee_estimator = FeeEstimator::new(
        cfg.name.clone(),
        cfg.gas_model.clone(),
        provider.as_ref().clone(),
        ArbitrumFeeConfig {
            per_byte_wei: cfg.arbitrum_l1_per_byte_wei,
            max_deviation_bps: cfg.arbitrum_l1_per_byte_max_deviation_bps,
        },
        cfg.gas_rpc_method.clone(),
    );

    let univ3_fee_tiers = collect_univ3_fee_tiers(ops_inputs, &cfg.name);
    let univ3_fee_tiers = if univ3_fee_tiers.is_empty() {
        None
    } else {
        Some(Arc::new(univ3_fee_tiers))
    };
    let erc3156_lender = resolve_erc3156_lender(&cfg.name, ops_inputs)?;
    let erc3156_fee_bps = if erc3156_lender.is_some() {
        resolve_erc3156_fee_bps(&cfg.name, ops_inputs).context("resolve ERC3156 fee")?
    } else {
        0
    };

    let runner_config = RunnerConfig {
        feature_gate,
        chain_name: cfg.name.clone(),
        provider,
        rpc_endpoint,
        rpc_health: rpc_health.clone(),
        univ3_quoter: cfg.univ3_quoter,
        univ3_factory: cfg.univ3_factory,
        univ3_validation: cfg.univ3_validation.clone(),
        univ3_fee_tiers: univ3_fee_tiers.clone(),
        bal_vault: cfg.bal_vault,
        aave_pool: cfg.aave_pool,
        erc3156_lender,
        erc3156_fee_bps,
        bal_flashloan_tokens: cfg
            .bal_flashloan_tokens
            .as_ref()
            .map(|tokens| tokens.iter().copied().collect::<HashSet<_>>()),
        aave_flashloan_tokens: cfg
            .aave_flashloan_tokens
            .as_ref()
            .map(|tokens| tokens.iter().copied().collect::<HashSet<_>>()),
        erc3156_flashloan_tokens: cfg
            .erc3156_flashloan_tokens
            .as_ref()
            .map(|tokens| tokens.iter().copied().collect::<HashSet<_>>()),
        univ2_flashloan_tokens: cfg
            .univ2_flashloan_tokens
            .as_ref()
            .map(|tokens| tokens.iter().copied().collect::<HashSet<_>>()),
        univ3_flashloan_tokens: cfg
            .univ3_flashloan_tokens
            .as_ref()
            .map(|tokens| tokens.iter().copied().collect::<HashSet<_>>()),
        chain_env_prefix: cfg.env_prefix.clone(),
        tokens: token_list.clone(),
        wrapped_native,
        capital: capital_manager.clone(),
        pool_depth_cache: pool_depth_cache.clone(),
        pool_monitor: pool_monitor.clone(),
        hot_univ2_pools: Arc::clone(&hot_univ2_pools),
        hot_univ3_pools: Arc::clone(&hot_univ3_pools),
        edge_slippage_bps,
        executor_max_slippage_bps,
        edge_prune_max_slippage_bps,
        edge_prune_min_score,
        edge_prune_liquidity_weight,
        edge_prune_profit_weight,
        edge_prune_slippage_weight,
        cycle_limits,
        search_budget,
        quote_budget,
        simulation_budget,
        max_edges_hot,
        topk_per_token,
        dynamic_top_tokens_30d,
        mandatory_universe_tokens,
        hub_tokens,
        max_gas_price_wei,
        max_gas_price_congestion_bps,
        profit_margin_bps,
        opportunity_cost_wei,
        cross_chain_profit_bps,
        cross_chain_min_profit_wei,
        max_candidate_paths,
        min_flash_loan_wei,
        min_edge_max_input,
        min_liquidity_tokens,
        max_quote_block_lag,
        congestion_alpha,
        competition_alpha,
        jit_config: jit_config.clone(),
        broadcast,
        shadow,
        chaos,
        wallet: Some(wallet),
        backrun_monitor,
        sandwich_monitor,
        bridge: bridge_planner,
        low_liquidity_scanner,
        liquidation_monitor,
        circuit_breaker: circuit_breaker.clone(),
        metrics: metrics.clone(),
        accounting,
        fee_estimator,
    };

    let runner = Runner::new(runner_config, executor);

    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let (status_tx, status_rx) =
        watch::channel(StatusSnapshot::new(RunnerState::Idle, "Booting", None));

    let chain_name = cfg.name.clone();
    let join_handle = tokio::spawn(async move {
        if let Err(err) = runner.run(cmd_rx, status_tx).await {
            warn!(chain = %chain_name, error = %err, "Chain runner terminated with error");
        }
    });

    Ok(ChainRuntimeHandle {
        name: cfg.name,
        command_tx: cmd_tx,
        status_rx,
        join_handle,
    })
}

const DEFAULT_BASE_AMOUNT_WEI: u64 = 1_000_000_000_000_000_000;

fn load_base_amount_wei() -> Result<U256> {
    match std::env::var("BASE_AMOUNT_WEI") {
        Ok(raw) => U256::from_dec_str(&raw).context("parse BASE_AMOUNT_WEI"),
        Err(_) => {
            let fallback = U256::from(DEFAULT_BASE_AMOUNT_WEI);
            warn!(
                default_base_amount_wei = %fallback,
                "BASE_AMOUNT_WEI not set; using production-safe quote baseline"
            );
            Ok(fallback)
        }
    }
}

fn validate_global_config() -> Result<()> {
    let raw_private_key = std::env::var("PRIVATE_KEY").context("PRIVATE_KEY")?;
    raw_private_key
        .parse::<LocalWallet>()
        .context("parse PRIVATE_KEY")?;

    if production_mode_enabled() {
        ensure!(
            !secret_looks_placeholder(&raw_private_key),
            "production mode rejected placeholder PRIVATE_KEY; rotate credentials before live trading"
        );
        if let Ok(alchemy_key) = std::env::var("ALCHEMY_KEY") {
            ensure!(
                !secret_looks_placeholder(&alchemy_key),
                "production mode rejected placeholder ALCHEMY_KEY"
            );
        }

        for (flag, label) in [
            ("CHAOS_RELAY_REJECT_BPS", "relay reject chaos"),
            ("CHAOS_PUBLIC_REJECT_BPS", "public reject chaos"),
            ("CHAOS_BROADCAST_DELAY_MS", "broadcast delay chaos"),
        ] {
            if let Ok(raw) = std::env::var(flag) {
                let enabled = raw.trim().parse::<u64>().unwrap_or(0) > 0;
                ensure!(
                    !enabled,
                    "production mode rejected non-zero {flag} ({label})"
                );
            }
        }
        if std::env::var("CHAOS_DISABLE_WS")
            .ok()
            .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
        {
            return Err(anyhow!(
                "production mode rejected CHAOS_DISABLE_WS=true"
            ));
        }
    }

    Ok(())
}

async fn validate_aave_pool_probes(
    cfg: &ChainCfg,
    provider: &Provider<Http>,
    ops_inputs: &crate::ops_inputs::OpsInputs,
) -> Result<()> {
    let mut probe_targets = Vec::new();
    if let Some(pool) = cfg.aave_pool {
        probe_targets.push((pool, "chain_cfg.aave_pool".to_string()));
    }
    for target in ops_inputs.configured_aave_pool_probe_targets(&cfg.name) {
        let pool = parse_address(&target.pool, &target.source)
            .with_context(|| format!("parse configured Aave pool for {}", cfg.name))?;
        probe_targets.push((pool, target.source));
    }

    let mut seen = HashSet::new();
    for (pool, source) in probe_targets {
        if !seen.insert(pool) {
            continue;
        }
        probe_aave_pool_interface(provider, pool, &source)
            .await
            .with_context(|| {
                format!(
                    "Aave pool interface probe failed on {} ({source})",
                    cfg.name
                )
            })?;
    }

    Ok(())
}
fn load_dotenv() {
    if dotenvy::dotenv().is_ok() {
        return;
    }
    let fallback = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    if fallback.exists() {
        let _ = dotenvy::from_path(&fallback);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();

    let env_filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| EnvFilter::try_new(raw).ok())
        .unwrap_or_else(|| EnvFilter::new("info,ethers_providers::rpc::transports::ws=off"));

    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let offline_mode = std::env::var("OFFLINE_MODE")
        .ok()
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    if offline_mode {
        info!("Offline mode enabled; skipping runtime initialization");
        return Ok(());
    }

    let ops_inputs = load_ops_inputs("ops/inputs.yaml").context("validate ops/inputs.yaml")?;
    validate_global_config().context("validate global config")?;

    let feature_liquidations = read_feature_flag("FEATURE_LIQUIDATIONS", false);
    ops_inputs
        .validate_liquidation_safety(feature_liquidations)
        .context("validate liquidation config safety")?;

    let accounting: Option<Arc<Accounting>> = Accounting::from_env()?.map(Arc::new);
    if let Some(acct) = &accounting {
        acct.clone().spawn_daily_rollups();
    }

    let registry = maybe_load_registry().await?;
    let chain_targets = parse_chain_targets();

    if chain_targets.is_empty() {
        return Err(anyhow!("No chains configured; set CHAIN or CHAIN_LIST"));
    }

    let mut chain_handles = Vec::new();
    let mut status_monitors = Vec::new();

    for chain_name in chain_targets {
        let registry_chain = registry
            .as_ref()
            .and_then(|reg| reg.chains.get(&chain_name))
            .cloned();

        let ops_chain = ops_inputs.chain_overrides(&chain_name);
        let cfg = load_chain_from_sources(
            &chain_name,
            registry.as_ref().map(|reg| reg.version.clone()),
            registry_chain.as_ref(),
            ops_chain.as_ref(),
        )
        .context("load chain config")?;
        let chain_inputs = ops_inputs.chain_inputs(&chain_name);
        validate_chain_cfg(&cfg, ops_chain.as_ref(), chain_inputs)
            .context("validate chain config")?;

        info!(
            chain = %chain_name,
            registry_version = %cfg.registry_version.clone().unwrap_or_default(),
            "Loaded chain configuration",
        );

        let chain_handle = launch_chain_runtime(
            cfg,
            registry_chain,
            ops_chain.as_ref(),
            accounting.clone(),
            &ops_inputs,
        )
        .await?;
        status_monitors.push(tokio::spawn(monitor_status(
            chain_handle.name.clone(),
            chain_handle.status_rx.clone(),
        )));
        chain_handles.push(chain_handle);
    }

    let command_txs: Vec<_> = chain_handles
        .iter()
        .map(|handle| handle.command_tx.clone())
        .collect();

    broadcast_command(&command_txs, Command::Start).await.ok();

    let command_handle = tokio::spawn(command_listener(command_txs.clone()));

    let ctrl_handle = tokio::spawn({
        let txs = command_txs.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                broadcast_command(&txs, Command::Quit).await.ok();
            }
        }
    });

    for handle in chain_handles {
        let _ = handle.join_handle.await;
    }

    let _ = command_handle.await;
    let _ = ctrl_handle.await;
    for monitor in status_monitors {
        let _ = monitor.await;
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::TransactionReceipt;
    use std::env;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_private_relays_match_bundle_pipeline_targets() {
        let labels: std::collections::HashSet<&str> = DEFAULT_PRIVATE_RELAYS
            .iter()
            .map(|(label, _)| *label)
            .collect();

        assert!(labels.contains("flashbots"));
        assert!(labels.contains("beaverbuild"));
        assert!(labels.contains("builder0x69"));
        assert!(labels.contains("rsync-builder"));
        assert!(labels.contains("alchemy"));
        assert!(!labels.contains("titan"));
    }

    #[test]
    fn build_send_bundle_request_uses_eth_send_bundle_method() {
        let raw = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);
        let body = build_send_bundle_request(&raw, U64::from(0xabcdu64));

        assert_eq!(body["method"], "eth_sendBundle");
        assert_eq!(body["params"][0]["txs"][0], "0xdeadbeef");
        assert_eq!(body["params"][0]["blockNumber"], "0xabcd");
    }

    #[tokio::test]
    async fn send_private_rpc_bundle_falls_back_to_private_raw_when_allowed() {
        let (provider, mock) = Provider::mocked();
        mock.push_response(ethers::providers::MockResponse::Value(
            serde_json::json!({}),
        )); // raw tx fallback success
        mock.push_response(ethers::providers::MockResponse::Error(
            ethers::providers::JsonRpcError {
                code: -32601,
                message: "Method not found".into(),
                data: None,
            },
        )); // bundle attempt fails first
        let raw = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);

        let (tx_hash, method) =
            send_private_rpc_bundle(&provider, raw.clone(), U64::from(42u64), "base", true)
                .await
                .expect("fallback should succeed");

        assert_eq!(tx_hash, H256::from(ethers::utils::keccak256(&raw)));
        assert!(matches!(method, PrivateSubmissionMethod::PrivateRaw));
        mock.assert_request(
            "eth_sendBundle",
            serde_json::json!([{
                "txs": ["0xdeadbeef"],
                "blockNumber": "0x2a"
            }]),
        )
        .expect("bundle request asserted");
        mock.assert_request("eth_sendRawTransaction", serde_json::json!(["0xdeadbeef"]))
            .expect("raw tx fallback request asserted");
    }

    #[tokio::test]
    async fn send_private_rpc_bundle_does_not_fallback_when_disabled() {
        let (provider, mock) = Provider::mocked();
        mock.push_response(ethers::providers::MockResponse::Error(
            ethers::providers::JsonRpcError {
                code: -32601,
                message: "Method not found".into(),
                data: None,
            },
        ));
        let raw = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);

        let err = send_private_rpc_bundle(&provider, raw, U64::from(42u64), "base", false)
            .await
            .expect_err("bundle error should be returned");

        assert!(format!("{err}").contains("Method not found"));
        mock.assert_request(
            "eth_sendBundle",
            serde_json::json!([{
                "txs": ["0xdeadbeef"],
                "blockNumber": "0x2a"
            }]),
        )
        .expect("bundle request asserted");
    }

    #[test]
    fn parse_chain_targets_prefers_chain_list() {
        let targets = parse_chain_targets_from_env(
            Some("Arbitrum, Optimism , ,base".to_string()),
            Some("polygon".to_string()),
        );

        assert_eq!(targets, vec!["arbitrum", "optimism", "base"]);
    }

    #[test]
    fn load_base_amount_defaults_to_production_baseline_when_missing() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("BASE_AMOUNT_WEI").ok();
        env::remove_var("BASE_AMOUNT_WEI");

        let amount = load_base_amount_wei().expect("default base amount");
        assert_eq!(amount, U256::from(DEFAULT_BASE_AMOUNT_WEI));

        match prior {
            Some(value) => env::set_var("BASE_AMOUNT_WEI", value),
            None => env::remove_var("BASE_AMOUNT_WEI"),
        }
    }

    #[test]
    fn load_base_amount_parses_env_value() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("BASE_AMOUNT_WEI").ok();
        env::set_var("BASE_AMOUNT_WEI", "4200");

        let amount = load_base_amount_wei().expect("parse base amount");
        assert_eq!(amount, U256::from(4200u64));

        match prior {
            Some(value) => env::set_var("BASE_AMOUNT_WEI", value),
            None => env::remove_var("BASE_AMOUNT_WEI"),
        }
    }

    #[test]
    fn parse_chain_targets_falls_back_to_chain_when_list_empty() {
        let targets =
            parse_chain_targets_from_env(Some(" ,  ,".to_string()), Some("Base".to_string()));

        assert_eq!(targets, vec!["base"]);
    }

    #[test]
    fn parse_chain_targets_defaults_to_arbitrum_when_unset() {
        let targets = parse_chain_targets_from_env(None, Some("   ".to_string()));

        assert_eq!(targets, vec!["arbitrum"]);
    }

    #[test]
    fn validate_global_config_requires_private_key() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("PRIVATE_KEY").ok();
        env::remove_var("PRIVATE_KEY");

        let err = validate_global_config().expect_err("missing private key should error");
        assert!(err.to_string().contains("PRIVATE_KEY"));

        match prior {
            Some(value) => env::set_var("PRIVATE_KEY", value),
            None => env::remove_var("PRIVATE_KEY"),
        }
    }

    #[test]
    fn validate_global_config_accepts_valid_private_key() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("PRIVATE_KEY").ok();
        env::set_var(
            "PRIVATE_KEY",
            "0x59c6995e998f97a5a0044966f0945382d4a4d1d3786ea5e1473f2e0b64c1f0b7",
        );

        validate_global_config().expect("valid private key");

        match prior {
            Some(value) => env::set_var("PRIVATE_KEY", value),
            None => env::remove_var("PRIVATE_KEY"),
        }
    }

    fn ops_inputs_with_erc3156_fee(
        chain_name: &str,
        fee_bps: Option<u32>,
    ) -> crate::ops_inputs::OpsInputs {
        crate::ops_inputs::OpsInputs {
            chains: vec![crate::ops_inputs::ChainInputs {
                chain_name: chain_name.to_string(),
                chain_id: 1,
                env_prefix: "TEST".to_string(),
                rpc_http_urls: vec![],
                rpc_ws_urls: vec![],
                gas_model: None,
                gas_rpc_method: None,
                arbitrum_l1_per_byte_wei: None,
                arbitrum_l1_per_byte_max_deviation_bps: None,
                executor_address: "0x1111111111111111111111111111111111111111".to_string(),
                executor_owner: None,
                permit2_address: "0x000000000022D473030F116dDEE9F6B43aC78BA3".to_string(),
                venues: vec![],
                flashloans: vec![crate::ops_inputs::FlashloanConfig {
                    name: Some("erc3156".to_string()),
                    kind: Some(crate::ops_inputs::FlashloanKind::Erc3156Like),
                    pool: None,
                    vault: None,
                    lender: Some("0x1111111111111111111111111111111111111111".to_string()),
                    fee_bps,
                    factory: None,
                    max_loan_assets: vec!["0x2222222222222222222222222222222222222222".to_string()],
                    allowlist_tokens: vec!["0x2222222222222222222222222222222222222222".to_string()],
                }],
                broadcast: None,
                health: None,
                extras: Default::default(),
            }],
            universe: Default::default(),
            risk: Default::default(),
            features: Default::default(),
        }
    }

    #[test]
    fn resolve_erc3156_fee_bps_accepts_valid_value() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("ERC3156_FEE_BPS").ok();
        env::set_var("ERC3156_FEE_BPS", "10000");
        let ops_inputs = ops_inputs_with_erc3156_fee("base", Some(9));

        let fee_bps = resolve_erc3156_fee_bps("base", &ops_inputs).expect("valid fee bps");
        assert_eq!(fee_bps, 9);

        match prior {
            Some(value) => env::set_var("ERC3156_FEE_BPS", value),
            None => env::remove_var("ERC3156_FEE_BPS"),
        }
    }

    #[test]
    fn resolve_erc3156_fee_bps_errors_when_missing_everywhere() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("ERC3156_FEE_BPS").ok();
        env::remove_var("ERC3156_FEE_BPS");
        let ops_inputs = ops_inputs_with_erc3156_fee("base", None);

        let err =
            resolve_erc3156_fee_bps("base", &ops_inputs).expect_err("missing fee should fail");
        assert!(err
            .to_string()
            .contains("missing ERC3156 fee for chain base"));

        match prior {
            Some(value) => env::set_var("ERC3156_FEE_BPS", value),
            None => env::remove_var("ERC3156_FEE_BPS"),
        }
    }

    #[test]
    fn resolve_erc3156_fee_bps_rejects_malformed_value() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("ERC3156_FEE_BPS").ok();
        env::set_var("ERC3156_FEE_BPS", "abc");
        let ops_inputs = ops_inputs_with_erc3156_fee("base", None);

        let err = resolve_erc3156_fee_bps("base", &ops_inputs)
            .expect_err("malformed fee bps should fail");
        assert!(err
            .to_string()
            .contains("ERC3156_FEE_BPS must be an integer in range 0..=10000"));

        match prior {
            Some(value) => env::set_var("ERC3156_FEE_BPS", value),
            None => env::remove_var("ERC3156_FEE_BPS"),
        }
    }

    #[test]
    fn resolve_erc3156_fee_bps_rejects_out_of_range_value() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("ERC3156_FEE_BPS").ok();
        env::set_var("ERC3156_FEE_BPS", "10001");
        let ops_inputs = ops_inputs_with_erc3156_fee("base", None);

        let err = resolve_erc3156_fee_bps("base", &ops_inputs)
            .expect_err("out-of-range fee bps should fail");
        assert!(err
            .to_string()
            .contains("ERC3156_FEE_BPS must be in range 0..=10000"));

        match prior {
            Some(value) => env::set_var("ERC3156_FEE_BPS", value),
            None => env::remove_var("ERC3156_FEE_BPS"),
        }
    }

    #[test]
    fn rejects_multi_loan_allocations_before_plan_build() {
        let allocations = vec![
            FlashLoanSelection {
                provider: FlashLoanProvider::Balancer,
                amount: U256::from(1u64),
                fee_bps: 0,
                provider_addr: None,
            },
            FlashLoanSelection {
                provider: FlashLoanProvider::AaveV3,
                amount: U256::from(1u64),
                fee_bps: 9,
                provider_addr: None,
            },
        ];

        let err = ensure_single_loan_allocation(&allocations).expect_err("reject multi-loan");
        match err {
            PlannerAllocationError::MultiLoanNotSupported { allocations } => {
                assert_eq!(allocations, 2);
            }
        }
    }

    #[test]
    fn expected_univ3_edge_upper_bound_matches_combinatorics() {
        assert_eq!(expected_univ3_edge_upper_bound(0), 0);
        assert_eq!(expected_univ3_edge_upper_bound(1), 2);
        assert_eq!(expected_univ3_edge_upper_bound(2), 4);
        assert_eq!(expected_univ3_edge_upper_bound(8), 16);
    }

    #[test]
    fn derive_chain_hot_pool_cap_scales_with_budget() {
        let low = derive_chain_hot_pool_cap("ethereum", 100);
        let base = derive_chain_hot_pool_cap("ethereum", 250);
        let high = derive_chain_hot_pool_cap("ethereum", 600);
        assert!(low < base);
        assert!(high > base);
    }

    #[test]
    fn derive_chain_max_edges_hot_respects_chain_profile() {
        let eth = derive_chain_max_edges_hot("ethereum", 250);
        let base = derive_chain_max_edges_hot("base", 250);
        let ink = derive_chain_max_edges_hot("ink", 250);
        assert!(eth >= base);
        assert!(base >= ink);
        assert!(ink >= 1_200);
    }

    #[test]
    fn sanitize_token_whitelist_cap_enforces_minimum_pair_capacity() {
        assert_eq!(sanitize_token_whitelist_cap(0), 64);
        assert_eq!(sanitize_token_whitelist_cap(1), 64);
        assert_eq!(sanitize_token_whitelist_cap(2), 64);
        assert_eq!(sanitize_token_whitelist_cap(63), 64);
        assert_eq!(sanitize_token_whitelist_cap(64), 64);
    }

    #[test]
    fn sanitize_dynamic_top_tokens_30d_enforces_top_60_floor() {
        assert_eq!(sanitize_dynamic_top_tokens_30d(0), 60);
        assert_eq!(sanitize_dynamic_top_tokens_30d(1), 60);
        assert_eq!(sanitize_dynamic_top_tokens_30d(59), 60);
        assert_eq!(sanitize_dynamic_top_tokens_30d(60), 60);
        assert_eq!(sanitize_dynamic_top_tokens_30d(120), 120);
    }

    #[test]
    fn graph_change_detection_skips_identical_snapshots() {
        let mut graph = Graph::default();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [0u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 50_000,
            weight: -10,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        let digest = graph_digest(&graph);
        assert!(!graph_changed_significantly(Some(digest), digest));
    }

    #[test]
    fn collects_univ3_fee_tiers_from_ops_inputs() {
        let ops = crate::ops_inputs::OpsInputs {
            chains: vec![crate::ops_inputs::ChainInputs {
                chain_name: "arbitrum".into(),
                chain_id: 42_161,
                env_prefix: "ARB".into(),
                rpc_http_urls: vec![],
                rpc_ws_urls: vec![],
                gas_model: None,
                gas_rpc_method: None,
                arbitrum_l1_per_byte_wei: None,
                arbitrum_l1_per_byte_max_deviation_bps: None,
                executor_address: String::new(),
                executor_owner: None,
                permit2_address: String::new(),
                venues: vec![
                    crate::ops_inputs::VenueConfig {
                        name: "uniswap_v3".into(),
                        kind: Some(crate::ops_inputs::VenueKind::Univ3Like),
                        factory: None,
                        router: None,
                        quoter: None,
                        pool_manager: None,
                        vault: None,
                        registry: None,
                        fee_tiers: Some(vec![100, 500]),
                        fee_bps: None,
                        pool_init_code_hash: None,
                    },
                    crate::ops_inputs::VenueConfig {
                        name: "uniswap_v2".into(),
                        kind: Some(crate::ops_inputs::VenueKind::Univ2Like),
                        factory: None,
                        router: None,
                        quoter: None,
                        pool_manager: None,
                        vault: None,
                        registry: None,
                        fee_tiers: Some(vec![3000]),
                        fee_bps: None,
                        pool_init_code_hash: None,
                    },
                ],
                flashloans: vec![],
                broadcast: None,
                health: None,
                extras: Default::default(),
            }],
            universe: Default::default(),
            risk: Default::default(),
            features: Default::default(),
        };

        let tiers = collect_univ3_fee_tiers(&ops, "arbitrum");
        assert_eq!(tiers.len(), 2);
        assert!(tiers.contains(&100));
        assert!(tiers.contains(&500));
        assert!(!tiers.contains(&3000));
    }

    #[test]
    fn build_univ3_price_path_places_fee_on_second_hop() {
        let token_in = Address::from_low_u64_be(1);
        let token_out = Address::from_low_u64_be(2);
        let fee = 500u32;

        let path = build_univ3_price_path(token_in, token_out, fee);

        assert_eq!(path.len(), 2);
        assert_eq!(path[0].0, token_in);
        assert_eq!(path[0].1, None);
        assert_eq!(path[1].0, token_out);
        assert_eq!(path[1].1, Some(fee));
    }

    #[test]
    fn require_pool_inventory_rejects_empty_records() {
        let path = std::path::Path::new("data/arbitrum/uniswap_v3/pools.jsonl");
        let err = require_pool_inventory("arbitrum", "uniswap_v3", path, &[])
            .expect_err("expected missing inventory error");
        assert!(format!("{err:#}").contains("no pool inventory loaded"));
    }

    #[test]
    fn require_pool_inventory_accepts_non_empty_records() {
        let path = std::path::Path::new("data/arbitrum/uniswap_v3/pools.jsonl");
        let records = vec![PoolRecord {
            pool: Address::from_low_u64_be(1),
            token0: Address::from_low_u64_be(2),
            token1: Address::from_low_u64_be(3),
            fee: 500,
            created_block: 1,
        }];
        require_pool_inventory("arbitrum", "uniswap_v3", path, &records)
            .expect("inventory should be accepted");
    }
    #[test]
    fn caps_cycles_per_start_token() {
        let mut graph = Graph::default();
        let a = Address::from_low_u64_be(10);
        let b = Address::from_low_u64_be(11);
        let c = Address::from_low_u64_be(12);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [1u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 50_000,
            weight: -5,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: b,
            to: a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [2u8; 32],
                token_in: b,
                token_out: a,
            },
            estimated_gas: 50_000,
            weight: -5,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: a,
            to: c,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [3u8; 32],
                token_in: a,
                token_out: c,
            },
            estimated_gas: 50_000,
            weight: -1,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: c,
            to: a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [4u8; 32],
                token_in: c,
                token_out: a,
            },
            estimated_gas: 50_000,
            weight: -1,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let cycles = vec![vec![0, 1, 0], vec![0, 2, 0]];
        let capped = cap_cycles_per_start(cycles, &graph, 1);
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn filter_cycles_by_hubs_allows_non_hub_intermediates_by_default() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("STRICT_HUB_INTERMEDIATES").ok();
        env::remove_var("STRICT_HUB_INTERMEDIATES");

        let mut graph = Graph::default();
        let start = Address::from_low_u64_be(11);
        let middle = Address::from_low_u64_be(12);
        graph.nodes = vec![start, middle];
        let mut hubs = HashSet::new();
        hubs.insert(start);

        let cycles = vec![vec![0, 1, 0]];
        let filtered = filter_cycles_by_hubs(cycles, &graph, &hubs);
        assert_eq!(
            filtered.len(),
            1,
            "default should keep non-hub intermediate"
        );

        match prior {
            Some(value) => env::set_var("STRICT_HUB_INTERMEDIATES", value),
            None => env::remove_var("STRICT_HUB_INTERMEDIATES"),
        }
    }

    #[test]
    fn filter_cycles_by_hubs_strict_mode_rejects_non_hub_intermediates() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("STRICT_HUB_INTERMEDIATES").ok();
        env::set_var("STRICT_HUB_INTERMEDIATES", "true");

        let mut graph = Graph::default();
        let start = Address::from_low_u64_be(21);
        let middle = Address::from_low_u64_be(22);
        graph.nodes = vec![start, middle];
        let mut hubs = HashSet::new();
        hubs.insert(start);

        let cycles = vec![vec![0, 1, 0]];
        let filtered = filter_cycles_by_hubs(cycles, &graph, &hubs);
        assert!(
            filtered.is_empty(),
            "strict mode should reject non-hub intermediate"
        );

        match prior {
            Some(value) => env::set_var("STRICT_HUB_INTERMEDIATES", value),
            None => env::remove_var("STRICT_HUB_INTERMEDIATES"),
        }
    }

    #[test]
    fn filter_cycles_by_hubs_strict_start_rejects_non_hub_start() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior_start = env::var("STRICT_START_TOKEN_HUB_ONLY").ok();
        env::set_var("STRICT_START_TOKEN_HUB_ONLY", "true");

        let mut graph = Graph::default();
        let start = Address::from_low_u64_be(31);
        let middle = Address::from_low_u64_be(32);
        graph.nodes = vec![start, middle];
        let hubs = HashSet::new();
        let cycles = vec![vec![0, 1, 0]];
        let filtered = filter_cycles_by_hubs(cycles, &graph, &hubs);
        assert!(
            filtered.is_empty(),
            "strict start should reject non-hub start token"
        );

        match prior_start {
            Some(value) => env::set_var("STRICT_START_TOKEN_HUB_ONLY", value),
            None => env::remove_var("STRICT_START_TOKEN_HUB_ONLY"),
        }
    }

    #[test]
    fn pricing_reliability_rejects_non_native_without_price() {
        let wrapped_native = Address::from_low_u64_be(1);
        let non_native = Address::from_low_u64_be(2);
        let unreliable = NativePrice::new(U256::zero(), U256::zero(), false);
        assert!(!start_token_pricing_reliable(
            non_native,
            wrapped_native,
            unreliable
        ));
    }

    #[test]
    fn pricing_reliability_accepts_wrapped_native_unit_pricing() {
        let wrapped_native = Address::from_low_u64_be(1);
        let unreliable = NativePrice::unit();
        assert!(start_token_pricing_reliable(
            wrapped_native,
            wrapped_native,
            unreliable
        ));
    }

    #[test]
    fn unreliable_non_native_price_entries_are_not_reused_or_cached() {
        let wrapped_native = Address::from_low_u64_be(1);
        let non_native = Address::from_low_u64_be(2);
        let unreliable = NativePrice::new(U256::zero(), U256::zero(), false);

        assert!(!should_reuse_cached_native_price(
            non_native,
            wrapped_native,
            unreliable
        ));
        assert!(!should_cache_native_price(
            non_native,
            wrapped_native,
            unreliable
        ));
    }

    #[test]
    fn reliable_or_wrapped_native_prices_can_stay_cached() {
        let wrapped_native = Address::from_low_u64_be(1);
        let non_native = Address::from_low_u64_be(2);
        let reliable = NativePrice::new(U256::from(1u64), U256::from(2u64), true);
        let wrapped_unreliable = NativePrice::new(U256::zero(), U256::zero(), false);

        assert!(should_reuse_cached_native_price(
            non_native,
            wrapped_native,
            reliable
        ));
        assert!(should_cache_native_price(
            non_native,
            wrapped_native,
            reliable
        ));
        assert!(should_reuse_cached_native_price(
            wrapped_native,
            wrapped_native,
            wrapped_unreliable
        ));
        assert!(should_cache_native_price(
            wrapped_native,
            wrapped_native,
            wrapped_unreliable
        ));
    }

    #[tokio::test]
    async fn prune_edges_prefers_recent_profitability() {
        let mut graph = Graph::default();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let c = Address::from_low_u64_be(3);
        let fee = 3000u32;

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(a, None), (b, Some(fee))],
                pool: Address::from_low_u64_be(9),
                fee,
            },
            estimated_gas: 70_000,
            weight: -2,
            max_input: U256::from(1_000_000u64),
            tolerance_bps: 10,
            observed_slippage_bps: 100,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: a,
            to: c,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(a, None), (c, Some(fee))],
                pool: Address::from_low_u64_be(10),
                fee,
            },
            estimated_gas: 70_000,
            weight: -2,
            max_input: U256::from(10_000u64),
            tolerance_bps: 5,
            observed_slippage_bps: 10,
            quote_block: None,
            active: true,
        });

        let hot_paths = HotPathCache::with_failure_backoff(
            0.0,
            Duration::from_secs(300),
            5,
            Duration::from_secs(60),
        );
        hot_paths.mark_profitable_pair(a, c, fee).await;
        let profitability = hot_paths.profitability_snapshot().await;

        prune_edges_by_quality(
            &mut graph,
            1,
            200,
            0.0,
            EdgeScoreWeights {
                liquidity: 1.0,
                profitability: 10.0,
                slippage: 0.1,
            },
            &profitability,
        )
        .await;

        let active_edges: Vec<&Edge> = graph.edges.iter().filter(|edge| edge.active).collect();
        assert_eq!(active_edges.len(), 1);
        assert_eq!(active_edges[0].to, c);
    }

    #[test]
    fn finalize_profit_threshold_preserves_flash_fee_buffer() {
        let base = U256::from(100u64);
        let competition_buffer = U256::from(50u64);
        let flash_fee = U256::from(1_000u64);
        let slippage_floor = U256::from(10u64);

        let threshold = finalize_profit_threshold(ProfitThresholdParams {
            base_threshold: base,
            competition_buffer,
            flash_fee_amount: flash_fee,
            slippage_floor,
            has_bridge_step: false,
            est_gross_after_fee: U256::from(5_000u64),
            cross_chain_profit_bps: 0,
            cross_chain_min_profit_wei: U256::zero(),
            backrun_hint: None,
        });

        assert_eq!(threshold, U256::from(1_150u64));
    }

    #[test]
    fn resolve_slippage_and_profit_floors_keeps_profit_floor_separate() {
        let (swap_slippage_bps, profit_floor_bps) = resolve_slippage_and_profit_floors(75, 120, 30);

        assert_eq!(swap_slippage_bps, 120);
        assert_eq!(profit_floor_bps, 30);
    }

    #[test]
    fn finalize_profit_threshold_applies_backrun_discount() {
        let base = U256::from(1_000u64);
        let competition_buffer = U256::from(100u64);
        let flash_fee = U256::from(100u64);
        let slippage_floor = U256::zero();
        let hint = BackrunHint {
            from: Address::zero(),
            to: Address::zero(),
            amount_in: U256::from(10_000u64),
            price_impact_bps: 500,
            source: "test".to_string(),
            observed_at: Instant::now(),
        };

        let threshold = finalize_profit_threshold(ProfitThresholdParams {
            base_threshold: base,
            competition_buffer,
            flash_fee_amount: flash_fee,
            slippage_floor,
            has_bridge_step: false,
            est_gross_after_fee: U256::from(5_000u64),
            cross_chain_profit_bps: 0,
            cross_chain_min_profit_wei: U256::zero(),
            backrun_hint: Some(&hint),
        });

        assert_eq!(threshold, U256::from(1_080u64));
    }

    #[test]
    fn reverted_receipt_reports_failure() {
        let expected_tx_hash = H256::from_low_u64_be(42);
        let receipt = TransactionReceipt {
            status: Some(U64::zero()),
            transaction_hash: expected_tx_hash,
            ..Default::default()
        };

        let outcome = classify_receipt(Some(receipt), H256::zero(), 7);

        match outcome {
            Err(ReceiptFailure {
                reason,
                tx_hash,
                edges,
            }) => {
                assert_eq!(tx_hash, expected_tx_hash);
                assert_eq!(edges, 7);
                assert!(
                    reason.contains("status 0"),
                    "expected revert reason to mention status 0, got {reason}"
                );
            }
            other => panic!("expected reverted receipt to produce failure, got {other:?}"),
        }
    }

    #[test]
    fn executor_op_ordering_matches_solidity() {
        assert_eq!(EXECUTOR_OP_UNIV3, 0);
        assert_eq!(EXECUTOR_OP_BALANCER, 1);
        assert_eq!(EXECUTOR_OP_GENERIC, 2);
        assert_eq!(EXECUTOR_OP_BRIDGE, 3);
        assert_eq!(EXECUTOR_OP_JIT_LP_ADD, 4);
        assert_eq!(EXECUTOR_OP_JIT_LP_REMOVE, 5);
    }

    #[test]
    fn dynamic_token_whitelist_expands_from_hot_pools() {
        let base_a = Address::from_low_u64_be(1);
        let base_b = Address::from_low_u64_be(2);
        let extra_c = Address::from_low_u64_be(3);
        let extra_d = Address::from_low_u64_be(4);

        let base = HashSet::from([base_a, base_b]);
        let hot_univ2 = vec![ResolvedUniV2PoolCfg {
            pair: Address::from_low_u64_be(10),
            token_in: base_a,
            token_out: extra_c,
            fee_bps: 30,
        }];
        let hot_univ3 = vec![PoolRecord {
            pool: Address::from_low_u64_be(11),
            token0: extra_c,
            token1: extra_d,
            fee: 500,
            created_block: 1,
        }];

        let mandatory = HashSet::new();
        let whitelist =
            build_dynamic_token_whitelist(&hot_univ2, &hot_univ3, &base, &mandatory, 60, 16);
        assert!(whitelist.contains(&base_a));
        assert!(whitelist.contains(&base_b));
        assert!(whitelist.contains(&extra_c));
        assert!(whitelist.contains(&extra_d));
    }

    #[test]
    fn dynamic_token_whitelist_respects_cap() {
        let base = HashSet::from([Address::from_low_u64_be(1)]);
        let hot_univ2 = vec![ResolvedUniV2PoolCfg {
            pair: Address::from_low_u64_be(10),
            token_in: Address::from_low_u64_be(2),
            token_out: Address::from_low_u64_be(3),
            fee_bps: 30,
        }];
        let hot_univ3 = vec![PoolRecord {
            pool: Address::from_low_u64_be(11),
            token0: Address::from_low_u64_be(4),
            token1: Address::from_low_u64_be(5),
            fee: 500,
            created_block: 1,
        }];

        let mandatory = HashSet::new();
        let whitelist =
            build_dynamic_token_whitelist(&hot_univ2, &hot_univ3, &base, &mandatory, 60, 2);
        assert_eq!(whitelist.len(), 2);
    }

    #[test]
    fn dynamic_token_whitelist_keeps_mandatory_tokens() {
        let mandatory = HashSet::from([Address::from_low_u64_be(777)]);
        let hot_univ2 = vec![ResolvedUniV2PoolCfg {
            pair: Address::from_low_u64_be(1),
            token_in: Address::from_low_u64_be(2),
            token_out: Address::from_low_u64_be(3),
            fee_bps: 30,
        }];
        let whitelist =
            build_dynamic_token_whitelist(&hot_univ2, &[], &HashSet::new(), &mandatory, 1, 1);
        assert!(whitelist.contains(&Address::from_low_u64_be(777)));
    }

    #[test]
    fn usd_stable_symbol_recognition_is_case_insensitive() {
        assert!(is_usd_stable_symbol("usdc"));
        assert!(is_usd_stable_symbol("USDT"));
        assert!(!is_usd_stable_symbol("WETH"));
    }

    #[tokio::test]
    async fn fork_negative_cycle_reconstruction_and_sizing_are_profitable() {
        if std::env::var("ARBOT_FORK_RPC_URL")
            .ok()
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            return;
        }

        let rpc = std::env::var("ARBOT_FORK_RPC_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_default();
        let provider = Arc::new(
            crate::util::connect_http_provider_with_fallbacks(
                "ethereum",
                &[rpc],
                Duration::from_secs(2),
            )
            .await
            .expect("fork RPC must be reachable"),
        );
        let block_number = provider
            .get_block_number()
            .await
            .expect("must fetch fork block number");

        let a = Address::from_low_u64_be(501);
        let b = Address::from_low_u64_be(502);
        let c = Address::from_low_u64_be(503);

        let mk_edge = |from, to, pool_id, reserve_in, reserve_out, weight| Edge {
            from,
            to,
            rate_num: reserve_out,
            rate_den: reserve_in,
            venue: VenueEdge::UniV2 {
                pair: Address::from_low_u64_be(pool_id),
                token_out: to,
                token0: from,
                token1: to,
                reserve_in,
                reserve_out,
                fee_bps: 30,
            },
            estimated_gas: 50_000,
            weight,
            max_input: U256::from(1_000_000u64),
            tolerance_bps: 10,
            observed_slippage_bps: 10,
            quote_block: Some(block_number),
            active: true,
        };

        let mut graph = Graph::default();
        graph.add_edge(mk_edge(
            a,
            b,
            600,
            U256::from(1_000_000u64),
            U256::from(1_050_000u64),
            -2,
        ));
        graph.add_edge(mk_edge(
            b,
            c,
            601,
            U256::from(1_000_000u64),
            U256::from(1_050_000u64),
            -2,
        ));
        graph.add_edge(mk_edge(
            c,
            a,
            602,
            U256::from(1_000_000u64),
            U256::from(1_020_000u64),
            -1,
        ));

        let priorities = HashMap::from([(a, 10i128), (b, 5i128), (c, 5i128)]);
        let limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 4,
            max_relaxations: 8,
            max_cycles: 4,
            timeout: Duration::from_millis(200),
        };

        let cycles = graph.bellman_ford(&priorities, &limits, 2, None);
        let cycle = cycles
            .iter()
            .find(|candidate| candidate.weight < 0)
            .expect("expected a negative cycle")
            .cycle
            .clone();

        let token_path: Vec<Address> = cycle.iter().map(|idx| graph.nodes[*idx]).collect();
        assert_eq!(token_path.first(), token_path.last());
        assert!(token_path.windows(2).any(|w| w[0] == a && w[1] == b));
        assert!(token_path.windows(2).any(|w| w[0] == b && w[1] == c));
        assert!(token_path.windows(2).any(|w| w[0] == c && w[1] == a));

        let cycle_edges: Vec<Edge> = cycle
            .windows(2)
            .filter_map(|window| {
                let from = graph.nodes[window[0]];
                let to = graph.nodes[window[1]];
                graph.edge_between(from, to).cloned()
            })
            .collect();
        assert_eq!(cycle_edges.len(), cycle.len().saturating_sub(1));

        let quotes = vec![FlashLoanQuote {
            provider: FlashLoanProvider::Balancer,
            max_amount: U256::from(1_000_000u64),
            fee_bps: 0,
            provider_addr: None,
        }];
        let quoter = UniQuoter::new(provider.clone(), Address::zero(), Address::zero());
        let bal_quote = BalQuote::new(provider.clone(), Address::zero());
        let curve_quote = CurveQuote::new(provider);

        let sizing = optimize_trade_size(OptimizeTradeParams {
            edges: &cycle_edges,
            quotes: &quotes,
            min_amount: U256::from(10_000u64),
            max_amount: U256::from(300_000u64),
            gas_price: U256::zero(),
            estimated_gas: 0,
            l1_data_fee: U256::zero(),
            native_price: NativePrice::unit(),
            quoter: &quoter,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            block_number,
        })
        .await
        .expect("expected non-zero profitable size");

        assert!(sizing.amount_in > U256::zero());
        assert!(sizing.net_after_fee_and_gas > U256::zero());
    }
}
