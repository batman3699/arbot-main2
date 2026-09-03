mod accounting;
mod backrun_state;
mod bridge;
mod capital;
mod chain;
mod continuity;
mod convex;
mod cl_math;
mod cl_sim;
mod cl_parity_gate;
mod cl_swap;
mod cl_ticks;
#[cfg(test)]
mod config_validation;
mod base_fast;
mod cycle_index;
mod discovery;
mod fees;
mod flash_loan;
mod graph;
mod health;
mod mempool;
mod hot_pools;
mod hot_path;
mod ingestion;
mod liquidations;
mod liquidity_cache;
mod live_state;
mod log_decode;
mod math;
mod metrics;
mod ops_inputs;
mod plan;
mod pool_store;
mod quote_balancer;
mod quote_cl;
mod quote_common;
mod quote_curve;
mod quote_solidly;
mod quote_univ2;
mod quote_univ3;
mod quote_slipstream;
mod quote_univ4;
mod reconcile;
mod registry;
mod risk_policy;
mod rpc_failover;
mod sandwich;
mod sim_quorum;
mod sim_revm;
mod sizing;
mod state_gate;
mod state_validation;
mod token_refresh;
mod util;
mod validation_select;
mod venue_adapter;
mod venues;

use accounting::Accounting;
use anyhow::{anyhow, ensure, Context, Result};
use capital::{CapitalManager, CapitalSnapshot};
use chain::{
    load_chain_from_sources, probe_aave_pool_interface, production_mode_enabled,
    secret_looks_placeholder, validate_chain_cfg, ChainCfg,
};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::{
    abi::Token,
    prelude::*,
    providers::{JsonRpcClient, ProviderError, Ws},
    types::{Address, BlockId, BlockNumber, Bytes, NameOrAddress, TxHash, H256, U256, U64},
};
use math::mul_div;
use plan::{build_plan_for_cycle, GenericPreAction, JitConfig, Plan, StepData};
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

use graph::{
    canonicalize_cycle, rotate_indexed_cycle, BellmanFordLimits, Edge, Graph,
    IndexedCycle, VenueEdge,
};
use hot_path::{HotPathCache, ProfitabilitySnapshot};
use hot_pools::{
    build_pinned_hub_pairs, hub_priority_order, log_hot_pool_refresh, rank_univ2_pools,
    rank_univ3_pools, HotPoolConfig, UniV3RankContext,
};
use ingestion::{
    block_head_channel, spawn_block_head_monitor, BlockHead, MonitoredPool,
    PoolMonitor,
};
use mempool::{spawn_live_mempool_monitor, spawn_mined_swap_monitor, BackrunHint, BackrunMonitor};
use pool_store::{
    load_pool_records, pool_data_path, prioritize_cold_pool_inventory, univ2_configs_from_records,
    PoolRecord, ResolvedUniV2PoolCfg,
};
use registry::{apply_pool_env_overrides, maybe_load_registry, parse_address, RegistryChain};
use serde::Serialize;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use venues::{
    populate_edges, solidly_monitored_pools, EdgeDigest, PopulateOptions,
};
use futures_util::{stream, stream::FuturesUnordered, StreamExt};

use crate::bridge::BridgePlanner;
use crate::discovery::{LowLiquidityPool, LowLiquidityScanner};
use crate::fees::{ArbitrumFeeConfig, FeeEstimate, FeeEstimator};
use crate::flash_loan::{FlashLoanProvider, FlashLoanQuote, FlashLoanSelection};
use crate::health::{HealthThresholds, HealthTracker};
use crate::liquidations::LiquidationMonitor;
use crate::liquidity_cache::PoolDepthCache;
use crate::metrics::Metrics;
use crate::ops_inputs::load_ops_inputs;
use crate::quote_slipstream::{default_slipstream_validation_base, SlipstreamQuoter};
use crate::quote_univ3::{
    default_pancakeswap_validation_base, UniQuoter, UniV3ValidationConfig, FEE_TIERS,
};
use crate::risk_policy::RuntimeRiskPolicy;
use crate::rpc_failover::FailoverClient;
use crate::sandwich::{SandwichMonitor, SandwichOpportunity};
use crate::sim_quorum::SimQuorum;
use crate::sim_revm::{
    record_revm_metrics, sim_revm_enabled, sim_revm_timeout_ms, simulate_via_revm, RevmSimOutcome,
    SimForkRequest,
};
use crate::backrun_state::{apply_post_state_hints, backrun_post_state_enabled, log_backrun_opportunity, targeted_bf_limits};
use crate::sizing::{optimize_trade_size, OptimizeTradeParams, SizingResult};
use crate::token_refresh::TokenList;
use crate::util::{
    CandidateDecisionLogger, CandidateDecisionRecord,
    coerce_http_url, coerce_ws_url, connect_ws_provider_with_fallbacks, erc20_decimals,
    parse_endpoint_list, u256_to_f64, NativePrice, TradeSizing,
};

use crate::{quote_balancer::BalQuote, quote_curve::CurveQuote};
use arb_exec::abi::{ExecutorLoan, ExecutorPlan, ExecutorStep, MultiVenueArbExecutor};

abigen!(
    BatchRouterAdmin,
    r#"[
        function owner() external view returns (address)
    ]"#
);

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
    fn from_env_for_chain(ops_inputs: &crate::ops_inputs::OpsInputs, chain_name: &str) -> Self {
        let backrun_default = ops_inputs.backrun_enabled_for(chain_name);
        Self {
            cycle_arb: read_feature_flag("FEATURE_CYCLE_ARB", true),
            backrun: read_feature_flag("FEATURE_BACKRUN", backrun_default),
            sandwich: read_feature_flag("FEATURE_SANDWICH", false),
            liquidations: read_feature_flag(
                "FEATURE_LIQUIDATIONS",
                ops_inputs.liquidations_enabled_for(chain_name),
            ),
            bridge: read_feature_flag("FEATURE_BRIDGE", false),
        }
    }
}

fn read_feature_flag(name: &str, default: bool) -> bool {
    crate::util::env_flag(name, default)
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

/// Slippage ceiling used when the executor preflight is skipped in shadow mode.
///
/// Normally this comes from the deployed executor's `getConfig()`. On a chain
/// with no executor there is nothing to read, so a shadow run needs a stand-in.
/// The default mirrors the value the deployed Base executor reports (150 bps),
/// which keeps shadow candidate filtering comparable to a live Base run rather
/// than accidentally permissive.
fn shadow_executor_max_slippage_bps() -> u32 {
    crate::util::env_parse_opt::<u32>("SHADOW_EXECUTOR_MAX_SLIPPAGE_BPS")
        .unwrap_or(150)
        .clamp(1, 10_000)
}

/// Decide how much a flash-loan provider may advertise, given what it can
/// actually lend. Extracted from `Runner::capacity_capped` so the policy is
/// testable without a live provider.
///
/// `None` withholds the provider. Unknown capacity with a configured allowlist
/// fails CLOSED: offering an unmeasured provider at full size is what let
/// Balancer win the 0-bps fee sort at amounts its vault could not fund.
fn capacity_capped_amount(
    available: Option<U256>,
    requested: U256,
    min_flash_loan: U256,
    allowlist_configured: bool,
) -> Option<U256> {
    match available {
        Some(available) => {
            let capped = requested.min(available);
            if capped < min_flash_loan {
                // Provider answered and simply cannot fund a viable trade. This
                // is a legitimate rejection, but it was SILENT: a chain where
                // every provider is short reads in the funnel exactly like a
                // chain with no arbitrage on it.
                tracing::debug!(
                    target: "flashcap",
                    %available,
                    %requested,
                    %min_flash_loan,
                    "capacity below min flash loan; provider cannot fund this cycle"
                );
                return None;
            }
            Some(capped)
        }
        // Capacity UNKNOWN and an allowlist is configured, so we fail closed.
        // Correct — lending blind risks a revert — but indistinguishable from
        // "no opportunity" unless it is logged. A stuck RPC or an unresolved
        // aToken silently disables a provider for as long as it persists, and
        // on Base that means falling back from 0 bps Balancer to 5 bps Aave, or
        // to nothing at all.
        None if allowlist_configured => {
            tracing::warn!(
                target: "flashcap",
                %requested,
                %min_flash_loan,
                "flash capacity UNKNOWN and allowlist configured; failing closed. \
                 If this persists, the provider balance read is broken (check the \
                 Aave aToken resolution, not the pool address balance) — it is not \
                 an absence of opportunity"
            );
            None
        }
        None => Some(requested),
    }
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
/// Upper bound on the L2 simulation budget, in ms. Override with
/// `ARBOT_L2_SIM_CEILING_MS` to fit measured simulation cost.
fn l2_simulation_ceiling_ms() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        crate::util::env_parse_opt::<u64>("ARBOT_L2_SIM_CEILING_MS")
            .unwrap_or(1_500)
            .clamp(100, 10_000)
    })
}

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
            search_ms.min(800),
            quoting_ms.min(350),
            // The L2 simulation ceiling was a bare `.min(400)`, so no config
            // value could ever raise it. Measured on Base, EVERY simulation
            // exceeded it (50/50 timed out at 400-402ms) — a budget that always
            // expires does not protect block cadence, it silently deletes the
            // simulation stage and with it any chance of verifying a trade.
            // Still capped, because this sits on the hot path, but the ceiling
            // is now tunable so it can be set from measured cost.
            simulation_ms.min(l2_simulation_ceiling_ms()),
        ),
        _ => (search_ms, quoting_ms, simulation_ms),
    }
}

fn univ3_quote_concurrency() -> usize {
    crate::util::env_parse_opt::<usize>("UNIV3_QUOTE_CONCURRENCY")
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

/// Aerodrome Slipstream pools are keyed by TICK SPACING, not fee tier, so the
/// UniV3 `FEE_TIERS` values are meaningless against that quoter. These are the
/// canonical Slipstream spacings on Base, cheapest/tightest first: 1 for
/// correlated pairs (stables, LSTs), 100/200 for majors, 2000 for volatile.
///
/// Used only for native-price discovery, where a miss is expensive: an
/// unpriceable start token is rejected before sizing, so it silently removes
/// every cycle beginning at that token.
const SLIPSTREAM_PRICE_TICK_SPACINGS: [u32; 5] = [1, 50, 100, 200, 2000];

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
            stable: false,
            kind: ingestion::PoolMonitorKind::UniV2,
        });
    }
    unique.into_values().collect()
}

fn load_token_decimals_map(ops_inputs: &crate::ops_inputs::OpsInputs) -> HashMap<Address, u8> {
    let mut decimals = HashMap::new();
    for seed in ops_inputs.universe.token_seeds.iter() {
        let Ok(addr) = Address::from_str(&seed.address) else {
            continue;
        };
        if let Some(value) = seed.decimals {
            decimals.insert(addr, value);
        }
    }
    decimals
}

fn build_univ3_rank_context(
    ops_inputs: &crate::ops_inputs::OpsInputs,
    env_prefix: &str,
) -> UniV3RankContext {
    let mut hub_tokens = Vec::new();
    for addr in &ops_inputs.universe.hub_tokens {
        if let Ok(parsed) = Address::from_str(addr) {
            hub_tokens.push(parsed);
        }
    }
    let hub_priority = hub_priority_order(&hub_tokens);
    let pinned_pairs = build_pinned_hub_pairs(&hub_tokens);
    let native_usd = native_usd_price(env_prefix).unwrap_or(2500.0);
    let mut token_decimals = HashMap::new();
    let mut hub_usd_prices = HashMap::new();
    for seed in ops_inputs.universe.token_seeds.iter() {
        let Ok(addr) = Address::from_str(&seed.address) else {
            continue;
        };
        if let Some(decimals) = seed.decimals {
            token_decimals.insert(addr, decimals);
        }
        let symbol = seed.symbol.as_deref().unwrap_or("").to_ascii_uppercase();
        let price = match symbol.as_str() {
            "WETH" | "CBETH" | "WSTETH" => Some(native_usd),
            "USDC" | "USDBC" | "DAI" | "USDT" | "EURC" => Some(1.0),
            "CBBTC" | "WBTC" => crate::util::env_parse_opt::<f64>("RANK_CBTC_USD")
                .or(Some(95_000.0)),
            "AERO" => crate::util::env_parse_opt::<f64>("RANK_AERO_USD")
                .or(Some(0.35)),
            _ => None,
        };
        if let Some(price) = price {
            hub_usd_prices.insert(addr, price);
        }
    }
    UniV3RankContext {
        hub_tokens: hub_priority,
        hub_usd_prices,
        token_decimals,
        pinned_pairs,
    }
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

/// Total fee owed to traverse a cycle, in bps, summed across hops.
///
/// This is the bar every candidate must clear before it can earn anything, and
/// nothing in the pipeline used to compute it. On Base it dominates: 53% of the
/// UniV3 inventory is the 100bps tier and 27% is 30bps, so a 3-hop cycle
/// routinely owes 90-300bps up front. Measured across five shadow runs, the best
/// candidate sat ~99.5bps underwater — the fee stack of exactly such a path, not
/// a lost race. Venues whose edges carry no explicit fee (bridge, liquidation)
/// contribute nothing here and are priced downstream.
fn cycle_fee_stack_bps(edges: &[Edge]) -> u32 {
    edges
        .iter()
        .map(|edge| match &edge.venue {
            VenueEdge::UniV3 { fee, .. } => fee / 100,
            VenueEdge::UniV2 { fee_bps, .. } => *fee_bps,
            VenueEdge::SolidlyV2 { fee_bps, .. } => *fee_bps,
            // Fee not carried on the edge (Slipstream is tick-spacing based and
            // charges a dynamic per-pool fee; Curve/Balancer/UniV4 encode theirs
            // in pool state). Count them as zero so an unknown fee can only make
            // this prune more permissive, never wrongly discard a live cycle.
            VenueEdge::Slipstream { .. }
            | VenueEdge::Curve { .. }
            | VenueEdge::Balancer { .. }
            | VenueEdge::Univ4 { .. }
            | VenueEdge::Bridge { .. }
            | VenueEdge::Liquidation { .. } => 0,
        })
        .sum()
}

/// Ceiling on the fee stack a cycle may carry, in bps. Above this the path
/// cannot realistically clear its own cost plus gas, so quoting it is wasted
/// budget on the hot path. `ARBOT_MAX_CYCLE_FEE_BPS=0` disables the prune.
/// Explicit start-token restriction from `ARBOT_START_TOKENS`, or `None` when
/// unset (every fundable token is allowed).
///
/// Parsed once. An entry that is not a valid address is ignored rather than
/// failing the process, but a list that parses to nothing yields `None` so a
/// typo widens the universe rather than silently halting all scanning.
fn start_token_allowlist() -> Option<HashSet<Address>> {
    static V: std::sync::OnceLock<Option<HashSet<Address>>> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let raw = std::env::var("ARBOT_START_TOKENS").ok()?;
        let set: HashSet<Address> = raw
            .split(',')
            .filter_map(|s| s.trim().parse::<Address>().ok())
            .collect();
        if set.is_empty() {
            warn!(
                target: "flashcap",
                raw = %raw,
                "ARBOT_START_TOKENS parsed to no valid addresses; ignoring the restriction"
            );
            return None;
        }
        Some(set)
    })
    .clone()
}

fn max_cycle_fee_bps() -> u32 {
    static V: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *V.get_or_init(|| crate::util::env_parse_opt::<u32>("ARBOT_MAX_CYCLE_FEE_BPS").unwrap_or(60))
}

/// Encode planner steps into executor calldata.
///
/// Extracted so the SAME encoding can produce both the dispatchable plan and
/// the floor-free diagnostic variant. Re-implementing it for the diagnostic
/// would let the two drift, and a diagnostic that encodes differently from the
/// real plan measures the wrong thing.
fn encode_plan_steps(steps: Vec<StepData>) -> Vec<ExecutorStep> {
    let mut ops: Vec<ExecutorStep> = Vec::with_capacity(steps.len());
    for step in steps {
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
    ops
}
fn pool_liquidity_weight(pool: &PoolRecord, rank_index: usize, total: usize) -> u64 {
    pool.hub_usd_liquidity
        .map(|usd| ((usd.max(1.0)) as u64).saturating_mul(1_000))
        .unwrap_or_else(|| {
            total
                .saturating_sub(rank_index)
                .max(1) as u64
        })
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
    let univ3_total = hot_univ3.len();
    for (idx, pool) in hot_univ3.iter().enumerate() {
        let weight = pool_liquidity_weight(pool, idx, univ3_total);
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

fn scan_idle_sleep_ms() -> u64 {
    crate::util::env_parse_opt::<u64>("ARBOT_SCAN_IDLE_SLEEP_MS")
        .unwrap_or(50)
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
        if is_pancakeswap_univ3_venue(&venue.name) {
            continue;
        }
        if let Some(venue_tiers) = venue.fee_tiers.as_ref() {
            tiers.extend(venue_tiers.iter().copied());
        }
    }
    tiers
}

fn collect_slipstream_tick_spacings(
    ops_inputs: &crate::ops_inputs::OpsInputs,
    chain_name: &str,
) -> HashSet<u32> {
    let mut spacings = HashSet::new();
    let Some(chain) = ops_inputs.chain_inputs(chain_name) else {
        return spacings;
    };
    for venue in &chain.venues {
        if !matches!(
            venue.kind,
            Some(crate::ops_inputs::VenueKind::SlipstreamLike)
        ) {
            continue;
        }
        if let Some(tiers) = venue.fee_tiers.as_ref() {
            spacings.extend(tiers.iter().copied());
        }
    }
    spacings
}

/// A venue's own router, looked up by NAME.
///
/// Venue KIND is not enough and grouping by hot-list is actively wrong. Base
/// configures aerodrome_slipstream_v3 as `univ3_like`, so its pools arrive in
/// the same hot list as Uniswap's -- and tagging that whole list with the
/// Uniswap router sent tick-spacing values to a quoter that reads them as fee
/// tiers. Measured 2026-09-03: pool 0x42d4a22c carries `fee: 10`, which is a
/// spacing; Uniswap V3 has no fee tier 10, and the quoter answered
/// `execution reverted` for every one of 194 attempts.
fn venue_router_by_name(
    ops_inputs: &crate::ops_inputs::OpsInputs,
    chain_name: &str,
    venue_name: &str,
) -> Option<Address> {
    let chain = ops_inputs.chain_inputs(chain_name)?;
    let venue = chain
        .venues
        .iter()
        .find(|v| v.name.eq_ignore_ascii_case(venue_name))?;
    venue.router.as_ref()?.parse().ok()
}

fn resolve_slipstream_venue(
    ops_inputs: &crate::ops_inputs::OpsInputs,
    chain_name: &str,
) -> Option<(Address, Address, Address)> {
    let chain = ops_inputs.chain_inputs(chain_name)?;
    let venue = chain.venues.iter().find(|venue| {
        matches!(
            venue.kind,
            Some(crate::ops_inputs::VenueKind::SlipstreamLike)
        )
    })?;
    let factory = venue.factory.as_ref()?.parse().ok()?;
    let router = venue.router.as_ref()?.parse().ok()?;
    let quoter = venue.quoter.as_ref()?.parse().ok()?;
    Some((factory, router, quoter))
}

const PANCAKESWAP_V3_VENUE: &str = "pancakeswap_v3";

fn is_pancakeswap_univ3_venue(name: &str) -> bool {
    name.eq_ignore_ascii_case(PANCAKESWAP_V3_VENUE)
}

fn resolve_pancakeswap_venue(
    ops_inputs: &crate::ops_inputs::OpsInputs,
    chain_name: &str,
) -> Option<(Address, Address, Address)> {
    let chain = ops_inputs.chain_inputs(chain_name)?;
    let venue = chain
        .venues
        .iter()
        .find(|venue| is_pancakeswap_univ3_venue(&venue.name))?;
    let factory = venue.factory.as_ref()?.parse().ok()?;
    let router = venue.router.as_ref()?.parse().ok()?;
    let quoter = venue.quoter.as_ref()?.parse().ok()?;
    Some((factory, router, quoter))
}

fn collect_pancakeswap_fee_tiers(
    ops_inputs: &crate::ops_inputs::OpsInputs,
    chain_name: &str,
) -> HashSet<u32> {
    let mut tiers = HashSet::new();
    let Some(chain) = ops_inputs.chain_inputs(chain_name) else {
        return tiers;
    };
    for venue in &chain.venues {
        if !is_pancakeswap_univ3_venue(&venue.name) {
            continue;
        }
        if let Some(venue_tiers) = venue.fee_tiers.as_ref() {
            tiers.extend(venue_tiers.iter().copied());
        }
    }
    tiers
}

fn filter_cycles_by_hubs(
    cycles: Vec<IndexedCycle>,
    graph: &Graph,
    hub_tokens: &HashSet<Address>,
) -> Vec<IndexedCycle> {
    let strict_intermediates = read_feature_flag("STRICT_HUB_INTERMEDIATES", false);
    let strict_start = read_feature_flag("STRICT_START_TOKEN_HUB_ONLY", false);
    if hub_tokens.is_empty() && !strict_start {
        return cycles;
    }
    cycles
        .into_iter()
        .filter(|indexed| {
            let cycle = &indexed.cycle;
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

#[derive(Clone, Debug)]
/// A cycle carried across scans as its ROUTE only.
///
/// Deliberately does not store edge indices: those are positions in a specific
/// `Graph::edges` and are invalid the moment the graph is rebuilt. The hop edges
/// are re-resolved from `addresses` on each scan.
struct CycleSeed {
    addresses: Vec<Address>,
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
        VenueEdge::Slipstream { path, .. } => {
            if path.len() < 2 {
                return None;
            }
            path.iter()
                .skip(1)
                .find_map(|(_, maybe_spacing)| maybe_spacing.as_ref().copied())
        }
        _ => None,
    }
}

/// Outcome of probing one token's native price.
///
/// `NoRoute` and `Unknown` must stay distinct all the way to the cache. Only
/// `NoRoute` is a statement about the chain; `Unknown` is a statement about our
/// connectivity, and recording it as a price verdict is what let a provider
/// rate-limit blind the engine to a token for the whole cache TTL — rejecting
/// every cycle that started there, with no log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativePriceProbe {
    /// The quoter returned a usable price.
    Priced(NativePrice),
    /// Every fee tier executed and reverted: genuinely no route to native.
    NoRoute,
    /// At least one tier failed for a non-execution reason (rate limit,
    /// timeout, dead endpoint). The price is unknown; do not cache.
    Unknown,
}

impl NativePriceProbe {
    /// Stable metric label. Kept next to the variants so a new variant cannot
    /// silently inherit another's label.
    fn label(&self) -> &'static str {
        match self {
            NativePriceProbe::Priced(_) => "priced",
            NativePriceProbe::NoRoute => "no_route",
            NativePriceProbe::Unknown => "unknown",
        }
    }

    /// What this probe may be written into the price cache as, if anything.
    ///
    /// `None` means "do not touch the cache" — the load-bearing case. Returning
    /// an unreliable price here instead of `None` is precisely the regression
    /// that caused the zero-fill outage, so this is asserted in tests.
    fn cache_entry(&self) -> Option<NativePrice> {
        match self {
            NativePriceProbe::Priced(price) => Some(*price),
            NativePriceProbe::NoRoute => {
                Some(NativePrice::new(U256::zero(), U256::zero(), false))
            }
            NativePriceProbe::Unknown => None,
        }
    }
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

#[allow(dead_code)]
fn should_reuse_cached_native_price(
    token: Address,
    wrapped_native: Address,
    native_price: NativePrice,
) -> bool {
    token == wrapped_native || native_price.is_reliable()
}

#[allow(dead_code)]
fn should_cache_native_price(
    token: Address,
    wrapped_native: Address,
    native_price: NativePrice,
) -> bool {
    token == wrapped_native || native_price.is_reliable()
}

/// How long a token's native (WETH-denominated) price is reused before being
/// re-quoted. Caching every verdict — including "unreliable" — for this window
/// is what stops the per-scan re-quote of unpriceable tokens (the dominant
/// native-price latency), while keeping prices fresh enough for the
/// profit-threshold conversion. Configurable via NATIVE_PRICE_TTL_SECS.
fn native_price_cache_ttl() -> Duration {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let secs = *V.get_or_init(|| {
        std::env::var("NATIVE_PRICE_TTL_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(120)
            .clamp(5, 3600)
    });
    Duration::from_secs(secs)
}

/// Bounded concurrency for refreshing native (token->WETH) prices. The TTL
/// refresh re-quotes every universe token at once; doing it sequentially was a
/// ~20s blind spike every TTL window, so fan it out.
fn native_price_concurrency() -> usize {
    std::env::var("NATIVE_PRICE_CONCURRENCY")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(32)
        .clamp(1, 128)
}

fn cap_cycles_per_start(
    cycles: Vec<IndexedCycle>,
    graph: &Graph,
    topk_per_token: usize,
) -> Vec<IndexedCycle> {
    if topk_per_token == 0 {
        return Vec::new();
    }
    let mut per_start: HashMap<Address, Vec<(i64, IndexedCycle)>> = HashMap::new();
    for indexed in cycles {
        let Some(start_ix) = indexed.cycle.first() else {
            continue;
        };
        let Some(start_token) = graph.nodes.get(*start_ix) else {
            continue;
        };
        let weight = if indexed.edge_indices_valid() {
            graph
                .cycle_weight_from_edge_indices(&indexed.edge_indices)
                .unwrap_or(i64::MAX)
        } else {
            graph.cycle_weight(&indexed.cycle).unwrap_or(i64::MAX)
        };
        per_start
            .entry(*start_token)
            .or_default()
            .push((weight, indexed));
    }
    let mut capped = Vec::new();
    for entries in per_start.values_mut() {
        entries.sort_by_key(|e| e.0);
        for (_, indexed) in entries.drain(..).take(topk_per_token) {
            capped.push(indexed);
        }
    }
    capped
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
    hub_tokens: &HashSet<Address>,
) {
    if max_edges == 0 || graph.edges.len() <= max_edges {
        return;
    }

    let mut heap: BinaryHeap<Reverse<ScoredEdge>> = BinaryHeap::with_capacity(max_edges + 1);
    let mut pinned_hub_edges = HashSet::new();
    for (idx, edge) in graph.edges.iter().enumerate() {
        if !edge.active {
            continue;
        }
        if matches!(edge.venue, VenueEdge::SolidlyV2 { .. })
            && hub_tokens.contains(&edge.from)
            && hub_tokens.contains(&edge.to)
        {
            pinned_hub_edges.insert(idx);
        }
    }
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

    let mut keep = pinned_hub_edges;
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

/// Default number of candidate cycles sized concurrently per scan. Sizing is
/// RPC-bound (quote grids), so modest parallelism multiplies how many
/// candidates fit inside the quote budget without saturating the endpoint.
const DEFAULT_CANDIDATE_PREP_CONCURRENCY: usize = 4;

fn candidate_prep_concurrency() -> usize {
    crate::util::env_parse_opt::<usize>("ARBOT_CANDIDATE_CONCURRENCY")
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_CANDIDATE_PREP_CONCURRENCY)
}

/// How many ranked candidates to attempt at simulation when the top choice
/// fails eth_call, quorum, or post-sim profit gates.
const DEFAULT_SIM_CASCADE_DEPTH: usize = 3;

fn sim_cascade_depth() -> usize {
    crate::util::env_parse_opt::<usize>("ARBOT_SIM_CASCADE_DEPTH")
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SIM_CASCADE_DEPTH)
}

/// Shared read-only inputs for concurrent candidate preparation.
/// Everything the Base fast path's drain needs, held until the runner exists.
///
/// Declared out here because the drain reads the runner's published snapshots
/// and the monitor is built first.
struct BaseFastDrain {
    fast: std::sync::Arc<crate::base_fast::BaseFastPath>,
    universe: std::sync::Arc<crate::cycle_index::PoolUniverse>,
    /// Candidate cycle starts, BEFORE the flash-fundability filter. Filtered
    /// once the runner exists, because only it knows the loan allowlists.
    starts: Vec<Address>,
    max_cycles: usize,
    venues: std::sync::Arc<HashMap<Address, crate::base_fast::FastVenue>>,
}

/// An owned `CandidatePrepCtx`, published once per scan for readers that run
/// between scans.
///
/// Everything in it is slow-moving relative to a 200ms flashblock -- native
/// prices, gas parameters, capital and competition all come from the same
/// per-scan RPC work -- so a reader gets numbers up to one scan old. That is
/// acceptable for SIZING, which is what these drive. It is not acceptable for
/// prices, which is why the fast path reads its own live state for those and
/// only borrows this for the economics.
///
/// `block_number` is the exception worth watching: it is the sealed block the
/// scan saw, so a fast-path candidate prepared against it is judged against a
/// block that may already be one or two old.
#[derive(Clone)]
struct PrepContextSnapshot {
    native_prices_map: Arc<HashMap<Address, NativePrice>>,
    base_profiles_map: Arc<HashMap<Address, TradeSizing>>,
    capital_snapshot: CapitalSnapshot,
    competition_snapshot: CompetitionSnapshot,
    gas_parameters: FeeEstimate,
    executor_address: Address,
    block_number: U64,
}

struct CandidatePrepCtx<'a> {
    native_prices_map: &'a HashMap<Address, NativePrice>,
    base_profiles_map: &'a HashMap<Address, TradeSizing>,
    capital_snapshot: &'a CapitalSnapshot,
    competition_snapshot: &'a CompetitionSnapshot,
    gas_parameters: &'a FeeEstimate,
    executor_address: Address,
    block_number: U64,
    edges_scanned: usize,
}

/// Result of the RPC-heavy candidate preparation stage (validation, flash
/// quotes, sizing grid, plan construction). Rejections have already emitted
/// their candidate-stage logs; the caller only consumes the skip detail.
enum CandidatePrep {
    /// Quote budget elapsed before this candidate could start sizing.
    Budgeted,
    Rejected {
        skip_detail: Option<String>,
    },
    Sized(Box<SizedCandidate>),
}

struct SizedCandidate {
    cycle_ix: Vec<usize>,
    candidate_id: String,
    cycle_start: Address,
    native_price: NativePrice,
    pricing_reliable: bool,
    competition_buffer: U256,
    cycle_latency_secs: f64,
    cycle_edges_vec: Vec<Edge>,
    has_bridge_step: bool,
    backrun_hint: Option<BackrunHint>,
    adjusted_cycle_gas: u64,
    sizing: SizingResult,
    plan: Plan,
    trade_amount: U256,
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

/// Inputs to the unified pre-/post-simulation profit threshold. Both gates
/// must be fed through `unified_min_profit_threshold` so they cannot drift.
struct UnifiedThresholdParams<'a> {
    fee: &'a FeeEstimate,
    est_gas: u64,
    est_gross_after_fee: U256,
    flash_fee_amount: U256,
    slippage_floor: U256,
    competition_buffer: U256,
    has_bridge_step: bool,
    backrun_hint: Option<&'a BackrunHint>,
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

fn parallel_private_relay_blast_enabled() -> bool {
    std::env::var("ARBOT_RELAY_PARALLEL_BLAST")
        .map(|raw| !matches!(raw.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true)
}

/// Blast the same signed bundle to every healthy relay against one target block,
/// then race inclusion polls. Avoids serial inclusion timeouts on relay #1 while
/// relay #2+ would still be viable on the same block.
async fn parallel_private_relay_blast<M>(
    client: Arc<M>,
    raw: Bytes,
    target_bundle_block: U64,
    relay_candidates: Vec<(usize, RelayEndpoint)>,
    inclusion_timeout: Duration,
    relay_health: Arc<StdMutex<HealthTracker>>,
    chaos: ChaosConfig,
) -> Result<(TxHash, TransactionReceipt, usize, PrivateSubmissionMethod), Vec<String>>
where
    M: Middleware + Send + Sync + 'static,
{
    let mut send_jobs = FuturesUnordered::new();
    for (index, relay) in relay_candidates {
        let chaos = chaos.clone();
        if chaos.should_reject_relay() {
            continue;
        }
        let raw = raw.clone();
        let target = target_bundle_block;
        let chain_name = relay.chain_name.clone();
        let allow_fallback = relay.allow_private_raw_fallback;
        let provider = relay.provider.clone();
        let label = relay.label().to_string();
        send_jobs.push(async move {
            let started = Instant::now();
            match provider
                .send_bundle_transaction(raw, target, &chain_name, allow_fallback)
                .await
            {
                Ok((tx_hash, method)) => Ok((index, label, tx_hash, method, started)),
                Err(err) => Err((label, err.to_string(), started)),
            }
        });
    }

    let mut accepted: Vec<(usize, String, TxHash, PrivateSubmissionMethod, Instant)> =
        Vec::new();
    let mut relay_errors: Vec<String> = Vec::new();
    while let Some(result) = send_jobs.next().await {
        match result {
            Ok(entry) => accepted.push(entry),
            Err((label, err, started)) => {
                lock_unpoison(relay_health.as_ref()).record_failure(
                    &label,
                    true,
                    Some(started.elapsed()),
                );
                relay_errors.push(format!("{label}: {err}"));
            }
        }
    }

    if accepted.is_empty() {
        return Err(relay_errors);
    }

    info!(
        target: "broadcast",
        relay_count = accepted.len(),
        target_block = %target_bundle_block,
        "Parallel private relay blast accepted submissions"
    );

    let mut inclusion_race = FuturesUnordered::new();
    for (index, label, tx_hash, method, send_started) in accepted {
        let client = client.clone();
        let relay_health = Arc::clone(&relay_health);
        inclusion_race.push(async move {
            let poll = async {
                loop {
                    match client.get_transaction_receipt(tx_hash).await {
                        Ok(Some(receipt)) => break Ok(receipt),
                        Ok(None) => {
                            sleep(Duration::from_millis(200)).await;
                        }
                        Err(err) => break Err(err),
                    }
                }
            };
            match timeout(inclusion_timeout, poll).await {
                Ok(Ok(receipt)) => {
                    lock_unpoison(relay_health.as_ref()).record_success(
                        &label,
                        Some(send_started.elapsed()),
                    );
                    Ok((tx_hash, receipt, index, method))
                }
                Ok(Err(err)) => {
                    lock_unpoison(relay_health.as_ref()).record_failure(
                        &label,
                        true,
                        Some(send_started.elapsed()),
                    );
                    Err(format!("{label}: {err}"))
                }
                Err(_) => {
                    lock_unpoison(relay_health.as_ref()).record_failure(
                        &label,
                        true,
                        Some(send_started.elapsed()),
                    );
                    Err(format!("{label}: timeout"))
                }
            }
        });
    }

    while let Some(result) = inclusion_race.next().await {
        match result {
            Ok(success) => return Ok(success),
            Err(err) => relay_errors.push(err),
        }
    }

    Err(relay_errors)
}

/// Runtime kill-switch for the private raw-transaction fallback. Lets ops
/// force bundle-only submission without editing ops/inputs.yaml.
fn private_raw_fallback_disabled() -> bool {
    read_feature_flag("ARBOT_DISABLE_PRIVATE_RAW_FALLBACK", false)
}

/// Only relay errors that mean "this endpoint does not implement
/// eth_sendBundle" may trigger the raw fallback. Content rejections (bundle
/// validation, reverts, rate limits) must NOT leak the transaction through
/// eth_sendRawTransaction, which lacks bundle atomicity/revert protection.
fn bundle_method_unsupported(err: &ProviderError) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    // JSON-RPC standard "method not found" code.
    if text.contains("-32601") {
        return true;
    }
    // Geth-style: "the method eth_sendBundle does not exist/is not available";
    // require the word "method" so content rejections mentioning e.g.
    // "pool does not exist" can never unlock the raw fallback.
    text.contains("method")
        && (text.contains("not found")
            || text.contains("not supported")
            || text.contains("unsupported")
            || text.contains("does not exist")
            || text.contains("not available"))
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
    let allow_fallback = allow_private_raw_fallback && !private_raw_fallback_disabled();
    match bundle_attempt {
        Ok(_) => Ok((tx_hash, PrivateSubmissionMethod::Bundle)),
        Err(err) if allow_fallback && bundle_method_unsupported(&err) => {
            warn!(
                target: "broadcast",
                chain = chain_name,
                error = %err,
                "eth_sendBundle unsupported on private RPC relay; falling back to private eth_sendRawTransaction (no bundle revert protection)"
            );
            provider
                .request::<serde_json::Value, serde_json::Value>(
                    "eth_sendRawTransaction",
                    serde_json::json!([format!("0x{}", hex::encode(&raw))]),
                )
                .await?;
            Ok((tx_hash, PrivateSubmissionMethod::PrivateRaw))
        }
        Err(err) => {
            if allow_private_raw_fallback && !allow_fallback {
                warn!(
                    target: "broadcast",
                    chain = chain_name,
                    "private raw fallback suppressed by ARBOT_DISABLE_PRIVATE_RAW_FALLBACK"
                );
            }
            Err(err)
        }
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
    /// Fraction (bps) of a candidate's expected NET profit that may be bid as
    /// priority fee / builder tip to win inclusion. 0 disables profit-aware
    /// bidding (static role fee only). Builders order by effective priority fee,
    /// so a flat 1-2 gwei tip loses every contested opportunity; bidding a share
    /// of profit is how searchers actually win blockspace while keeping margin.
    bid_profit_fraction_bps: u32,
}

impl BroadcastConfig {
    fn priority_fee(&self) -> Option<U256> {
        match self.role {
            MevRole::Searcher => self.searcher_priority_fee,
            MevRole::Filler => self.filler_priority_fee,
        }
    }

    /// Compute the priority fee (per gas) to bid for an opportunity.
    ///
    /// `net_profit_native` is the expected net profit in native wei AFTER the
    /// baseline gas cost already counted by the profit gate. We may spend up to
    /// `bid_profit_fraction_bps` of it as ADDITIONAL tip on top of the static
    /// floor, so the trade always retains at least `(1 - fraction)` of profit.
    /// `max_fee_per_gas_cap` (risk policy) and `base_fee` bound the result so
    /// `base_fee + priority` never exceeds the declared per-gas ceiling.
    fn competitive_priority_fee(
        &self,
        net_profit_native: Option<U256>,
        gas_units: u64,
        base_fee: Option<U256>,
        max_fee_per_gas_cap: Option<U256>,
    ) -> Option<U256> {
        let floor = self.priority_fee();
        let extra = match (net_profit_native, self.bid_profit_fraction_bps) {
            (Some(profit), fraction) if fraction > 0 && gas_units > 0 && !profit.is_zero() => {
                let budget = mul_div(profit, U256::from(fraction), U256::from(10_000u64));
                budget / U256::from(gas_units)
            }
            _ => U256::zero(),
        };

        let mut priority = match floor {
            Some(floor) => floor.saturating_add(extra),
            None if extra.is_zero() => return None,
            None => extra,
        };

        // The risk-policy fee ceiling is a hard safety limit and wins even over
        // the static floor: base_fee + priority must never breach the cap.
        if let Some(cap) = max_fee_per_gas_cap {
            let headroom = cap.saturating_sub(base_fee.unwrap_or_default());
            if priority > headroom {
                priority = headroom;
            }
        }

        Some(priority)
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

/// Apply fee AND gas-limit parameters from a `FeeEstimate` to a transaction.
///
/// The gas limit used to be dropped on the floor: only the fee fields were
/// applied, so the tx reached simulation with no `gas` set. The primary provider
/// tolerated that (it substitutes a default), but every quorum verifier rejected
/// it outright with `intrinsic gas too low` — so cross-endpoint verification
/// could never confirm a single simulation, and the safety check it represents
/// was silently inert while still costing a round trip per endpoint.
fn apply_gas_parameters(tx: &mut TypedTransaction, gas: &FeeEstimate) {
    // Zero would be worse than absent: it guarantees an intrinsic-gas failure
    // instead of letting the node fall back to its own default.
    if !gas.gas_limit.is_zero() {
        tx.set_gas(gas.gas_limit);
    }
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

/// Worst-case transactions the wallet must be able to fund before LIVE dispatch
/// is allowed. `0` removes the floor.
fn gas_reserve_txs() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| crate::util::env_parse_opt::<u64>("ARBOT_GAS_RESERVE_TXS").unwrap_or(20))
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
    _max_backoff: Duration,
    health: Arc<StdMutex<HealthTracker>>,
) -> Result<(Provider<FailoverClient>, String)> {
    if endpoints.is_empty() {
        return Err(anyhow!("no http endpoints configured for {label}"));
    }

    // Build ONE self-healing transport over every configured endpoint. The
    // failover client retries/rotates per request at runtime, so we no longer
    // pin to a single URL that can fail permanently.
    let failover = FailoverClient::new(endpoints)
        .with_context(|| format!("build failover rpc client for {label}"))?;
    let endpoint_label = failover.endpoint_label();
    let endpoint_count = failover.endpoint_count();
    let provider = Provider::new(failover);

    // Best-effort boot probe: confirm at least one endpoint answers, but proceed
    // regardless because runtime failover + the fail-closed scan guard handle
    // ongoing/transient outages without trading on stale state.
    let start = Instant::now();
    match provider.get_block_number().await {
        Ok(head) => {
            lock_unpoison(health.as_ref()).record_success(&endpoint_label, Some(start.elapsed()));
            info!(
                target: "rpc",
                %label,
                endpoints = %endpoint_label,
                endpoint_count,
                head = %head,
                "failover http rpc connected"
            );
        }
        Err(err) => {
            lock_unpoison(health.as_ref()).record_failure(&endpoint_label, true, Some(start.elapsed()));
            warn!(
                target: "rpc",
                %label,
                endpoints = %endpoint_label,
                error = %err,
                "failover http rpc boot probe failed; proceeding (runtime failover will retry)"
            );
        }
    }

    Ok((provider, endpoint_label))
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
                // A relay with no known name falls back to its URL — which on
                // Base is the sequencer RPC, carrying an embedded provider key.
                // This label is logged AND stored on RelayEndpoint, from where
                // it reaches trade JSON and execution metrics, so it must be
                // redacted at the point of construction rather than at each use.
                .unwrap_or_else(|| crate::util::redact_endpoint(endpoint));
            let safe_endpoint = crate::util::redact_endpoint(endpoint);
            info!(
                target: "broadcast",
                endpoint = %safe_endpoint,
                relay = %relay_label,
                "Connecting private relay endpoint"
            );

            match PrivateRelayProvider::connect(endpoint, signer).await {
                Ok(provider) => {
                    info!(
                        target: "broadcast",
                        endpoint = %safe_endpoint,
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
                        endpoint = %safe_endpoint,
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
    // Health-based triggers (plan.md: abnormal revert rate / RPC lag).
    revert_window: Mutex<VecDeque<(Instant, bool)>>,
    revert_window_dur: Duration,
    revert_rate_limit: f64,
    revert_min_samples: usize,
    rpc_errors: Mutex<VecDeque<Instant>>,
    rpc_error_window: Duration,
    rpc_error_limit: usize,
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
            // Calibrated for a BACKRUNNING workload, where a 55-65% revert rate
            // is the normal steady state, not a fault. Losing the race and
            // reverting is the executor working correctly — it costs gas instead
            // of filling at a loss — so the breaker must tolerate that band and
            // still catch a genuinely broken deploy.
            //
            // The previous defaults were not merely tight, they were INVERTED:
            // a 0.5 rate limit sits BELOW the expected 0.55-0.65 band, so normal
            // operation tripped the breaker permanently.
            //
            // rate limit 0.90 with 50 samples: at p=0.65 the sampling sigma is
            // sqrt(0.65*0.35/50) = 0.0675, so 0.90 is 3.7 sigma out — a false
            // trip roughly once in 9,000 windows. A broken deploy reverting
            // 100% trips as soon as 50 samples accumulate.
            //
            // A 1800s window (was 600s) is needed for 50 samples to accumulate
            // at realistic fill rates; without that the rate trigger would never
            // arm, which is a fail-OPEN. Fast detection is the consecutive-
            // failure trigger's job, not this one's.
            revert_window: Mutex::new(VecDeque::new()),
            revert_window_dur: Duration::from_secs(1800),
            revert_rate_limit: 0.90,
            revert_min_samples: 50,
            rpc_errors: Mutex::new(VecDeque::new()),
            rpc_error_window: Duration::from_secs(120),
            rpc_error_limit: 30,
        }
    }

    /// Override health-trigger thresholds from environment (production only).
    fn configure_health_from_env(mut self) -> Self {
        if let Some(v) = crate::util::env_parse_opt::<f64>("CB_REVERT_RATE_LIMIT")
        {
            self.revert_rate_limit = v;
        }
        if let Some(v) = crate::util::env_parse_opt::<usize>("CB_REVERT_MIN_SAMPLES")
        {
            self.revert_min_samples = v.max(1);
        }
        if let Some(v) = crate::util::env_parse_opt::<u64>("CB_REVERT_WINDOW_SECS")
        {
            self.revert_window_dur = Duration::from_secs(v.max(1));
        }
        if let Some(v) = crate::util::env_parse_opt::<usize>("CB_RPC_ERROR_LIMIT")
        {
            self.rpc_error_limit = v;
        }
        if let Some(v) = crate::util::env_parse_opt::<u64>("CB_RPC_ERROR_WINDOW_SECS")
        {
            self.rpc_error_window = Duration::from_secs(v.max(1));
        }
        self
    }

    /// Record an execution outcome (true = on-chain revert/failed inclusion).
    async fn record_execution_outcome(&self, reverted: bool) {
        let now = Instant::now();
        let mut window = self.revert_window.lock().await;
        window.push_back((now, reverted));
        while let Some((time, _)) = window.front() {
            if now.duration_since(*time) > self.revert_window_dur {
                window.pop_front();
            } else {
                break;
            }
        }
    }

    /// Record a runtime RPC failure for the RPC-lag trigger.
    async fn record_rpc_error(&self) {
        let now = Instant::now();
        let mut errors = self.rpc_errors.lock().await;
        errors.push_back(now);
        while let Some(time) = errors.front() {
            if now.duration_since(*time) > self.rpc_error_window {
                errors.pop_front();
            } else {
                break;
            }
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
        {
            let mut window = self.revert_window.lock().await;
            window.clear();
        }
        {
            let mut errors = self.rpc_errors.lock().await;
            errors.clear();
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

        let (revert_samples, revert_reverts) = {
            let mut window = self.revert_window.lock().await;
            while let Some((time, _)) = window.front() {
                if now.duration_since(*time) > self.revert_window_dur {
                    window.pop_front();
                } else {
                    break;
                }
            }
            let reverts = window.iter().filter(|(_, reverted)| *reverted).count();
            (window.len(), reverts)
        };
        let rpc_error_count = {
            let mut errors = self.rpc_errors.lock().await;
            while let Some(time) = errors.front() {
                if now.duration_since(*time) > self.rpc_error_window {
                    errors.pop_front();
                } else {
                    break;
                }
            }
            errors.len()
        };

        let reason = self.evaluate_reason(
            hourly_total,
            daily_total,
            consecutive_failures,
            revert_samples,
            revert_reverts,
            rpc_error_count,
        );
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
                self.rpc_error_window.as_secs(),
                self.rpc_error_limit
            ));
        }
        None
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
        crate::util::update_float_ema(&mut self.ema_win_rate, self.alpha, outcome);
    }

    fn update_rejection_rate(&mut self, rejected: bool) {
        let value = if rejected { 1.0 } else { 0.0 };
        crate::util::update_float_ema(&mut self.ema_rejection_rate, self.alpha, value);
    }

    fn update_latency(&mut self, latency: std::time::Duration) {
        let millis = latency.as_secs_f64() * 1_000.0;
        crate::util::update_float_ema(&mut self.ema_latency_ms, self.alpha, millis);
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
        // Boost both legs: victim sells `from` (pool receives) and buys `to`
        // (pool depletes). Backrun cycles often start from the dislocated token.
        for token in [hint.from, hint.to] {
            let entry = priorities.entry(token).or_insert(0);
            *entry = entry.saturating_add(boost).clamp(i128::MIN + 1, i128::MAX);
        }
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
    /// A REAL, single, un-redacted http endpoint for components that must issue
    /// their own JSON-RPC (REVM forking).
    ///
    /// `rpc_endpoint` above is `FailoverClient::endpoint_label()` — deliberately
    /// REDACTED and comma-joined, because it doubles as a log line and a
    /// HealthTracker key. It is not a URL and cannot be POSTed to. Passing it to
    /// the REVM fork made `eth_getBlockByNumber` fail on every single
    /// simulation, silently degrading to the slow `eth_call` path, which then
    /// exceeded the simulation budget — so REVM simulation had never once
    /// succeeded on any provider. Keep this field out of logs: it carries the
    /// provider credential.
    sim_rpc_url: String,
    rpc_health: Arc<StdMutex<HealthTracker>>,
    univ3_quoter: Address,
    univ3_factory: Address,
    univ3_validation: Option<UniV3ValidationConfig>,
    univ3_fee_tiers: Option<Arc<HashSet<u32>>>,
    bal_vault: Address,
    aave_pool: Option<Address>,
    aave_fee_bps: u32,
    erc3156_lender: Option<Address>,
    erc3156_fee_bps: u32,
    bal_flashloan_tokens: Option<HashSet<Address>>,
    aave_flashloan_tokens: Option<HashSet<Address>>,
    erc3156_flashloan_tokens: Option<HashSet<Address>>,
    univ2_flashloan_tokens: Option<HashSet<Address>>,
    univ3_flashloan_tokens: Option<HashSet<Address>>,
    /// Pool a univ2-style flash swap borrows from, and that pool's swap fee.
    /// Required: without an address the loan is rejected as unfundable.
    univ2_flash_pool: Option<Address>,
    univ2_flash_fee_bps: u32,
    univ3_flash_pool: Option<Address>,
    univ3_flash_fee_bps: u32,
    chain_env_prefix: String,
    tokens: TokenList,
    initial_token_decimals: HashMap<Address, u8>,
    wrapped_native: Address,
    capital: Arc<CapitalManager>,
    pool_depth_cache: Arc<PoolDepthCache>,
    pool_monitor: Option<Arc<ingestion::PoolMonitor<C>>>,
    hot_univ2_pools: Arc<tokio::sync::RwLock<Vec<ResolvedUniV2PoolCfg>>>,
    hot_univ3_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
    hot_slipstream_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
    slipstream_quoter_addr: Address,
    slipstream_factory: Address,
    slipstream_router: Address,
    slipstream_validation: Option<UniV3ValidationConfig>,
    slipstream_tick_spacings: Option<Arc<HashSet<u32>>>,
    hot_pancakeswap_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
    pancakeswap_quoter_addr: Address,
    pancakeswap_factory: Address,
    pancakeswap_router: Address,
    pancakeswap_validation: Option<UniV3ValidationConfig>,
    pancakeswap_fee_tiers: Option<Arc<HashSet<u32>>>,
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
    risk_policy: Option<RuntimeRiskPolicy>,
    sim_quorum: Arc<SimQuorum>,
    chain_id: u64,
    block_head_rx: Option<Arc<Mutex<watch::Receiver<BlockHead>>>>,
    bf_skip_on_stable_graph: bool,
    /// A/B switch for the hub-anchored cycle search (`ARBOT_HUB_SEARCH`).
    hub_search_enabled: bool,
    /// Parallel pools retained per token pair by the hub search.
    hub_search_parallel_edges: usize,
}

struct Runner<M, C>
where
    M: Middleware + 'static,
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    feature_gate: FeatureGate,
    provider: Arc<Provider<C>>,
    rpc_endpoint: String,
    /// A REAL, single, un-redacted http endpoint for components that must issue
    /// their own JSON-RPC (REVM forking).
    ///
    /// `rpc_endpoint` above is `FailoverClient::endpoint_label()` — deliberately
    /// REDACTED and comma-joined, because it doubles as a log line and a
    /// HealthTracker key. It is not a URL and cannot be POSTed to. Passing it to
    /// the REVM fork made `eth_getBlockByNumber` fail on every single
    /// simulation, silently degrading to the slow `eth_call` path, which then
    /// exceeded the simulation budget — so REVM simulation had never once
    /// succeeded on any provider. Keep this field out of logs: it carries the
    /// provider credential.
    sim_rpc_url: String,
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
    aave_fee_bps: u32,
    erc3156_lender: Option<Address>,
    erc3156_fee_bps: u32,
    bal_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    aave_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    erc3156_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    univ2_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    univ3_flashloan_tokens: Option<Arc<HashSet<Address>>>,
    univ2_flash_pool: Option<Address>,
    univ2_flash_fee_bps: u32,
    univ3_flash_pool: Option<Address>,
    univ3_flash_fee_bps: u32,
    chain_env_prefix: String,
    tokens: TokenList,
    wrapped_native: Address,
    capital: Arc<CapitalManager>,
    pool_depth_cache: Arc<PoolDepthCache>,
    pool_monitor: Option<Arc<ingestion::PoolMonitor<C>>>,
    hot_univ2_pools: Arc<tokio::sync::RwLock<Vec<ResolvedUniV2PoolCfg>>>,
    hot_univ3_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
    hot_slipstream_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
    hot_pancakeswap_pools: Arc<tokio::sync::RwLock<Vec<PoolRecord>>>,
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
    slipstream_quoter: Option<Arc<SlipstreamQuoter<C>>>,
    slipstream_router: Address,
    slipstream_validation: Option<UniV3ValidationConfig>,
    slipstream_tick_spacings: Option<Arc<HashSet<u32>>>,
    pancakeswap_quoter: Option<Arc<UniQuoter<C>>>,
    pancakeswap_validation: Option<UniV3ValidationConfig>,
    pancakeswap_fee_tiers: Option<Arc<HashSet<u32>>>,
    bal_quote: Arc<BalQuote<C>>,
    curve_quote: Arc<CurveQuote<C>>,
    quote_semaphore: Arc<Semaphore>,
    univ3_validation_once: Arc<OnceCell<()>>,
    slipstream_validation_once: Arc<OnceCell<()>>,
    pancakeswap_validation_once: Arc<OnceCell<()>>,
    circuit_breaker: Arc<CircuitBreaker>,
    metrics: Option<Arc<Metrics>>,
    accounting: Option<Arc<Accounting>>,
    token_decimals: Arc<Mutex<HashMap<Address, u8>>>,
    native_price_cache: Arc<Mutex<HashMap<Address, (NativePrice, Instant)>>>,
    /// RPC request total at the end of the previous scan, for per-scan deltas.
    last_scan_rpc_total: Arc<std::sync::atomic::AtomicU64>,
    fee_estimator: FeeEstimator<C>,
    last_graph_digest: Arc<Mutex<Option<GraphDigest>>>,
    previous_cycle_seeds: Arc<Mutex<Vec<CycleSeed>>>,
    bf_skip_on_stable_graph: bool,
    hub_search_enabled: bool,
    hub_search_parallel_edges: usize,
    candidate_logger: Arc<CandidateDecisionLogger>,
    risk_policy: Option<RuntimeRiskPolicy>,
    sim_quorum: Arc<SimQuorum>,
    chain_id: u64,
    last_scanned_block: Arc<Mutex<Option<U64>>>,
    block_head_rx: Option<Arc<Mutex<watch::Receiver<BlockHead>>>>,
    populate_cache: Arc<Mutex<PopulateCacheState>>,
    flash_capacity: Arc<StdMutex<FlashCapacityCache>>,
    /// Precomputed cycle set, rebuilt only when graph STRUCTURE changes.
    /// Populated only under ARBOT_CYCLE_INDEX_COMPARE; `None` otherwise.
    cycle_index: Arc<StdMutex<Option<crate::cycle_index::CycleIndex>>>,
    /// Immutable graph snapshot published after each scan, for readers that
    /// must not wait 4.2s for the next one. Written only here.
    graph_snapshot: Arc<StdMutex<Option<Arc<Graph>>>>,
    /// The economics half of what a reader needs to size a candidate,
    /// published alongside the graph. Written only in `scan_once`.
    prep_context: crate::base_fast::Published<PrepContextSnapshot>,
    /// Native (wei) per RAW unit of each token, for readers that must compare
    /// value across tokens. Only reliable prices are published: an unreliable
    /// one is a "no information" sentinel and would rank a worthless token's
    /// large numbers above a valuable token's small ones.
    token_native_prices: crate::base_fast::Published<crate::base_fast::TokenPrices>,
    /// Long-lived tick-ladder cache for the multi-tick CL simulator
    /// (`ARBOT_CL_MULTI_TICK`). Built once here and reused across every
    /// `scan_once()` call for this chain, so `CachedTickSource`'s epoch cache
    /// actually collapses repeat tick RPC across scans instead of being
    /// rebuilt (and its cache thrown away) once per scan. This `Runner` is
    /// the ONLY owner: one `Runner` per chain, each with its own `provider`
    /// and its own `cl_tick_cache` instance, so there is no path for one
    /// chain's tick data to reach another chain's pools.
    cl_tick_cache: Arc<crate::cl_ticks::CachedTickSource<crate::cl_ticks::RpcTickSource<C>>>,
}

#[derive(Clone, Debug, Default)]
struct PopulateCacheState {
    cached_edges: Vec<Edge>,
    last_digest: Option<EdgeDigest>,
    touched_pools: HashSet<Address>,
}

/// Per-(provider, token) flash-loan capacity, refreshed once per block.
///
/// `flash_loan_quotes` used to advertise the same config-derived cap for every
/// provider, so Balancer — quoted at 0 bps — won the fee sort at ANY size,
/// including sizes its vault cannot fund. On Base the vault holds ~27.5 WETH
/// while sizing routinely asked for more, and the loan reverts on-chain as
/// `BAL#528` (INSUFFICIENT_FLASH_LOAN_BALANCE). Shadow mode hid it because no
/// dispatch ever happened; the first funded run would have burned gas on it.
///
/// `aTokens` is resolved once per (pool, token) and reused: the aToken address
/// for a reserve does not change, only its balance does.
#[derive(Debug, Default)]
struct FlashCapacityCache {
    block: u64,
    caps: HashMap<(u8, Address), U256>,
    atokens: HashMap<Address, Address>,
}

impl<M, C> Runner<M, C>
where
    M: Middleware + 'static,
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    /// Executor calldata for a sized candidate, for preconfirmed simulation.
    ///
    /// Uses the RELAXED plan -- per-hop minimums and `min_profit` zeroed --
    /// which is the same diagnostic form `scan_once` builds alongside the
    /// enforced one. That choice is deliberate and it bounds what the result
    /// means: a success says the ROUTE EXECUTES against preconfirmed state and
    /// reports what it really costs in gas. It says nothing about whether the
    /// trade clears its profit threshold, because that threshold is computed
    /// downstream of this seam. Sizing already answered the profit question;
    /// this answers the one sizing cannot, which is whether the chain agrees.
    ///
    /// A simulation built with the ENFORCED minimums would conflate the two:
    /// a revert would mean either a broken route or an unprofitable one, and
    /// those need different responses.
    fn simulation_calldata(&self, sized: &SizedCandidate) -> Option<(Address, Address, Vec<u8>)> {
        let relaxed = sized.plan.with_relaxed_min_outs();
        let loans: Vec<ExecutorLoan> = sized
            .sizing
            .allocations
            .iter()
            .map(|alloc| ExecutorLoan {
                token: sized.cycle_start,
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
        let plan_args = ExecutorPlan {
            loans,
            cycle_slippage_bps: sized.sizing.max_slippage_bps.min(u32::from(u16::MAX)) as u16,
            steps: encode_plan_steps(relaxed.steps),
            min_profit: U256::zero(),
        };
        let call = self.build_executor_call(&plan_args)?;
        let to = match call.tx.to() {
            Some(ethers::types::NameOrAddress::Address(a)) => *a,
            _ => return None,
        };
        // `from` must be the signing EOA, not the executor. The executor
        // allowlists its callers, so simulating from the wrong account reverts
        // on authorisation and the result would read as a broken route.
        let from = self.wallet.as_ref()?.address();
        Some((from, to, call.calldata()?.to_vec()))
    }

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
            sim_rpc_url,
            rpc_health,
            univ3_quoter,
            univ3_factory,
            univ3_validation,
            univ3_fee_tiers,
            bal_vault,
            aave_pool,
            aave_fee_bps,
            erc3156_lender,
            erc3156_fee_bps,
            bal_flashloan_tokens,
            aave_flashloan_tokens,
            erc3156_flashloan_tokens,
            univ2_flashloan_tokens,
            univ3_flashloan_tokens,
            univ2_flash_pool,
            univ2_flash_fee_bps,
            univ3_flash_pool,
            univ3_flash_fee_bps,
            chain_env_prefix,
            tokens,
            initial_token_decimals,
            wrapped_native,
            capital,
            pool_depth_cache,
            pool_monitor,
            hot_univ2_pools,
            hot_univ3_pools,
            hot_slipstream_pools,
            slipstream_quoter_addr,
            slipstream_factory,
            slipstream_router,
            slipstream_validation,
            slipstream_tick_spacings,
            hot_pancakeswap_pools,
            pancakeswap_quoter_addr,
            pancakeswap_factory,
            pancakeswap_router: _pancakeswap_router,
            pancakeswap_validation,
            pancakeswap_fee_tiers,
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
            risk_policy,
            sim_quorum,
            chain_id,
            block_head_rx,
            bf_skip_on_stable_graph,
            hub_search_enabled,
            hub_search_parallel_edges,
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
        let slipstream_quoter = if slipstream_quoter_addr != Address::zero()
            && slipstream_factory != Address::zero()
        {
            Some(Arc::new(SlipstreamQuoter::new(
                provider.clone(),
                slipstream_quoter_addr,
                slipstream_factory,
            )))
        } else {
            None
        };
        let pancakeswap_quoter = if pancakeswap_quoter_addr != Address::zero()
            && pancakeswap_factory != Address::zero()
        {
            Some(Arc::new(UniQuoter::new(
                provider.clone(),
                pancakeswap_quoter_addr,
                pancakeswap_factory,
            )))
        } else {
            None
        };
        let bal_quote = Arc::new(BalQuote::new(provider.clone(), bal_vault));
        let curve_quote = Arc::new(CurveQuote::new(provider.clone()));
        // One cache per `Runner`, i.e. one per chain: see the field doc on
        // `cl_tick_cache` for why this must never be shared across chains.
        let cl_tick_cache = Arc::new(crate::cl_ticks::CachedTickSource::new(
            crate::cl_ticks::RpcTickSource::new(provider.clone()),
            32,
        ));
        Self {
            feature_gate,
            provider,
            rpc_endpoint,
            sim_rpc_url,
            rpc_health,
            univ3_quoter,
            univ3_factory,
            univ3_validation,
            univ3_fee_tiers,
            bal_vault,
            aave_pool,
            aave_fee_bps,
            erc3156_lender,
            erc3156_fee_bps,
            bal_flashloan_tokens,
            aave_flashloan_tokens,
            erc3156_flashloan_tokens,
            univ2_flashloan_tokens,
            univ3_flashloan_tokens,
            univ2_flash_pool,
            univ2_flash_fee_bps,
            univ3_flash_pool,
            univ3_flash_fee_bps,
            chain_env_prefix,
            tokens,
            wrapped_native,
            capital,
            pool_depth_cache,
            pool_monitor,
            hot_univ2_pools,
            hot_univ3_pools,
            hot_slipstream_pools,
            hot_pancakeswap_pools,
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
            slipstream_quoter,
            slipstream_router,
            slipstream_validation,
            slipstream_tick_spacings,
            pancakeswap_quoter,
            pancakeswap_validation,
            pancakeswap_fee_tiers,
            bal_quote,
            curve_quote,
            quote_semaphore: Arc::new(Semaphore::new(univ3_quote_concurrency())),
            univ3_validation_once: Arc::new(OnceCell::new()),
            slipstream_validation_once: Arc::new(OnceCell::new()),
            pancakeswap_validation_once: Arc::new(OnceCell::new()),
            circuit_breaker,
            metrics,
            accounting,
            token_decimals: Arc::new(Mutex::new(initial_token_decimals)),
            native_price_cache: Arc::new(Mutex::new(HashMap::new())),
            last_scan_rpc_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            fee_estimator,
            last_graph_digest: Arc::new(Mutex::new(None)),
            previous_cycle_seeds: Arc::new(Mutex::new(Vec::new())),
            bf_skip_on_stable_graph,
            hub_search_enabled,
            hub_search_parallel_edges,
            candidate_logger: Arc::new(CandidateDecisionLogger::from_env()),
            risk_policy,
            sim_quorum,
            chain_id,
            last_scanned_block: Arc::new(Mutex::new(None)),
            block_head_rx,
            populate_cache: Arc::new(Mutex::new(PopulateCacheState::default())),
            flash_capacity: Arc::new(StdMutex::new(FlashCapacityCache::default())),
            cycle_index: Arc::new(StdMutex::new(None)),
            graph_snapshot: Arc::new(StdMutex::new(None)),
            prep_context: Arc::new(StdMutex::new(None)),
            token_native_prices: Arc::new(StdMutex::new(None)),
            cl_tick_cache,
        }
    }

    async fn wait_for_scan_cadence(&self) {
        let idle = Duration::from_millis(scan_idle_sleep_ms());
        if let Some(rx) = &self.block_head_rx {
            let mut guard = rx.lock().await;
            if guard.has_changed().unwrap_or(false) {
                return;
            }
            let _ = timeout(idle.max(Duration::from_millis(25)), guard.changed()).await;
        } else {
            sleep(idle).await;
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

    #[allow(clippy::too_many_arguments)]
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
        // Shadow mode requires nothing: no broadcast happens, so no gas is spent
        // and the balance is irrelevant. In LIVE mode the reserve is how many
        // worst-case transactions the wallet can still pay for — dispatching
        // without it produces failed sends, not savings. The 20x multiple was
        // hardcoded; it is now tunable via ARBOT_GAS_RESERVE_TXS (0 disables the
        // floor entirely) so the trade-off is an explicit operator decision.
        let min_balance = if self.shadow.enabled {
            U256::zero()
        } else {
            gas_per_tx.saturating_mul(U256::from(gas_reserve_txs()))
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
        let hot_slipstream = { self.hot_slipstream_pools.read().await.clone() };
        let hot_pancakeswap = { self.hot_pancakeswap_pools.read().await.clone() };
        let slipstream_quoter = self.slipstream_quoter.clone();
        let slipstream_router = self.slipstream_router;
        let slipstream_validation = self.slipstream_validation.clone();
        let slipstream_tick_spacings = self.slipstream_tick_spacings.clone();
        let slipstream_validation_once = Arc::clone(&self.slipstream_validation_once);
        let pancakeswap_quoter = self.pancakeswap_quoter.clone();
        let pancakeswap_validation = self.pancakeswap_validation.clone();
        let pancakeswap_fee_tiers = self.pancakeswap_fee_tiers.clone();
        let pancakeswap_validation_once = Arc::clone(&self.pancakeswap_validation_once);
        let populate_cache = Arc::clone(&self.populate_cache);
        let hub_tokens = Arc::new(self.hub_tokens.clone());
        let wrapped_native = self.wrapped_native;
        let metrics = self.metrics.clone();
        let cl_tick_cache = Arc::clone(&self.cl_tick_cache);
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
                 _gas_price,
                 token_decimals,
                 _native_prices,
                 min_liquidity_tokens,
                 min_edge_max_input,
                 low_liquidity,
                 hot_paths,
                 quote_semaphore,
                 block_number| {
                    let hot_univ2 = hot_univ2.clone();
                    let hot_univ3 = hot_univ3.clone();
                    let hot_slipstream = hot_slipstream.clone();
                    let hot_pancakeswap = hot_pancakeswap.clone();
                    let validation = univ3_validation.clone();
                    let fee_tiers = univ3_fee_tiers.clone();
                    let chain = chain_name.clone();
                    let once = validation_once.clone();
                    let slipstream_quoter = slipstream_quoter.clone();
                    let slipstream_validation = slipstream_validation.clone();
                    let slipstream_tick_spacings = slipstream_tick_spacings.clone();
                    let slipstream_once = slipstream_validation_once.clone();
                    let pancakeswap_quoter = pancakeswap_quoter.clone();
                    let pancakeswap_validation = pancakeswap_validation.clone();
                    let pancakeswap_fee_tiers = pancakeswap_fee_tiers.clone();
                    let pancakeswap_once = pancakeswap_validation_once.clone();
                    let populate_cache = populate_cache.clone();
                    let hub_tokens = hub_tokens.clone();
                    let metrics = metrics.clone();
                    let cl_tick_cache = Arc::clone(&cl_tick_cache);
                    Box::pin(async move {
                        let populate_options = {
                            let guard = populate_cache.lock().await;
                            PopulateOptions {
                                touched_pools: guard.touched_pools.clone(),
                                last_digest: guard.last_digest,
                                cached_edges: if guard.cached_edges.is_empty() {
                                    None
                                } else {
                                    Some(guard.cached_edges.clone())
                                },
                            }
                        };
                        let result = populate_edges(
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
                            // gas_price / native_prices are no longer passed:
                            // Stage-1 edge weights are rate-only, so detection
                            // needs neither gas nor a native price oracle.
                            token_decimals.clone(),
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
                            slipstream_quoter,
                            slipstream_router,
                            slipstream_tick_spacings,
                            slipstream_validation,
                            slipstream_once,
                            &hot_slipstream,
                            pancakeswap_quoter,
                            pancakeswap_fee_tiers,
                            pancakeswap_validation,
                            pancakeswap_once,
                            &hot_pancakeswap,
                            hub_tokens,
                            wrapped_native,
                            populate_options,
                            metrics,
                            cl_tick_cache,
                        )
                        .await?;
                        Ok(result.edges)
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

        // 1) Snapshot the tokens still missing a cached decimals value under a
        //    short lock, then release it. The lock must NOT be held across the
        //    RPC fetches below: erc20_decimals with its 3-attempt backoff can
        //    take seconds, and holding token_decimals across it serialized every
        //    concurrent consumer of the cache (the whole scan pipeline stalled
        //    behind one cold-token lookup).
        let to_fetch: Vec<Address> = {
            let cache = self.token_decimals.lock().await;
            tokens
                .iter()
                .copied()
                .filter(|token| !cache.contains_key(token))
                .collect()
        };

        // 2) Fetch the missing decimals CONCURRENTLY (lock released), preserving
        //    the per-token 3-attempt retry with exponential backoff. A token
        //    whose fetch fails is left absent so callers fall back to 18, exactly
        //    as before.
        // Batch the cold-start decimals read first (spec §3.4): one Multicall3
        // instead of one eth_call per token before any quoting can begin. Only
        // tokens the batch could not resolve fall through to the retrying
        // per-token path below.
        let to_fetch: Vec<Address> = if to_fetch.is_empty() {
            to_fetch
        } else {
            let batched = crate::util::erc20_decimals_batched(
                self.provider.clone(),
                &to_fetch,
                U64::zero(),
            )
            .await;
            if !batched.is_empty() {
                let mut cache = self.token_decimals.lock().await;
                for (token, decimals) in &batched {
                    cache.insert(*token, *decimals);
                }
            }
            to_fetch
                .into_iter()
                .filter(|token| !batched.contains_key(token))
                .collect()
        };

        if !to_fetch.is_empty() {
            let concurrency = native_price_concurrency();
            let fetched: Vec<(Address, Option<u8>)> = stream::iter(to_fetch.into_iter().map(
                |token| async move {
                    let mut last_err = None;
                    for attempt in 0..3 {
                        match erc20_decimals(self.provider.clone(), token).await {
                            Ok(decimals) => return (token, Some(decimals)),
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
                    (token, None)
                },
            ))
            .buffer_unordered(concurrency)
            .collect()
            .await;

            let mut cache = self.token_decimals.lock().await;
            for (token, decimals) in fetched {
                if let Some(decimals) = decimals {
                    cache.insert(token, decimals);
                }
            }
        }

        // 3) Return the full decimals snapshot from the (now-updated) cache.
        let cache = self.token_decimals.lock().await;
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

    /// Probe a token's native price against the UniV3 quoter.
    ///
    /// Returns a tri-state rather than `Option` because the caller must not
    /// treat "no route exists" and "the RPC call failed" the same way. Only the
    /// former is a fact about the chain that may be cached; conflating them is
    /// what let a provider rate-limit silently zero out fills for months.
    async fn fetch_native_price(
        &self,
        token: Address,
        decimals: u8,
        block_number: U64,
    ) -> NativePriceProbe {
        if token == self.wrapped_native || self.wrapped_native.is_zero() {
            let amount = U256::exp10(decimals.min(18) as usize);
            return NativePriceProbe::Priced(NativePrice::new(amount, amount, true));
        }

        let amount_in = U256::exp10(decimals.min(18) as usize);
        let mut transport_failed = false;
        for fee in FEE_TIERS {
            let path = build_univ3_price_path(token, self.wrapped_native, fee);
            match self.quoter.quote_path(path, amount_in, block_number).await {
                Ok(out) if !out.is_zero() => {
                    return NativePriceProbe::Priced(NativePrice::new(amount_in, out, true));
                }
                // Executed and returned zero: a real answer for this tier.
                Ok(_) => {}
                Err(err) => {
                    if !crate::quote_common::is_execution_revert(&err) {
                        transport_failed = true;
                    }
                }
            }
        }

        // Uniswap V3 had no route. Aerodrome Slipstream is the dominant CL venue
        // on Base, and it is keyed by TICK SPACING rather than fee tier, so a
        // token with deep Aerodrome liquidity but no Uniswap pool concludes
        // `NoRoute` here. That verdict is cached, and an unpriceable start token
        // is rejected as `unreliable_native_price_for_start_token` BEFORE sizing
        // runs — so every cycle starting at that token disappears silently, and
        // looks identical to "no opportunity" in the funnel.
        if let Some(slipstream) = self.slipstream_quoter.as_deref() {
            for spacing in SLIPSTREAM_PRICE_TICK_SPACINGS {
                let path = build_univ3_price_path(token, self.wrapped_native, spacing);
                match slipstream.quote_path(path, amount_in, block_number).await {
                    Ok(out) if !out.is_zero() => {
                        return NativePriceProbe::Priced(NativePrice::new(amount_in, out, true));
                    }
                    Ok(_) => {}
                    Err(err) => {
                        if !crate::quote_common::is_execution_revert(&err) {
                            transport_failed = true;
                        }
                    }
                }
            }
        }

        // A tier we never got an answer for could have held the price, so the
        // "no route" conclusion is only sound when every tier truly answered —
        // across BOTH venues now.
        if transport_failed {
            NativePriceProbe::Unknown
        } else {
            NativePriceProbe::NoRoute
        }
    }

    async fn load_native_prices(
        &self,
        tokens: &[Address],
        token_decimals: &HashMap<Address, u8>,
        block_number: U64,
    ) -> HashMap<Address, NativePrice> {
        let ttl = native_price_cache_ttl();
        // 1) Pick the tokens whose cached verdict (reliable OR unreliable) has
        //    expired. Caching the unreliable verdict too is what stops the
        //    per-scan re-quote of unpriceable tokens.
        let to_fetch: Vec<(Address, u8)> = {
            let cache = self.native_price_cache.lock().await;
            tokens
                .iter()
                .filter_map(|&token| {
                    let fresh = cache
                        .get(&token)
                        .map(|(_, at)| at.elapsed() < ttl)
                        .unwrap_or(false);
                    if fresh {
                        None
                    } else {
                        Some((token, token_decimals.get(&token).copied().unwrap_or(18)))
                    }
                })
                .collect()
        };

        // 2) Fetch the expired tokens CONCURRENTLY (lock released). The previous
        //    sequential refresh was a ~20s blind spike every TTL window.
        if !to_fetch.is_empty() {
            let concurrency = native_price_concurrency();
            let fetched: Vec<(Address, NativePriceProbe)> = stream::iter(to_fetch.into_iter().map(
                |(token, decimals)| async move {
                    (
                        token,
                        self.fetch_native_price(token, decimals, block_number).await,
                    )
                },
            ))
            .buffer_unordered(concurrency)
            .collect()
            .await;

            let mut cache = self.native_price_cache.lock().await;
            let now = Instant::now();
            let mut unknown = 0usize;
            for (token, probe) in fetched {
                if let Some(metrics) = self.metrics.as_deref() {
                    metrics.record_native_price_probe(&self.chain_name, probe.label());
                }
                // `None` => leave the cache entry stale (or absent) so the next
                // scan retries, instead of recording an RPC outage as a
                // permanent verdict about the token.
                match probe.cache_entry() {
                    Some(price) => {
                        cache.insert(token, (price, now));
                    }
                    None => unknown += 1,
                }
            }
            if unknown > 0 {
                warn!(
                    target: "pricing",
                    chain = %self.chain_name,
                    unknown,
                    "native price probes inconclusive (transport failures); \
                     leaving cache stale to retry rather than marking unpriceable"
                );
            }
        }

        // 3) Build the result snapshot from the (now-fresh) cache.
        let cache = self.native_price_cache.lock().await;
        tokens
            .iter()
            .filter_map(|&token| cache.get(&token).map(|(price, _)| (token, *price)))
            .collect()
    }

    /// Per-token trade notionals, in each token's own raw units.
    ///
    /// The capital bounds (`MIN_FLASH_LOAN_WEI` / `MAX_FLASH_LOAN_WEI`) are
    /// NATIVE-denominated, so they must be converted into the target token
    /// before they can bound it. Clamping a token's raw units directly against
    /// a wei constant is a decimals bug: with `MIN_FLASH_LOAN_WEI = 1e18`, a
    /// 6-decimal token like USDC was floored at 1e18 raw units — one trillion
    /// USDC. That propagates two ways, both fatal:
    ///   1. it becomes `base_amount_in`, the denominator of `gas_ratio` in
    ///      `compute_edge_weight`, so hops in one cycle are normalised against
    ///      wildly different economic values and the summed gas toll is
    ///      meaningless (see `cycle_weight_from_edge_indices`);
    ///   2. it reaches `optimize_trade_size` as `min_amount`, where
    ///      `upper_cap < min_amount` rejects EVERY cycle starting at a
    ///      sub-18-decimal token, unconditionally.
    ///
    /// The depth-derived amount is already value-coherent — `estimate_liquidity`
    /// returns `(liquidity_usd / 2) / price_usd` scaled by the token's own
    /// decimals — so the clamp is the only thing that breaks coherence. When the
    /// token has no reliable native price we cannot express the bounds in its
    /// units at all, so we skip the clamp rather than apply a wrong one, and a
    /// token with no depth stays at zero (fail closed) instead of being floored
    /// up to an arbitrary notional.
    async fn compute_base_amounts(
        &self,
        token_decimals: &HashMap<Address, u8>,
        capital: &CapitalSnapshot,
        native_prices: &HashMap<Address, NativePrice>,
    ) -> HashMap<Address, TradeSizing> {
        let tokens = self.tokens.current();
        let min_flash_native = capital.min_flash_loan;
        let max_flash_native = capital.max_flash_loan.max(min_flash_native);
        // Anchor for the size ladder, as a fraction of pool depth.
        //
        // This was hardcoded to 5 — every ladder was centred on 20% of pool
        // depth. Arbitrage sizes on Base sit nearer 0.1-2% of depth; at 20% the
        // price impact of the trade itself is larger than any spread it could
        // capture, so the centre of the search was permanently outside the
        // profitable band and only the ladder's long tail reached back into it.
        // 100 (1% of depth) centres the search where the optimum actually lives.
        let divisor = U256::from(
            crate::util::env_parse_opt::<u64>("ARBOT_DEPTH_DIVISOR")
                .unwrap_or(100)
                .max(1),
        );
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
            // Bounds in THIS token's units, or `None` when it cannot be priced.
            let price = self.native_price_for(token, native_prices);
            let bounds = price
                .tokens_for_native_strict(min_flash_native)
                .zip(price.tokens_for_native_strict(max_flash_native));
            if let Some((min_in_token, max_in_token)) = bounds {
                if sized < min_in_token {
                    sized = min_in_token;
                }
                if sized > max_in_token.max(min_in_token) {
                    sized = max_in_token.max(min_in_token);
                }
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

    /// Gas price used for Stage-1 edge weights.
    ///
    /// This is the L2 execution price only. The Base L1 data fee is deliberately
    /// NOT folded in here.
    ///
    /// The L1 data fee is a per-TRANSACTION constant, and a per-edge additive
    /// weight cannot represent a constant — only quantities proportional to that
    /// edge's gas. The previous attempt spread it as
    /// `l1_data_fee / (210_000 * max_hops)` per gas unit, which recovers the
    /// true fee only for a cycle whose total gas happens to equal
    /// `210_000 * max_hops`. For the 2-hop cycles Phase 1 targets it recovered
    /// 280_000 / 1_260_000 = 22% of the fee at `max_hops = 6` — and the error
    /// scales with the graph's configured hop limit, so changing an unrelated
    /// search knob silently repriced every edge.
    ///
    /// Rather than replace one wrong constant with another, the split of
    /// responsibility is made explicit: Stage 1 models the size-proportional
    /// execution cost, and Stage 2 owns the per-transaction constant. Stage 2
    /// already charges the full `l1_data_fee` exactly once, from the raw gas
    /// price, in `optimize_trade_size` — it never consults this function.
    ///
    /// Direction of the residual error is deliberate. `record_cycle` treats the
    /// summed weight as a hard reject, so under-charging gas at Stage 1 lets a
    /// few extra candidates through for Stage 2 to kill (wasted work, no loss),
    /// whereas over-charging would silently discard genuinely profitable cycles.
    /// Omitting a constant we cannot apportion errs on the safe side.
    fn gas_price_for_weights(&self, gas: &FeeEstimate) -> U256 {
        gas.gas_price
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

    /// Single source of truth for the dispatch profit threshold.
    ///
    /// Pre-simulation and post-simulation gating MUST use this same formula:
    /// dynamic gas/competition base plus revert-risk premium, flash-fee
    /// buffer, bridge premium, backrun discount, slippage floor and risk net
    /// floor. The historical drift (post-sim recomputation dropping the
    /// flash-fee buffer, bridge premium, and backrun discount) made the two
    /// gates disagree and could either skip profitable trades or dispatch
    /// trades the pre-sim gate would have rejected.
    fn unified_min_profit_threshold(&self, params: UnifiedThresholdParams<'_>) -> U256 {
        let mut base_threshold = self.dynamic_min_profit(DynamicProfitParams {
            fee: params.fee,
            est_gas: params.est_gas,
            est_gross: params.est_gross_after_fee,
            congestion: params.congestion,
            competition: params.competition,
            latency_secs: params.latency_secs,
            native_price: params.native_price,
        });

        // Risk policy: price expected revert losses into the threshold as a
        // premium over the gas cost (revert_penalty_model from ops inputs).
        if let Some(policy) = &self.risk_policy {
            let penalty_bps = policy.revert_penalty_bps(params.est_gas);
            if penalty_bps > 0 {
                let gas_cost_native = params
                    .fee
                    .gas_price
                    .saturating_mul(U256::from(params.est_gas))
                    .saturating_add(params.fee.l1_data_fee);
                if let Some(gas_cost_tokens) = params
                    .native_price
                    .tokens_for_native_strict(gas_cost_native)
                {
                    let premium = mul_div(
                        gas_cost_tokens,
                        U256::from(penalty_bps as u64),
                        U256::from(10_000u64),
                    );
                    base_threshold = base_threshold.saturating_add(premium);
                }
            }
        }

        let threshold = finalize_profit_threshold(ProfitThresholdParams {
            base_threshold,
            competition_buffer: params.competition_buffer,
            flash_fee_amount: params.flash_fee_amount,
            slippage_floor: params.slippage_floor,
            has_bridge_step: params.has_bridge_step,
            est_gross_after_fee: params.est_gross_after_fee,
            cross_chain_profit_bps: self.cross_chain_profit_bps,
            cross_chain_min_profit_wei: self.cross_chain_min_profit_wei,
            backrun_hint: params.backrun_hint,
        });

        // Risk policy: the declared net-profit floor also floors the on-chain
        // min_profit so the executor itself refuses sub-floor fills.
        match self.risk_min_net_profit_tokens(params.native_price) {
            Some(floor) => threshold.max(floor),
            None => threshold,
        }
    }

    /// Declared risk net-profit floor converted into start-token units.
    fn risk_min_net_profit_tokens(&self, native_price: NativePrice) -> Option<U256> {
        let floor_wei = self.risk_policy.as_ref()?.min_net_profit_wei?;
        native_price.tokens_for_native_strict(floor_wei)
    }

    /// Enforce the gas-unit and fee-per-gas caps from the risk policy against
    /// a candidate's fee estimate. Returns the rejection reason when violated.
    fn risk_gas_violation(&self, fee: &FeeEstimate, gas_units: u64) -> Option<&'static str> {
        let policy = self.risk_policy.as_ref()?;
        if let Some(max_gas) = policy.max_gas_units_per_tx {
            if gas_units > max_gas {
                return Some("risk_max_gas_units_exceeded");
            }
        }
        if let Some(cap) = policy.max_fee_per_gas_cap {
            let effective_fee = fee.max_fee_per_gas.unwrap_or(fee.gas_price);
            if effective_fee > cap {
                return Some("risk_max_fee_per_gas_exceeded");
            }
        }
        None
    }

    /// Enforce slippage and price-impact tolerances from the risk policy.
    fn risk_slippage_violation(
        &self,
        swap_slippage_bps: u32,
        price_impact_bps: u32,
    ) -> Option<&'static str> {
        let policy = self.risk_policy.as_ref()?;
        if let Some(max_slippage) = policy.max_slippage_bps {
            if swap_slippage_bps > max_slippage {
                return Some("risk_max_slippage_exceeded");
            }
        }
        if let Some(max_impact) = policy.max_price_impact_bps {
            if price_impact_bps > max_impact {
                return Some("risk_max_price_impact_exceeded");
            }
        }
        None
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

    /// Tokens a cycle may start at: the union of every flash-loan provider's
    /// allowlist, restricted to tokens that are actually in the graph.
    ///
    /// This is the hub set for [`Graph::hub_anchored_cycles`]. Anything outside
    /// it cannot be borrowed, so a cycle anchored there could never execute.
    /// Can this token be borrowed from ANY configured flash-loan provider?
    ///
    /// Same union as [`Self::flash_loan_hub_tokens`], asked per token. The
    /// hub-anchored search already restricts starts to that set, but the
    /// Bellman-Ford path did not — so every cycle it anchored at an unfundable
    /// token was generated, quoted, sized and only then rejected as
    /// `no_flashloan_provider`. Measured on Base that was 100% of one run's
    /// candidate budget (61/61, all one token), crowding real candidates out of
    /// the funnel entirely.
    ///
    /// A `None` set means "no allowlist configured", which the quote path treats
    /// as unrestricted for Balancer; mirror that here so this filter can never
    /// be stricter than the funding logic it is predicting.
    fn can_flash_fund(&self, token: Address) -> bool {
        if self.bal_flashloan_tokens.is_none() {
            return true;
        }
        [
            self.bal_flashloan_tokens.as_deref(),
            self.aave_flashloan_tokens.as_deref(),
            self.erc3156_flashloan_tokens.as_deref(),
            self.univ2_flashloan_tokens.as_deref(),
            self.univ3_flashloan_tokens.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|set| set.contains(&token))
    }

    /// Tokens a cycle may START at: allowlisted for a flash loan, present in the
    /// graph, and backed by measured non-zero capacity from some provider.
    ///
    /// Ordered with the wrapped native token first. Everything downstream is
    /// per-start-token work — sizing, quoting, plan building — so a start token
    /// that cannot be funded, or that reliably fails, spends scan budget that
    /// WETH and USDC would convert. Measured on Base: cbBTC starts produced 126
    /// of 470 records (27%) and 49 of the 62 `no_profitable_size` rejections.
    ///
    /// The capacity test here is deliberately only "known and non-zero". The
    /// precise `>= min_flash_loan` gate lives in `capacity_capped_amount`, which
    /// receives bounds already converted into token units. Comparing a raw token
    /// balance against a NATIVE-denominated minimum here would re-introduce
    /// exactly the decimals bug that zeroed every USDC cycle.
    ///
    /// `ARBOT_START_TOKENS` (comma-separated addresses) restricts starts to an
    /// explicit set — e.g. WETH,USDC to defer cbBTC/cbETH. Unset means every
    /// fundable token, which is the previous behaviour.
    fn flash_loan_hub_tokens(&self, graph: &Graph) -> Vec<Address> {
        let mut hubs: Vec<Address> = Vec::new();
        let mut seen: HashSet<Address> = HashSet::new();
        let sets = [
            self.bal_flashloan_tokens.as_deref(),
            self.aave_flashloan_tokens.as_deref(),
            self.erc3156_flashloan_tokens.as_deref(),
            self.univ2_flashloan_tokens.as_deref(),
            self.univ3_flashloan_tokens.as_deref(),
        ];
        for set in sets.into_iter().flatten() {
            for token in set {
                if graph.ix.contains_key(token) && seen.insert(*token) {
                    hubs.push(*token);
                }
            }
        }

        let allowed = start_token_allowlist();
        if let Some(allowed) = allowed.as_ref() {
            hubs.retain(|t| allowed.contains(t));
        }

        // Drop tokens with no measured capacity anywhere. `None` means the
        // refresh has not run or the read failed, so keep those rather than
        // silently narrowing the universe on a degraded RPC.
        let providers = [
            FlashLoanProvider::Balancer,
            FlashLoanProvider::AaveV3,
            FlashLoanProvider::Erc3156,
            FlashLoanProvider::Univ2Flashswap,
            FlashLoanProvider::Univ3Flash,
        ];
        let before = hubs.len();
        hubs.retain(|token| {
            let mut any_known = false;
            for provider in providers {
                match self.flash_capacity_for(provider, *token) {
                    Some(cap) if !cap.is_zero() => return true,
                    Some(_) => any_known = true,
                    None => {}
                }
            }
            // Every provider answered and every answer was zero -> unfundable.
            !any_known
        });

        // Wrapped native first: it is the identity case for native pricing and
        // the deepest flash-loan asset on every chain we run.
        let native = self.wrapped_native;
        hubs.sort_by_key(|t| (*t != native, *t));

        if hubs.len() != before {
            debug!(
                target: "flashcap",
                before,
                after = hubs.len(),
                restricted = allowed.is_some(),
                "start tokens filtered to fundable set"
            );
        }
        hubs
    }

    /// Available balance a provider can actually lend for `token`, or `None`
    /// when this block's capacity has not been read yet.
    fn flash_capacity_for(&self, provider: FlashLoanProvider, token: Address) -> Option<U256> {
        let guard = self.flash_capacity.lock().ok()?;
        guard.caps.get(&(provider.as_id(), token)).copied()
    }

    /// Clamp an advertised loan size to what the provider can actually lend.
    ///
    /// `None` means withhold the provider entirely: either its capacity is below
    /// the minimum viable loan, or capacity is unknown while an allowlist is
    /// configured. That second case fails CLOSED on purpose — a provider we
    /// could have measured but did not must never be offered at an unbounded
    /// size, which is exactly how Balancer won the fee sort at sizes its vault
    /// could not fund.
    ///
    /// When no allowlist is configured the token set cannot be enumerated to
    /// refresh, so the previous unbounded behavior is preserved rather than
    /// silently disabling the provider on chains that never set one.
    fn capacity_capped(
        &self,
        provider: FlashLoanProvider,
        token: Address,
        requested: U256,
        min_flash_loan: U256,
        allowlist_configured: bool,
    ) -> Option<U256> {
        capacity_capped_amount(
            self.flash_capacity_for(provider, token),
            requested,
            min_flash_loan,
            allowlist_configured,
        )
    }

    /// Refresh per-provider flash-loan capacity for every allowlisted token.
    ///
    /// Cheap and bounded: the allowlists hold a handful of tokens, aToken
    /// addresses are resolved once and reused, and the whole thing is skipped
    /// when the cached block is still current. Failures leave the entry absent
    /// rather than stale — `flash_loan_quotes` fails closed on a missing entry,
    /// so a degraded RPC withholds the provider instead of over-promising it.
    async fn refresh_flash_capacity(&self, block_number: u64) {
        if let Ok(guard) = self.flash_capacity.lock() {
            if guard.block == block_number && !guard.caps.is_empty() {
                return;
            }
        }

        let mut wanted: Vec<(u8, Address, Address)> = Vec::new();
        if let Some(tokens) = self.bal_flashloan_tokens.as_ref() {
            for token in tokens.iter() {
                wanted.push((FlashLoanProvider::Balancer.as_id(), *token, self.bal_vault));
            }
        }
        let aave_pool = self.aave_pool;
        if let (Some(tokens), Some(pool)) = (self.aave_flashloan_tokens.as_ref(), aave_pool) {
            for token in tokens.iter() {
                // Holder is the aToken, resolved below; `pool` marks the lookup.
                wanted.push((FlashLoanProvider::AaveV3.as_id(), *token, pool));
            }
        }
        if wanted.is_empty() {
            return;
        }

        let mut caps: HashMap<(u8, Address), U256> = HashMap::new();
        let mut atokens: HashMap<Address, Address> = self
            .flash_capacity
            .lock()
            .map(|g| g.atokens.clone())
            .unwrap_or_default();

        for (provider_id, token, target) in wanted {
            let holder = if provider_id == FlashLoanProvider::AaveV3.as_id() {
                match atokens.get(&token).copied() {
                    Some(addr) => addr,
                    None => match self.resolve_aave_atoken(target, token).await {
                        Some(addr) => {
                            atokens.insert(token, addr);
                            addr
                        }
                        // Silent `continue` here withheld Aave for the token
                        // indefinitely and looked identical to "Aave has no
                        // liquidity". It is neither: the aToken holds the
                        // lendable reserve, and on Base that is ~7,000 WETH and
                        // ~30.4M USDC, versus 0.0002 WETH at the pool address.
                        // If this fires, capacity is not absent — the lookup is
                        // broken and the deepest WETH provider is offline.
                        None => {
                            warn!(
                                target: "flashcap",
                                token = %format!("0x{}", hex::encode(token)),
                                pool = %format!("0x{}", hex::encode(target)),
                                "aToken resolution FAILED (getReserveData); Aave \
                                 withheld for this token. This is a broken lookup, \
                                 not missing liquidity"
                            );
                            continue;
                        }
                    },
                }
            } else {
                target
            };

            let contract = crate::util::IERC20::new(token, Arc::clone(&self.provider));
            match contract.balance_of(holder).call().await {
                Ok(balance) => {
                    caps.insert((provider_id, token), balance);
                }
                Err(err) => {
                    warn!(
                        token = %format!("0x{}", hex::encode(token)),
                        provider_id,
                        error = %err,
                        "flash-loan capacity read failed; provider withheld for this block"
                    );
                }
            }
        }

        // Success-path visibility. Without this, "capacity enforced correctly"
        // and "capacity never read" are indistinguishable in the log, and the
        // fail-closed branch would silently withhold every provider.
        if caps.is_empty() {
            warn!(
                block = block_number,
                "flash-loan capacity refresh produced no entries; all allowlisted \
                 providers will be withheld this block"
            );
        } else {
            // INFO, not debug: this is the success path, and its absence is the
            // only way to tell "capacity enforced correctly" apart from
            // "capacity never read". Each entry carries whether the provider can
            // actually fund a viable trade, so a chain that is merely SHORT is
            // distinguishable at a glance from one with no opportunity.
            // NATIVE-denominated, same basis as the per-provider balances read
            // above for WETH. For a non-native token this is an approximation
            // used only to label the log line, never to gate a trade — the real
            // gate is `capacity_capped_amount`, which works in token units.
            let min_loan = self.min_flash_loan_wei;
            let mut ok_count = 0usize;
            let summary = caps
                .iter()
                .map(|((provider_id, token), amount)| {
                    let provider = match *provider_id {
                        0 => "balancer",
                        1 => "aave",
                        other => return format!("provider{other}=?"),
                    };
                    // `min_loan` is NATIVE-denominated, so the comparison is
                    // only meaningful for the native token itself. Annotating a
                    // 6-decimal balance against an 18-decimal minimum labelled
                    // Aave's 30.4M USDC as "BELOW_MIN" — the same unit confusion
                    // that actually starved these tokens. Say nothing rather
                    // than say something false.
                    let native_basis = *token == self.wrapped_native;
                    let fundable = !native_basis || *amount >= min_loan;
                    if fundable {
                        ok_count += 1;
                    }
                    format!(
                        "{provider}:0x{}={amount}{}",
                        hex::encode(&token[..4]),
                        if native_basis && !fundable {
                            "(BELOW_MIN)"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            info!(
                target: "flashcap",
                block = block_number,
                entries = caps.len(),
                fundable = ok_count,
                %min_loan,
                %summary,
                "flash-loan capacity refreshed"
            );
            if ok_count == 0 {
                warn!(
                    target: "flashcap",
                    block = block_number,
                    %min_loan,
                    "every provider is BELOW the min flash loan; no cycle can be \
                     funded this block regardless of available spread"
                );
            }
        }

        if let Ok(mut guard) = self.flash_capacity.lock() {
            guard.block = block_number;
            guard.caps = caps;
            guard.atokens = atokens;
        }
    }

    /// `getReserveData(address).aTokenAddress` — the contract holding a reserve's
    /// underlying, whose balance is Aave's lendable liquidity. Word 8 of the
    /// returned struct; verified against Base WETH.
    async fn resolve_aave_atoken(&self, pool: Address, token: Address) -> Option<Address> {
        const GET_RESERVE_DATA: [u8; 4] = [0x35, 0xea, 0x6a, 0x75];
        let mut data = Vec::with_capacity(36);
        data.extend_from_slice(&GET_RESERVE_DATA);
        data.extend(ethers::abi::encode(&[ethers::abi::Token::Address(token)]));
        let tx: TypedTransaction = TransactionRequest {
            to: Some(NameOrAddress::Address(pool)),
            data: Some(data.into()),
            ..Default::default()
        }
        .into();
        let raw = self.provider.call(&tx, None).await.ok()?;
        let bytes = raw.as_ref();
        if bytes.len() < 9 * 32 {
            return None;
        }
        let word = &bytes[8 * 32..9 * 32];
        let addr = Address::from_slice(&word[12..32]);
        (!addr.is_zero()).then_some(addr)
    }

    /// Token pairs the configured pool inventory can serve, across every venue.
    ///
    /// Reads the hot-pool lists rather than the graph so transient quote
    /// failures do not register as topology changes.
    async fn pool_universe(&self) -> crate::cycle_index::PoolUniverse {
        let mut triples: Vec<(Address, Address, Address)> = Vec::new();
        for records in [
            &self.hot_univ3_pools,
            &self.hot_slipstream_pools,
            &self.hot_pancakeswap_pools,
        ] {
            for r in records.read().await.iter() {
                triples.push((r.pool, r.token0, r.token1));
            }
        }
        for cfg in self.hot_univ2_pools.read().await.iter() {
            triples.push((cfg.pair, cfg.token_in, cfg.token_out));
        }
        crate::cycle_index::PoolUniverse::from_pools(triples)
    }

    /// Compare the precomputed cycle index against what the live search found.
    ///
    /// Gated on `ARBOT_CYCLE_INDEX_COMPARE`. The search stays authoritative —
    /// this only observes — because the cut-over question is whether the index
    /// is a SUPERSET of what the search surfaces. A miss means a profitable
    /// cycle exists that the precomputed set does not contain, and switching
    /// over would silently drop that trade. Cut over only once misses are 0
    /// across a long window.
    ///
    /// The index is rebuilt only when `structure_digest` changes, which is the
    /// whole point: adjacency is near-static while state churns every block.
    fn compare_cycle_index(
        &self,
        universe: &crate::cycle_index::PoolUniverse,
        changed_pools: &HashSet<Address>,
        graph: &Graph,
        found: &[crate::graph::CycleCandidate],
    ) {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits};

        let Ok(mut guard) = self.cycle_index.lock() else {
            return;
        };
        let stale = guard
            .as_ref()
            .map(|idx| idx.is_stale(universe))
            .unwrap_or(true);
        if stale {
            let hubs = self.flash_loan_hub_tokens(graph);
            let limits = CycleIndexLimits {
                min_hops: self.cycle_limits.min_hops,
                max_hops: self.cycle_limits.max_hops,
                ..CycleIndexLimits::default()
            };
            let began = Instant::now();
            let idx = CycleIndex::build(universe, &hubs, limits);
            info!(
                cycles = idx.len(),
                hubs = hubs.len(),
                pools = universe.pool_count(),
                pairs = universe.pair_count(),
                build_ms = began.elapsed().as_millis(),
                truncated = idx.truncated,
                "cycle index rebuilt (graph structure changed)"
            );
            *guard = Some(idx);
        }

        let Some(idx) = guard.as_ref() else {
            return;
        };
        if found.is_empty() {
            return;
        }

        let mut hits = 0usize;
        let mut misses: Vec<String> = Vec::new();
        for candidate in found {
            if idx.contains_nodes_in(graph, &candidate.cycle) {
                hits += 1;
            } else if misses.len() < 3 {
                let path: Vec<String> = candidate
                    .cycle
                    .iter()
                    .filter_map(|n| graph.nodes.get(*n))
                    .map(|a| format!("{a:#x}")[..10].to_string())
                    .collect();
                misses.push(format!("{}bps:{}", candidate.estimated_profit_bps, path.join(">")));
            }
        }
        let missed = found.len() - hits;

        // Selectivity: what the index would have re-priced this block, versus
        // the full search the graph actually paid for.
        // Selectivity: the point of the index. Measured against the pools that
        // ACTUALLY moved this block — an earlier version passed every active
        // pool, which answered "if the whole graph moved" and reported ~88%.
        let touched = idx
            .cycles_touching(universe.hops_for_pools(changed_pools))
            .len();

        if missed > 0 {
            warn!(
                found = found.len(),
                hits,
                missed,
                index_cycles = idx.len(),
                examples = ?misses,
                "cycle index MISSED cycles the search found; not safe to cut over"
            );
        } else {
            debug!(
                found = found.len(),
                hits,
                index_cycles = idx.len(),
                changed_pools = changed_pools.len(),
                touched,
                "cycle index covered every cycle the search found"
            );
        }
    }

    /// Flash-loan quotes for `token`, sized in that token's RAW UNITS.
    ///
    /// `min_in_token` / `max_in_token` MUST already be converted into `token`'s
    /// units by the caller. They were previously taken straight off
    /// `capital.{min,max}_flash_loan`, which are NATIVE-denominated (18dp) —
    /// mixing them with a raw token amount silently starved every non-18-decimal
    /// start token.
    ///
    /// Worked example, USDC (6dp), native min 0.1 WETH = 1e17:
    ///   capped = max_cycle_input.max(1e17) = 1e17 raw USDC = 100 BILLION USDC
    ///   capacity_capped_amount(available=5.59e10, requested=1e17, min=1e17)
    ///     -> capped = 5.59e10, and 5.59e10 >= 1e17 is FALSE -> None
    /// So every USDC-start cycle received zero providers regardless of the
    /// 55,886 USDC actually sitting in the Balancer vault. Measured:
    /// `no_flashloan_provider` was the single largest rejection reason,
    /// 2,286 of the last 6,000 candidate records.
    ///
    /// The identical bug was already found and fixed in the sizing path (see
    /// the `min_amount_in_start_token` conversion) — this call site was missed.
    fn flash_loan_quotes(
        &self,
        token: Address,
        max_cycle_input: U256,
        min_in_token: U256,
        max_in_token: U256,
    ) -> Vec<FlashLoanQuote> {
        let mut quotes = Vec::new();

        // Gate every path here rather than at hub selection alone. Cycles are
        // ROTATED to start at any token that has flash quotes (see the
        // `unfundable_anchors` pass), so filtering only the hub list let cbBTC
        // back in as a start via rotation — measured: 74 of 254 records after
        // the hub filter was applied. "May not start a cycle" and "has no flash
        // loan as a start token" are the same statement, so this is the honest
        // place for it.
        if let Some(allowed) = start_token_allowlist() {
            if !allowed.contains(&token) {
                return quotes;
            }
        }

        let upper = max_in_token.max(min_in_token);
        let capped_amount = max_cycle_input.min(upper).max(min_in_token);

        if capped_amount < min_in_token {
            return quotes;
        }
        let capital_min = min_in_token;

        let balancer_supported = self
            .bal_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(true);

        if balancer_supported {
            if let Some(max_amount) = self.capacity_capped(
                FlashLoanProvider::Balancer,
                token,
                capped_amount,
                capital_min,
                self.bal_flashloan_tokens.is_some(),
            ) {
                quotes.push(FlashLoanQuote {
                    provider: FlashLoanProvider::Balancer,
                    max_amount,
                    fee_bps: 0,
                    provider_addr: Some(self.bal_vault),
                });
            }
        }

        let aave_supported = self
            .aave_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(false);
        if aave_supported && self.aave_pool.is_some() {
            if let Some(max_amount) = self.capacity_capped(
                FlashLoanProvider::AaveV3,
                token,
                capped_amount,
                capital_min,
                self.aave_flashloan_tokens.is_some(),
            ) {
                quotes.push(FlashLoanQuote {
                    provider: FlashLoanProvider::AaveV3,
                    max_amount,
                    fee_bps: self.aave_fee_bps,
                    provider_addr: self.aave_pool,
                });
            }
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
        // A flash swap borrows from a specific pool, so it needs that pool's
        // address. Without one the plan builder substitutes `Address::zero()`
        // and the candidate is rejected as `no_flashloan_provider` — which is
        // why every univ2-flashswap token was structurally unfundable.
        if univ2_supported {
            if let Some(pool) = self.univ2_flash_pool {
                quotes.push(FlashLoanQuote {
                    provider: FlashLoanProvider::Univ2Flashswap,
                    max_amount: capped_amount,
                    fee_bps: self.univ2_flash_fee_bps,
                    provider_addr: Some(pool),
                });
            } else {
                warn!(
                    token = %format!("0x{}", hex::encode(token)),
                    "token is univ2-flashswap allowlisted but no flash pool is configured; \
                     set flashloans[kind=univ2_flashswap].pool"
                );
            }
        }

        let univ3_supported = self
            .univ3_flashloan_tokens
            .as_ref()
            .map(|set| set.contains(&token))
            .unwrap_or(false);
        // Same address requirement as univ2 above. The fee here is a flat bps of
        // principal (the pool's tier), and it must not be zero: `flash_fee` takes
        // `fee_bps == 0` literally and would price the loan as free.
        if univ3_supported {
            if let Some(pool) = self.univ3_flash_pool {
                quotes.push(FlashLoanQuote {
                    provider: FlashLoanProvider::Univ3Flash,
                    max_amount: capped_amount,
                    fee_bps: self.univ3_flash_fee_bps,
                    provider_addr: Some(pool),
                });
            } else {
                warn!(
                    token = %format!("0x{}", hex::encode(token)),
                    "token is univ3-flash allowlisted but no flash pool is configured; \
                     set flashloans[kind=univ3_flash].pool"
                );
            }
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

    /// RPC-heavy candidate preparation: validation, flash-loan quotes, sizing
    /// grid, and executor plan construction for one cycle. Safe to run
    /// concurrently across candidates (only shared reads; quote concurrency is
    /// bounded by the UniV3 semaphore). Rejections emit their candidate-stage
    /// logs and zero-loss metrics here, exactly as the sequential pipeline did.
    /// Handle to the immutable graph snapshot this runner publishes each scan.
    ///
    /// Readers get a consistent `Arc<Graph>`; the runner is the only writer.
    fn graph_snapshot(&self) -> Arc<StdMutex<Option<Arc<Graph>>>> {
        Arc::clone(&self.graph_snapshot)
    }

    /// The published pricing context, for readers running between scans.
    fn prep_context(&self) -> crate::base_fast::Published<PrepContextSnapshot> {
        Arc::clone(&self.prep_context)
    }

    fn token_native_prices(&self) -> crate::base_fast::Published<crate::base_fast::TokenPrices> {
        Arc::clone(&self.token_native_prices)
    }

    async fn prepare_candidate(
        &self,
        graph: &Graph,
        indexed: IndexedCycle,
        ctx: &CandidatePrepCtx<'_>,
    ) -> CandidatePrep {
        let cycle_ix = indexed.cycle;
        let edge_indices = indexed.edge_indices;
        if cycle_ix.len() < 2 {
            return CandidatePrep::Rejected { skip_detail: None };
        }
        let hops = cycle_ix.len().saturating_sub(1);
        if edge_indices.len() != hops {
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                None,
                cycle_ix.first().and_then(|ix| graph.nodes.get(*ix).copied()),
                Some(hops),
                ctx.edges_scanned,
                None,
                None,
                None,
                None,
                false,
                None,
                Some("edge_arity_mismatch"),
                None,
                false,
                false,
            );
            return CandidatePrep::Rejected { skip_detail: None };
        }
        let cycle_start_ix = match cycle_ix.first() {
            Some(ix) => *ix,
            None => return CandidatePrep::Rejected { skip_detail: None },
        };
        let cycle_start = graph.nodes[cycle_start_ix];
        let mut native_price = self.native_price_for(cycle_start, ctx.native_prices_map);
        if cycle_start == self.wrapped_native && !native_price.is_reliable() {
            let amount = U256::exp10(18);
            native_price = NativePrice::new(amount, amount, true);
        }
        let pricing_reliable =
            start_token_pricing_reliable(cycle_start, self.wrapped_native, native_price);
        let candidate_id =
            self.stage_candidate_id(cycle_start, &cycle_ix, graph, ctx.block_number, &[]);
        if !pricing_reliable {
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                Some(candidate_id),
                Some(cycle_start),
                Some(cycle_ix.len().saturating_sub(1)),
                ctx.edges_scanned,
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
            return CandidatePrep::Rejected {
                skip_detail: Some(format!(
                    "cycle start=0x{} rejected: unreliable native price for start token",
                    hex::encode(cycle_start)
                )),
            };
        }
        let Some(competition_buffer) =
            native_price.tokens_for_native_strict(ctx.competition_snapshot.extra_buffer_wei)
        else {
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                Some(candidate_id.clone()),
                Some(cycle_start),
                Some(cycle_ix.len().saturating_sub(1)),
                ctx.edges_scanned,
                None,
                None,
                Some(ctx.competition_snapshot.extra_buffer_wei),
                None,
                false,
                None,
                Some("unreliable_native_price_for_start_token"),
                None,
                false,
                false,
            );
            return CandidatePrep::Rejected { skip_detail: None };
        };
        if let Some(&last_ix) = cycle_ix.last() {
            debug_assert_eq!(
                graph.nodes[last_ix], cycle_start,
                "cycle must terminate at the starting token"
            );
        }

        let cycle_base_amount = ctx
            .base_profiles_map
            .get(&cycle_start)
            .map(|profile| profile.base_amount)
            .unwrap_or(ctx.capital_snapshot.base_amount);
        let mut estimated_cycle_gas: u64 = 0;
        let cycle_latency_secs = self.estimate_cycle_latency(graph, &cycle_ix);
        let mut cycle_edges_vec: Vec<Edge> = Vec::with_capacity(hops);
        let mut backrun_hint: Option<BackrunHint> = None;
        let mut has_bridge_step = false;
        for (hop, window) in cycle_ix.windows(2).enumerate() {
            let u = graph.nodes[window[0]];
            let v = graph.nodes[window[1]];
            let edge_idx = edge_indices[hop];
            let Some(edge) = graph.edge_by_index(edge_idx) else {
                warn!(edge_idx, from = %u, to = %v, "Skipping cycle due to stale edge index");
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(hops),
                    ctx.edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some("edge_index_out_of_range"),
                    None,
                    false,
                    false,
                );
                if let Some(metrics) = &self.metrics {
                    metrics.record_failure(U256::zero());
                }
                return CandidatePrep::Rejected { skip_detail: None };
            };
            // These two were one `invalid_edge` bucket, which made 67 rejections
            // undiagnosable. They are completely different events:
            //   * `edge_inactive` — the edge was deactivated (e.g. quarantined on
            //     block lag) between cycle discovery and prep. Routine churn.
            //   * `edge_endpoint_mismatch` — the index resolves to an edge
            //     connecting different tokens, i.e. it came from a DIFFERENT
            //     graph. That is the stale-index bug `best_edge_indices_for_node_path`
            //     exists to prevent, and it means a profitable cycle was silently
            //     discarded. Never routine; always worth investigating.
            let endpoint_mismatch = edge.from != u || edge.to != v;
            if !edge.active || endpoint_mismatch {
                let reason = if endpoint_mismatch {
                    "edge_endpoint_mismatch"
                } else {
                    "edge_inactive"
                };
                warn!(
                    edge_idx,
                    from = %u,
                    to = %v,
                    edge_from = %edge.from,
                    edge_to = %edge.to,
                    active = edge.active,
                    reason,
                    "Skipping cycle due to stale or mismatched edge index"
                );
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    Some(candidate_id.clone()),
                    Some(cycle_start),
                    Some(hops),
                    ctx.edges_scanned,
                    None,
                    None,
                    None,
                    None,
                    pricing_reliable,
                    None,
                    Some(reason),
                    None,
                    false,
                    false,
                );
                if let Some(metrics) = &self.metrics {
                    metrics.record_failure(U256::zero());
                }
                return CandidatePrep::Rejected { skip_detail: None };
            };
            estimated_cycle_gas = estimated_cycle_gas.saturating_add(edge.estimated_gas);
            if matches!(edge.venue, VenueEdge::Bridge { .. }) {
                has_bridge_step = true;
            }
            cycle_edges_vec.push(edge.clone());
        }

        // Per-hop capacities live in each hop's own input token, so they are
        // projected back to the start token along the cycle's quoted rates
        // before the tightest one is taken. Folding them with a bare `min()`
        // compared WETH wei against USDC's 6-decimal units and pinned every
        // cycle to a dust ceiling.
        let cycle_max_input =
            crate::graph::cycle_input_capacity(&cycle_edges_vec, cycle_base_amount)
                .min(cycle_base_amount);

        // Reject on arithmetic before spending a quote. The fee stack is known
        // from the graph alone, so a path that owes more than it could plausibly
        // earn never needs an RPC round-trip to be rejected.
        let fee_stack_bps = cycle_fee_stack_bps(&cycle_edges_vec);
        let fee_cap = max_cycle_fee_bps();
        if fee_cap > 0 && fee_stack_bps > fee_cap && !has_bridge_step {
            debug!(
                fee_stack_bps,
                fee_cap,
                hops,
                start = %format!("0x{}", hex::encode(cycle_start)),
                "pruned cycle: fee stack exceeds cap"
            );
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                Some(candidate_id.clone()),
                Some(cycle_start),
                Some(hops),
                ctx.edges_scanned,
                None,
                None,
                None,
                None,
                pricing_reliable,
                None,
                Some("fee_stack_too_high"),
                None,
                has_bridge_step,
                false,
            );
            return CandidatePrep::Rejected {
                skip_detail: Some(format!(
                    "cycle fee stack {fee_stack_bps}bps exceeds {fee_cap}bps cap"
                )),
            };
        }

        if has_bridge_step && !self.feature_gate.bridge {
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                Some(candidate_id.clone()),
                Some(cycle_start),
                Some(cycle_ix.len().saturating_sub(1)),
                ctx.edges_scanned,
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
            return CandidatePrep::Rejected {
                skip_detail: Some(
                    "cycle requires bridge step but FEATURE_BRIDGE=0".to_string(),
                ),
            };
        }

        if cycle_max_input.is_zero() {
            // `cycle_max_input = cycle_input_capacity(..).min(cycle_base_amount)`,
            // so zero has two very different causes and `no_liquidity` reports
            // both identically. Name which one fired: a zero BASE AMOUNT means
            // the start token has no sizing entry (not in `self.tokens`, or the
            // depth cache missed AND it could not be priced), which is a
            // pipeline gap. A zero CAPACITY means an edge genuinely cannot take
            // input. Only the second is about liquidity.
            debug!(
                target: "capacity",
                start = %format!("0x{}", hex::encode(cycle_start)),
                hops = cycle_ix.len().saturating_sub(1),
                %cycle_base_amount,
                projected = %crate::graph::cycle_input_capacity(
                    &cycle_edges_vec,
                    cycle_base_amount,
                ),
                cause = if cycle_base_amount.is_zero() {
                    "base_amount_zero (start token has no sizing entry)"
                } else {
                    "projected_capacity_zero (an edge cannot take input)"
                },
                // Per-hop inputs, so a dead edge (max_input=0) is instantly
                // distinguishable from an extreme rate product. Without these
                // the projection is a black box and the next person guesses.
                hops_detail = %cycle_edges_vec
                    .iter()
                    .map(|e| format!(
                        "{}->{}:max_in={},rate={}/{}",
                        hex::encode(&e.from[..3]),
                        hex::encode(&e.to[..3]),
                        e.max_input,
                        e.rate_num,
                        e.rate_den
                    ))
                    .collect::<Vec<_>>()
                    .join(" | "),
                "no_liquidity rejection"
            );
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                Some(candidate_id.clone()),
                Some(cycle_start),
                Some(cycle_ix.len().saturating_sub(1)),
                ctx.edges_scanned,
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
            return CandidatePrep::Rejected {
                skip_detail: Some(format!(
                    "cycle start=0x{} has zero capacity after slippage control",
                    hex::encode(cycle_start)
                )),
            };
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
                ctx.edges_scanned,
                None,
                None,
                None,
                None,
                pricing_reliable,
                None,
                Some("trade_cap_zero"),
                None,
                has_bridge_step,
                false,
            );
            return CandidatePrep::Rejected {
                skip_detail: Some(format!(
                    "cycle start=0x{} reduced to zero trade size",
                    hex::encode(cycle_start)
                )),
            };
        }

        // NATIVE -> start-token units before any comparison against a raw token
        // amount. `capital.{min,max}_flash_loan` are native-denominated; passing
        // them through unconverted is what zeroed every 6-decimal start token.
        // Fail closed when the start token cannot be priced — an unconverted
        // fallback would silently reinstate the bug.
        // Two very different failures used to share one label. "The start
        // token's native price could not be converted" is a PRICING gap and
        // says nothing about funding; "no provider had capacity" is a funding
        // answer. Measured 2026-09-03: 351 rejections carried this label after
        // the allowlist filter already removed the unfundable starts, and
        // there was no way to tell which of the two they were.
        let converted = native_price
            .tokens_for_native_strict(ctx.capital_snapshot.min_flash_loan)
            .zip(native_price.tokens_for_native_strict(ctx.capital_snapshot.max_flash_loan));
        let price_conversion_failed = converted.is_none();
        let quotes = match converted {
            Some((min_in_token, max_in_token)) => {
                self.flash_loan_quotes(cycle_start, trade_cap, min_in_token, max_in_token)
            }
            None => Vec::new(),
        };
        if quotes.is_empty() {
            // The three inputs that decide this, because reasoning about them
            // from the code has now been wrong twice. `trade_cap` is
            // `cycle_base_amount.min(cycle_input_capacity(edges))`, and the
            // rejection fires when it lands under `min_flash_loan` -- so these
            // say which of the two caps bound, and whether capacity was even
            // known.
            warn!(
                target: "flashcap",
                %trade_cap,
                %cycle_base_amount,
                %cycle_max_input,
                min_flash_loan = %ctx.capital_snapshot.min_flash_loan,
                price_conversion_failed,
                start = %format!("{cycle_start:#x}"),
                "no flash-loan quote; recording which cap bound"
            );
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                Some(candidate_id.clone()),
                Some(cycle_start),
                Some(cycle_ix.len().saturating_sub(1)),
                ctx.edges_scanned,
                None,
                None,
                None,
                None,
                pricing_reliable,
                None,
                Some(if price_conversion_failed {
                    // The token passed `pricing_reliable` but its native price
                    // still would not convert the native-denominated loan
                    // bounds into token units. That is a pricing defect, not a
                    // statement about any lender.
                    "no_flashloan_price_conversion"
                } else {
                    "no_flashloan_capacity"
                }),
                None,
                has_bridge_step,
                false,
            );
            return CandidatePrep::Rejected {
                skip_detail: Some(format!(
                    "cycle start=0x{} unsupported by flash loan providers",
                    hex::encode(cycle_start)
                )),
            };
        }

        let preview_plan = match build_plan_for_cycle(
            graph,
            &cycle_ix,
            cycle_base_amount,
            ctx.executor_address,
            self.jit_config.as_ref(),
            Some(self.quoter.as_ref()),
            ctx.block_number,
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
                    ctx.edges_scanned,
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
                // Pre-execution graph/plan errors are zero-loss and must NOT trip
                // the circuit breaker (former false-positive halt source). Only real
                // execution reverts/losses count, recorded at dispatch time.
                return CandidatePrep::Rejected { skip_detail: None };
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

        let pancakeswap_pool_set: HashSet<Address> = self
            .hot_pancakeswap_pools
            .read()
            .await
            .iter()
            .map(|pool| pool.pool)
            .collect();

        // `min_flash_loan` is NATIVE-denominated; `min_amount` is compared
        // against `upper_cap` in start-token raw units (src/sizing.rs). Passing
        // it unconverted floored a 6-decimal start token at 1e18 raw units — a
        // trillion USDC — so `upper_cap < min_amount` held for every USDC-start
        // cycle and sizing returned None 100% of the time. Convert into the
        // start token; fail closed if it cannot be priced.
        let Some(min_amount_in_start_token) = native_price
            .tokens_for_native_strict(ctx.capital_snapshot.min_flash_loan)
        else {
            return CandidatePrep::Rejected {
                skip_detail: Some(
                    "unreliable native price for start token: cannot convert min flash-loan bound"
                        .to_string(),
                ),
            };
        };
        let Some(sizing) = optimize_trade_size(OptimizeTradeParams {
            edges: &cycle_edges_vec,
            quotes: &quotes,
            min_amount: min_amount_in_start_token,
            max_amount: trade_cap,
            gas_price: ctx.gas_parameters.gas_price,
            estimated_gas: adjusted_cycle_gas,
            l1_data_fee: ctx.gas_parameters.l1_data_fee,
            native_price,
            quoter: self.quoter.as_ref(),
            slipstream_quoter: self.slipstream_quoter.as_deref(),
            pancakeswap_quoter: self.pancakeswap_quoter.as_deref(),
            pancakeswap_pools: if pancakeswap_pool_set.is_empty() {
                None
            } else {
                Some(&pancakeswap_pool_set)
            },
            bal_quote: self.bal_quote.as_ref(),
            curve_quote: self.curve_quote.as_ref(),
            block_number: ctx.block_number,
        })
        .await
        else {
            self.log_candidate_stage(
                "candidate_rejected_pre_sim",
                &self.chain_name,
                Some(candidate_id.clone()),
                Some(cycle_start),
                Some(cycle_ix.len().saturating_sub(1)),
                ctx.edges_scanned,
                None,
                None,
                None,
                None,
                pricing_reliable,
                None,
                Some("no_profitable_size"),
                None,
                has_bridge_step,
                false,
            );
            return CandidatePrep::Rejected {
                skip_detail: Some(format!(
                    "cycle start=0x{} had no profitable sizing",
                    hex::encode(cycle_start)
                )),
            };
        };

        let trade_amount = sizing.amount_in;
        let plan = match build_plan_for_cycle(
            graph,
            &cycle_ix,
            trade_amount,
            ctx.executor_address,
            self.jit_config.as_ref(),
            Some(self.quoter.as_ref()),
            ctx.block_number,
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
                    ctx.edges_scanned,
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
                // Pre-execution graph/plan errors are zero-loss and must NOT trip
                // the circuit breaker (former false-positive halt source). Only real
                // execution reverts/losses count, recorded at dispatch time.
                return CandidatePrep::Rejected { skip_detail: None };
            }
        };

        CandidatePrep::Sized(Box::new(SizedCandidate {
            cycle_ix,
            candidate_id,
            cycle_start,
            native_price,
            pricing_reliable,
            competition_buffer,
            cycle_latency_secs,
            cycle_edges_vec,
            has_bridge_step,
            backrun_hint,
            adjusted_cycle_gas,
            sizing,
            plan,
            trade_amount,
        }))
    }

    /// Current canonical head as `(base_fee, number)`.
    ///
    /// Fast path: when the websocket-fed head in `block_head_rx` is strictly
    /// ahead of the last block we scanned, trust it directly — it already
    /// carries the block number and base fee, removing a synchronous
    /// `get_block(Latest)` HTTP round-trip from the most latency-sensitive point
    /// of the block race (the instant a new head lands and we sprint to submit).
    ///
    /// Liveness guard: if the websocket head is NOT ahead of `last_scanned`
    /// (channel still at the zero default, WS disabled, or — critically — the
    /// feed has gone zombie/laggy and frozen at an old head), fall back to an
    /// authoritative `get_block(Latest)`. This preserves the pre-websocket
    /// behaviour of reading the true tip every idle turn, so a silently stalled
    /// newHeads subscription can never freeze the head and make the bot stop
    /// trading. Fails closed on a missing/zero head exactly as the direct fetch
    /// did.
    async fn current_block_head(&self, last_scanned: Option<U64>) -> Result<(Option<U256>, U64)> {
        if let Some(rx) = &self.block_head_rx {
            let head = *rx.lock().await.borrow();
            if !head.number.is_zero() && last_scanned.is_none_or(|ls| head.number > ls) {
                return Ok((head.base_fee_per_gas, head.number));
            }
        }
        match self.provider.get_block(BlockNumber::Latest).await {
            Ok(Some(block)) => {
                let number = block.number.unwrap_or_default();
                if number.is_zero() {
                    return Err(anyhow!(
                        "rpc returned latest block with no number; refusing to scan on stale state (fail-closed)"
                    ));
                }
                Ok((block.base_fee_per_gas, number))
            }
            Ok(None) => Err(anyhow!(
                "rpc returned no latest block; refusing to scan on stale state (fail-closed)"
            )),
            Err(err) => {
                warn!(error = %err, "Failed to fetch latest block; failing closed");
                Err(anyhow::Error::new(err)
                    .context("fetch latest block (rpc); refusing to scan on stale state"))
            }
        }
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
        let expected_univ3_edges =
            expected_univ3_edge_upper_bound(self.hot_univ3_pools.read().await.len());

        // FAIL CLOSED: the latest block number is a required freshness signal. It
        // drives stale-edge quarantine (max_quote_block_lag) and congestion limits.
        // If we cannot obtain a non-zero head, we must NOT scan or broadcast on
        // unknown/stale state. Returning an rpc-classified error aborts this cycle
        // and lets the run loop back off and retry instead of trading blind.
        //
        // Block-driven cadence: rebuilding the full graph against an unchanged
        // head re-quotes identical chain state for zero information gain. When
        // the head has not advanced since the previous scan, poll cheaply for a
        // new block instead of re-running the populate/quote pipeline, unless a
        // live mempool backrun hint justifies a same-block rescan.
        // These scan-cadence tunables are fixed at startup; resolve each once
        // instead of re-reading the environment on every scan.
        let rescan_same_block = {
            static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *V.get_or_init(|| {
                read_feature_flag("ARBOT_RESCAN_SAME_BLOCK", false)
            })
        };
        let block_poll_interval = {
            static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
            *V.get_or_init(|| {
                Duration::from_millis(
                    crate::util::env_parse_opt::<u64>("ARBOT_BLOCK_POLL_MS")
                        .filter(|ms| *ms > 0)
                        .unwrap_or(150),
                )
            })
        };
        let new_block_wait = {
            static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
            *V.get_or_init(|| {
                Duration::from_millis(
                    crate::util::env_parse_opt::<u64>("ARBOT_NEW_BLOCK_WAIT_MS")
                        .filter(|ms| *ms > 0)
                        .unwrap_or(4_000),
                )
            })
        };
        let block_wait_start = Instant::now();
        // The last block this runner scanned is stable for the duration of this
        // call (only written after the loop), so read it once. It drives both
        // the "already scanned" cadence gate and the websocket-head freshness
        // guard in current_block_head (a head at/behind this is treated as
        // stale and forces an authoritative get_block(Latest)).
        let last_scanned = { *self.last_scanned_block.lock().await };
        let (base_fee, block_number) = loop {
            // Prefer the websocket-fed head (number + base fee) when it is ahead
            // of the last scanned block; otherwise fall back to get_block(Latest)
            // so a frozen/laggy feed can never stall trading. Fail closed on a
            // missing/zero head. See current_block_head.
            let fetched = self.current_block_head(last_scanned).await?;
            if rescan_same_block {
                break fetched;
            }
            let already_scanned = last_scanned == Some(fetched.1);
            if !already_scanned {
                break fetched;
            }
            if let Some(monitor) = &self.backrun {
                if !monitor.active_hints(Duration::from_secs(45)).await.is_empty() {
                    break fetched;
                }
            }
            if block_wait_start.elapsed() >= new_block_wait {
                return Ok(ScanOutcome::NotProfitable {
                    reason: format!(
                        "block {} already scanned; no new head within {}ms",
                        fetched.1,
                        new_block_wait.as_millis()
                    ),
                    edges: 0,
                    expected_univ3_edges,
                });
            }
            if let Some(rx) = &self.block_head_rx {
                let remaining = new_block_wait.saturating_sub(block_wait_start.elapsed());
                let head_wait = {
                    let mut guard = rx.lock().await;
                    timeout(remaining, guard.changed()).await
                };
                match head_wait {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => sleep(block_poll_interval).await,
                    Err(_) => sleep(block_poll_interval).await,
                }
            } else {
                sleep(block_poll_interval).await;
            }
        };
        {
            let mut guard = self.last_scanned_block.lock().await;
            *guard = Some(block_number);
        }

        // Read what each flash-loan provider can actually lend at this head,
        // before any candidate is sized against it.
        self.refresh_flash_capacity(block_number.as_u64()).await;

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

        // Risk policy fee cap is a hard ceiling: congestion scaling must never
        // raise the effective cap above the declared per-chain limit.
        if let Some(cap) = self
            .risk_policy
            .as_ref()
            .and_then(|policy| policy.max_fee_per_gas_cap)
        {
            max_gas_threshold = max_gas_threshold.min(cap);
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
        let t_native = Instant::now();
        let native_prices_map = Arc::new(
            self.load_native_prices(tokens.as_ref(), token_decimals_map.as_ref(), block_number)
                .await,
        );
        let native_ms = t_native.elapsed().as_millis() as u64;
        let base_profiles_map = Arc::new(
            self.compute_base_amounts(
                token_decimals_map.as_ref(),
                &capital_snapshot,
                native_prices_map.as_ref(),
            )
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
        let t_lowliq = Instant::now();
        let low_liquidity_pools = if let Some(scanner) = &self.low_liquidity {
            let mut guard = scanner.lock().await;
            guard.poll(token_decimals_map.as_ref()).await?
        } else {
            Vec::new()
        };
        let lowliq_ms = t_lowliq.elapsed().as_millis() as u64;
        let base_token_whitelist = self.tokens.current_set();
        let hot_univ2_tokens = self.hot_univ2_pools.read().await.clone();
        let hot_univ3_tokens = self.hot_univ3_pools.read().await.clone();
        let hot_pancake_tokens = self.hot_pancakeswap_pools.read().await.clone();
        let mut hot_cl_tokens = hot_univ3_tokens;
        hot_cl_tokens.extend(hot_pancake_tokens);
        let raw_token_whitelist_cap = {
            static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            *V.get_or_init(|| {
                crate::util::env_parse_opt::<usize>("TOKEN_WHITELIST_MAX")
                    .unwrap_or(512)
            })
        };
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
            &hot_cl_tokens,
            base_token_whitelist.as_ref(),
            &self.mandatory_universe_tokens,
            self.dynamic_top_tokens_30d,
            token_whitelist_cap,
        ));
        let t_populate = Instant::now();
        // Pools that actually moved this block. `populate_cache.touched_pools`
        // is cleared once populate finishes, so keep a copy for downstream
        // instrumentation that runs after it.
        let changed_pools: HashSet<Address>;
        {
            let mut touched = HashSet::new();
            if let Some(monitor) = &self.pool_monitor {
                touched.extend(monitor.drain_touched());
            }
            if let Some(monitor) = &self.backrun {
                let hints = monitor.active_hints(Duration::from_secs(45)).await;
                // `hint.from`/`hint.to` are TOKENS, and `post_state_from_hint`
                // leaves `pool` zeroed — hints carry no pool identity. Putting
                // tokens in a pool set does not merely fail to match: it makes
                // the set non-empty, which flips populate to incremental with a
                // filter that matches nothing, dropping every CL re-quote for
                // that scan. Resolve to real pools, or contribute nothing.
                if !hints.is_empty() {
                    let universe = self.pool_universe().await;
                    for hint in &hints {
                        for pool in universe.pools_for_hop(hint.from, hint.to) {
                            touched.insert(*pool);
                        }
                    }
                }
            }
            {
                let guard = self.populate_cache.lock().await;
                for edge in guard.cached_edges.iter() {
                    if let Some(quote_block) = edge.quote_block {
                        if block_number.saturating_sub(quote_block) > self.max_quote_block_lag {
                            if let Some(pool) = venues::edge_pool_address(edge) {
                                touched.insert(pool);
                            }
                        }
                    }
                }
            }
            changed_pools = touched.clone();
            let mut guard = self.populate_cache.lock().await;
            guard.touched_pools = touched;
        }
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
        let populate_ms = t_populate.elapsed().as_millis() as u64;
        {
            let mut guard = self.populate_cache.lock().await;
            guard.cached_edges = edges.clone();
            guard.last_digest = Some(venues::edge_digest(&edges));
            guard.touched_pools.clear();
        }

        let t_liq = Instant::now();
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
        let liq_ms = t_liq.elapsed().as_millis() as u64;
        // RPC calls consumed by THIS scan. This is the number that decides what
        // provider throughput the engine needs: rpc_calls / target_scan_seconds.
        let rpc_calls = {
            let now = crate::rpc_failover::total_rpc_requests();
            let prev = self
                .last_scan_rpc_total
                .swap(now, std::sync::atomic::Ordering::Relaxed);
            now.saturating_sub(prev)
        };
        info!(
            target: "arb_exec",
            chain = %self.chain_name,
            native_ms,
            lowliq_ms,
            populate_ms,
            liq_ms,
            rpc_calls,
            "scan phase timing breakdown"
        );

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
            &self.hub_tokens,
        )
        .await;

        graph.refresh_incremental_adjacency_with_metrics(
            self.metrics.as_deref(),
            Some(&self.chain_name),
        );

        for edge in graph.edges.iter_mut() {
            if let Some(quote_block) = edge.quote_block {
                if block_number.saturating_sub(quote_block) > self.max_quote_block_lag {
                    edge.active = false;
                }
            }
        }

        let edges_scanned = graph.edges.iter().filter(|edge| edge.active).count();
        // Publish before the search: readers want the priced graph, and a
        // snapshot taken after the search would be one scan stale by the time
        // anyone read it.
        if let Ok(mut g) = self.graph_snapshot.lock() {
            *g = Some(Arc::new(graph.clone()));
        }
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
            /// Same plan with every per-hop `min_out` removed. DIAGNOSTIC ONLY —
            /// never dispatched. When the real plan reverts with `Too little
            /// received` the router reports no amounts, so this variant is
            /// simulated to observe what the pools actually pay and measure the
            /// shortfall instead of guessing at it.
            relaxed_plan_args: ExecutorPlan,
            cycle_start: Address,
            amount_in: U256,
            est_gross_after_fee: U256,
            flash_fee_amount: U256,
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
            backrun_hint: Option<BackrunHint>,
            liquidation_markets: Vec<String>,
            strategy: Strategy,
            venue_path: Vec<String>,
            candidate_id: String,
        }

        let mut ranked_candidates: Vec<CandidatePlan> = Vec::new();
        let sim_cascade_cap = sim_cascade_depth();
        let executor_address = self.executor.address();
        let backrun_hints = if let Some(monitor) = &self.backrun {
            monitor.active_hints(Duration::from_secs(45)).await
        } else {
            Vec::new()
        };

        let start_priorities =
            self.compute_start_priorities(&graph, base_profiles_map.as_ref(), &backrun_hints);
        let seed_cycles: Vec<IndexedCycle> = {
            let guard = self.previous_cycle_seeds.lock().await;
            guard
                .iter()
                .filter_map(|seed| {
                    let cycle = map_cycle_addresses_to_indices(&graph, &seed.addresses)?;
                    // Re-resolve the hop edges against THIS scan's graph. The
                    // graph is rebuilt from scratch every scan and its edge
                    // vector is repopulated in a different order (concurrent
                    // venue collectors, varying pool counts), so an edge index
                    // from the previous scan almost never denotes the same edge.
                    // Carrying them forward made every seeded candidate fail the
                    // `edge.from != u` check in candidate prep and get dropped
                    // SILENTLY (skip_detail: None) — which is why a cycle with a
                    // positive gross edge was discarded on 67 of 68 scans while
                    // the log only ever said "found no viable cycles".
                    let edge_indices = graph.best_edge_indices_for_node_path(&cycle)?;
                    Some(IndexedCycle {
                        cycle,
                        edge_indices,
                    })
                })
                .collect()
        };
        let search_start = Instant::now();
        let run_full_bf = !self.bf_skip_on_stable_graph
            || significant_change
            || seed_cycles.is_empty()
            || block_number.as_u64().is_multiple_of(8)
            || !backrun_hints.is_empty();
        let mut best_gross_scaled: Option<i64> = None;
        let mut raw_cycles: Vec<IndexedCycle> = if run_full_bf {
            let (found, best) = tokio::task::block_in_place(|| {
                // A/B switch (`ARBOT_HUB_SEARCH=1`). Every executable cycle has
                // to start and end at a flash-loan asset, so the hub-anchored
                // walk enumerates exactly that set instead of asking
                // Bellman-Ford to rediscover it from generic negative-cycle
                // detection. Both paths return the same candidate type, so the
                // rest of the scan is untouched and the two can be compared on
                // identical graph state.
                if self.hub_search_enabled {
                    let hubs = self.flash_loan_hub_tokens(&graph);
                    let limits = crate::graph::HubSearchLimits {
                        min_hops: self.cycle_limits.min_hops,
                        max_hops: self.cycle_limits.max_hops,
                        max_cycles: self.cycle_limits.max_cycles,
                        timeout: self.cycle_limits.timeout,
                        parallel_edges_per_pair: self.hub_search_parallel_edges,
                    };
                    let found =
                        graph.hub_anchored_cycles(&hubs, &limits, self.max_candidate_paths);
                    let best = found.first().map(|c| c.estimated_profit_bps);
                    debug!(
                        hubs = hubs.len(),
                        cycles = found.len(),
                        best_bps = ?best,
                        "hub-anchored search complete"
                    );
                    (found, best)
                } else {
                    graph.bellman_ford_diagnostic(
                        &start_priorities,
                        &self.cycle_limits,
                        self.max_candidate_paths,
                        self.metrics.as_deref(),
                    )
                }
            });
            best_gross_scaled = best;

            // Observation only: the live search above stays authoritative.
            // Gated so a bad index can never affect detection while we build
            // confidence that it covers everything the search finds.
            if read_feature_flag("ARBOT_CYCLE_INDEX_COMPARE", false) {
                // Structure comes from the pool INVENTORY, not the realised edge
                // set: a pool whose quote timed out this scan is still part of
                // the topology, and treating it as a structure change was what
                // made the index rebuild on half of all scans.
                let universe = self.pool_universe().await;
                self.compare_cycle_index(&universe, &changed_pools, &graph, &found);
            }

            let found_total = found.len();
            let filter_unfundable = crate::util::env_parse_opt::<u8>("ARBOT_FILTER_UNFUNDABLE")
                .map(|v| v != 0)
                .unwrap_or(true);
            let mut unfundable_anchors: HashMap<Address, usize> = HashMap::new();
            for candidate in found.iter() {
                if let Some(start) = candidate
                    .cycle
                    .first()
                    .and_then(|ix| graph.nodes.get(*ix).copied())
                {
                    let any_fundable = candidate
                        .cycle
                        .iter()
                        .filter_map(|ix| graph.nodes.get(*ix).copied())
                        .any(|token| self.can_flash_fund(token));
                    if !any_fundable {
                        *unfundable_anchors.entry(start).or_insert(0) += 1;
                    }
                }
            }
            if !unfundable_anchors.is_empty() {
                let mut top: Vec<_> = unfundable_anchors.iter().collect();
                top.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
                debug!(
                    found_total,
                    distinct_unfundable_anchors = top.len(),
                    top_anchor = %format!("0x{}", hex::encode(top[0].0)),
                    top_anchor_cycles = top[0].1,
                    "unfundable anchor census"
                );
            }
            let mut cycles: Vec<IndexedCycle> = found
            .into_iter()
            .filter(|candidate| {
                if !filter_unfundable {
                    return true;
                }
                // A cycle is a LOOP: its Bellman-Ford anchor is arbitrary, and
                // `rotate_indexed_cycle` downstream already re-anchors it at a
                // fundable token. Testing only `cycle.first()` therefore threw
                // away cycles that were perfectly fundable from another node —
                // measured live, it discarded a WETH/0xcb327b99 two-hop showing
                // 332bps gross, because BF happened to anchor it at the token
                // without a flash-loan route rather than at WETH.
                //
                // Keep the cycle if ANY node in it can be funded; rotation picks
                // the viable start. This is the weakest correct form of the
                // filter, which is the only safe direction for a predictive one.
                candidate
                    .cycle
                    .iter()
                    .filter_map(|ix| graph.nodes.get(*ix).copied())
                    .any(|token| self.can_flash_fund(token))
            })
            .map(|candidate| IndexedCycle {
                cycle: candidate.cycle,
                edge_indices: candidate.edge_indices,
            })
            .collect();
            if found_total > cycles.len() {
                debug!(
                    dropped = found_total - cycles.len(),
                    kept = cycles.len(),
                    "filtered cycles anchored at unfundable start tokens"
                );
            }
            if !seed_cycles.is_empty() {
                cycles.splice(0..0, seed_cycles.clone());
            }
            cycles
        } else {
            seed_cycles
        };

        if !backrun_hints.is_empty()
            && backrun_post_state_enabled()
        {
            let backrun_limits = targeted_bf_limits();
            let state_hints: Vec<crate::backrun_state::BackrunHint> = backrun_hints
                .iter()
                .map(|h| crate::backrun_state::BackrunHint {
                    from: h.from,
                    to: h.to,
                    amount_in: h.amount_in,
                    price_impact_bps: h.price_impact_bps,
                    source: h.source.clone(),
                })
                .collect();
            let mut touched = HashSet::new();
            apply_post_state_hints(&mut graph, &state_hints, &mut touched);
            if let Some(monitor) = &self.pool_monitor {
                for pool in touched {
                    monitor.mark_touched(pool);
                }
            }
            let backrun_cycles: Vec<IndexedCycle> = tokio::task::block_in_place(|| {
                graph.bellman_ford(
                    &start_priorities,
                    &backrun_limits,
                    10,
                    self.metrics.as_deref(),
                )
            })
            .into_iter()
            .map(|candidate| IndexedCycle {
                cycle: candidate.cycle,
                edge_indices: candidate.edge_indices,
            })
            .collect();
            log_backrun_opportunity(
                backrun_hints[0].from,
                backrun_hints[0].to,
                backrun_hints[0].amount_in,
                backrun_hints[0].price_impact_bps,
                backrun_hints[0].source.as_str(),
                &backrun_cycles,
            );
            for indexed in backrun_cycles {
                if !raw_cycles.iter().any(|existing| {
                    canonicalize_cycle(existing.cycle.clone())
                        == canonicalize_cycle(indexed.cycle.clone())
                }) {
                    raw_cycles.push(indexed);
                }
            }
        }

        let mut canonical_order: Vec<Vec<usize>> = Vec::new();
        let mut canonical_buckets: HashMap<Vec<usize>, Vec<(usize, IndexedCycle)>> =
            HashMap::new();

        for mut indexed in raw_cycles {
            let cycle = &mut indexed.cycle;
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
                    entries.push((start_ix, indexed));
                }
            } else {
                canonical_order.push(signature.clone());
                canonical_buckets.insert(signature, vec![(start_ix, indexed)]);
            }
        }

        let mut candidate_cycles: Vec<IndexedCycle> = Vec::new();
        for signature in canonical_order {
            if let Some(mut entries) = canonical_buckets.remove(&signature) {
                let mut added = false;
                let mut fallback: Option<IndexedCycle> = None;

                for (start_ix, indexed) in entries.drain(..) {
                    let start_token = graph.nodes[start_ix];
                    let rotated = if indexed.edge_indices_valid() {
                        rotate_indexed_cycle(
                            &indexed.cycle,
                            &indexed.edge_indices,
                            start_ix,
                        )
                        .unwrap_or(indexed)
                    } else {
                        IndexedCycle {
                            cycle: indexed.cycle,
                            edge_indices: Vec::new(),
                        }
                    };
                    if !self
                        .flash_loan_quotes(start_token, capital_snapshot.max_flash_loan, U256::zero(), U256::MAX)
                        .is_empty()
                    {
                        candidate_cycles.push(rotated);
                        added = true;
                    } else if fallback.is_none() {
                        fallback = Some(rotated);
                    }
                }

                if !added {
                    if let Some(indexed) = fallback {
                        // Bellman-Ford emits a cycle anchored wherever its
                        // relaxation happened to close the loop, and the loop
                        // above can only try the anchors BF chose to emit. When
                        // none of them is fundable the cycle used to be pushed
                        // as-is and rejected downstream as
                        // `no_flashloan_provider` — even when another node in
                        // the very same loop was borrowable.
                        //
                        // Measured live on Base: a WETH/0xcb327b99 two-hop
                        // showing 332bps gross (vs a ~60bps fee stack) was
                        // discarded on every scan purely because BF anchored it
                        // at the token without a flash-loan route instead of at
                        // WETH. A cycle is a loop; the anchor is a detail of how
                        // it was discovered, not a property of the trade.
                        //
                        // So rotate it ourselves to the first fundable node
                        // before giving up. Rotation needs valid edge indices;
                        // without them the shape cannot be re-derived and the
                        // original is kept so behaviour is never worse.
                        let rescued = if indexed.edge_indices_valid() {
                            indexed
                                .cycle
                                .iter()
                                .filter_map(|node_ix| {
                                    let token = *graph.nodes.get(*node_ix)?;
                                    if self
                                        .flash_loan_quotes(
                                            token,
                                            capital_snapshot.max_flash_loan,
                                            U256::zero(),
                                            U256::MAX,
                                        )
                                        .is_empty()
                                    {
                                        return None;
                                    }
                                    rotate_indexed_cycle(
                                        &indexed.cycle,
                                        &indexed.edge_indices,
                                        *node_ix,
                                    )
                                    .map(|rotated| (rotated, token))
                                })
                                .next()
                        } else {
                            None
                        };
                        match rescued {
                            Some((rotated, token)) => {
                                debug!(
                                    start = %format!("0x{}", hex::encode(token)),
                                    hops = rotated.cycle.len().saturating_sub(1),
                                    "re-anchored cycle at a fundable token"
                                );
                                candidate_cycles.push(rotated);
                            }
                            None => candidate_cycles.push(indexed),
                        }
                    }
                }
            }
        }

        for indexed in candidate_cycles.iter() {
            if cycle_rejected_by_hub_filter(&indexed.cycle, &graph, &self.hub_tokens) {
                let start_token = indexed
                    .cycle
                    .first()
                    .and_then(|ix| graph.nodes.get(*ix))
                    .copied();
                let candidate_id = start_token.map(|token| {
                    self.stage_candidate_id(token, &indexed.cycle, &graph, block_number, &[])
                });
                self.log_candidate_stage(
                    "candidate_rejected_pre_sim",
                    &self.chain_name,
                    candidate_id,
                    start_token,
                    Some(indexed.cycle.len().saturating_sub(1)),
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
                .map(|indexed| CycleSeed {
                    addresses: cycle_indices_to_addresses(&graph, &indexed.cycle),
                })
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
        // Candidate preparation (validation, flash quotes, sizing grids, plan
        // construction) is RPC-bound and previously ran strictly sequentially:
        // with tight quote budgets only the first candidate was ever sized and
        // the hard budget break silently dropped the rest. Prepare candidates
        // concurrently (order-preserving, so the best-ranked cycles from the
        // search phase are still evaluated first), then run the cheap
        // threshold/gas pipeline over the sized results.
        let prep_deadline = quote_start + self.quote_budget;
        let prep_ctx = CandidatePrepCtx {
            native_prices_map: native_prices_map.as_ref(),
            base_profiles_map: base_profiles_map.as_ref(),
            capital_snapshot: &capital_snapshot,
            competition_snapshot: &competition_snapshot,
            gas_parameters: &gas_parameters,
            executor_address,
            block_number,
            edges_scanned,
        };
        // Native value per raw token unit, for readers ranking across tokens.
        // Unreliable entries are dropped rather than defaulted: NativePrice's
        // "no information" sentinel is a 1:1 rate, and publishing that would
        // make every unpriced token look like the native asset.
        if let Ok(mut p) = self.token_native_prices.lock() {
            let mut out: crate::base_fast::TokenPrices =
                HashMap::with_capacity(native_prices_map.len());
            for (token, np) in native_prices_map.iter() {
                if !np.is_reliable() {
                    continue;
                }
                let (t, n) = (
                    np.token_amount.to_string().parse::<f64>(),
                    np.native_amount.to_string().parse::<f64>(),
                );
                if let (Ok(t), Ok(n)) = (t, n) {
                    if t > 0.0 && n > 0.0 && (n / t).is_finite() {
                        out.insert(*token, n / t);
                    }
                }
            }
            *p = Some(Arc::new(out));
        }
        // Published for the fast path, which prepares candidates between scans
        // and cannot rebuild any of this itself -- it is all RPC-derived.
        if let Ok(mut c) = self.prep_context.lock() {
            *c = Some(Arc::new(PrepContextSnapshot {
                native_prices_map: Arc::clone(&native_prices_map),
                base_profiles_map: Arc::clone(&base_profiles_map),
                capital_snapshot,
                competition_snapshot: competition_snapshot.clone(),
                gas_parameters: gas_parameters.clone(),
                executor_address,
                block_number,
            }));
        }
        let prep_ctx_ref = &prep_ctx;
        let graph_ref = &graph;
        let prepared_candidates: Vec<CandidatePrep> = stream::iter(
            candidate_cycles.into_iter().map(|indexed| async move {
                if Instant::now() >= prep_deadline {
                    return CandidatePrep::Budgeted;
                }
                self.prepare_candidate(graph_ref, indexed, prep_ctx_ref)
                    .await
            }),
        )
        .buffered(candidate_prep_concurrency())
        .collect()
        .await;
        let budget_skipped = prepared_candidates
            .iter()
            .filter(|prep| matches!(prep, CandidatePrep::Budgeted))
            .count();
        if budget_skipped > 0 {
            warn!(
                elapsed_ms = quote_start.elapsed().as_millis(),
                budget_ms = self.quote_budget.as_millis(),
                skipped = budget_skipped,
                "quote budget exhausted during candidate sizing; lower-ranked candidates skipped"
            );
        }

        let mut evaluated_sized = 0usize;
        for prep in prepared_candidates {
            let sized = match prep {
                CandidatePrep::Budgeted => continue,
                CandidatePrep::Rejected { skip_detail } => {
                    if let Some(detail) = skip_detail {
                        last_skip_reason = Some(detail);
                    }
                    continue;
                }
                CandidatePrep::Sized(sized) => sized,
            };
            // Always evaluate at least one sized candidate; afterwards stop
            // once the post-sizing pipeline has consumed twice the quote
            // budget (gas estimation below costs one RPC round-trip each).
            if evaluated_sized > 0
                && quote_start.elapsed() > self.quote_budget.saturating_mul(2)
            {
                warn!(
                    elapsed_ms = quote_start.elapsed().as_millis(),
                    budget_ms = self.quote_budget.as_millis(),
                    "post-sizing budget exceeded; skipping remaining sized candidates"
                );
                break;
            }
            evaluated_sized += 1;
            let SizedCandidate {
                cycle_ix,
                candidate_id,
                cycle_start,
                native_price,
                pricing_reliable,
                competition_buffer,
                cycle_latency_secs,
                cycle_edges_vec,
                has_bridge_step,
                backrun_hint,
                adjusted_cycle_gas,
                sizing,
                plan,
                trade_amount,
            } = *sized;
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

            // Risk policy: declared slippage / price-impact tolerances are
            // hard limits, not advisory. sizing.max_slippage_bps is the sized
            // price impact estimate from the quote grid.
            if let Some(reason) =
                self.risk_slippage_violation(swap_slippage_bps, sizing.max_slippage_bps)
            {
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
                    Some(reason),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} rejected by risk policy: {} (swap_slippage_bps={}, price_impact_bps={})",
                    hex::encode(cycle_start),
                    reason,
                    swap_slippage_bps,
                    sizing.max_slippage_bps
                ));
                continue;
            }

            let mut min_profit_requirement =
                self.unified_min_profit_threshold(UnifiedThresholdParams {
                    fee: &gas_parameters,
                    est_gas: adjusted_cycle_gas,
                    est_gross_after_fee,
                    flash_fee_amount,
                    slippage_floor,
                    competition_buffer,
                    has_bridge_step,
                    backrun_hint: backrun_hint.as_ref(),
                    congestion: congestion_multiplier,
                    competition: &competition_snapshot,
                    latency_secs: cycle_latency_secs,
                    native_price,
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
            let relaxed_plan = plan.with_relaxed_min_outs();
            let ops = encode_plan_steps(plan.steps);
            let relaxed_ops = encode_plan_steps(relaxed_plan.steps);

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
                loans: loans.clone(),
                cycle_slippage_bps: swap_slippage_bps.min(u32::from(u16::MAX)) as u16,
                steps: ops,
                min_profit: min_profit_requirement,
            };
            // `min_profit` is zeroed too: the executor enforces it on top of the
            // per-hop floors, and leaving it would make the diagnostic revert for
            // a different reason than the one being measured.
            let relaxed_plan_args = ExecutorPlan {
                loans,
                cycle_slippage_bps: swap_slippage_bps.min(u32::from(u16::MAX)) as u16,
                steps: relaxed_ops,
                min_profit: U256::zero(),
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

            // Risk policy: per-tx gas-unit cap and fee-per-gas ceiling are
            // enforced against the real (estimated) fee profile, not just the
            // scan-level baseline.
            if let Some(reason) = self.risk_gas_violation(&fee_estimate, gas_units) {
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
                    None,
                    pricing_reliable,
                    Some(min_profit_requirement),
                    Some(reason),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} rejected by risk policy: {} (gas_units={}, max_fee_per_gas={:?})",
                    hex::encode(cycle_start),
                    reason,
                    gas_units,
                    fee_estimate.max_fee_per_gas
                ));
                continue;
            }

            min_profit_requirement = self.unified_min_profit_threshold(UnifiedThresholdParams {
                fee: &fee_estimate,
                est_gas: gas_units,
                est_gross_after_fee,
                flash_fee_amount,
                slippage_floor,
                competition_buffer,
                has_bridge_step,
                backrun_hint: backrun_hint.as_ref(),
                congestion: congestion_multiplier,
                competition: &competition_snapshot,
                latency_secs: cycle_latency_secs,
                native_price,
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
            // Risk policy: the declared net-profit floor (USD/native from ops
            // inputs) is enforced on estimated NET profit, not just gross.
            let risk_net_floor = self
                .risk_min_net_profit_tokens(native_price)
                .unwrap_or_else(U256::zero);
            if net_profit.is_zero() || net_profit < risk_net_floor {
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
                    Some(min_profit_requirement.max(risk_net_floor)),
                    Some("below_min_profit_threshold"),
                    None,
                    has_bridge_step,
                    false,
                );
                last_skip_reason = Some(format!(
                    "cycle start=0x{} net profit {} below floor (risk floor {})",
                    hex::encode(cycle_start),
                    net_profit,
                    risk_net_floor
                ));
                continue;
            }

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
            let candidate_plan = CandidatePlan {
                    plan_args,
                    relaxed_plan_args,
                    cycle_start,
                    amount_in: trade_amount,
                    est_gross_after_fee,
                    flash_fee_amount,
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
                    backrun_hint,
                    liquidation_markets,
                    strategy,
                    venue_path,
                    candidate_id,
                };
            ranked_candidates.push(candidate_plan);
            ranked_candidates.sort_by_key(|c| Reverse(c.net_profit));
            ranked_candidates.truncate(sim_cascade_cap);
        }

        if let Some(metrics) = &self.metrics {
            let latency_ms = quote_start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            metrics.record_stage_latency(&self.chain_name, "quote", latency_ms);
        }

        // The decision-loop split, in the log rather than only in metrics.
        //
        // The loop was measured at 4.20s median (220 scans) against a 2s Base
        // block and a 200ms flashblock. `populate_ms` accounted for 1.71s of
        // that and the rest was invisible without scraping Prometheus, which
        // made it impossible to say whether a faster FEED or a faster DECISION
        // was the bigger win. search covers cycle discovery; quote covers
        // candidate preparation, which is where sizing lives.
        info!(
            target: "arb_exec::latency",
            chain = %self.chain_name,
            search_ms = search_start.elapsed().saturating_sub(quote_start.elapsed()).as_millis(),
            quote_and_size_ms = quote_start.elapsed().as_millis(),
            candidates = ranked_candidates.len(),
            "scan decision phases"
        );

        let attempt_limit = ranked_candidates.len();
        let mut cascade_failures: Vec<String> = Vec::new();
        let mut selected_candidate: Option<CandidatePlan> = None;

        if let Some(first) = ranked_candidates.first() {
            if let Some(metrics) = &self.metrics {
                metrics.record_opportunity_seen(&self.chain_name, first.strategy.as_str());
            }
        }

        'cascade: for (cascade_idx, mut candidate) in
            ranked_candidates.into_iter().take(attempt_limit).enumerate()
        {
            if cascade_idx > 0 {
                info!(
                    chain = %self.chain_name,
                    cascade_rank = cascade_idx,
                    candidate_id = %candidate.candidate_id,
                    net_profit = %candidate.net_profit,
                    "simulation cascade: trying next ranked candidate"
                );
            }
            let simulate_start = Instant::now();
            self.log_candidate_stage(
                "candidate_sent_to_sim",
                &self.chain_name,
                Some(candidate.candidate_id.to_string()),
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
            // Time every simulation, including the ones that blow the budget.
            // The budget is a hard `timeout`, so an over-budget sim is recorded
            // only as `simulation_timeout` with no duration — which makes the
            // right budget unknowable: measured on Base, 49/49 candidates timed
            // out at 380ms and nothing said whether the true cost was 400ms or
            // 40s. Log the elapsed either way so the budget can be set from data.
            let sim_started = Instant::now();
            // Round-trip count, not just wall time: at a fixed RTT the only way
            // to make simulation faster is to make fewer calls, so the call
            // count is the number that has to move.
            let sim_rpc_before = crate::rpc_failover::total_rpc_requests();
            let (simulated_gas_used, simulated_profit, simulated_l1_fee) = match timeout(
                self.simulation_budget,
                self.simulate_plan_execution(
                    &candidate.plan_args,
                    &candidate.fee_estimate,
                    block_number,
                ),
            )
            .await
            .inspect(|_| {
                debug!(
                    target: "sim_timing",
                    elapsed_ms = sim_started.elapsed().as_millis() as u64,
                    rpc_calls = crate::rpc_failover::total_rpc_requests()
                        .saturating_sub(sim_rpc_before),
                    budget_ms = self.simulation_budget.as_millis() as u64,
                    outcome = "completed",
                    "plan simulation finished"
                );
            })
            .inspect_err(|_| {
                warn!(
                    target: "sim_timing",
                    elapsed_ms = sim_started.elapsed().as_millis() as u64,
                    budget_ms = self.simulation_budget.as_millis() as u64,
                    outcome = "timeout",
                    hops = candidate.hops,
                    "plan simulation exceeded its budget"
                );
            })
            {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => {
                    // A `Too little received` revert names no amounts: the router
                    // only reports that `amountOut >= amountOutMinimum` failed,
                    // never by how much. Re-simulate the SAME plan with the
                    // per-hop floors removed — that variant cannot trip the check,
                    // so it returns what the pools actually pay. The difference is
                    // the shortfall, which is the number needed to size
                    // `tolerance_bps` from evidence instead of guesswork.
                    //
                    // Only on this specific revert, so a healthy run never pays
                    // for it, and only ever simulated: `relaxed_plan_args` has no
                    // slippage protection and must never reach dispatch.
                    // `{:#}` renders anyhow's full cause chain. Plain Display
                    // shows only the outermost context ("pre-broadcast
                    // simulation reverted"), which does not contain the revert
                    // string — so matching on it silently never fired.
                    let err_chain = format!("{err:#}");
                    if crate::quote_common::is_execution_revert(&err_chain) {
                        let demanded = candidate.plan_args.min_profit;
                        match self
                            .simulate_plan_execution(
                                &candidate.relaxed_plan_args,
                                &candidate.fee_estimate,
                                block_number,
                            )
                            .await
                        {
                            Ok((_, achieved, _)) => {
                                let shortfall_bps = if demanded.is_zero() || achieved >= demanded {
                                    0u64
                                } else {
                                    mul_div(
                                        demanded.saturating_sub(achieved),
                                        U256::from(10_000u64),
                                        demanded,
                                    )
                                    .as_u64()
                                };
                                warn!(
                                    target: "minout",
                                    venue_path = ?candidate.venue_path,
                                    hops = candidate.hops,
                                    demanded_min_profit = %demanded,
                                    achieved_unfloored = %achieved,
                                    shortfall_bps,
                                    "reverted plan re-simulated without floors"
                                );
                            }
                            Err(diag_err) => {
                                // Still reverting with NO floor means the failure
                                // is not a min_out sizing problem at all.
                                warn!(
                                    target: "minout",
                                    error = %format!("{diag_err:#}"),
                                    venue_path = ?candidate.venue_path,
                                    "floor-free re-simulation ALSO failed; \
                                     cause is not min_out sizing"
                                );
                            }
                        }
                    }
                    self.log_candidate_stage(
                        "candidate_rejected_post_sim",
                        &self.chain_name,
                        Some(candidate.candidate_id.to_string()),
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
                    cascade_failures.push(format!(
                        "cycle start=0x{} simulation failed: {}",
                        hex::encode(candidate.cycle_start),
                        err
                    ));
                    continue 'cascade;
                }
                Err(_) => {
                    self.log_candidate_stage(
                        "candidate_rejected_post_sim",
                        &self.chain_name,
                        Some(candidate.candidate_id.to_string()),
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
                    cascade_failures.push(format!(
                        "cycle start=0x{} simulation budget exceeded",
                        hex::encode(candidate.cycle_start)
                    ));
                    continue 'cascade;
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
                    Some(candidate.candidate_id.to_string()),
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
                cascade_failures.push(format!(
                    "cycle start=0x{} simulation returned zero profit",
                    hex::encode(candidate.cycle_start)
                ));
                continue 'cascade;
            }

            if !simulated_l1_fee.is_zero() {
                candidate.fee_estimate.l1_data_fee = simulated_l1_fee;
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

            // Risk policy: re-check the gas-unit cap against the simulated
            // (buffered) gas limit before committing to dispatch.
            if let Some(reason) = self.risk_gas_violation(&candidate.fee_estimate, gas_limit_u64) {
                self.log_candidate_stage(
                    "candidate_rejected_post_sim",
                    &self.chain_name,
                    Some(candidate.candidate_id.to_string()),
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
                    Some(reason),
                    Some("risk_gate_failed"),
                    candidate.has_bridge_step,
                    !candidate.liquidation_markets.is_empty(),
                );
                cascade_failures.push(format!(
                    "cycle start=0x{} rejected by risk policy after simulation: {} (gas_units={})",
                    hex::encode(candidate.cycle_start),
                    reason,
                    gas_limit_u64
                ));
                continue 'cascade;
            }

            // Post-simulation threshold MUST be derived from the exact same
            // formula as the pre-simulation gate (flash-fee buffer, bridge
            // premium, backrun discount, revert premium, risk floor included),
            // only refreshed with the simulated gas profile.
            let recomputed_min_profit =
                self.unified_min_profit_threshold(UnifiedThresholdParams {
                    fee: &candidate.fee_estimate,
                    est_gas: gas_limit_u64,
                    est_gross_after_fee: candidate.est_gross_after_fee,
                    flash_fee_amount: candidate.flash_fee_amount,
                    slippage_floor: candidate.slippage_floor,
                    competition_buffer: candidate.competition_buffer,
                    has_bridge_step: candidate.has_bridge_step,
                    backrun_hint: candidate.backrun_hint.as_ref(),
                    congestion: candidate.congestion_multiplier,
                    competition: &candidate.competition_snapshot,
                    latency_secs: candidate.cycle_latency_secs,
                    native_price: candidate.native_price,
                });
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
                    Some(candidate.candidate_id.to_string()),
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
                cascade_failures.push(format!(
                    "cycle start=0x{} failed: unreliable native pricing after simulation",
                    hex::encode(candidate.cycle_start)
                ));
                continue 'cascade;
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
                cascade_failures.push(format!(
                    "cycle start=0x{} failed simulation profit check grossWei={} gasWei={} thresholdWei={}",
                    hex::encode(candidate.cycle_start),
                    candidate.est_gross_after_fee,
                    candidate.gas_cost_native,
                    candidate.plan_args.min_profit
                ));
                continue 'cascade;
            }

            selected_candidate = Some(candidate);
            break 'cascade;
        }

        if let Some(candidate) = selected_candidate {
            let min_profit_target = candidate.plan_args.min_profit;
            self.log_candidate_stage(
                "candidate_dispatch_eligible",
                &self.chain_name,
                Some(candidate.candidate_id.to_string()),
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
            // Final safety gate: the run-loop breaker check can be seconds stale by
            // the time we finish scanning/simulating. Re-check immediately before we
            // allocate a nonce or broadcast, so a trip mid-cycle still blocks the send.
            {
                let breaker_status = self.circuit_breaker.current_status().await;
                if breaker_status.is_tripped {
                    warn!(
                        chain = %self.chain_name,
                        candidate = %candidate.candidate_id,
                        reason = %breaker_status.active_reason(),
                        "Circuit breaker tripped; blocking broadcast before dispatch"
                    );
                    return Err(anyhow!(
                        "circuit breaker tripped before broadcast: {}",
                        breaker_status.active_reason()
                    ));
                }
            }
            let broadcast_start = Instant::now();
            // Denominate the candidate's net profit in native wei so the
            // dispatcher can bid a share of it as priority fee. Fail closed to
            // the static fee when pricing is unreliable (never bid on a guess).
            let bid_net_profit_native =
                candidate.native_price.native_for_tokens_strict(candidate.net_profit);
            let dispatch = match self
                .dispatch_call(
                    call,
                    candidate.gas_limit,
                    &candidate.fee_estimate,
                    Some(shadow_meta),
                    true,
                    bid_net_profit_native,
                )
                .await
            {
                Ok(dispatch) => dispatch,
                Err(err) => {
                    self.log_candidate_stage(
                        "candidate_rejected_post_sim",
                        &self.chain_name,
                        Some(candidate.candidate_id.to_string()),
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
                Some(candidate.candidate_id.to_string()),
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

        if !cascade_failures.is_empty() {
            return Ok(ScanOutcome::NotProfitable {
                reason: format!(
                    "simulation cascade exhausted ({} attempts): {}",
                    cascade_failures.len(),
                    cascade_failures.join("; ")
                ),
                edges: edges_scanned,
                expected_univ3_edges,
            });
        }

        // Distance to profit, computed once and reported on BOTH terminal
        // branches. It previously lived only on the "no viable cycles" path, so
        // the moment detection started producing candidates the number went
        // invisible — exactly when it became most useful.
        let two_hop = graph.best_two_hop_roundtrip();
        let two_hop_bps = two_hop
            .map(|p| format!("{:.3}", p.best_bps))
            .unwrap_or_else(|| "no closed 2-hop route".to_string());
        let two_hop_pair = two_hop
            .map(|p| {
                format!(
                    "{}/{}{}",
                    hex::encode(&p.token_a.as_bytes()[..4]),
                    hex::encode(&p.token_b.as_bytes()[..4]),
                    if p.cross_pool { "" } else { " (same-pool)" }
                )
            })
            .unwrap_or_else(|| "-".to_string());

        if let Some(reason) = last_skip_reason {
            debug!(
                edges = edges_scanned,
                expected_univ3_edges,
                %reason,
                best_two_hop_bps = %two_hop_bps,
                best_two_hop_pair = %two_hop_pair,
                "Opportunity scanner filtered all candidates"
            );
            Ok(ScanOutcome::NotProfitable {
                reason,
                edges: edges_scanned,
                expected_univ3_edges,
            })
        } else {
            // Report DISTANCE TO PROFIT, not just absence. `best_gross_bps` is
            // the best gross edge (prod rate - 1, post-fee) across every cycle
            // considered, including rejected ones. -2 bps means the market was
            // nearly there and a fee tier or an extra venue might close it;
            // -500 bps means nothing in this graph is remotely close and the
            // universe or the pool set is the problem. A bare "no cycles" cannot
            // tell those apart, which is why this run reported nothing useful
            // for months.
            let best_gross_bps = best_gross_scaled.map(|scaled| {
                let log_rate_sum = scaled as f64 / crate::util::WEIGHT_SCALE as f64;
                (log_rate_sum.exp() - 1.0) * 10_000.0
            });
            debug!(
                edges = edges_scanned,
                expected_univ3_edges,
                best_gross_bps = best_gross_bps
                    .map(|bps| format!("{bps:.3}"))
                    .unwrap_or_else(|| "none (no cycle surfaced)".to_string()),
                best_two_hop_bps = %two_hop_bps,
                best_two_hop_pair = %two_hop_pair,
                "Opportunity scanner found no viable cycles"
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
        simulation_verified: bool,
        bid_net_profit_native: Option<U256>,
    ) -> Result<DispatchResult> {
        // Risk policy: must_simulate_before_send is enforced at the dispatch
        // boundary, not assumed. Any future call path that skips simulation
        // fails closed here instead of broadcasting unverified plans.
        if !simulation_verified
            && self
                .risk_policy
                .as_ref()
                .map(|policy| policy.must_simulate_before_send)
                .unwrap_or(true)
        {
            return Err(anyhow!(
                "risk policy requires simulation before send; refusing to dispatch unsimulated plan"
            ));
        }
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
                let base_fee =
                    client
                        .get_block(BlockNumber::Latest)
                        .await
                        .ok()
                        .and_then(|maybe_block| {
                            maybe_block.and_then(|block| block.base_fee_per_gas)
                        });
                // Profit-aware bid: lift the priority fee toward a share of the
                // opportunity's net profit so we actually win builder ordering,
                // bounded by the risk policy fee ceiling and the static floor.
                let risk_fee_cap = self
                    .risk_policy
                    .as_ref()
                    .and_then(|policy| policy.max_fee_per_gas_cap);
                let effective_priority_fee = self.broadcast.competitive_priority_fee(
                    bid_net_profit_native,
                    gas_limit.min(U256::from(u64::MAX)).as_u64(),
                    base_fee,
                    risk_fee_cap,
                );
                if let Some(priority_fee) = effective_priority_fee {
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
                            // Keep max_fee within the risk ceiling if one is set.
                            let suggested_max_fee = match risk_fee_cap {
                                Some(cap) if suggested_max_fee > cap => cap,
                                _ => suggested_max_fee,
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
                        candidates.push((index, relay.clone()));
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

                if parallel_private_relay_blast_enabled() {
                    match parallel_private_relay_blast(
                        Arc::clone(&client),
                        raw.clone(),
                        target_bundle_block,
                        relay_candidates,
                        self.broadcast.private_inclusion_timeout,
                        Arc::clone(&self.broadcast.relay_health),
                        self.chaos.clone(),
                    )
                    .await
                    {
                        Ok((tx_hash, receipt, relay_index, method)) => {
                            if let Some((manager, nonce)) = &nonce_record {
                                manager.mark_confirmed(*nonce).await;
                            }
                            info!(
                                target: "broadcast",
                                relay_index,
                                latency_ms = start.elapsed().as_millis(),
                                method = method.as_str(),
                                "Private relay inclusion confirmed (parallel blast)",
                            );
                            return Ok(DispatchResult {
                                tx_hash,
                                receipt: Some(receipt),
                                latency: start.elapsed(),
                                private_relay_rejected,
                                private_submission_method: Some(method),
                            });
                        }
                        Err(errors) => {
                            relay_errors = errors;
                        }
                    }
                } else {
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

                // OP Stack / Arbitrum / Linea have no bundle relay market. When
                // relay RPC submission fails (auth, rate limits, bundle unsupported),
                // fall back to a direct signed send on the primary provider — the
                // same path live Base searchers use for sequencer inclusion.
                if gas_model_uses_sequencer_submission_by_chain(&self.chain_name) {
                    warn!(
                        target: "broadcast",
                        chain = %self.chain_name,
                        "Relay submission exhausted; falling back to direct sequencer send"
                    );
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
                    return Ok(DispatchResult {
                        tx_hash,
                        receipt,
                        latency: start.elapsed(),
                        private_relay_rejected: true,
                        private_submission_method: Some(PrivateSubmissionMethod::PrivateRaw),
                    });
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
        let first_err = match call.estimate_gas().await {
            Ok(limit) => return Ok(limit),
            Err(err) => {
                warn!(error = %err, "Gas estimation failed, applying fallbacks");
                err.to_string()
            }
        };

        // A revert is deterministic: the same calldata against the same state
        // reverts identically on the pending block, so the retry below is a
        // guaranteed-failing round trip. Measured on Base this fired 48 times in
        // one 200s run, burning ~240ms of RTT each on candidates whose primary
        // simulation had already reverted. Transport errors still retry, because
        // those genuinely can succeed on a second attempt.
        let deterministic = crate::quote_common::is_execution_revert(&first_err);

        let base_gas = U256::from(200_000u64);
        let per_hop_gas = U256::from(50_000u64);
        let hop_count = U256::from(hops as u64);
        let heuristic = base_gas.saturating_add(per_hop_gas.saturating_mul(hop_count));

        if deterministic {
            let buffered = heuristic.saturating_mul(U256::from(15u64));
            return Ok(buffered.checked_div(U256::from(10u64)).unwrap_or(heuristic));
        }

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
        block_number: U64,
    ) -> Result<(U256, U256, U256)> {
        let mut call = self.executor.start_v2(plan.clone());
        if let Some(wallet) = &self.wallet {
            call.tx.set_from(wallet.address());
        }
        apply_gas_parameters(&mut call.tx, gas);
        let tx = call.tx.clone();
        // Calldata capture for offline replay. Off unless ARBOT_DUMP_CALLDATA=1.
        //
        // NOT temporary, despite what this comment said for several commits.
        // The candidate log records only `failed` with no reason, so replaying
        // a captured frame is the only way to get a real revert string, and
        // HANDOFF.md section 3 -- "Replay a failing plan to get the REAL revert
        // reason", which calls itself the first step of every diagnosis -- is
        // built directly on this file.
        //
        // The bug it was originally written for is closed, and it closed BOTH
        // mechanisms behind `InvalidGenericAction()` using these frames:
        //   - a transposed Slipstream router selector, 0xc04b8d70 for
        //     0xc04b8d59, which reverts with empty data (see plan.rs; pinned
        //     now by `slipstream_step_targets_the_routers_real_exact_input`);
        //   - an out-of-gas that also returns empty data and so reads as a plan
        //     defect rather than a budget -- ops/inputs.yaml records "verified
        //     by replaying captured calldata at block 50222943: 450k/3M ->
        //     0xf19db938, 4M+ -> the real router error".
        // Both are fixed. The capture outlived them because it is not specific
        // to either: it is how any revert gets diagnosed here.
        //
        // Delete it when the candidate log carries real revert reasons itself,
        // which is what would make the replay round trip unnecessary. Until
        // then HANDOFF.md's shadow recipe sets the flag and expects the file.
        //
        // Writes are best-effort: a diagnostic must not be able to fail a
        // simulation, so a write error is not propagated. It IS reported --
        // once, at the first failure -- because the alternative is an operator
        // finishing a shadow pass, reaching for the replay, and finding an
        // empty file with nothing anywhere saying why.
        if std::env::var("ARBOT_DUMP_CALLDATA").ok().as_deref() == Some("1") {
            let path = std::env::var("ARBOT_DUMP_CALLDATA_PATH")
                .unwrap_or_else(|_| "/tmp/arbot_failing_calldata.jsonl".to_string());
            let to = tx
                .to()
                .cloned()
                .map(|dest| match dest {
                    NameOrAddress::Address(addr) => format!("{addr:#x}"),
                    NameOrAddress::Name(name) => name,
                })
                .unwrap_or_else(|| "<none>".to_string());
            let from = tx
                .from()
                .map(|addr| format!("{addr:#x}"))
                .unwrap_or_else(|| "<none>".to_string());
            let data = tx
                .data()
                .map(|d| format!("0x{}", hex::encode(d.as_ref())))
                .unwrap_or_else(|| "0x".to_string());
            let ops: Vec<u8> = plan.steps.iter().map(|s| s.op).collect();
            let providers: Vec<String> = plan
                .loans
                .iter()
                .map(|l| {
                    format!(
                        "{}@{:#x}:{:#x}",
                        l.provider, l.provider_addr, l.token
                    )
                })
                .collect();
            let line = format!(
                "{{\"block\":{},\"chain_id\":{},\"to\":\"{}\",\"from\":\"{}\",\"gas\":\"{}\",\"value\":\"{}\",\"ops\":{:?},\"loans\":{:?},\"min_profit\":\"{}\",\"cycle_slippage_bps\":{},\"data\":\"{}\"}}\n",
                block_number.as_u64(),
                self.chain_id,
                to,
                from,
                tx.gas().copied().unwrap_or_default(),
                tx.value().copied().unwrap_or_default(),
                ops,
                providers,
                plan.min_profit,
                plan.cycle_slippage_bps,
                data
            );
            // Warned ONCE, not per candidate: this runs on every simulated
            // candidate, so a warning per frame would bury the log it exists to
            // protect. Still a warning and not a silent skip -- the failure mode
            // being guarded against is an operator running a whole shadow pass,
            // reaching for the replay in HANDOFF.md section 3, and finding an
            // empty file with nothing anywhere saying why.
            static DUMP_WRITE_WARNED: std::sync::Once = std::sync::Once::new();
            let wrote = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| f.write_all(line.as_bytes()));
            if let Err(err) = wrote {
                DUMP_WRITE_WARNED.call_once(|| {
                    warn!(
                        path = %path,
                        error = %err,
                        "ARBOT_DUMP_CALLDATA is set but the capture cannot be \
                         written; replay diagnosis will have no frames"
                    );
                });
            }
        }
        // ===== end calldata capture =====
        let client = self.executor.client();
        let executor_address = self.executor.address();

        if sim_revm_enabled() {
            // Fork at the head this scan was built against (from the websocket
            // block feed) rather than re-fetching it, and use the statically
            // configured chain id instead of an RPC round-trip — both are known
            // and constant for the scan, so querying them per candidate only
            // added latency to the pre-broadcast path.
            let block_number = block_number.as_u64();
            let chain_id = self.chain_id;
            let timeout_ms = sim_revm_timeout_ms();
            let mut prefetch_extra = Vec::new();
            for loan in &plan.loans {
                if !loan.token.is_zero() {
                    prefetch_extra.push(loan.token);
                }
                if !loan.provider_addr.is_zero() {
                    prefetch_extra.push(loan.provider_addr);
                }
            }
            if !self.wrapped_native.is_zero() {
                prefetch_extra.push(self.wrapped_native);
            }
            if !self.bal_vault.is_zero() {
                prefetch_extra.push(self.bal_vault);
            }
            if let Some(pool) = self.aave_pool {
                if !pool.is_zero() {
                    prefetch_extra.push(pool);
                }
            }
            let fork_req = SimForkRequest {
                rpc_url: self.sim_rpc_url.clone(),
                block_number,
                chain_id,
                executor_address,
                executor_bytecode: None,
                tx: tx.clone(),
                prefetch_addresses: prefetch_extra,
                metrics: self.metrics.clone(),
            };
            match timeout(
                Duration::from_millis(timeout_ms),
                simulate_via_revm(fork_req),
            )
            .await
            {
                Ok(Ok(result)) => {
                    if let Some(metrics) = &self.metrics {
                        if result.success {
                            record_revm_metrics(metrics, RevmSimOutcome::Success);
                        } else {
                            record_revm_metrics(metrics, RevmSimOutcome::Failure);
                        }
                    }
                    if result.profit < plan.min_profit {
                        return Err(anyhow!("revm sim profit below min_profit"));
                    }
                    return Ok((
                        U256::from(result.gas_used),
                        result.profit,
                        result.l1_fee_wei,
                    ));
                }
                Ok(Err(err)) => {
                    if let Some(metrics) = &self.metrics {
                        record_revm_metrics(metrics, RevmSimOutcome::Fallback);
                    }
                    warn!(
                        error = %err,
                        block_number,
                        "ARBOT_SIM_REVM=1 revm path failed; falling back to eth_call"
                    );
                }
                Err(_) => {
                    if let Some(metrics) = &self.metrics {
                        record_revm_metrics(metrics, RevmSimOutcome::Fallback);
                    }
                    warn!(
                        timeout_ms,
                        block_number,
                        "ARBOT_SIM_REVM=1 revm path timed out; falling back to eth_call"
                    );
                }
            }
        }

        // Sequential BY DESIGN. Parallelising these three was measured and did
        // NOT help: simulation stayed at p50 ~2.8s (2808 vs 2802ms) because the
        // cost is ~17 RPC round trips per simulation, not these three. It was
        // strictly worse in practice — with the primary reverting on every
        // candidate today, running quorum and gas estimation concurrently pays
        // for both on a tx already known to be doomed.
        // Simulate at the block the plan was QUOTED against, not `pending`.
        //
        // `pending` resolves to head+1 (verified on Base). The plan's min_out
        // floors were computed from quotes taken at `block_number`, and a scan
        // takes ~8.4s = ~4 Base blocks, so simulating at `pending` evaluates the
        // plan against state ~5 blocks newer than it was priced on.
        //
        // Measured drift on the failing pair (USDC->WETH, slipstream
        // 0xdbc6998296caa1652a810dc8d3baf4a8294330f1), quoted through the router:
        //   2 blocks  +6.63 bps
        //  10 blocks -23.70 bps
        //  20 blocks -45.70 bps
        // against edge tolerance_bps = 5. `amountOutMinimum` therefore fails on
        // essentially every candidate, which is exactly what was observed: 100%
        // "Too little received", invariant to trade size, pricing model and
        // min_out slack (tick buffer tested at 150 and 400 bps, execution buffer
        // at 75) — because the drift is time-dependent and SIGNED, not a fixed
        // offset any buffer can cover.
        let raw = client
            .call(
                &tx,
                Some(BlockId::Number(BlockNumber::Number(block_number))),
            )
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

        // Cross-check the primary simulation against independent RPC
        // endpoints. A compromised or stale primary must not be able to
        // single-handedly green-light a dispatch.
        self.sim_quorum
            .verify(&tx, plan.min_profit)
            .await
            .context("simulation quorum verification failed")?;

        let gas_used = client
            .estimate_gas(&tx, Some(BlockId::Number(BlockNumber::Pending)))
            .await
            .context("simulation gas estimate failed")?;
        Ok((gas_used, profit, gas.l1_data_fee))
    }

    async fn run(
        self: Arc<Self>,
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
                            // Relay rejected but a fallback path still landed the
                            // tx: count it so the funnel reflects private-relay
                            // market losses even on eventual success.
                            if summary.private_relay_rejected {
                                metrics.record_relay_rejected(
                                    &self.chain_name,
                                    summary.strategy.as_str(),
                                );
                            }
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
                        self.circuit_breaker.record_execution_outcome(false).await;
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
                        self.wait_for_scan_cadence().await;
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
                        self.wait_for_scan_cadence().await;
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
                            // Failed dispatch that the private relay refused (no
                            // successful fallback). Strategy isn't carried on the
                            // Failed outcome, so label it "unknown".
                            if private_relay_rejected {
                                metrics.record_relay_rejected(&self.chain_name, "unknown");
                            }
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
                        self.circuit_breaker.record_execution_outcome(true).await;
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
                        self.wait_for_scan_cadence().await;
                    }
                    Err(err) => {
                        if is_rpc_error(&err) {
                            if let Some(metrics) = &self.metrics {
                                metrics.record_rpc_error(&self.chain_name);
                            }
                            self.circuit_breaker.record_rpc_error().await;
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

/// Supervised replacement for fire-and-forget `tokio::spawn` of long-lived
/// background workers (pool refreshers, mempool monitors, exporters).
///
/// These workers feed the trading path with liquidity/competition data; if one
/// dies silently the bot keeps trading on progressively staler state. The
/// supervisor isolates panics in an inner task, logs every exit, bumps the
/// `worker_restarts_total{chain,worker}` metric, and restarts the worker with
/// capped exponential backoff (2s..64s) so a persistent fault degrades to a
/// loud periodic retry instead of a silent feature loss.
fn spawn_supervised<F, Fut>(
    worker: &'static str,
    chain: String,
    metrics: Option<Arc<Metrics>>,
    mut factory: F,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut restarts: u32 = 0;
        loop {
            // Inner spawn so a worker panic is contained and observable
            // instead of unwinding this supervisor.
            match tokio::spawn(factory()).await {
                Ok(()) => {
                    warn!(
                        worker,
                        chain = %chain,
                        restarts,
                        "supervised worker exited unexpectedly; restarting"
                    );
                }
                Err(err) if err.is_panic() => {
                    error!(
                        worker,
                        chain = %chain,
                        restarts,
                        error = %err,
                        "supervised worker PANICKED; restarting"
                    );
                }
                Err(err) => {
                    // Cancelled: runtime is shutting down.
                    info!(worker, chain = %chain, error = %err, "supervised worker cancelled");
                    return;
                }
            }
            if let Some(metrics) = &metrics {
                metrics.record_worker_restart(&chain, worker);
            }
            restarts = restarts.saturating_add(1);
            let backoff_secs = 2u64.saturating_pow(restarts.min(6));
            sleep(Duration::from_secs(backoff_secs)).await;
        }
    });
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

/// Shadow and unattended runs should not block on stdin waiting for `start`.
fn interactive_command_listener_enabled() -> bool {
    if std::env::var("ARBOT_NONINTERACTIVE")
        .ok()
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
    {
        return false;
    }
    if std::env::var("SHADOW_MODE")
        .ok()
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
        && !std::env::var("ARBOT_INTERACTIVE")
            .ok()
            .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    {
        return false;
    }
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
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

    #[tokio::test]
    async fn circuit_breaker_tolerates_expected_backrun_revert_rate() {
        // THE calibration requirement. Backrunning loses most races, and losing
        // means the executor reverts rather than filling at a loss — a 55-65%
        // revert rate is the healthy steady state. A breaker that halts on it is
        // not protection, it is an outage.
        //
        // This previously asserted the opposite: a 62.5% rate was expected to
        // TRIP, because the default limit (0.5) sat below the normal operating
        // band. Normal operation tripped the breaker permanently.
        let breaker = CircuitBreaker::new(U256::zero(), U256::zero(), 100);
        // 200 samples at exactly 65% reverts.
        for i in 0..200 {
            breaker.record_execution_outcome(i % 20 < 13).await;
        }
        let status = breaker.current_status().await;
        assert!(
            !status.is_tripped,
            "a 65% revert rate is expected operation and must not trip: {}",
            status.active_reason()
        );
    }

    #[tokio::test]
    async fn circuit_breaker_trips_on_abnormal_revert_rate() {
        // What the trigger is actually for: a broken deploy reverting on
        // essentially every attempt, well clear of the expected band.
        let breaker = CircuitBreaker::new(U256::zero(), U256::zero(), 100);
        for i in 0..200 {
            breaker.record_execution_outcome(i % 50 != 0).await; // 98% reverts
        }
        let status = breaker.current_status().await;
        assert!(status.is_tripped, "98% reverts must trip the breaker");
        assert!(
            status.active_reason().contains("revert rate"),
            "reason was: {}",
            status.active_reason()
        );
    }

    #[tokio::test]
    async fn circuit_breaker_ignores_revert_rate_below_min_samples() {
        let breaker = CircuitBreaker::new(U256::zero(), U256::zero(), 100);
        // Below the 50-sample minimum: even 100% reverts must not trip yet,
        // because a handful of losses says nothing about the true rate.
        for _ in 0..40 {
            breaker.record_execution_outcome(true).await;
        }
        assert!(!breaker.current_status().await.is_tripped);
    }

    #[tokio::test]
    async fn circuit_breaker_consecutive_limit_survives_a_realistic_losing_streak() {
        // At a 65% revert rate the longest run seen in a 20-trade cycle is 13.
        // The limit must sit above that; the old default of 3 tripped every few
        // fills. A success resets the counter, as a real fill would.
        // NOTE the boundary: the trip test is `failures > limit`, so a limit of
        // 15 halts on the SIXTEENTH consecutive revert, not the fifteenth.
        let breaker = CircuitBreaker::new(U256::max_value(), U256::max_value(), 15);
        for _ in 0..15 {
            breaker.record_failure(U256::zero()).await;
        }
        assert!(
            !breaker.current_status().await.is_tripped,
            "15 consecutive reverts is still normal variance at a 65% revert rate"
        );
        breaker.record_success().await;
        for _ in 0..16 {
            breaker.record_failure(U256::zero()).await;
        }
        assert!(
            breaker.current_status().await.is_tripped,
            "16 consecutive reverts must trip: that is a broken deploy, not variance"
        );
    }

    #[tokio::test]
    async fn circuit_breaker_trips_on_rpc_error_burst() {
        let breaker = CircuitBreaker::new(U256::zero(), U256::zero(), 100);
        for _ in 0..30 {
            breaker.record_rpc_error().await;
        }
        let status = breaker.current_status().await;
        assert!(status.is_tripped, "rpc error burst should trip the breaker");
        assert!(
            status.active_reason().contains("rpc errors"),
            "reason was: {}",
            status.active_reason()
        );
    }

    #[tokio::test]
    async fn circuit_breaker_reset_clears_health_windows() {
        let breaker = CircuitBreaker::new(U256::zero(), U256::zero(), 100);
        for _ in 0..30 {
            breaker.record_rpc_error().await;
        }
        assert!(breaker.current_status().await.is_tripped);
        breaker.reset().await;
        assert!(
            !breaker.current_status().await.is_tripped,
            "reset must clear rpc/revert health windows"
        );
    }

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
                univ2_flash_pool: None,
                univ2_flash_fee_bps: 0,
                univ3_flash_pool: None,
                univ3_flash_fee_bps: 0,
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
                sim_rpc_url: "http://test".into(),
                rpc_health,
                univ3_quoter: Address::zero(),
                univ3_factory: Address::zero(),
                univ3_validation: None,
                univ3_fee_tiers: None,
                bal_vault: Address::zero(),
                aave_pool: None,
                aave_fee_bps: 9,
                erc3156_lender: None,
                erc3156_fee_bps: 9,
                bal_flashloan_tokens: None,
                aave_flashloan_tokens: None,
                erc3156_flashloan_tokens: None,
                univ2_flashloan_tokens: None,
                univ3_flashloan_tokens: None,
                chain_env_prefix: "TEST".into(),
                tokens,
                initial_token_decimals: HashMap::new(),
                wrapped_native: Address::zero(),
                capital,
                pool_depth_cache: pool_cache,
                pool_monitor: None,
                hot_univ2_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_univ3_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_slipstream_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                slipstream_quoter_addr: Address::zero(),
                slipstream_factory: Address::zero(),
                slipstream_router: Address::zero(),
                slipstream_validation: None,
                slipstream_tick_spacings: None,
                hot_pancakeswap_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                pancakeswap_quoter_addr: Address::zero(),
                pancakeswap_factory: Address::zero(),
                pancakeswap_router: Address::zero(),
                pancakeswap_validation: None,
                pancakeswap_fee_tiers: None,
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
                    bid_profit_fraction_bps: 0,
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
                risk_policy: None,
                sim_quorum: Arc::new(SimQuorum::disabled("test")),
                chain_id: 8453,
                block_head_rx: None,
                hub_search_enabled: false,
                hub_search_parallel_edges: 3,
                bf_skip_on_stable_graph: false,
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

    // ENV_LOCK must span the whole test body: it serializes env-var mutation
    // against every other test, so it cannot be dropped before the awaits.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn ensure_contract_deployed_enforces_pinned_codehash() {
        use std::env;
        let _guard = crate::tests::ENV_LOCK.lock().expect("env lock");
        let code = vec![1u8, 2u8];
        let good_hash = H256::from(ethers::utils::keccak256(&code));

        // Pin matches on-chain code -> pass.
        env::set_var("PINTEST_EXECUTOR_CODEHASH", format!("{good_hash:#x}"));
        let (provider, mock) = Provider::mocked();
        mock.push::<Bytes, _>(Bytes::from(code.clone())).unwrap();
        let ok = ensure_contract_deployed(
            &provider,
            ContractDeploymentCheck {
                chain: "test",
                env_prefix: "PINTEST",
                label: "executor",
                suffix: "EXECUTOR_ADDRESS",
                address: Address::random(),
                expected_chain_id: 1,
                rpc_endpoint: "http://test-rpc",
            },
        )
        .await;
        assert!(ok.is_ok(), "matching codehash must pass: {ok:?}");

        // Pin differs from on-chain code -> hard failure.
        env::set_var(
            "PINTEST_EXECUTOR_CODEHASH",
            format!("{:#x}", H256::repeat_byte(0xab)),
        );
        let (provider, mock) = Provider::mocked();
        mock.push::<Bytes, _>(Bytes::from(code)).unwrap();
        let err = ensure_contract_deployed(
            &provider,
            ContractDeploymentCheck {
                chain: "test",
                env_prefix: "PINTEST",
                label: "executor",
                suffix: "EXECUTOR_ADDRESS",
                address: Address::random(),
                expected_chain_id: 1,
                rpc_endpoint: "http://test-rpc",
            },
        )
        .await
        .expect_err("mismatched codehash must fail startup");
        assert!(err.to_string().contains("bytecode hash mismatch"));
        env::remove_var("PINTEST_EXECUTOR_CODEHASH");
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
                univ2_flash_pool: None,
                univ2_flash_fee_bps: 0,
                univ3_flash_pool: None,
                univ3_flash_fee_bps: 0,
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
                sim_rpc_url: "http://test".into(),
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
                aave_fee_bps: 9,
                erc3156_lender: None,
                erc3156_fee_bps: 9,
                bal_flashloan_tokens: None,
                aave_flashloan_tokens: None,
                erc3156_flashloan_tokens: None,
                univ2_flashloan_tokens: None,
                univ3_flashloan_tokens: None,
                chain_env_prefix: "TEST".into(),
                tokens: tokens.clone(),
                initial_token_decimals: HashMap::new(),
                wrapped_native: Address::zero(),
                capital,
                pool_depth_cache: pool_cache,
                pool_monitor: None,
                hot_univ2_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_univ3_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_slipstream_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                slipstream_quoter_addr: Address::zero(),
                slipstream_factory: Address::zero(),
                slipstream_router: Address::zero(),
                slipstream_validation: None,
                slipstream_tick_spacings: None,
                hot_pancakeswap_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                pancakeswap_quoter_addr: Address::zero(),
                pancakeswap_factory: Address::zero(),
                pancakeswap_router: Address::zero(),
                pancakeswap_validation: None,
                pancakeswap_fee_tiers: None,
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
                    bid_profit_fraction_bps: 0,
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
                risk_policy: None,
                sim_quorum: Arc::new(SimQuorum::disabled("test")),
                chain_id: 8453,
                block_head_rx: None,
                hub_search_enabled: false,
                hub_search_parallel_edges: 3,
                bf_skip_on_stable_graph: false,
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

        let (gas_used, profit, _l1_fee) = runner
            .simulate_plan_execution(&plan_args, &fee, U64::from(1u64))
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
                univ2_flash_pool: None,
                univ2_flash_fee_bps: 0,
                univ3_flash_pool: None,
                univ3_flash_fee_bps: 0,
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
                sim_rpc_url: "http://test".into(),
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
                aave_fee_bps: 9,
                erc3156_lender: None,
                erc3156_fee_bps: 9,
                bal_flashloan_tokens: None,
                aave_flashloan_tokens: None,
                erc3156_flashloan_tokens: None,
                univ2_flashloan_tokens: None,
                univ3_flashloan_tokens: None,
                chain_env_prefix: "TEST".into(),
                tokens,
                initial_token_decimals: HashMap::new(),
                wrapped_native: Address::zero(),
                capital,
                pool_depth_cache: pool_cache,
                pool_monitor: None,
                hot_univ2_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_univ3_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                hot_slipstream_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                slipstream_quoter_addr: Address::zero(),
                slipstream_factory: Address::zero(),
                slipstream_router: Address::zero(),
                slipstream_validation: None,
                slipstream_tick_spacings: None,
                hot_pancakeswap_pools: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                pancakeswap_quoter_addr: Address::zero(),
                pancakeswap_factory: Address::zero(),
                pancakeswap_router: Address::zero(),
                pancakeswap_validation: None,
                pancakeswap_fee_tiers: None,
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
                    bid_profit_fraction_bps: 0,
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
                risk_policy: None,
                sim_quorum: Arc::new(SimQuorum::disabled("test")),
                chain_id: 8453,
                block_head_rx: None,
                hub_search_enabled: false,
                hub_search_parallel_edges: 3,
                bf_skip_on_stable_graph: false,
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
            tick_ladder: None,
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

    #[test]
    fn sim_cascade_depth_defaults_and_respects_env() {
        let prior = std::env::var("ARBOT_SIM_CASCADE_DEPTH").ok();
        std::env::remove_var("ARBOT_SIM_CASCADE_DEPTH");
        assert_eq!(sim_cascade_depth(), DEFAULT_SIM_CASCADE_DEPTH);

        std::env::set_var("ARBOT_SIM_CASCADE_DEPTH", "5");
        assert_eq!(sim_cascade_depth(), 5);

        std::env::set_var("ARBOT_SIM_CASCADE_DEPTH", "0");
        assert_eq!(sim_cascade_depth(), DEFAULT_SIM_CASCADE_DEPTH);

        match prior {
            Some(value) => std::env::set_var("ARBOT_SIM_CASCADE_DEPTH", value),
            None => std::env::remove_var("ARBOT_SIM_CASCADE_DEPTH"),
        }
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

fn resolve_aave_fee_bps(
    chain_name: &str,
    ops_inputs: &crate::ops_inputs::OpsInputs,
) -> u32 {
    const DEFAULT_AAVE_FEE_BPS: u32 = 9;
    if let Some(chain) = ops_inputs
        .chains
        .iter()
        .find(|chain| chain.chain_name.eq_ignore_ascii_case(chain_name))
    {
        if let Some(fee_bps) = chain.flashloans.iter().find_map(|fl| {
            if matches!(fl.kind, Some(crate::ops_inputs::FlashloanKind::AaveV3Like)) {
                fl.fee_bps
            } else {
                None
            }
        }) {
            return fee_bps;
        }
    }

    let prefixed = format!(
        "{}_AAVE_FLASH_FEE_BPS",
        chain_env_prefix_for_name(chain_name)
    );
    if let Ok(raw) = std::env::var(&prefixed) {
        if let Ok(fee_bps) = raw.trim().parse::<u32>() {
            if fee_bps <= 10_000 {
                return fee_bps;
            }
        }
    }
    if let Ok(raw) = std::env::var("AAVE_FLASH_FEE_BPS") {
        if let Ok(fee_bps) = raw.trim().parse::<u32>() {
            if fee_bps <= 10_000 {
                return fee_bps;
            }
        }
    }
    DEFAULT_AAVE_FEE_BPS
}

fn chain_env_prefix_for_name(chain_name: &str) -> &'static str {
    match chain_name.to_ascii_lowercase().as_str() {
        "ethereum" => "ETH",
        "arbitrum" => "ARB",
        "optimism" => "OPT",
        "base" => "BASE",
        "polygon" => "POLYGON",
        "linea" => "LINEA",
        "abstract" => "ABSTRACT",
        "ink" => "INK",
        "mantle" => "MANTLE",
        "scroll" => "SCROLL",
        _ => "CHAIN",
    }
}

fn gas_model_uses_sequencer_submission(gas_model: &crate::ops_inputs::GasModel) -> bool {
    matches!(
        gas_model,
        crate::ops_inputs::GasModel::OpStack
            | crate::ops_inputs::GasModel::Arbitrum
            | crate::ops_inputs::GasModel::LineaEstimateGas
    )
}

fn gas_model_uses_sequencer_submission_by_chain(chain_name: &str) -> bool {
    matches!(
        chain_name.to_ascii_lowercase().as_str(),
        "base" | "optimism" | "arbitrum" | "linea"
    )
}

fn merge_sequencer_submission_relays(cfg: &ChainCfg, relays: Vec<String>) -> Vec<String> {
    if !gas_model_uses_sequencer_submission(&cfg.gas_model) {
        return relays;
    }
    let mut merged = relays;
    for endpoint in cfg.rpc_endpoints() {
        let trimmed = endpoint.trim();
        if trimmed.is_empty() {
            continue;
        }
        if merged
            .iter()
            .any(|existing| existing.trim().eq_ignore_ascii_case(trimmed))
        {
            continue;
        }
        info!(
            target: "broadcast",
            chain = %cfg.name,
            endpoint = %crate::util::redact_endpoint(trimmed),
            "Injecting primary RPC as sequencer submission endpoint (L2 has no bundle relay market)"
        );
        merged.insert(0, trimmed.to_owned());
    }
    merged
}

async fn ensure_executor_ownership_chain<M: Middleware + 'static>(
    executor: &MultiVenueArbExecutor<M>,
    expected_operator_owner: Option<Address>,
    chain: &str,
) -> Result<Address>
where
    M::Error: 'static,
{
    let batch_router = executor
        .owner()
        .call()
        .await
        .with_context(|| format!("fetch executor.owner() (BatchRouter) on {chain}"))?;
    if batch_router.is_zero() {
        anyhow::bail!("executor.owner() is zero on {chain}; deployment is broken");
    }

    let router_admin = BatchRouterAdmin::new(batch_router, executor.client());
    let operator_owner = router_admin
        .owner()
        .call()
        .await
        .with_context(|| format!("fetch BatchRouter.owner() on {chain}"))?;

    info!(
        chain = %chain,
        executor = %format!("{:#x}", executor.address()),
        batch_router = %format!("{batch_router:#x}"),
        operator_owner = %format!("{operator_owner:#x}"),
        "executor ownership chain: clone → BatchRouter → operator wallet"
    );

    if let Some(expected) = expected_operator_owner {
        if operator_owner != expected {
            anyhow::bail!(
                "BatchRouter.owner() is {operator_owner:#x} but configured EXECUTOR_OWNER is {expected:#x} on {chain}; \
fix ops/inputs.yaml and {}_EXECUTOR_OWNER to match the wallet that controls the router",
                chain_env_prefix_for_name(chain)
            );
        }
    } else {
        warn!(
            chain = %chain,
            operator = %format!("{operator_owner:#x}"),
            "EXECUTOR_OWNER not configured; set it to BatchRouter.owner() for startup validation"
        );
    }

    Ok(batch_router)
}

async fn ensure_executor_allowlisted<M: Middleware + 'static>(
    executor: &MultiVenueArbExecutor<M>,
    signer: Address,
    chain: &str,
) -> Result<()>
where
    M::Error: 'static,
{
    let allowed = executor
        .executors(signer)
        .call()
        .await
        .with_context(|| format!("fetch executor allowlist for signer {signer:#x}"))?;
    if allowed {
        info!(
            chain = %chain,
            signer = %format!("{signer:#x}"),
            "executor allowlist: hot wallet approved for startV2"
        );
        return Ok(());
    }
    anyhow::bail!(
        "hot wallet {signer:#x} is NOT in the executor allowlist on {chain}; \
every startV2 call (including simulation) reverts NotExecutor. \
Approve the signer via BatchRouter.setExecutor(signer, true) from the router owner, \
then verify with: cast call $EXECUTOR \"executors(address)(bool)\" {signer:#x} --rpc-url $RPC"
    );
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

/// Resolve the pinned bytecode hash for a contract check from the environment.
/// For a suffix like `EXECUTOR_ADDRESS` this reads `{PREFIX}_EXECUTOR_CODEHASH`
/// then `EXECUTOR_CODEHASH`.
fn resolve_expected_codehash(env_prefix: &str, suffix: &str) -> Result<Option<H256>> {
    let hash_suffix = suffix
        .strip_suffix("_ADDRESS")
        .map(|stem| format!("{stem}_CODEHASH"))
        .unwrap_or_else(|| format!("{suffix}_CODEHASH"));
    let prefixed_key = format!("{env_prefix}_{hash_suffix}");
    let raw = std::env::var(&prefixed_key)
        .or_else(|_| std::env::var(&hash_suffix))
        .ok();
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim().trim_start_matches("0x");
    let bytes = hex::decode(trimmed)
        .map_err(|err| anyhow!("invalid codehash in {prefixed_key}/{hash_suffix}: {err}"))?;
    ensure!(
        bytes.len() == 32,
        "codehash in {prefixed_key}/{hash_suffix} must be 32 bytes, got {}",
        bytes.len()
    );
    Ok(Some(H256::from_slice(&bytes)))
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

    // Attestation: pin the deployed bytecode to a known-good hash so a wrong
    // or malicious implementation at the configured address fails startup
    // instead of passing on a non-empty-code check alone.
    let onchain_codehash = H256::from(ethers::utils::keccak256(&code.0));
    match resolve_expected_codehash(check.env_prefix, check.suffix)? {
        Some(expected) => {
            ensure!(
                onchain_codehash == expected,
                "{} bytecode hash mismatch on {} at {:#x}: on-chain {:#x} != pinned {:#x}; \
refusing to start against unattested code",
                check.label,
                check.chain,
                check.address,
                onchain_codehash,
                expected,
            );
            info!(
                chain = %check.chain,
                label = %check.label,
                address = %format!("{:#x}", check.address),
                codehash = %format!("{:#x}", onchain_codehash),
                "contract bytecode hash attested against pinned value"
            );
        }
        None => {
            if check.label == "executor" && production_mode_enabled() {
                anyhow::bail!(
                    "production mode requires a pinned executor bytecode hash; set {}_EXECUTOR_CODEHASH={:#x} \
(current on-chain hash at {:#x}) after verifying the deployment",
                    check.env_prefix,
                    onchain_codehash,
                    check.address,
                );
            }
            warn!(
                chain = %check.chain,
                label = %check.label,
                address = %format!("{:#x}", check.address),
                codehash = %format!("{:#x}", onchain_codehash),
                "no pinned bytecode hash configured; set {}_{}_CODEHASH to attest this deployment",
                check.env_prefix,
                check.suffix.strip_suffix("_ADDRESS").unwrap_or(check.suffix),
            );
        }
    }
    Ok(())
}

fn resolve_public_jitter_bps(env_prefix: &str) -> Option<u32> {
    let prefixed = format!("{env_prefix}_PUBLIC_MEMPOOL_JITTER_BPS");
    std::env::var(&prefixed)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .or_else(|| {
            crate::util::env_parse_opt::<u32>("PUBLIC_MEMPOOL_JITTER_BPS")
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
            crate::util::env_parse_opt::<f64>("NATIVE_USD_PRICE")
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
        VenueEdge::Slipstream { .. } => "slipstream".to_string(),
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

fn relay_env_endpoints(env_prefix: &str, chain_name: &str) -> Option<Vec<String>> {
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
    // Global Flashbots-style relays are Ethereum-only. Using them on L2 sequencers
    // causes silent submission failures at live broadcast time.
    if chain_name.eq_ignore_ascii_case("ethereum") {
        if let Ok(urls) = std::env::var("PRIVATE_RELAY_URLS") {
            return Some(parse_endpoint_list(&urls));
        }
        if let Ok(url) = std::env::var("PRIVATE_RELAY_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                return Some(vec![trimmed.to_string()]);
            }
        }
    }
    None
}

fn is_ethereum_bundle_relay(endpoint: &str) -> bool {
    let lower = endpoint.to_ascii_lowercase();
    lower.contains("flashbots")
        || lower.contains("titanbuilder")
        || lower.contains("beaverbuild")
        || lower.contains("builder0x69")
        || DEFAULT_PRIVATE_RELAYS
            .iter()
            .any(|(_, url)| lower.starts_with(&url.to_ascii_lowercase()))
}

fn validate_broadcast_relays_for_chain(chain_name: &str, endpoints: &[String]) -> Result<()> {
    if !gas_model_uses_sequencer_submission_by_chain(chain_name) {
        return Ok(());
    }
    let bad: Vec<&str> = endpoints
        .iter()
        .filter(|ep| is_ethereum_bundle_relay(ep))
        .map(|s| s.as_str())
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    let prefix = chain_env_prefix_for_name(chain_name);
    anyhow::bail!(
        "chain {chain_name} is an L2 sequencer chain but broadcast relays include Ethereum bundle builders: {bad:?}. \
Remove PRIVATE_RELAY_URL(S) from .env or set {prefix}_PRIVATE_RELAY_URLS to your sequencer RPC (see ops/inputs.yaml broadcast.private_relays)"
    );
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
    let env_relays = relay_env_endpoints(&cfg.env_prefix, &cfg.name).filter(|relays| !relays.is_empty());
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
        validate_broadcast_relays_for_chain(&cfg.name, &endpoints)?;
        let endpoints = merge_sequencer_submission_relays(cfg, endpoints);
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

    let allow_default = read_feature_flag("ENABLE_DEFAULT_PRIVATE_RELAYS", true);
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

/// Env- and ops-inputs-derived runtime tuning knobs, resolved once at startup
/// (RuntimeTuning::from_env) and destructured into launch_chain_runtime so the
/// per-knob resolution lives in one place instead of inline in the launcher.
struct RuntimeTuning {
    edge_slippage_bps: u32,
    edge_prune_max_slippage_bps: u32,
    edge_prune_min_score: f64,
    edge_prune_liquidity_weight: f64,
    edge_prune_profit_weight: f64,
    edge_prune_slippage_weight: f64,
    max_gas_price_wei: U256,
    max_gas_price_congestion_bps: u32,
    profit_margin_bps: u32,
    opportunity_cost_wei: U256,
    cross_chain_profit_bps: u32,
    cross_chain_min_profit_wei: U256,
    max_candidate_paths: usize,
    quote_budget_ms: u64,
    min_edge_max_input: U256,
    min_liquidity_tokens: f64,
    max_quote_block_lag: U64,
    congestion_alpha: f64,
    competition_alpha: f64,
    cycle_limits: BellmanFordLimits,
    search_budget: Duration,
    quote_budget: Duration,
    simulation_budget: Duration,
    auto_hot_pool_cap: usize,
    max_edges_hot: usize,
    topk_per_token: usize,
    dynamic_top_tokens_30d: usize,
    mandatory_universe_tokens: HashSet<Address>,
    hub_tokens: HashSet<Address>,
    jit_config: Option<JitConfig>,
    cb_hourly_loss_limit: U256,
    cb_daily_loss_limit: U256,
    cb_max_consecutive_failures: u32,
}

impl RuntimeTuning {
    fn from_env(
        ops_inputs: &crate::ops_inputs::OpsInputs,
        chain_name: &str,
    ) -> Result<RuntimeTuning> {
    let universe_cfg = &ops_inputs.universe;
    let max_hops: usize = universe_cfg
        .max_hops
        .or_else(|| {
            crate::util::env_parse_opt("MAX_HOPS")
        })
        .unwrap_or(6);
    let max_hops_cap: usize = crate::util::env_parse_opt::<usize>("MAX_HOPS_CAP")
        .unwrap_or(8)
        .max(1);
    let bounded_max_hops = max_hops.min(max_hops_cap);
    let max_relaxations: usize = crate::util::env_parse_opt::<usize>("BELLMAN_MAX_RELAXATIONS")
        .unwrap_or(bounded_max_hops.saturating_mul(4).max(24))
        .clamp(1, 256);
    let max_hops = bounded_max_hops;
    let edge_slippage_bps: u32 = std::env::var("EDGE_SLIPPAGE_BPS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .context("parse EDGE_SLIPPAGE_BPS")?;
    let edge_prune_max_slippage_bps: u32 = universe_cfg
        .edge_prune_max_slippage_bps
        .or_else(|| {
            crate::util::env_parse_opt("EDGE_PRUNE_MAX_SLIPPAGE_BPS")
        })
        .unwrap_or(edge_slippage_bps);
    let edge_prune_min_score: f64 = universe_cfg
        .edge_prune_min_score
        .or_else(|| {
            crate::util::env_parse_opt("EDGE_PRUNE_MIN_SCORE")
        })
        .unwrap_or(0.0);
    let edge_prune_liquidity_weight: f64 = universe_cfg
        .edge_prune_liquidity_weight
        .or_else(|| {
            crate::util::env_parse_opt("EDGE_PRUNE_LIQUIDITY_WEIGHT")
        })
        .unwrap_or(1.0);
    let edge_prune_profit_weight: f64 = universe_cfg
        .edge_prune_profit_weight
        .or_else(|| {
            crate::util::env_parse_opt("EDGE_PRUNE_PROFIT_WEIGHT")
        })
        .unwrap_or(1.5);
    let edge_prune_slippage_weight: f64 = universe_cfg
        .edge_prune_slippage_weight
        .or_else(|| {
            crate::util::env_parse_opt("EDGE_PRUNE_SLIPPAGE_WEIGHT")
        })
        .unwrap_or(0.05);
    let max_gas_price_wei = crate::util::env_u256_opt("MAX_GAS_PRICE_WEI")
        .unwrap_or_else(|| U256::from(150_000_000_000u64));
    let max_gas_price_congestion_bps: u32 = std::env::var("MAX_GAS_PRICE_CONGESTION_BPS")
        .unwrap_or_else(|_| "12000".into())
        .parse()
        .context("parse MAX_GAS_PRICE_CONGESTION_BPS")?;
    let profit_margin_bps: u32 = std::env::var("PROFIT_MARGIN_BPS")
        .unwrap_or_else(|_| "200".into())
        .parse()
        .context("parse PROFIT_MARGIN_BPS")?;
    let opportunity_cost_wei = crate::util::env_u256_opt("OPPORTUNITY_COST_WEI")
        .unwrap_or_else(U256::zero);
    let cross_chain_profit_bps: u32 = std::env::var("CROSS_CHAIN_PROFIT_BPS")
        .unwrap_or_else(|_| "175".into())
        .parse()
        .context("parse CROSS_CHAIN_PROFIT_BPS")?;
    let cross_chain_min_profit_wei = crate::util::env_u256_opt("CROSS_CHAIN_MIN_PROFIT_WEI")
        .unwrap_or_else(U256::zero);
    let max_candidate_paths: usize = universe_cfg
        .cycle_candidate_cap_per_block
        .or_else(|| {
            crate::util::env_parse_opt("MAX_CANDIDATE_PATHS")
        })
        .unwrap_or(8)
        .max(1);
    let cycle_search_timeout_ms: u64 = universe_cfg
        .time_budget_ms
        .search
        .or_else(|| {
            crate::util::env_parse_opt("CYCLE_SEARCH_TIMEOUT_MS")
        })
        .unwrap_or(250);
    let quote_budget_ms: u64 = universe_cfg
        .time_budget_ms
        .quoting
        .or_else(|| {
            crate::util::env_parse_opt("QUOTE_BUDGET_MS")
        })
        .unwrap_or(250);
    let simulation_budget_ms: u64 = universe_cfg
        .time_budget_ms
        .simulation
        .or_else(|| {
            crate::util::env_parse_opt("SIMULATION_BUDGET_MS")
        })
        .unwrap_or(400);
    let (cycle_search_timeout_ms, quote_budget_ms, simulation_budget_ms) =
        derive_chain_time_budget_ms(
            chain_name,
            cycle_search_timeout_ms,
            quote_budget_ms,
            simulation_budget_ms,
        );
    let cycle_search_timeout = Duration::from_millis(cycle_search_timeout_ms.max(1));
    let max_bellman_cycles: usize = crate::util::env_parse_opt::<usize>("MAX_BELLMAN_CYCLES")
        .unwrap_or_else(|| {
            max_candidate_paths
                .saturating_mul(4)
                .max(max_candidate_paths)
                .max(1)
        });
    let min_edge_max_input = crate::util::env_u256_opt("MIN_EDGE_MAX_INPUT_WEI")
        .unwrap_or_else(U256::zero);
    let mut min_liquidity_tokens: f64 = std::env::var("MIN_LIQUIDITY_TOKENS")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .unwrap_or(0.0);
    if let Some(value) = ops_inputs.universe.min_pool_liquidity_tokens {
        min_liquidity_tokens = value;
    }
    let max_quote_block_lag = crate::util::env_parse_opt::<u64>("MAX_QUOTE_BLOCK_LAG")
        .map(U64::from)
        .unwrap_or_else(|| U64::from(2u64));
    let jit_enabled = read_feature_flag("JIT_LP_ENABLED", false);
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
    let jit_disable_on_quote_failure = read_feature_flag("JIT_DISABLE_ON_MIN_OUT_FAIL", true);
    let congestion_alpha: f64 = std::env::var("CONGESTION_EMA_ALPHA")
        .unwrap_or_else(|_| "0.3".into())
        .parse()
        .unwrap_or(0.3);
    let competition_alpha: f64 = std::env::var("COMPETITION_EMA_ALPHA")
        .unwrap_or_else(|_| "0.45".into())
        .parse()
        .unwrap_or(0.45);
    // Two-pool arbs (a single token pair priced differently across two venues,
    // e.g. WETH/USDC on Uniswap vs Aerodrome) are 2-hop cycles and are the most
    // frequent, highest-turnover opportunity on every chain. The token graph
    // resolves the best edge per direction, so same-pool round-trips self-reject
    // (weight >= 0) and only genuine cross-venue spreads survive. A min_hops of
    // 3 silently excluded this entire opportunity class; 2 is the correct floor.
    let min_hops: usize = crate::util::env_parse_opt("MIN_HOPS")
        .unwrap_or(2);
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
    let auto_hot_pool_cap = derive_chain_hot_pool_cap(chain_name, quote_budget_ms);
    let auto_max_edges_hot = derive_chain_max_edges_hot(chain_name, quote_budget_ms);
    let max_edges_hot = universe_cfg
        .max_edges_hot
        .unwrap_or(auto_max_edges_hot)
        .max(1);
    let topk_per_token = universe_cfg.topk_per_token.unwrap_or(3).max(1);
    let raw_dynamic_top_tokens_30d = crate::util::env_parse_opt::<usize>("DYNAMIC_TOP_TOKENS_30D")
        .or(universe_cfg.dynamic_top_tokens_30d)
        .unwrap_or(200);
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

    let cb_hourly_loss_limit = crate::util::env_u256_opt("CB_HOURLY_LOSS_LIMIT_WEI")
        .unwrap_or_else(U256::zero);
    let cb_daily_loss_limit = crate::util::env_u256_opt("CB_DAILY_LOSS_LIMIT_WEI")
        .unwrap_or_else(U256::zero);
    // Expected trades until k consecutive reverts at revert rate p is
    // (1 - p^k) / (p^k * (1 - p)). At the old default of 3-4, a normal 60-65%
    // revert rate trips the breaker every ~13-17 fills, which makes it noise
    // rather than protection:
    //   k=4,  p=0.65 -> ~13 trades between false trips
    //   k=15, p=0.65 -> ~1,800 trades;  k=15, p=0.55 -> ~17,400 trades
    // Note the boundary: the check is `failures > limit`, so 15 halts on the
    // SIXTEENTH consecutive revert. At p=0.65 that is a false trip roughly every
    // 2,800 trades, while still catching a 100%-reverting deploy within 16
    // transactions — on Base that costs well under a dollar in gas.
    let cb_max_consecutive_failures = crate::util::env_parse_opt::<u32>("CB_MAX_CONSECUTIVE_FAILURES")
        .unwrap_or(15);
        Ok(RuntimeTuning {
            edge_slippage_bps,
            edge_prune_max_slippage_bps,
            edge_prune_min_score,
            edge_prune_liquidity_weight,
            edge_prune_profit_weight,
            edge_prune_slippage_weight,
            max_gas_price_wei,
            max_gas_price_congestion_bps,
            profit_margin_bps,
            opportunity_cost_wei,
            cross_chain_profit_bps,
            cross_chain_min_profit_wei,
            max_candidate_paths,
            quote_budget_ms,
            min_edge_max_input,
            min_liquidity_tokens,
            max_quote_block_lag,
            congestion_alpha,
            competition_alpha,
            cycle_limits,
            search_budget,
            quote_budget,
            simulation_budget,
            auto_hot_pool_cap,
            max_edges_hot,
            topk_per_token,
            dynamic_top_tokens_30d,
            mandatory_universe_tokens,
            hub_tokens,
            jit_config,
            cb_hourly_loss_limit,
            cb_daily_loss_limit,
            cb_max_consecutive_failures,
        })
    }
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
    let min_flash_loan_wei = crate::util::env_u256_opt("MIN_FLASH_LOAN_WEI")
        .unwrap_or_else(|| default_min_flash.max(U256::one()));
    let default_max_flash = base_amount_wei
        .checked_mul(U256::from(5u64))
        .unwrap_or(U256::MAX);
    let mut max_flash_loan_wei = crate::util::env_u256_opt("MAX_FLASH_LOAN_WEI")
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
    let growth_unit = crate::util::env_u256_opt("COMPOUND_GROWTH_UNIT_WEI")
        .unwrap_or_else(|| base_amount_wei.max(U256::one()));
    let max_base_cap = crate::util::env_u256_opt("COMPOUND_MAX_BASE_WEI")
        .unwrap_or_else(|| {
            max_flash_loan_wei
                .checked_mul(U256::from(10u64))
                .unwrap_or(U256::MAX)
        });
    let siphon_threshold = crate::util::env_u256_opt("SIPHON_THRESHOLD_WEI")
        .unwrap_or_else(|| default_min_flash.max(U256::one()));
    let siphon_target = crate::util::env_parse_opt::<Address>("SIPHON_TARGET_ADDRESS");
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

    // Metrics are created before any background workers so every supervised
    // worker can report restarts via worker_restarts_total.
    let metrics_port = crate::util::env_parse_opt::<u16>("PROMETHEUS_PORT");
    let metrics: Option<Arc<Metrics>> = if let Some(port) = metrics_port {
        let metrics = Arc::new(Metrics::new()?);
        let exporter = metrics.clone();
        spawn_supervised(
            "prometheus_exporter",
            cfg.name.clone(),
            Some(metrics.clone()),
            move || {
                let exporter = exporter.clone();
                async move {
                    if let Err(err) = exporter.export_to_prometheus(port).await {
                        warn!(port = port, error = %err, "Prometheus exporter terminated");
                    }
                }
            },
        );
        Some(metrics)
    } else {
        None
    };

    let pool_depth_refresh_secs = crate::util::env_parse_opt::<u64>("POOL_DEPTH_REFRESH_SECS")
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
        spawn_supervised(
            "pool_depth_refresh",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let cache = cache.clone();
                async move {
                    cache.refresh_all().await;
                    loop {
                        sleep(interval).await;
                        cache.refresh_all().await;
                    }
                }
            },
        );
    }

    let RuntimeTuning {
        edge_slippage_bps,
        edge_prune_max_slippage_bps,
        edge_prune_min_score,
        edge_prune_liquidity_weight,
        edge_prune_profit_weight,
        edge_prune_slippage_weight,
        max_gas_price_wei,
        max_gas_price_congestion_bps,
        profit_margin_bps,
        opportunity_cost_wei,
        cross_chain_profit_bps,
        cross_chain_min_profit_wei,
        max_candidate_paths,
        quote_budget_ms,
        min_edge_max_input,
        min_liquidity_tokens,
        max_quote_block_lag,
        congestion_alpha,
        competition_alpha,
        cycle_limits,
        search_budget,
        quote_budget,
        simulation_budget,
        auto_hot_pool_cap,
        max_edges_hot,
        topk_per_token,
        dynamic_top_tokens_30d,
        mandatory_universe_tokens,
        hub_tokens,
        jit_config,
        cb_hourly_loss_limit,
        cb_daily_loss_limit,
        cb_max_consecutive_failures,
    } = RuntimeTuning::from_env(ops_inputs, &cfg.name)?;
    let universe_cfg = &ops_inputs.universe;

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
    let chaos_disable_ws = read_feature_flag("CHAOS_DISABLE_WS", false);
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

    // Every check below validates the on-chain executor: that it is deployed,
    // attested, allowlists our signer, and is owned through the expected chain.
    // They exist to stop a LIVE run against the wrong contract.
    //
    // A shadow run never broadcasts (`shadow_dispatch` short-circuits
    // `dispatch_call`), so these validate a contract that is never called —
    // while hard-failing startup on any chain without a deployed executor. That
    // blocks detection-only runs, which is exactly the venue-comparison
    // workflow: pointing the scanner at a new chain to measure whether edge
    // exists there before committing to a deployment.
    //
    // So gate strictly on shadow. This is deliberately NOT a standalone bypass
    // flag: the instant SHADOW_MODE is off, every check runs again
    // unconditionally, so no stray env var can start a funded run against an
    // unvalidated executor.
    let shadow_enabled = read_feature_flag("SHADOW_MODE", false);
    let executor_max_slippage_bps = if shadow_enabled {
        warn!(
            chain = %cfg.name,
            executor = %format!("{executor_address:#x}"),
            max_slippage_bps = shadow_executor_max_slippage_bps(),
            "SHADOW_MODE: executor preflight SKIPPED (deployment, attestation, \
             allowlist, ownership, permit2). This run cannot broadcast and the \
             executor is NOT validated — never reuse this config for a live run"
        );
        shadow_executor_max_slippage_bps()
    } else {
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
        ensure_executor_allowlisted(&executor, wallet.address(), &cfg.name).await?;
        let batch_router =
            ensure_executor_ownership_chain(&executor, executor_owner, &cfg.name).await?;
        let (_, executor_max_slippage_bps_raw, _) = executor
            .get_config()
            .call()
            .await
            .context("fetch executor config")?;

        info!(
            chain = %cfg.name,
            batch_router = %format!("{batch_router:#x}"),
            signer = %format!("{:#x}", wallet.address()),
            "live execution path: signer → executor.startV2 (allowlisted) or BatchRouter.startV2 (router owner)"
        );

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

        u32::from(executor_max_slippage_bps_raw)
    };

    let (broadcast_endpoint, public_jitter_override) =
        select_broadcast_endpoint(&cfg, ops_chain, &wallet, ws_backoff).await?;

    let mev_role = std::env::var("MEV_ROLE")
        .ok()
        .map(|raw| raw.parse())
        .transpose()?
        .unwrap_or_default();

    let filler_priority_fee = crate::util::env_u256_opt("FILLER_PRIORITY_FEE_WEI")
        .or_else(|| Some(U256::from(2_000_000_000u64)));

    let searcher_priority_fee = crate::util::env_u256_opt("SEARCHER_PRIORITY_FEE_WEI");

    let public_jitter_bps = public_jitter_override
        .or_else(|| resolve_public_jitter_bps(&cfg.env_prefix))
        .unwrap_or(75);

    // Profit-aware bidding: share of expected net profit biddable as priority
    // tip. Default 50% — aggressive enough to win contested inclusion while
    // guaranteeing the trade keeps at least half its edge. Clamped to <=90% so
    // a bid can never erase the entire margin. Set 0 to disable.
    let bid_profit_fraction_bps = std::env::var("ARBOT_TIP_BPS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .or_else(|| {
            std::env::var("ARBOT_BID_PROFIT_FRACTION_BPS")
                .ok()
                .and_then(|raw| raw.trim().parse::<u32>().ok())
        })
        .unwrap_or(5_000)
        .min(9_000);

    if cfg.chain_id == 8453 && matches!(broadcast_endpoint, BroadcastEndpoint::Private { .. }) {
        warn!(
            chain = %cfg.name,
            chain_id = cfg.chain_id,
            "Base (8453) uses public eth_sendRawTransaction for inclusion; Flashbots-style bundle relays are ignored on this chain"
        );
    }

    let broadcast = BroadcastConfig {
        endpoint: broadcast_endpoint,
        role: mev_role,
        filler_priority_fee,
        searcher_priority_fee,
        public_jitter_bps,
        private_inclusion_timeout: crate::util::env_parse_opt::<u64>("PRIVATE_RELAY_TIMEOUT_MS")
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(4500)),
        relay_health: relay_health.clone(),
        bid_profit_fraction_bps,
    };

    // `shadow_enabled` is read once, before the executor preflight above, so the
    // preflight gate and the dispatch path can never disagree about whether this
    // is a shadow run.
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

    let chaos_relay_reject_bps = crate::util::env_parse_opt::<u32>("CHAOS_RELAY_REJECT_BPS")
        .unwrap_or(0);
    let chaos_public_reject_bps = crate::util::env_parse_opt::<u32>("CHAOS_PUBLIC_REJECT_BPS")
        .unwrap_or(0);
    let chaos_broadcast_delay_ms = crate::util::env_parse_opt::<u64>("CHAOS_BROADCAST_DELAY_MS")
        .unwrap_or(0);
    let chaos = ChaosConfig {
        relay_reject_bps: chaos_relay_reject_bps,
        public_reject_bps: chaos_public_reject_bps,
        broadcast_delay_ms: chaos_broadcast_delay_ms,
    };

    let feature_gate = FeatureGate::from_env_for_chain(ops_inputs, &cfg.name);
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
        .unwrap_or_else(|_| ops_inputs.backrun_enabled_for(&cfg.name));
    let backrun_enabled = if backrun_requested && !feature_gate.backrun {
        warn!("Backrun monitor requested but FEATURE_BACKRUN=0; forcing disabled");
        false
    } else {
        backrun_requested && feature_gate.backrun
    };

    let backrun_monitor = if backrun_enabled {
        let min_amount = crate::util::env_u256_opt("BACKRUN_MIN_AMOUNT_WEI")
            .unwrap_or(base_amount_wei);
        let min_price_impact_bps = crate::util::env_parse_opt::<u32>("BACKRUN_MIN_PRICE_IMPACT_BPS")
            .unwrap_or(75);
        let poll_interval = crate::util::env_parse_opt::<u64>("BACKRUN_POLL_INTERVAL_MS")
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(400));
        let monitor = Arc::new(BackrunMonitor::new(
            min_amount,
            min_price_impact_bps,
            token_list.clone(),
        ));
        info!(
            chain = %cfg.name,
            min_amount_wei = %min_amount,
            min_price_impact_bps,
            poll_interval_ms = poll_interval.as_millis(),
            "Backrun/mempool monitor enabled"
        );
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
    let sandwich_min_profit = crate::util::env_u256_opt("SANDWICH_MIN_PROFIT_WEI")
        .unwrap_or_else(|| U256::from(100_000_000_000_000u64));
    let sandwich_monitor = if sandwich_enabled {
        match SandwichMonitor::new(provider.clone(), &cfg.env_prefix, sandwich_min_profit) {
            Ok(monitor) => {
                let monitor = Arc::new(monitor);
                let task_monitor = monitor.clone();
                spawn_supervised(
                    "sandwich_monitor",
                    cfg.name.clone(),
                    metrics.clone(),
                    move || {
                        let monitor = task_monitor.clone();
                        async move {
                            monitor.run(Duration::from_millis(300)).await;
                        }
                    },
                );
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
        let bridge_max_time_secs = crate::util::env_parse_opt::<u64>("BRIDGE_MAX_TIME_SECS")
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

    let circuit_breaker = Arc::new(
        CircuitBreaker::new(
            cb_hourly_loss_limit,
            cb_daily_loss_limit,
            cb_max_consecutive_failures,
        )
        .configure_health_from_env(),
    );

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
    let mut cold_pancakeswap_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut cold_slipstream_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut hot_univ2_by_venue: HashMap<String, Vec<ResolvedUniV2PoolCfg>> = HashMap::new();
    let mut hot_univ3_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut hot_pancakeswap_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut hot_slipstream_by_venue: HashMap<String, Vec<PoolRecord>> = HashMap::new();
    let mut combined_univ2: Vec<ResolvedUniV2PoolCfg> = Vec::new();
    let mut combined_univ3: Vec<PoolRecord> = Vec::new();
    let mut combined_pancakeswap: Vec<PoolRecord> = Vec::new();
    let mut combined_slipstream: Vec<PoolRecord> = Vec::new();
    let token_decimals_hint: HashMap<Address, u8> = load_token_decimals_map(ops_inputs);

    for venue in venues.iter() {
        let Some(kind) = venue.kind.as_ref() else {
            continue;
        };
        match kind {
            crate::ops_inputs::VenueKind::Univ2Like => {
                if cfg.name.eq_ignore_ascii_case("base") {
                    info!(
                        chain = %cfg.name,
                        venue = %venue.name,
                        kind = "univ2_like",
                        "skipping univ2 cold inventory on Base; Aerodrome + UniV3 carry liquidity"
                    );
                    cold_univ2_by_venue.insert(venue.name.clone(), Vec::new());
                    continue;
                }
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
                    prioritize_cold_pool_inventory(&mut cold, hot_pool_config.max_cold_pools);
                }
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "univ2_like",
                    cold_pool_records = cold.len(),
                    max_cold_pools = hot_pool_config.max_cold_pools,
                    "loaded cold pool inventory"
                );
                cold_univ2_by_venue.insert(venue.name.clone(), cold);
            }
            crate::ops_inputs::VenueKind::Univ3Like if is_pancakeswap_univ3_venue(&venue.name) => {
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
                        "failed to load cold pancakeswap pools for chain={} venue={} path={}",
                        cfg.name,
                        venue.name,
                        path.display()
                    )
                })?;

                if feature_gate.cycle_arb {
                    require_pool_inventory(&cfg.name, &venue.name, &path, &cold)?;
                }

                if cold.len() > hot_pool_config.max_cold_pools {
                    prioritize_cold_pool_inventory(&mut cold, hot_pool_config.max_cold_pools);
                }
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "univ3_like",
                    cold_pool_records = cold.len(),
                    max_cold_pools = hot_pool_config.max_cold_pools,
                    "loaded cold pool inventory"
                );
                cold_pancakeswap_by_venue.insert(venue.name.clone(), cold);
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
                    prioritize_cold_pool_inventory(&mut cold, hot_pool_config.max_cold_pools);
                }
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "univ3_like",
                    cold_pool_records = cold.len(),
                    max_cold_pools = hot_pool_config.max_cold_pools,
                    "loaded cold pool inventory"
                );
                cold_univ3_by_venue.insert(venue.name.clone(), cold);
            }
            crate::ops_inputs::VenueKind::SlipstreamLike => {
                let path = pool_data_path(&cfg.name, &venue.name);
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "slipstream_like",
                    pool_inventory_path = %path.display(),
                    "loading cold pool inventory"
                );
                let mut cold = load_pool_records(&path).with_context(|| {
                    format!(
                        "failed to load cold slipstream pools for chain={} venue={} path={}",
                        cfg.name,
                        venue.name,
                        path.display()
                    )
                })?;

                if feature_gate.cycle_arb {
                    require_pool_inventory(&cfg.name, &venue.name, &path, &cold)?;
                }

                if cold.len() > hot_pool_config.max_cold_pools {
                    prioritize_cold_pool_inventory(&mut cold, hot_pool_config.max_cold_pools);
                }
                info!(
                    chain = %cfg.name,
                    venue = %venue.name,
                    kind = "slipstream_like",
                    cold_pool_records = cold.len(),
                    max_cold_pools = hot_pool_config.max_cold_pools,
                    "loaded cold pool inventory"
                );
                cold_slipstream_by_venue.insert(venue.name.clone(), cold);
            }
            _ => {}
        }
    }

    let v2_rank_inputs: Vec<(String, Vec<PoolRecord>)> = cold_univ2_by_venue
        .iter()
        .map(|(name, cold)| (name.clone(), cold.clone()))
        .collect();
    let v3_rank_inputs: Vec<(String, Vec<PoolRecord>)> = cold_univ3_by_venue
        .iter()
        .map(|(name, cold)| (name.clone(), cold.clone()))
        .collect();
    let pancake_rank_inputs: Vec<(String, Vec<PoolRecord>)> = cold_pancakeswap_by_venue
        .iter()
        .map(|(name, cold)| (name.clone(), cold.clone()))
        .collect();
    let slipstream_rank_inputs: Vec<(String, Vec<PoolRecord>)> = cold_slipstream_by_venue
        .iter()
        .map(|(name, cold)| (name.clone(), cold.clone()))
        .collect();
    let chain_for_rank = cfg.name.clone();
    let chain_for_rank_v3 = chain_for_rank.clone();
    let chain_for_rank_slipstream = chain_for_rank.clone();
    let provider_v2 = provider.clone();
    let provider_v3 = provider.clone();
    let provider_slipstream = provider.clone();
    let hot_config_v2 = hot_pool_config.clone();
    let hot_config_v3 = hot_pool_config.clone();
    let hot_config_slipstream = hot_pool_config.clone();
    let decimals_for_rank = token_decimals_hint.clone();
    let univ3_rank_ctx = Arc::new(build_univ3_rank_context(ops_inputs, &cfg.env_prefix));
    let univ3_rank_ctx_for_startup = Arc::clone(&univ3_rank_ctx);
    let slipstream_rank_ctx = Arc::clone(&univ3_rank_ctx_for_startup);

    let chain_for_rank_pancake = chain_for_rank.clone();
    let provider_pancake = provider.clone();
    let hot_config_pancake = hot_pool_config.clone();
    let pancake_rank_ctx = Arc::clone(&univ3_rank_ctx_for_startup);

    let (v2_ranked, v3_ranked, pancake_ranked, slipstream_ranked) = tokio::join!(
        async move {
            let mut ranked = Vec::new();
            for (venue_name, cold) in v2_rank_inputs {
                let hot = rank_univ2_pools(
                    provider_v2.clone(),
                    cold.as_slice(),
                    &decimals_for_rank,
                    &hot_config_v2,
                )
                .await
                .unwrap_or_default();
                log_hot_pool_refresh(&chain_for_rank, &venue_name, "univ2_like", hot.len());
                ranked.push((venue_name, univ2_configs_from_records(&hot)));
            }
            ranked
        },
        async move {
            let mut ranked = Vec::new();
            let rank_ctx = univ3_rank_ctx_for_startup.clone();
            for (venue_name, cold) in v3_rank_inputs {
                let hot = match rank_univ3_pools(
                    provider_v3.clone(),
                    cold.as_slice(),
                    &hot_config_v3,
                    rank_ctx.as_ref(),
                )
                .await
                {
                    Ok(hot) => hot,
                    Err(err) => {
                        warn!(
                            error = %err,
                            chain = %chain_for_rank_v3,
                            venue = %venue_name,
                            "failed to rank initial univ3 hot pools; continuing with empty set"
                        );
                        Vec::new()
                    }
                };
                log_hot_pool_refresh(&chain_for_rank_v3, &venue_name, "univ3_like", hot.len());
                ranked.push((venue_name, hot));
            }
            ranked
        },
        async move {
            let mut ranked = Vec::new();
            let rank_ctx = pancake_rank_ctx;
            for (venue_name, cold) in pancake_rank_inputs {
                let hot = match rank_univ3_pools(
                    provider_pancake.clone(),
                    cold.as_slice(),
                    &hot_config_pancake,
                    rank_ctx.as_ref(),
                )
                .await
                {
                    Ok(hot) => hot,
                    Err(err) => {
                        warn!(
                            error = %err,
                            chain = %chain_for_rank_pancake,
                            venue = %venue_name,
                            "failed to rank initial pancakeswap hot pools; continuing with empty set"
                        );
                        Vec::new()
                    }
                };
                log_hot_pool_refresh(
                    &chain_for_rank_pancake,
                    &venue_name,
                    "univ3_like",
                    hot.len(),
                );
                ranked.push((venue_name, hot));
            }
            ranked
        },
        async move {
            let mut ranked = Vec::new();
            let rank_ctx = slipstream_rank_ctx;
            for (venue_name, cold) in slipstream_rank_inputs {
                let hot = match rank_univ3_pools(
                    provider_slipstream.clone(),
                    cold.as_slice(),
                    &hot_config_slipstream,
                    rank_ctx.as_ref(),
                )
                .await
                {
                    Ok(hot) => hot,
                    Err(err) => {
                        warn!(
                            error = %err,
                            chain = %chain_for_rank_slipstream,
                            venue = %venue_name,
                            "failed to rank initial slipstream hot pools; continuing with empty set"
                        );
                        Vec::new()
                    }
                };
                log_hot_pool_refresh(
                    &chain_for_rank_slipstream,
                    &venue_name,
                    "slipstream_like",
                    hot.len(),
                );
                ranked.push((venue_name, hot));
            }
            ranked
        }
    );

    for (venue_name, configs) in v2_ranked {
        combined_univ2.extend(configs.clone());
        hot_univ2_by_venue.insert(venue_name, configs);
    }
    for (venue_name, hot) in v3_ranked {
        combined_univ3.extend(hot.clone());
        hot_univ3_by_venue.insert(venue_name, hot);
    }
    for (venue_name, hot) in pancake_ranked {
        combined_pancakeswap.extend(hot.clone());
        hot_pancakeswap_by_venue.insert(venue_name, hot);
    }
    for (venue_name, hot) in slipstream_ranked {
        combined_slipstream.extend(hot.clone());
        hot_slipstream_by_venue.insert(venue_name, hot);
    }

    let hot_univ2_pools = Arc::new(tokio::sync::RwLock::new(combined_univ2));
    let hot_univ3_pools = Arc::new(tokio::sync::RwLock::new(combined_univ3));
    let hot_pancakeswap_pools = Arc::new(tokio::sync::RwLock::new(combined_pancakeswap));
    let hot_slipstream_pools = Arc::new(tokio::sync::RwLock::new(combined_slipstream));
    let hot_univ2_by_venue = Arc::new(tokio::sync::RwLock::new(hot_univ2_by_venue));
    let hot_univ3_by_venue = Arc::new(tokio::sync::RwLock::new(hot_univ3_by_venue));
    let hot_pancakeswap_by_venue = Arc::new(tokio::sync::RwLock::new(hot_pancakeswap_by_venue));
    let hot_slipstream_by_venue = Arc::new(tokio::sync::RwLock::new(hot_slipstream_by_venue));

    // Declared out here so the drain can be spawned AFTER the runner exists:
    // it needs the runner's graph snapshot, and the monitor is built first.
    let mut base_fast_drain: Option<BaseFastDrain> = None;
    let pool_monitor = {
        let poll_ms = crate::util::env_parse_opt::<u64>("POOL_MONITOR_POLL_MS")
            .unwrap_or(1_200);
        let stale_ms = crate::util::env_parse_opt::<u64>("POOL_MONITOR_STALE_MS")
            .unwrap_or_else(|| poll_ms.saturating_mul(3));
        let pools = hot_univ2_pools.read().await.clone();
        let solidly_pools = solidly_monitored_pools(&cfg.env_prefix);
        let monitored = venues::merge_monitored_pools(
            monitored_pools_from_configs(&pools),
            solidly_pools,
        );
        // Cloned before PoolMonitor::new takes ownership: the Base fast path
        // needs the SAME V2 set. Until now its universe was CL-only, so every
        // cycle through an Aerodrome or UniV2 pool was structurally unpriceable
        // no matter how good the CL state was.
        let v2_pools: Vec<MonitoredPool> = monitored.clone();
        // Solidly-family venue parameters. `stable` selects the invariant and
        // comes from config -- a stable pool routed as volatile prices on the
        // wrong curve. Decimals matter for the same reason and are hinted, not
        // guessed: a missing hint takes 18, which is the Base default for
        // everything except the stablecoins, so it is recorded as a known risk
        // rather than a silent one.
        let mut v2_venues: HashMap<Address, crate::base_fast::FastVenue> = HashMap::new();
        for p in &v2_pools {
            let d = |t: &Address| token_decimals_hint.get(t).copied().unwrap_or(18);
            v2_venues.insert(
                p.pair,
                crate::base_fast::FastVenue::Solidly {
                    stable: p.stable,
                    fee_bps: p.fee_bps,
                    decimals0: d(&p.token_in),
                    decimals1: d(&p.token_out),
                },
            );
        }

        // CL pools are subscribed for logs but never polled. Without them the
        // Swap decoder never sees a log: the subscription carried 23 of ~985
        // Base pools, so 97.7% of the graph had no event source at all.
        //
        // Passed as STICKY rather than merged here: the univ2 hot-pool refresh
        // calls set_pools with a set rebuilt from scratch, which dropped these
        // and silently reverted the subscription to 23 pools after 5 minutes.
        let mut cl_pools: Vec<MonitoredPool> = Vec::new();
        // Venue parameters captured HERE, where UniV3 and Slipstream are still
        // separate lists. Once they merge into `cl_pools` the distinction is
        // gone, and it cannot be recovered: routing a UniV3 path through the
        // Slipstream router would build calldata for the wrong contract.
        // Venue routers, resolved here because venue identity is still known.
        // `cfg.univ3_router` is the contract the executor itself uses for
        // `op: EXECUTOR_OP_UNIV3`; anything else has to be named explicitly in
        // the calldata. Zero means unresolved, which is treated as "use the
        // built-in one" -- the pre-existing behaviour, not a new guess.
        let univ3_router_address = cfg.univ3_router;
        let slipstream_router_for_fast = resolve_slipstream_venue(ops_inputs, &cfg.name)
            .map(|(_, router, _)| router)
            .unwrap_or_else(Address::zero);
        let pancakeswap_router_for_fast = resolve_pancakeswap_venue(ops_inputs, &cfg.name)
            .map(|(_, router, _)| router)
            .unwrap_or_else(Address::zero);
        info!(
            univ3_router = %format!("{univ3_router_address:#x}"),
            slipstream_router = %format!("{slipstream_router_for_fast:#x}"),
            pancakeswap_router = %format!("{pancakeswap_router_for_fast:#x}"),
            "base fast path venue routers"
        );
        let mut fast_venues: HashMap<Address, crate::base_fast::FastVenue> = HashMap::new();
        // One rule, no per-venue special cases: a pool may use the executor's
        // BUILT-IN UniV3 router only if that is genuinely its venue's router.
        // `StepData::Uniswap` carries no target, so the executor chooses --
        // correct for Uniswap, and silently wrong for every fork. Aerodrome
        // Slipstream shipped that way until 2026-09-03 and every cycle through
        // its 84 pools would have reverted. Anything else routes explicitly.
        //
        // Applied by comparing addresses rather than venue names, so a new fork
        // is handled by configuring it, not by editing this match.
        let builtin_univ3_router = univ3_router_address;
        // Iterated BY VENUE, not by hot list. Base configures
        // aerodrome_slipstream_v3 as univ3_like, so its pools share a hot list
        // with Uniswap's -- and tagging the list sent tick spacings to a quoter
        // that reads them as fee tiers. Measured 2026-09-03: pool 0x42d4a22c
        // has `fee: 10`, a spacing, and Uniswap V3 has no tier 10, so the
        // quoter returned `execution reverted` on all 194 attempts. Every
        // unquotable cycle in that capture was this, 1:1.
        for by_venue in [&hot_univ3_by_venue, &hot_pancakeswap_by_venue, &hot_slipstream_by_venue]
        {
            for (venue_name, records) in by_venue.read().await.iter() {
                let venue_router =
                    venue_router_by_name(ops_inputs, &cfg.name, venue_name)
                        .unwrap_or_else(Address::zero);
                let explicit = venue_router != builtin_univ3_router && !venue_router.is_zero();
                info!(
                    venue = %venue_name,
                    pools = records.len(),
                    router = %format!("{venue_router:#x}"),
                    routing = if explicit { "explicit" } else { "executor-builtin" },
                    "base fast path venue routing"
                );
            for r in records.iter() {
                cl_pools.push(MonitoredPool {
                    pair: r.pool,
                    token_in: r.token0,
                    token_out: r.token1,
                    fee_bps: r.fee,
                    stable: false,
                    kind: ingestion::PoolMonitorKind::ConcentratedLiquidity,
                });
                fast_venues.insert(
                    r.pool,
                    if venue_router == builtin_univ3_router || venue_router.is_zero() {
                        crate::base_fast::FastVenue::UniV3 { fee: r.fee }
                    } else {
                        // `r.fee` verbatim: a fee tier for Pancake, a tick
                        // spacing for Slipstream. Both occupy the same path
                        // field and each router reads its own meaning.
                        crate::base_fast::FastVenue::RoutedCl {
                            path_param: r.fee,
                            router: venue_router,
                        }
                    },
                );
            }
            }
        }

        if monitored.is_empty() {
            None
        } else {
            match PoolMonitor::new(
                provider.clone(),
                ws_provider.clone(),
                monitored,
                Duration::from_millis(poll_ms.max(250)),
                Duration::from_millis(stale_ms.max(poll_ms)),
                // Was `None`, which made ingestion_ws_events, ingestion_poll_refresh,
                // ingestion_active_pools and ingestion_stale_pools structurally
                // unreachable: registered at startup so they appeared in /metrics,
                // but never incremented. ingestion_ws_events_total therefore read 0
                // whether the log subscription was healthy or completely dead.
                metrics.clone(),
            ) {
                Ok(monitor) => {
                    let chaos_gap = std::env::var("CHAOS_WS_GAP_SECS")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                        .filter(|s| *s > 0)
                        .map(Duration::from_secs);
                    if let Some(d) = chaos_gap {
                        warn!(
                            secs = d.as_secs(),
                            "CHAOS_WS_GAP_SECS set; forcing websocket gaps. Test harness only"
                        );
                    }
                    // CL and V2 together. Every pool the fast path subscribes
                    // to, prices from, and resolves cycles through.
                    let fast_all: Vec<&MonitoredPool> =
                        cl_pools.iter().chain(v2_pools.iter()).collect();
                    let fast_pools: Vec<Address> =
                        fast_all.iter().map(|p| p.pair).collect();
                    // Token pairs for the same pools, so the drain loop can map
                    // a dirty POOL to the token HOP the cycle index is keyed by.
                    let fast_universe: Vec<(Address, Address, Address)> = fast_all
                        .iter()
                        .map(|p| (p.pair, p.token_in, p.token_out))
                        .collect();
                    // `MonitoredPool.fee_bps` is a lie by omission: it holds
                    // whatever unit its SOURCE used. Verified 2026-09-03 against
                    // the Base inventories -- all 141,364 uniswap_v2 records
                    // carry fee 30 and config/base_aerodrome_pools.json carries
                    // feeBps 30 (basis points), while uniswap_v3 carries
                    // 100/500/3000/10000 (ppm). Reading bps as ppm undercharges
                    // by 100x, which is ~87 bps of fabricated edge on a triangle.
                    let fast_meta: std::collections::HashMap<
                        Address,
                        crate::base_fast::PoolMeta,
                    > = fast_all
                        .iter()
                        .map(|p| {
                            let cl = matches!(
                                p.kind,
                                ingestion::PoolMonitorKind::ConcentratedLiquidity
                            );
                            let (kind, unit) = if cl {
                                (
                                    crate::base_fast::PoolKind::ConcentratedLiquidity,
                                    crate::base_fast::FeeUnit::Ppm,
                                )
                            } else if p.stable {
                                (
                                    crate::base_fast::PoolKind::StableSwap,
                                    crate::base_fast::FeeUnit::Bps,
                                )
                            } else {
                                (
                                    crate::base_fast::PoolKind::ConstantProduct,
                                    crate::base_fast::FeeUnit::Bps,
                                )
                            };
                            (
                                p.pair,
                                crate::base_fast::PoolMeta {
                                    token0: p.token_in,
                                    token1: p.token_out,
                                    fee_ppm: crate::base_fast::fee_to_ppm(unit, p.fee_bps),
                                    kind,
                                    // CL pairs come straight from the factory's
                                    // PoolCreated event, so they are already
                                    // chain-ordered. V2 metadata is collapsed
                                    // from a both-directions config list and
                                    // has to be read from the pool itself.
                                    verified: cl,
                                    // Nothing is confirmed until the reconcile
                                    // reads it. Metadata from an inventory file
                                    // is a claim about the pool, not a look at
                                    // its current state.
                                    confirmed_at: None,
                                    // Filled by the reconcile's balanceOf
                                    // reads. Nothing here is depth until the
                                    // chain has been asked.
                                    balances: None,
                                },
                            )
                        })
                        .collect();
                    let fast_starts: Vec<Address> = {
                        let mut t: Vec<Address> = fast_all.iter().map(|p| p.token_in).collect();
                        t.sort_unstable();
                        t.dedup();
                        t
                    };
                    info!(
                        cl = cl_pools.len(),
                        v2 = v2_pools.len(),
                        stable = v2_pools.iter().filter(|p| p.stable).count(),
                        total = fast_pools.len(),
                        starts = fast_starts.len(),
                        "base fast path universe"
                    );
                    let monitor = monitor
                        .with_chaos_gap(chaos_gap)
                        .with_sticky_pools(cl_pools)
                        // Without this the monitor can never rebuild a dead
                        // socket; BlockPI closes them every 30 minutes.
                        .with_ws_reconnect(ws_endpoints.clone(), ws_backoff);
                    // Phase 1 shadow: decode logs into live state and measure
                    // it. Nothing reads this store for pricing — that is
                    // Phase 2, gated on the divergence this run produces.
                    let monitor = if crate::util::env_flag("ARBOT_LIVE_STATE_SHADOW", false) {
                        let live = Arc::new(crate::live_state::LiveState::new());
                        info!(
                            "live-state shadow mode enabled; state is recorded and measured, \
                             never priced"
                        );
                        // Validation runs off the hot path (spec 4.5): it owns
                        // its own RPC budget and the searcher never waits on it.
                        let validation_live = Arc::clone(&live);
                        let validation_provider = provider.clone();
                        let validation_metrics = metrics.clone();
                        let interval_secs =
                            crate::util::env_parse_opt::<u64>("ARBOT_STATE_VALIDATION_SECS")
                                .unwrap_or(15)
                                .max(1);
                        spawn_supervised(
                            "state_validation",
                            cfg.name.clone(),
                            metrics.clone(),
                            move || {
                                let live = Arc::clone(&validation_live);
                                let provider = validation_provider.clone();
                                let metrics = validation_metrics.clone();
                                async move {
                                    crate::state_validation::run_state_validation(
                                        provider,
                                        live,
                                        metrics,
                                        Duration::from_secs(interval_secs),
                                    )
                                    .await;
                                }
                            },
                        );
                        // Base flashblock fast path: preconfirmed logs into the
                        // SAME LiveState the poll path writes, so both feed one
                        // store and their latency is directly comparable.
                        //
                        // Off by default. The canonical loop measures 4.20s
                        // median against a 200ms flashblock cadence; this exists
                        // to produce the receive->applied number that says
                        // whether that gap is closing, before anything is
                        // rewired to depend on it.
                        if cfg.chain_id == 8453
                            && crate::util::env_flag("ARBOT_BASE_FAST", false)
                        {
                            if crate::base_fast::worth_subscribing(&fast_pools) {
                                let fast = std::sync::Arc::new(
                                    crate::base_fast::BaseFastPath::new(
                                        crate::base_fast::FlashFeed::PendingLogs {
                                            ws_url: ws_endpoints
                                                .first()
                                                .cloned()
                                                .unwrap_or_default(),
                                        },
                                        fast_pools.clone(),
                                        // Its OWN LiveState, never the pool
                                        // monitor's. Sharing one was measured
                                        // 2026-09-02: 27,085 pendingLogs events
                                        // produced 37 applies and 584 continuity
                                        // breaks, because preconfirmed and
                                        // sealed delivery are two ORDERINGS of
                                        // the same stream and LiveState has one
                                        // global cursor demanding monotonic
                                        // ordinals. Each break invalidates all
                                        // 683 pools, so the fast path did not
                                        // merely fail to help -- it destroyed
                                        // the state the canonical path was
                                        // maintaining, and candidates went to 0.
                                        std::sync::Arc::new(
                                            crate::live_state::LiveState::new(),
                                        ),
                                        std::sync::Arc::new(std::sync::Mutex::new(
                                            std::collections::HashSet::new(),
                                        )),
                                        metrics.clone(),
                                    )
                                    .with_pool_tokens(fast_meta)
                                    .with_sim_http(
                                        &std::env::var("BASE_FLASHBLOCK_HTTP_URL")
                                            .ok()
                                            .unwrap_or_else(|| {
                                                http_endpoints.first().cloned().unwrap_or_default()
                                            }),
                                    )
                                    .with_ws_reconnect(ws_endpoints.clone(), ws_backoff),
                                );
                                info!(
                                    pools = fast_pools.len(),
                                    "base fast path enabled (pendingLogs)"
                                );
                                fast.clone().spawn();
                                // Seed local state instead of waiting for every
                                // pool to trade. Measured in the 2026-09-02
                                // bridge run: 90.1% of touched cycles were
                                // unpriceable, because a cycle needs EVERY hop
                                // and per-pool coverage compounds -- at 68% per
                                // pool a 6-hop loop prices 10% of the time.
                                // The same loop repairs coverage after a gap,
                                // which invalidates all of it at once.
                                let reconcile_secs = crate::util::env_parse_opt::<u64>(
                                    "ARBOT_BASE_FAST_RECONCILE_SECS",
                                )
                                .filter(|v| *v > 0)
                                .unwrap_or(2);
                                fast.clone().spawn_reconcile(
                                    provider.clone(),
                                    Duration::from_secs(reconcile_secs),
                                );
                                // The consumer. Without it the dirty set has a
                                // writer and no reader: it climbed to 159 pools
                                // and never fell in the previous run.
                                // Index built from the SAME pools the feed
                                // subscribes to, so a dirty pool always resolves
                                // to a hop this index knows. Sharing the scan
                                // loop's index instead would reintroduce the
                                // two-writers problem in a different place: it
                                // is rebuilt every scan from a different pool
                                // set.
                                let fast_uni = std::sync::Arc::new(
                                    crate::cycle_index::PoolUniverse::from_pools(
                                        fast_universe.clone(),
                                    ),
                                );
                                // NOT built here. Cycle STARTS must be
                                // restricted to tokens a flash loan can fund,
                                // and that is a runner question. Skipping it is
                                // the same defect the Bellman-Ford path had:
                                // cycles anchored at an unfundable token are
                                // generated, priced, routed and sized, and only
                                // then rejected as no_flashloan_provider.
                                // Measured 2026-09-03: 438 of 850 prepared
                                // candidates, 52% of the sample, discarded
                                // before any economics happened.
                                // 32 discarded 98.2% of touched cycles: the
                                // index is 6-hop (7,858 cycles), not the 2-hop
                                // ~536 the cap was sized against, and it bound
                                // on 85% of drains. Resolution costs 141us
                                // median against a 200ms flashblock, so the
                                // constraint was the cap, not the clock.
                                let max_touched = crate::util::env_parse_opt::<usize>(
                                    "ARBOT_BASE_FAST_MAX_CYCLES",
                                )
                                .filter(|v| *v > 0)
                                .unwrap_or(512);
                                let mut all_venues = fast_venues.clone();
                                all_venues.extend(v2_venues.clone());
                                info!(
                                    venues = all_venues.len(),
                                    pools = fast_pools.len(),
                                    "base fast path venue parameters captured"
                                );
                                base_fast_drain = Some(BaseFastDrain {
                                    fast: fast.clone(),
                                    universe: fast_uni.clone(),
                                    starts: fast_starts.clone(),
                                    max_cycles: max_touched,
                                    venues: std::sync::Arc::new(all_venues),
                                });
                            } else {
                                warn!(
                                    "ARBOT_BASE_FAST set but no CL pools to subscribe; \
                                     pendingLogs with no address filter is every log on Base"
                                );
                            }
                        }
                        monitor.with_live_state(live)
                    } else {
                        monitor
                    };
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
        let chain_env_prefix = cfg.env_prefix.clone();
        let hot_config = hot_pool_config.clone();
        let hot_univ2_pools = Arc::clone(&hot_univ2_pools);
        let hot_univ2_by_venue = Arc::clone(&hot_univ2_by_venue);
        let token_decimals_hint = token_decimals_hint.clone();
        let pool_monitor = pool_monitor.clone();
        spawn_supervised(
            "hot_pools_univ2",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let provider = provider.clone();
                let venue = venue.clone();
                let chain = chain.clone();
                let hot_config = hot_config.clone();
                let hot_univ2_pools = Arc::clone(&hot_univ2_pools);
                let hot_univ2_by_venue = Arc::clone(&hot_univ2_by_venue);
                let token_decimals_hint = token_decimals_hint.clone();
                let pool_monitor = pool_monitor.clone();
                let cold = cold.clone();
                let chain_env_prefix = chain_env_prefix.clone();
                async move {
                    info!(
                        chain = %chain,
                        venue = %venue,
                        kind = "univ2_like",
                        cold_pool_records = cold.len(),
                        refresh_secs = hot_config.refresh_interval.as_secs(),
                        "spawned hot pool refresh worker"
                    );
                    loop {
                        sleep(hot_config.refresh_interval).await;
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
                                    let solidly = solidly_monitored_pools(&chain_env_prefix);
                                    let monitored = venues::merge_monitored_pools(
                                        monitored_pools_from_configs(&combined),
                                        solidly,
                                    );
                                    monitor.set_pools(monitored).await;
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, chain = %chain, venue = %venue, "failed to refresh univ2 hot pools");
                            }
                        }
                    }
                }
            },
        );
    }

    for (venue_name, cold) in cold_univ3_by_venue.clone() {
        let provider = provider.clone();
        let venue = venue_name.clone();
        let chain = cfg.name.clone();
        let hot_config = hot_pool_config.clone();
        let hot_univ3_pools = Arc::clone(&hot_univ3_pools);
        let hot_univ3_by_venue = Arc::clone(&hot_univ3_by_venue);
        let univ3_rank_ctx = Arc::clone(&univ3_rank_ctx);
        spawn_supervised(
            "hot_pools_univ3",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let provider = provider.clone();
                let venue = venue.clone();
                let chain = chain.clone();
                let hot_config = hot_config.clone();
                let hot_univ3_pools = Arc::clone(&hot_univ3_pools);
                let hot_univ3_by_venue = Arc::clone(&hot_univ3_by_venue);
                let rank_ctx = univ3_rank_ctx.clone();
                let cold = cold.clone();
                async move {
                    info!(
                        chain = %chain,
                        venue = %venue,
                        kind = "univ3_like",
                        cold_pool_records = cold.len(),
                        refresh_secs = hot_config.refresh_interval.as_secs(),
                        "spawned hot pool refresh worker"
                    );
                    loop {
                        sleep(hot_config.refresh_interval).await;
                        match rank_univ3_pools(
                            provider.clone(),
                            cold.as_slice(),
                            &hot_config,
                            rank_ctx.as_ref(),
                        )
                            .await
                        {
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
                    }
                }
            },
        );
    }

    for (venue_name, cold) in cold_pancakeswap_by_venue.clone() {
        let provider = provider.clone();
        let venue = venue_name.clone();
        let chain = cfg.name.clone();
        let hot_config = hot_pool_config.clone();
        let hot_pancakeswap_pools = Arc::clone(&hot_pancakeswap_pools);
        let hot_pancakeswap_by_venue = Arc::clone(&hot_pancakeswap_by_venue);
        let pancake_rank_ctx = Arc::clone(&univ3_rank_ctx);
        spawn_supervised(
            "hot_pools_pancakeswap",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let provider = provider.clone();
                let venue = venue.clone();
                let chain = chain.clone();
                let hot_config = hot_config.clone();
                let hot_pancakeswap_pools = Arc::clone(&hot_pancakeswap_pools);
                let hot_pancakeswap_by_venue = Arc::clone(&hot_pancakeswap_by_venue);
                let rank_ctx = pancake_rank_ctx.clone();
                let cold = cold.clone();
                async move {
                    info!(
                        chain = %chain,
                        venue = %venue,
                        kind = "univ3_like",
                        cold_pool_records = cold.len(),
                        refresh_secs = hot_config.refresh_interval.as_secs(),
                        "spawned hot pool refresh worker"
                    );
                    loop {
                        sleep(hot_config.refresh_interval).await;
                        match rank_univ3_pools(
                            provider.clone(),
                            cold.as_slice(),
                            &hot_config,
                            rank_ctx.as_ref(),
                        )
                        .await
                        {
                            Ok(hot) => {
                                log_hot_pool_refresh(&chain, &venue, "univ3_like", hot.len());
                                {
                                    let mut guard = hot_pancakeswap_by_venue.write().await;
                                    guard.insert(venue.clone(), hot.clone());
                                }
                                let combined = {
                                    let guard = hot_pancakeswap_by_venue.read().await;
                                    guard.values().flat_map(|v| v.clone()).collect::<Vec<_>>()
                                };
                                let mut guard = hot_pancakeswap_pools.write().await;
                                *guard = combined;
                            }
                            Err(err) => {
                                warn!(error = %err, chain = %chain, venue = %venue, "failed to refresh pancakeswap hot pools");
                            }
                        }
                    }
                }
            },
        );
    }

    for (venue_name, cold) in cold_slipstream_by_venue.clone() {
        let provider = provider.clone();
        let venue = venue_name.clone();
        let chain = cfg.name.clone();
        let hot_config = hot_pool_config.clone();
        let hot_slipstream_pools = Arc::clone(&hot_slipstream_pools);
        let hot_slipstream_by_venue = Arc::clone(&hot_slipstream_by_venue);
        let slipstream_rank_ctx = Arc::clone(&univ3_rank_ctx);
        spawn_supervised(
            "hot_pools_slipstream",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let provider = provider.clone();
                let venue = venue.clone();
                let chain = chain.clone();
                let hot_config = hot_config.clone();
                let hot_slipstream_pools = Arc::clone(&hot_slipstream_pools);
                let hot_slipstream_by_venue = Arc::clone(&hot_slipstream_by_venue);
                let rank_ctx = slipstream_rank_ctx.clone();
                let cold = cold.clone();
                async move {
                    info!(
                        chain = %chain,
                        venue = %venue,
                        kind = "slipstream_like",
                        cold_pool_records = cold.len(),
                        refresh_secs = hot_config.refresh_interval.as_secs(),
                        "spawned hot pool refresh worker"
                    );
                    loop {
                        sleep(hot_config.refresh_interval).await;
                        match rank_univ3_pools(
                            provider.clone(),
                            cold.as_slice(),
                            &hot_config,
                            rank_ctx.as_ref(),
                        )
                        .await
                        {
                            Ok(hot) => {
                                log_hot_pool_refresh(
                                    &chain,
                                    &venue,
                                    "slipstream_like",
                                    hot.len(),
                                );
                                {
                                    let mut guard = hot_slipstream_by_venue.write().await;
                                    guard.insert(venue.clone(), hot.clone());
                                }
                                let combined = {
                                    let guard = hot_slipstream_by_venue.read().await;
                                    guard.values().flat_map(|v| v.clone()).collect::<Vec<_>>()
                                };
                                let mut guard = hot_slipstream_pools.write().await;
                                *guard = combined;
                            }
                            Err(err) => {
                                warn!(error = %err, chain = %chain, venue = %venue, "failed to refresh slipstream hot pools");
                            }
                        }
                    }
                }
            },
        );
    }

    let (block_head_tx, block_head_rx) = block_head_channel();
    let block_head_rx_for_runner = Some(Arc::new(Mutex::new(block_head_rx)));

    // The mempool monitor exists ONLY to feed the backrun monitor, so it must
    // follow the same gate. It previously spawned whenever a websocket was
    // configured, ignoring BACKRUN_MONITOR entirely — which made it impossible
    // to turn off. On Base that is a loop that cannot succeed: the chain has no
    // public mempool (spec §1), so `eth_subscribe` for pending transactions is
    // answered with `-32616 invalid subscription type` and retried forever,
    // burning websocket connections and RPC budget against a rate limit the
    // engine is already pinned against.
    if backrun_monitor.is_some() && (ws_provider.is_some() || !ws_endpoints.is_empty()) {
        let pending_provider = provider.clone();
        let pending_ws = ws_provider.clone();
        let pending_endpoints = ws_endpoints.clone();
        let mempool_backrun = backrun_monitor.clone();
        let mempool_metrics = metrics.clone();
        spawn_supervised(
            "mempool_monitor",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let pending_provider = pending_provider.clone();
                let pending_ws = pending_ws.clone();
                let pending_endpoints = pending_endpoints.clone();
                let mempool_backrun = mempool_backrun.clone();
                let mempool_metrics = mempool_metrics.clone();
                spawn_live_mempool_monitor(
                    pending_provider,
                    pending_ws,
                    pending_endpoints,
                    ws_backoff,
                    mempool_backrun,
                    mempool_metrics,
                )
            },
        );

        // Chains with no public mempool (Base: single sequencer) deliver zero
        // pending transactions, so the monitor above receives nothing and
        // backrun is enabled in name only. Verified empirically:
        // eth_newPendingTransactionFilter is accepted and returns 0 txs while
        // blocks advance normally. Backrunning the block that just landed is the
        // pattern that does work there, so run it alongside — on a chain WITH a
        // mempool it is simply a slower duplicate feed, and hints dedupe.
        let mined_provider = provider.clone();
        let mined_backrun = backrun_monitor.clone();
        spawn_supervised(
            "mined_swap_monitor",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let mined_provider = mined_provider.clone();
                let mined_backrun = mined_backrun.clone();
                spawn_mined_swap_monitor(
                    mined_provider,
                    mined_backrun,
                    Duration::from_millis(
                        crate::util::env_parse_opt::<u64>("BACKRUN_MINED_POLL_MS")
                            .unwrap_or(1_000)
                            .clamp(200, 10_000),
                    ),
                    Duration::from_secs(45),
                )
            },
        );
    }

    // The `newHeads` monitor was spawned inside the backrun guard above, so
    // disabling backrun disabled it too. Those are unrelated subsystems: backrun
    // needs a public mempool (Base has none, so it is correctly off here), while
    // newHeads is the engine's PRIMARY latency path. With it off, head discovery
    // silently degraded to `spawn_block_head_monitor`'s 1s HTTP fallback poll —
    // ~500ms average staleness before a single quote is issued, on a chain with
    // ~200ms flashblocks. `current_block_head`'s websocket fast path could never
    // engage, and its liveness guard masked the regression by quietly serving
    // correct-but-late heads.
    if ws_provider.is_some() || !ws_endpoints.is_empty() {
        let head_provider = provider.clone();
        let head_ws = ws_provider.clone();
        let head_endpoints = ws_endpoints.clone();
        let chain_name = cfg.name.clone();
        let head_tx = block_head_tx.clone();
        spawn_supervised(
            "block_head_monitor",
            cfg.name.clone(),
            metrics.clone(),
            move || {
                let head_provider = head_provider.clone();
                let head_ws = head_ws.clone();
                let head_endpoints = head_endpoints.clone();
                let head_tx = head_tx.clone();
                let chain_name = chain_name.clone();
                spawn_block_head_monitor(
                    head_provider,
                    head_ws,
                    head_endpoints,
                    ws_backoff,
                    head_tx,
                    chain_name,
                )
            },
        );
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
    let aave_fee_bps = if cfg.aave_pool.is_some() {
        resolve_aave_fee_bps(&cfg.name, ops_inputs)
    } else {
        0
    };

    // Resolve the declared per-chain risk policy into enforceable runtime
    // limits. Fails closed when the declaration cannot be honored (e.g. USD
    // floor without a native USD price).
    let risk_policy =
        RuntimeRiskPolicy::resolve(&cfg.name, ops_inputs, native_usd_price(&cfg.env_prefix))?;
    match &risk_policy {
        Some(policy) => info!(
            chain = %cfg.name,
            min_net_profit_wei = ?policy.min_net_profit_wei,
            max_gas_units_per_tx = ?policy.max_gas_units_per_tx,
            max_fee_per_gas_cap = ?policy.max_fee_per_gas_cap,
            max_slippage_bps = ?policy.max_slippage_bps,
            max_price_impact_bps = ?policy.max_price_impact_bps,
            must_simulate_before_send = policy.must_simulate_before_send,
            revert_penalty = ?policy.revert_penalty,
            "risk policy ACTIVE: ops/inputs.yaml limits enforced at runtime"
        ),
        None => warn!(
            chain = %cfg.name,
            "no risk.per_chain entry in ops inputs for this chain; runtime risk limits DISABLED"
        ),
    }
    let sim_quorum = Arc::new(SimQuorum::from_endpoints(&cfg.name, &http_endpoints));

    let bf_skip_on_stable_graph = read_feature_flag("ARBOT_BF_SKIP_ON_STABLE_GRAPH", false);
    // Hub-anchored cycle search. Off by default so it can be A/B'd against
    // Bellman-Ford on identical graph state before it takes over.
    let hub_search_enabled = read_feature_flag("ARBOT_HUB_SEARCH", false);
    let hub_search_parallel_edges = crate::util::env_parse_opt::<usize>("ARBOT_HUB_SEARCH_PARALLEL_EDGES")
        .unwrap_or(3)
        .clamp(1, 8);
    info!(
        chain = %cfg.name,
        bf_skip_enabled = bf_skip_on_stable_graph,
        "Bellman-Ford stable-graph skip configuration"
    );

    let (slipstream_factory, slipstream_router, slipstream_quoter_addr) = resolve_slipstream_venue(
        ops_inputs, &cfg.name,
    )
    .unwrap_or((Address::zero(), Address::zero(), Address::zero()));
    let slipstream_tick_spacings_set = collect_slipstream_tick_spacings(ops_inputs, &cfg.name);
    let slipstream_tick_spacings = if slipstream_tick_spacings_set.is_empty() {
        None
    } else {
        Some(Arc::new(slipstream_tick_spacings_set))
    };
    let slipstream_validation = if slipstream_quoter_addr != Address::zero()
        && cfg.name.eq_ignore_ascii_case("base")
    {
        Some(default_slipstream_validation_base())
    } else {
        None
    };

    let (pancakeswap_factory, pancakeswap_router, pancakeswap_quoter_addr) =
        resolve_pancakeswap_venue(ops_inputs, &cfg.name).unwrap_or((
            Address::zero(),
            Address::zero(),
            Address::zero(),
        ));
    let pancakeswap_fee_tiers_set = collect_pancakeswap_fee_tiers(ops_inputs, &cfg.name);
    let pancakeswap_fee_tiers = if pancakeswap_fee_tiers_set.is_empty() {
        None
    } else {
        Some(Arc::new(pancakeswap_fee_tiers_set))
    };
    let pancakeswap_validation = if pancakeswap_quoter_addr != Address::zero()
        && cfg.name.eq_ignore_ascii_case("base")
    {
        Some(default_pancakeswap_validation_base())
    } else {
        None
    };

    // Flash-swap borrow pools. A flash swap draws from one specific pool, so the
    // allowlist alone is not enough — without the address the plan builder falls
    // back to `Address::zero()` and every candidate is rejected as unfundable.
    let (univ2_flash_pool, univ2_flash_fee_bps, univ3_flash_pool, univ3_flash_fee_bps) = {
        let find = |kind: &str| -> (Option<Address>, u32) {
            let loan = ops_inputs
                .chain_inputs(&cfg.name)
                .and_then(|chain| {
                    chain.flashloans.iter().find(|loan| {
                        loan.kind
                            .as_ref()
                            .map(|k| format!("{k:?}").eq_ignore_ascii_case(kind))
                            .unwrap_or(false)
                    })
                });
            let addr = loan
                .and_then(|loan| loan.pool.as_deref())
                .and_then(|raw| parse_address(raw, "flashloan pool").ok())
                .filter(|addr| !addr.is_zero());
            let fee = loan.and_then(|loan| loan.fee_bps).unwrap_or(0);
            (addr, fee)
        };
        let (u2, u2_fee) = find("Univ2Flashswap");
        let (u3, u3_fee) = find("Univ3Flash");
        (u2, u2_fee, u3, u3_fee)
    };
    info!(
        chain = %cfg.name,
        univ2_flash_pool = ?univ2_flash_pool.map(|a| format!("0x{}", hex::encode(a))),
        univ2_flash_fee_bps,
        univ3_flash_pool = ?univ3_flash_pool.map(|a| format!("0x{}", hex::encode(a))),
        univ3_flash_fee_bps,
        "resolved flash-swap borrow pools"
    );

    let runner_config = RunnerConfig {
        univ2_flash_pool,
        univ2_flash_fee_bps,
        univ3_flash_pool,
        univ3_flash_fee_bps,
        feature_gate,
        hub_search_enabled,
        hub_search_parallel_edges,
        chain_name: cfg.name.clone(),
        provider,
        rpc_endpoint,
        sim_rpc_url: http_endpoints.first().cloned().unwrap_or_default(),
        rpc_health: rpc_health.clone(),
        univ3_quoter: cfg.univ3_quoter,
        univ3_factory: cfg.univ3_factory,
        univ3_validation: cfg.univ3_validation.clone(),
        univ3_fee_tiers: univ3_fee_tiers.clone(),
        bal_vault: cfg.bal_vault,
        aave_pool: cfg.aave_pool,
        aave_fee_bps,
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
        initial_token_decimals: token_decimals_hint.clone(),
        wrapped_native,
        capital: capital_manager.clone(),
        pool_depth_cache: pool_depth_cache.clone(),
        pool_monitor: pool_monitor.clone(),
        hot_univ2_pools: Arc::clone(&hot_univ2_pools),
        hot_univ3_pools: Arc::clone(&hot_univ3_pools),
        hot_slipstream_pools: Arc::clone(&hot_slipstream_pools),
        slipstream_quoter_addr,
        slipstream_factory,
        slipstream_router,
        slipstream_validation,
        slipstream_tick_spacings,
        hot_pancakeswap_pools: Arc::clone(&hot_pancakeswap_pools),
        pancakeswap_quoter_addr,
        pancakeswap_factory,
        pancakeswap_router,
        pancakeswap_validation,
        pancakeswap_fee_tiers,
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
        risk_policy,
        sim_quorum,
        chain_id: cfg.chain_id,
        block_head_rx: block_head_rx_for_runner,
        bf_skip_on_stable_graph,
    };

    let runner = Arc::new(Runner::new(runner_config, executor));

    // Now the runner exists, the fast path can read its published graph
    // snapshot. Read-only: the runner is the sole writer, which is what makes
    // this safe where sharing LiveState was not -- that had two writers and one
    // global ordinal cursor, and cost 584 continuity breaks.
    if let Some(bf) = base_fast_drain {
        let BaseFastDrain { fast, universe: uni, starts, max_cycles: cap, venues } = bf;
        // Cycles may only START where a flash loan can fund them. Without this
        // the fast path repeats the Bellman-Ford defect: it prices, routes and
        // sizes cycles anchored at tokens no provider will lend, and learns
        // that only at the end. Measured 2026-09-03, that was 438 of 850
        // prepared candidates -- 52% of the sample deleted after all the work,
        // and before any economic question was asked.
        let fundable: Vec<Address> = starts
            .iter()
            .copied()
            .filter(|t| runner.can_flash_fund(*t))
            .collect();
        info!(
            starts = starts.len(),
            fundable = fundable.len(),
            dropped = starts.len().saturating_sub(fundable.len()),
            "base fast path cycle starts restricted to flash-fundable tokens"
        );
        let fast_index = crate::cycle_index::CycleIndex::build(
            &uni,
            &fundable,
            crate::cycle_index::CycleIndexLimits::default(),
        );
        info!(
            cycles = fast_index.len(),
            truncated = fast_index.truncated,
            starts = fundable.len(),
            "base fast path cycle index built"
        );
        let index = std::sync::Arc::new(std::sync::Mutex::new(Some(fast_index)));
        // How many ranked cycles are actually SIZED per flashblock. Each one is
        // several RPC round trips and one round trip to the configured provider
        // measured 250-293ms on 2026-09-02, so this is an RPC-budget decision
        // and not a ranking one. The ranking already happened upstream.
        let prepare_top = crate::util::env_parse_opt::<usize>("ARBOT_BASE_FAST_PREPARE")
            .filter(|v| *v > 0)
            .unwrap_or(2);
        let sink_runner = Arc::clone(&runner);
        let fast_sim_src = Arc::clone(&fast);
        let sink: crate::base_fast::CandidateSink =
            Arc::new(move |graph: Arc<Graph>, ready| {
                let runner = Arc::clone(&sink_runner);
                let fast_sim = Arc::clone(&fast_sim_src);
                Box::pin(async move {
                    let mut report = crate::base_fast::PrepReport::default();
                    // The economics are all RPC-derived and the fast path cannot
                    // rebuild them; without a published context there is nothing
                    // to size against, which is not the same as rejecting.
                    let snap = runner.prep_context().lock().ok().and_then(|c| c.clone());
                    let Some(snap) = snap else {
                        report.no_context = true;
                        return report;
                    };
                    let ctx = CandidatePrepCtx {
                        native_prices_map: snap.native_prices_map.as_ref(),
                        base_profiles_map: snap.base_profiles_map.as_ref(),
                        capital_snapshot: &snap.capital_snapshot,
                        competition_snapshot: &snap.competition_snapshot,
                        gas_parameters: &snap.gas_parameters,
                        executor_address: snap.executor_address,
                        block_number: snap.block_number,
                        // A diagnostic field on the scan's own logs. The fast
                        // path did not scan edges, and claiming a number here
                        // would put a fiction in the candidate record.
                        edges_scanned: 0,
                    };
                    for (priced, indexed) in ready.into_iter().take(prepare_top) {
                        match runner.prepare_candidate(&graph, indexed, &ctx).await {
                            CandidatePrep::Sized(sized) => {
                                report.sized += 1;
                                // The real candidate, against preconfirmed
                                // state: real calldata, real loan amount, real
                                // swap path, real gasUsed. What it does NOT
                                // enforce is min_profit -- see
                                // `simulation_calldata`.
                                let mut sim_gas = 0u64;
                                let mut sim_verdict = "not_simulated";
                                let mut sim_reason = String::new();
                                if let Some((from, to, data)) =
                                    runner.simulation_calldata(&sized)
                                {
                                    let max_fee = snap
                                        .gas_parameters
                                        .max_fee_per_gas
                                        .unwrap_or(snap.gas_parameters.gas_price)
                                        .min(U256::from(u128::MAX))
                                        .as_u128();
                                    if let Some((r, took)) = fast_sim
                                        .simulate_candidate(from, to, &data, max_fee)
                                        .await
                                    {
                                        report.sim_micros = report
                                            .sim_micros
                                            .saturating_add(took.as_micros().min(
                                                u128::from(u64::MAX),
                                            ) as u64);
                                        report.sim_samples += 1;
                                        sim_gas = r.gas_used;
                                        if r.success {
                                            report.sim_ok += 1;
                                            sim_verdict = "ok";
                                        } else {
                                            report.sim_failed += 1;
                                            sim_verdict = "reverted";
                                            sim_reason = r.failure.clone().unwrap_or_default();
                                        }
                                    }
                                }
                                info!(
                                    target: "arb_exec::latency",
                                    gross_bps = priced.gross_bps,
                                    hops = priced.hops,
                                    amount_in = %sized.sizing.amount_in,
                                    gross = %sized.sizing.gross,
                                    flash_fee = %sized.sizing.flash_fee,
                                    net = %sized.sizing.net_after_fee_and_gas,
                                    quotes = sized.sizing.quote_count,
                                    est_gas = sized.adjusted_cycle_gas,
                                    sim = sim_verdict,
                                    sim_gas,
                                    sim_reason = %sim_reason,
                                    "flashblock candidate sized"
                                );
                            }
                            CandidatePrep::Rejected { .. } => report.rejected += 1,
                            CandidatePrep::Budgeted => report.budgeted += 1,
                        }
                    }
                    report
                }) as futures_util::future::BoxFuture<'static, crate::base_fast::PrepReport>
            });
        fast.spawn_drain(
            uni,
            index,
            crate::base_fast::BaseFastPath::drain_feeds(
                runner.graph_snapshot(),
                runner.token_native_prices(),
            ),
            crate::base_fast::DrainConsumer {
                router: Some(std::sync::Arc::new(crate::base_fast::LiveRouter {
                    venues,
                })),
                sink: Some(sink),
            },
            Duration::from_millis(200),
            cap,
        );
    }

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
        if std::env::var("CHAOS_WS_GAP_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|s| s > 0)
        {
            return Err(anyhow!(
                "production mode rejected CHAOS_WS_GAP_SECS; it deliberately \
                 discards websocket coverage"
            ));
        }
    }

    Ok(())
}

async fn validate_aave_pool_probes<C>(
    cfg: &ChainCfg,
    provider: &Provider<C>,
    ops_inputs: &crate::ops_inputs::OpsInputs,
) -> Result<()>
where
    C: JsonRpcClient + 'static,
{
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

    let command_handle = if interactive_command_listener_enabled() {
        Some(tokio::spawn(command_listener(command_txs.clone())))
    } else {
        info!("Non-interactive mode: auto-started (set ARBOT_INTERACTIVE=1 for stdin commands)");
        None
    };

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

    if let Some(command_handle) = command_handle {
        let _ = command_handle.await;
    }
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

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn hint_tokens_never_enter_the_touched_pool_set() {
        use crate::cycle_index::PoolUniverse;

        let weth = Address::from_low_u64_be(1);
        let usdc = Address::from_low_u64_be(2);
        let pool_a = Address::from_low_u64_be(100);
        let pool_b = Address::from_low_u64_be(101);
        let universe =
            PoolUniverse::from_pools(vec![(pool_a, weth, usdc), (pool_b, weth, usdc)]);

        let mut touched: HashSet<Address> = HashSet::new();
        for pool in universe.pools_for_hop(weth, usdc) {
            touched.insert(*pool);
        }

        assert!(touched.contains(&pool_a) && touched.contains(&pool_b));
        assert!(
            !touched.contains(&weth) && !touched.contains(&usdc),
            "token addresses in a pool set flip populate to incremental and \
             filter every CL pool out of the scan"
        );
    }

    #[test]
    fn a_hint_on_an_unknown_pair_dirties_nothing() {
        use crate::cycle_index::PoolUniverse;

        let universe = PoolUniverse::from_pools(vec![(
            Address::from_low_u64_be(100),
            Address::from_low_u64_be(1),
            Address::from_low_u64_be(2),
        )]);

        let mut touched: HashSet<Address> = HashSet::new();
        for pool in universe.pools_for_hop(
            Address::from_low_u64_be(50),
            Address::from_low_u64_be(51),
        ) {
            touched.insert(*pool);
        }

        assert!(
            touched.is_empty(),
            "an unresolvable hint must leave the set empty so populate stays \
             on the full path, not go incremental with a filter matching nothing"
        );
    }

    /// The quorum bug: a tx reaching simulation with no gas limit is rejected by
    /// every verifier as `intrinsic gas too low`, so cross-endpoint verification
    /// silently never confirms. The limit must actually be applied.
    #[test]
    fn apply_gas_parameters_sets_the_gas_limit() {
        let gas = crate::fees::FeeEstimate {
            gas_limit: U256::from(450_000u64),
            gas_price: U256::from(1_000_000u64),
            base_fee_per_gas: None,
            priority_fee_per_gas: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            l1_data_fee: U256::zero(),
            total_fee_native: U256::zero(),
        };
        let mut tx: TypedTransaction = TransactionRequest::new().into();
        assert!(tx.gas().is_none(), "precondition: no limit set");
        apply_gas_parameters(&mut tx, &gas);
        assert_eq!(tx.gas(), Some(&U256::from(450_000u64)));
    }

    #[test]
    fn apply_gas_parameters_leaves_a_zero_limit_unset() {
        // Zero is worse than absent: it guarantees intrinsic-gas failure, where
        // absent lets the node substitute its own default.
        let gas = crate::fees::FeeEstimate {
            gas_limit: U256::zero(),
            gas_price: U256::from(1_000_000u64),
            base_fee_per_gas: None,
            priority_fee_per_gas: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            l1_data_fee: U256::zero(),
            total_fee_native: U256::zero(),
        };
        let mut tx: TypedTransaction = TransactionRequest::new().into();
        apply_gas_parameters(&mut tx, &gas);
        assert!(tx.gas().is_none(), "zero must not be applied");
    }

    /// Guards the retry-skip: only deterministic reverts may short-circuit the
    /// pending-block retry. Misclassifying a transport blip would throw away a
    /// gas estimate that would have succeeded.
    #[test]
    fn only_reverts_short_circuit_the_gas_retry() {
        for revert in [
            "execution reverted: Too little received",
            "execution reverted",
            "invalid opcode",
        ] {
            assert!(crate::quote_common::is_execution_revert(&revert), "{revert:?}");
        }
        for transient in [
            "error sending request for url",
            "operation timed out",
            "503 Service Unavailable",
            "intrinsic gas too low",
        ] {
            assert!(
                !crate::quote_common::is_execution_revert(&transient),
                "{transient:?} must still retry"
            );
        }
    }

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

    #[tokio::test]
    async fn send_private_rpc_bundle_never_leaks_content_rejections_to_raw() {
        let (provider, mock) = Provider::mocked();
        // Bundle rejected for a content reason (not method support): the tx
        // must NOT be re-broadcast via eth_sendRawTransaction even though the
        // policy allows the fallback.
        mock.push_response(ethers::providers::MockResponse::Error(
            ethers::providers::JsonRpcError {
                code: -32000,
                message: "bundle rejected: reverting transaction".into(),
                data: None,
            },
        ));
        let raw = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);

        let err = send_private_rpc_bundle(&provider, raw, U64::from(42u64), "base", true)
            .await
            .expect_err("content rejection must propagate, not fall back");

        assert!(format!("{err}").contains("bundle rejected"));
    }

    fn bid_broadcast(filler: Option<U256>, fraction_bps: u32) -> BroadcastConfig {
        BroadcastConfig {
            endpoint: BroadcastEndpoint::Public,
            role: MevRole::Filler,
            filler_priority_fee: filler,
            searcher_priority_fee: None,
            public_jitter_bps: 0,
            private_inclusion_timeout: Duration::from_millis(1000),
            relay_health: Arc::new(StdMutex::new(HealthTracker::new(
                0.5,
                HealthThresholds::default(),
            ))),
            bid_profit_fraction_bps: fraction_bps,
        }
    }

    #[test]
    fn competitive_priority_fee_bids_share_of_profit() {
        // 1 ETH net profit, 1e6 gas, 50% => extra = 0.5e18 / 1e6 = 5e11 wei/gas,
        // added on top of the 2 gwei static floor.
        let bc = bid_broadcast(Some(U256::from(2_000_000_000u64)), 5_000);
        let fee = bc
            .competitive_priority_fee(
                Some(U256::exp10(18)),
                1_000_000,
                Some(U256::from(1_000_000_000u64)),
                None,
            )
            .expect("fee present");
        let expected = U256::from(2_000_000_000u64)
            + U256::exp10(18) / U256::from(2u64) / U256::from(1_000_000u64);
        assert_eq!(fee, expected);
    }

    #[test]
    fn competitive_priority_fee_hard_caps_at_risk_ceiling() {
        // base 9 gwei, cap 10 gwei => priority headroom is 1 gwei. The risk cap
        // wins even though the aggressive bid (and the floor) want more.
        let bc = bid_broadcast(Some(U256::from(2_000_000_000u64)), 9_000);
        let fee = bc
            .competitive_priority_fee(
                Some(U256::exp10(18)),
                1_000_000,
                Some(U256::from(9_000_000_000u64)),
                Some(U256::from(10_000_000_000u64)),
            )
            .expect("fee present");
        assert_eq!(fee, U256::from(1_000_000_000u64));
    }

    #[test]
    fn competitive_priority_fee_falls_back_to_floor_without_reliable_profit() {
        let bc = bid_broadcast(Some(U256::from(2_000_000_000u64)), 5_000);
        let fee = bc
            .competitive_priority_fee(None, 1_000_000, None, None)
            .expect("fee present");
        assert_eq!(fee, U256::from(2_000_000_000u64));
    }

    #[test]
    fn competitive_priority_fee_disabled_returns_static_floor() {
        let bc = bid_broadcast(Some(U256::from(2_000_000_000u64)), 0);
        let fee = bc
            .competitive_priority_fee(Some(U256::exp10(18)), 1_000_000, None, None)
            .expect("fee present");
        assert_eq!(fee, U256::from(2_000_000_000u64));
    }

    #[test]
    fn competitive_priority_fee_none_without_floor_or_profit() {
        let bc = bid_broadcast(None, 5_000);
        assert!(bc
            .competitive_priority_fee(None, 1_000_000, None, None)
            .is_none());
    }

    #[test]
    fn bundle_method_unsupported_classifies_errors() {
        let unsupported =
            ProviderError::CustomError("(code: -32601, message: Method not found)".into());
        assert!(bundle_method_unsupported(&unsupported));

        let by_text = ProviderError::CustomError("the method eth_sendBundle does not exist".into());
        assert!(bundle_method_unsupported(&by_text));

        let content =
            ProviderError::CustomError("bundle rejected: nonce too low".into());
        assert!(!bundle_method_unsupported(&content));
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
    fn jit_lp_enabled_flag_accepts_all_truthy_spellings() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = env::var("JIT_LP_ENABLED").ok();

        for truthy in ["1", "true", "yes", "TRUE", "Yes"] {
            env::set_var("JIT_LP_ENABLED", truthy);
            assert!(
                read_feature_flag("JIT_LP_ENABLED", false),
                "JIT_LP_ENABLED={truthy} must enable the feature",
            );
        }
        for falsy in ["0", "false", "no", "off", ""] {
            env::set_var("JIT_LP_ENABLED", falsy);
            assert!(
                !read_feature_flag("JIT_LP_ENABLED", false),
                "JIT_LP_ENABLED={falsy} must not enable the feature",
            );
        }
        env::remove_var("JIT_LP_ENABLED");
        assert!(!read_feature_flag("JIT_LP_ENABLED", false));

        match prior {
            Some(value) => env::set_var("JIT_LP_ENABLED", value),
            None => env::remove_var("JIT_LP_ENABLED"),
        }
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
    fn parse_chain_targets_defaults_to_base_when_unset() {
        // When neither CHAIN_LIST nor CHAIN yields a target, the engine defaults
        // to the production primary chain (base), matching the live .env CHAIN=base.
        let targets = parse_chain_targets_from_env(None, Some("   ".to_string()));

        assert_eq!(targets, vec!["base"]);
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
    fn resolve_aave_fee_bps_reads_ops_flashloan_fee() {
        let ops_inputs = crate::ops_inputs::parse_ops_inputs(
            r#"
chains:
  - chain_name: base
    chain_id: 8453
    env_prefix: BASE
    flashloans:
      - name: aave_v3
        kind: aave_v3_like
        pool: "0xA238Dd80C259a72e81d7e4664a9801593F98d1c5"
        fee_bps: 5
        allowlist_tokens: []
"#,
        )
        .expect("parse ops yaml");
        assert_eq!(resolve_aave_fee_bps("base", &ops_inputs), 5);
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
    fn capacity_caps_request_to_what_the_provider_can_lend() {
        let min = U256::from(100u64);
        // Balancer's real Base failure: 100 WETH asked, 27.5 WETH in the vault.
        // Previously the full 100 was advertised and reverted as BAL#528.
        assert_eq!(
            capacity_capped_amount(Some(U256::from(275u64)), U256::from(1000u64), min, true),
            Some(U256::from(275u64)),
            "must clamp down to available capacity"
        );
        // Ample capacity leaves the request untouched.
        assert_eq!(
            capacity_capped_amount(Some(U256::from(9999u64)), U256::from(1000u64), min, true),
            Some(U256::from(1000u64))
        );
    }

    /// `ARBOT_START_TOKENS` parsing: a valid list restricts, a garbage list is
    /// ignored rather than silently halting all scanning.
    #[test]
    fn start_token_allowlist_parses_or_fails_open() {
        fn parse(raw: &str) -> Option<HashSet<Address>> {
            let set: HashSet<Address> = raw
                .split(',')
                .filter_map(|s| s.trim().parse::<Address>().ok())
                .collect();
            if set.is_empty() {
                return None;
            }
            Some(set)
        }
        let weth: Address = "0x4200000000000000000000000000000000000006"
            .parse()
            .expect("weth");
        let usdc: Address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913"
            .parse()
            .expect("usdc");

        let both = parse("0x4200000000000000000000000000000000000006, 0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913")
            .expect("two valid addresses parse");
        assert!(both.contains(&weth) && both.contains(&usdc));
        assert_eq!(both.len(), 2);

        // One good, one malformed: keep the good one rather than dropping both.
        let partial = parse("0x4200000000000000000000000000000000000006,not-an-address")
            .expect("partial list still restricts");
        assert_eq!(partial.len(), 1);

        // All garbage -> fail OPEN (None), never an empty restriction, which
        // would allow no start token at all and stop the scanner dead.
        assert!(parse("nonsense,,also-nonsense").is_none());
    }

    /// The start-token capacity gate must not compare a raw token balance
    /// against a NATIVE-denominated minimum.
    ///
    /// That is the decimals bug that zeroed every USDC cycle
    /// (`no_flashloan_provider`, 2,286 of 6,000 records). The gate is therefore
    /// "known and non-zero" only; the `>= min` comparison belongs in
    /// `capacity_capped_amount`, which receives converted bounds.
    #[test]
    fn start_token_gate_is_unit_free() {
        // 55,886 USDC at 6dp — real Balancer vault holding on Base.
        let usdc_capacity = U256::from(55_886_974_681u64);
        let native_min = U256::exp10(17); // 0.1 WETH at 18dp

        assert!(
            usdc_capacity < native_min,
            "precondition: raw USDC compares BELOW a native minimum, which is why \
             the start gate must not make that comparison"
        );
        // Non-zero is the only property the start gate may rely on.
        assert!(!usdc_capacity.is_zero(), "USDC is fundable and must survive the gate");
    }

    #[test]
    fn capacity_withholds_provider_below_min_flash_loan() {
        let min = U256::from(100u64);
        assert_eq!(
            capacity_capped_amount(Some(U256::from(99u64)), U256::from(1000u64), min, true),
            None,
            "dust capacity must withhold the provider, not offer an unfillable loan"
        );
        assert_eq!(
            capacity_capped_amount(Some(U256::zero()), U256::from(1000u64), min, true),
            None
        );
    }

    /// Real-world regression: a 6-decimal start token must not be starved by a
    /// native-denominated (18dp) minimum.
    ///
    /// Measured on Base at block 50461601: the Balancer vault held 55,886 USDC
    /// (5.5886e10 raw) while `min_flash_loan` was 0.1 WETH (1e17 native). Passed
    /// unconverted, `capacity_capped_amount` compares 5.5886e10 against 1e17 and
    /// withholds the provider — despite ~$55k of genuinely available liquidity.
    /// `no_flashloan_provider` was the single largest rejection reason in the
    /// funnel, 2,286 of the last 6,000 candidate records.
    ///
    /// The fix is at the CALLER (convert native -> token units first); this test
    /// pins the arithmetic that makes the bug inevitable if that conversion is
    /// ever dropped again.
    #[test]
    fn native_denominated_min_starves_a_six_decimal_token() {
        let usdc_available = U256::from(55_886_974_681u64); // 55,886 USDC @ 6dp
        let native_min = U256::exp10(17); // 0.1 WETH @ 18dp
        assert_eq!(
            capacity_capped_amount(Some(usdc_available), usdc_available, native_min, true),
            None,
            "unconverted native minimum starves USDC — this is the bug, pinned"
        );

        // Correctly converted: 0.1 WETH ~ $360 ~ 360e6 raw USDC. The same
        // capacity now funds the trade.
        let min_in_token = U256::from(360_000_000u64);
        assert_eq!(
            capacity_capped_amount(Some(usdc_available), usdc_available, min_in_token, true),
            Some(usdc_available),
            "converted to token units, 55,886 USDC must fund a $360 minimum"
        );
    }

    #[test]
    fn capacity_unknown_fails_closed_only_when_allowlist_configured() {
        let min = U256::from(100u64);
        // Allowlist configured => we could have measured it => withhold.
        assert_eq!(
            capacity_capped_amount(None, U256::from(1000u64), min, true),
            None,
            "unmeasured provider must not be advertised at full size"
        );
        // No allowlist => token set not enumerable => preserve prior behavior.
        assert_eq!(
            capacity_capped_amount(None, U256::from(1000u64), min, false),
            Some(U256::from(1000u64))
        );
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
    fn derive_chain_time_budget_base_allows_600ms_search() {
        let (search, quote, sim) = derive_chain_time_budget_ms("base", 600, 300, 350);
        assert_eq!(search, 600);
        assert_eq!(quote, 300);
        assert_eq!(sim, 350);
        // The L2 simulation ceiling was 400ms, which nothing could ever meet:
        // simulation cost p50 2802ms over 17 RPC calls, so every candidate timed
        // out. The gas-limit and revert-retry fixes cut that to p50 ~180ms over
        // 2 calls; then allowlisting the signer made simulation EXECUTE the full
        // plan rather than revert early at NotExecutor, raising it to p50 670ms /
        // max 924ms. The ceiling is 1500ms: clear of the observed max with
        // headroom, still inside a 2s block.
        let (search_capped, quote_capped, sim_capped) =
            derive_chain_time_budget_ms("base", 1200, 400, 5_000);
        assert_eq!(search_capped, 800);
        assert_eq!(quote_capped, 350);
        assert_eq!(sim_capped, 1_500, "sim budget must still be capped");
        // Below the ceiling, the configured value is honoured unchanged.
        let (_, _, sim_uncapped) = derive_chain_time_budget_ms("base", 600, 300, 500);
        assert_eq!(sim_uncapped, 500);
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
            tick_ladder: None,
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
    fn inconclusive_native_price_probe_is_never_cached() {
        // THE regression guard. Caching an `Unknown` (transport failure) as a
        // price verdict poisons the token for the whole cache TTL: every cycle
        // starting there is rejected pre-simulation and every edge touching it
        // is silently dropped from the search graph. That single behaviour
        // accounted for 94.6% of all candidate rejections ever recorded here.
        assert_eq!(
            NativePriceProbe::Unknown.cache_entry(),
            None,
            "an inconclusive probe must never be written to the price cache"
        );
    }

    #[test]
    fn definitive_no_route_is_cached_as_unreliable() {
        // The legitimate suppression case: every fee tier answered, and the
        // answer was "no pool". Worth caching so we stop re-quoting it.
        let entry = NativePriceProbe::NoRoute
            .cache_entry()
            .expect("a definitive no-route verdict should be cached");
        assert!(
            !entry.is_reliable(),
            "no-route must cache as unreliable, never as a usable price"
        );
    }

    #[test]
    fn priced_probe_round_trips_through_the_cache_entry() {
        let price = NativePrice::new(U256::exp10(6), U256::from(532_286_096_352_062u64), true);
        let entry = NativePriceProbe::Priced(price)
            .cache_entry()
            .expect("a priced probe should be cached");
        assert_eq!(entry, price);
        assert!(entry.is_reliable());
    }

    #[test]
    fn native_price_probe_labels_are_distinct() {
        // Labels become Prometheus label values; collisions would merge
        // distinct outcomes into one series and hide the failure mode.
        let labels = [
            NativePriceProbe::Priced(NativePrice::unit()).label(),
            NativePriceProbe::NoRoute.label(),
            NativePriceProbe::Unknown.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "probe labels must be distinct");
    }

    #[test]
    fn native_flash_bounds_convert_into_six_decimal_tokens() {
        // Regression guard for the decimals bug that made every USDC-start
        // cycle unsizable. MIN_FLASH_LOAN_WEI is native-denominated (1e18 =
        // 1 ETH). Passed unconverted into sizing it meant 1e18 RAW USDC units —
        // one trillion USDC — so `upper_cap < min_amount` held for every cycle
        // and `optimize_trade_size` returned None 100% of the time.
        let min_flash_native = U256::exp10(18); // 1 ETH

        // Measured live on Base: 1 USDC (1e6 raw) = 532_286_096_352_062 wei.
        let usdc = NativePrice::new(U256::exp10(6), U256::from(532_286_096_352_062u64), true);
        let min_in_usdc = usdc
            .tokens_for_native_strict(min_flash_native)
            .expect("a reliable price must convert");

        // 1 ETH ~= $1880, so ~1.88e9 raw units at 6 decimals.
        assert!(
            min_in_usdc > U256::from(1_000_000_000u64)
                && min_in_usdc < U256::from(10_000_000_000u64),
            "1 ETH should convert to ~1.88e9 raw USDC, got {min_in_usdc}"
        );
        // And it must be nowhere near the raw wei value that caused the bug.
        assert!(
            min_in_usdc < min_flash_native / U256::exp10(8),
            "converted bound must not remain wei-scaled"
        );
    }

    #[test]
    fn native_flash_bounds_are_identity_for_wrapped_native() {
        // The 18-decimal path must be unchanged: a 1 ETH bound stays 1 WETH.
        let weth = NativePrice::new(U256::exp10(18), U256::exp10(18), true);
        let min_flash_native = U256::exp10(18);
        assert_eq!(
            weth.tokens_for_native_strict(min_flash_native),
            Some(min_flash_native)
        );
    }

    #[test]
    fn unpriceable_token_yields_no_flash_bounds() {
        // Fail closed: with no price we cannot express the bound in the token's
        // units, so we must decline rather than clamp against a wei constant.
        let poison = NativePrice::new(U256::zero(), U256::zero(), false);
        assert_eq!(poison.tokens_for_native_strict(U256::exp10(18)), None);
        assert_eq!(NativePrice::unit().tokens_for_native_strict(U256::exp10(18)), None);
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
            hub_usd_liquidity: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
        });

        let cycles = vec![
            IndexedCycle::from_nodes(vec![0, 1, 0]),
            IndexedCycle::from_nodes(vec![0, 2, 0]),
        ];
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

        let cycles = vec![IndexedCycle::from_nodes(vec![0, 1, 0])];
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

        let cycles = vec![IndexedCycle::from_nodes(vec![0, 1, 0])];
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
        let cycles = vec![IndexedCycle::from_nodes(vec![0, 1, 0])];
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
                state: None,
            },
            estimated_gas: 70_000,
            weight: -2,
            max_input: U256::from(1_000_000u64),
            tolerance_bps: 10,
            observed_slippage_bps: 100,
            quote_block: None,
            active: true,
            tick_ladder: None,
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
                state: None,
            },
            estimated_gas: 70_000,
            weight: -2,
            max_input: U256::from(10_000u64),
            tolerance_bps: 5,
            observed_slippage_bps: 10,
            quote_block: None,
            active: true,
            tick_ladder: None,
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
            &HashSet::new(),
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
            hub_usd_liquidity: None,
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
            hub_usd_liquidity: None,
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
            tick_ladder: None,
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
            slipstream_quoter: None,
            pancakeswap_quoter: None,
            pancakeswap_pools: None,
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
