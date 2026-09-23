//! Where every legacy environment variable goes.
//!
//! Blueprint §2.4 forbids late configuration lookup on the dispatch path, and
//! this repository reads **192** distinct environment variables at call sites
//! throughout the hot path.
//!
//! # The `ARBOT_` prefix was never the boundary
//!
//! This table covered 84 `ARBOT_*` variables and its test scanned for that
//! literal prefix, so it read as complete while **132** variables went
//! unaccounted for — including `PRIVATE_KEY`, `ALCHEMY_KEY` and
//! `EDGE_SLIPPAGE_BPS`, which sets the ceiling on the per-edge tolerance band
//! that becomes the on-chain `min_out` floor. A manifest that is complete over
//! a prefix and silent about everything else is worse than an obviously
//! partial one, because nothing reads as missing. Found 2026-09-23 while
//! resolving §4.7's `liquidity_cache` entry. Retiring them is not one change -- each belongs to
//! the crate that will own its behaviour, and most of those crates do not exist
//! yet.
//!
//! So this table is the contract between Phase 0 and Phases 1-8: every variable
//! is accounted for exactly once, with a destination and a reason. The test in
//! `tests/env_coverage.rs` enforces it in BOTH directions -- a variable in the
//! source but not the table fails, and a table entry no longer present in the
//! source fails too, so the manifest cannot rot as modules migrate.
//!
//! Nothing here changes runtime behaviour. The legacy crate keeps reading these
//! until its consumer is ported.

/// Where a legacy variable's behaviour will live after migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    /// Becomes a typed field on `ApexConfig` itself (boot-time wiring).
    Config,
    State,
    Math,
    Venues,
    Search,
    Econ,
    Sim,
    Capture,
    Chain,
    /// Feature is removed by the blueprint; the variable dies with it.
    Retired,
    /// Test-harness or fixture plumbing, never production configuration.
    TestOnly,
    /// Operator/research tooling that legitimately stays environment-driven
    /// because it is invoked ad hoc, not on the trading path.
    Tooling,
    /// Metrics, logs, alerting and P&L accounting -> `apex-obs` (§26, §27).
    Observability,
    /// A CREDENTIAL. Never becomes a plain `ApexConfig` field, never appears in
    /// a log, a metric label or a snapshot (§43, INV-46). `apex_config::Secret`
    /// carries it; §18's signer pool replaces the key itself.
    Secret,
}

pub struct LegacyEnvVar {
    pub name: &'static str,
    pub destination: Destination,
    pub note: &'static str,
}

const fn v(name: &'static str, destination: Destination, note: &'static str) -> LegacyEnvVar {
    LegacyEnvVar { name, destination, note }
}

use Destination::*;

pub const LEGACY_ENV_VARS: &[LegacyEnvVar] = &[
    // ---- Base fast path -> apex-state / apex-chain --------------------------
    v("ARBOT_BASE_FAST", State, "master switch for the preconfirmed-log fast path"),
    v("ARBOT_BASE_FAST_ALLOW_MARGIN_HOPS", Search, "frontier admission width"),
    v("ARBOT_BASE_FAST_MAX_CYCLES", Search, "cap on cycles re-priced per dirty-set drain; a §29 compute budget"),
    v("ARBOT_BASE_FAST_MAX_INDEX_CYCLES", Search, "cycle-index fan-out cap"),
    v("ARBOT_BASE_FAST_PREPARE", Search, "whether a drain builds executable routes"),
    v("ARBOT_BASE_FAST_PREP_SLOTS", Econ, "concurrent prep slots; a compute budget (§29)"),
    v("ARBOT_BASE_FAST_RECONCILE_SECS", State, "fast-path state reconcile cadence"),
    v("ARBOT_BASE_FAST_REF_NATIVE", Econ, "reference native size used for ranking"),
    v("ARBOT_BASE_FAST_VERIFY_BATCH", State, "verification batch size"),
    v("ARBOT_BASE_FAST_VERIFY_TTL_SECS", State, "verification verdict TTL"),

    // ---- Concentrated liquidity -> apex-math -------------------------------
    v("ARBOT_CL_MULTI_TICK", Math, "multi-tick CL pricing; default OFF today and the fast path carries no ladder regardless -- §1.1.1 / G-PRICE-2"),
    v("ARBOT_CL_LADDER_WORDS", Math, "tick-bitmap words fetched per side"),
    v("ARBOT_CL_MAX_TICKS", Math, "max tick crossings per quote"),
    v("ARBOT_CL_TICK_BUFFER_BPS", Math, "50 bps haircut on single-tick CL quotes; must reach 0 once the ladder is wired (§1.1.1)"),
    v("ARBOT_CL_EXEC_BUFFER_BPS", Econ, "execution buffer applied to CL min-out"),
    v("ARBOT_LOCAL_CL_QUOTES", Math, "local CL quoting vs on-chain quoter fallback"),
    v("ARBOT_CL_QUOTE_PARITY", Math, "enable the per-pool parity gate"),
    v("ARBOT_CL_PARITY_CHECKS_PER_SCAN", Math, "how many pools are measured against their own quoter per scan"),
    v("ARBOT_CL_PARITY_MAX_ERR_BPS", Math, "parity trust threshold"),
    v("ARBOT_CL_PARITY_TTL_SECS", Math, "how long a per-pool parity verdict is trusted before re-measuring"),
    v("ARBOT_SLIPSTREAM_LIVE_QUOTE", Venues, "force live quoting for Slipstream"),

    // ---- State trust -> apex-state -----------------------------------------
    v("ARBOT_LIVE_STATE_SHADOW", State, "shadow-mode live state"),
    v("ARBOT_STATE_GATE_CHECKS_PER_SCAN", State, "state-gate sampling rate"),
    v("ARBOT_STATE_GATE_MAX_ERR_BPS", State, "state-gate trust threshold"),
    v("ARBOT_STATE_GATE_TTL_SECS", State, "state-gate verdict TTL"),
    v("ARBOT_STATE_VALIDATION_SECS", State, "background validation cadence"),

    // ---- Search -> apex-search ---------------------------------------------
    v("ARBOT_HUB_SEARCH", Search, "hub-anchored cycle search"),
    v("ARBOT_HUB_SEARCH_PARALLEL_EDGES", Search, "parallel-edge expansion"),
    v("ARBOT_BF_SKIP_ON_STABLE_GRAPH", Search, "skip Bellman-Ford when structure is unchanged"),
    v("ARBOT_CYCLE_INDEX_COMPARE", Search, "differential: cycle index vs Bellman-Ford"),
    v("ARBOT_MAX_CYCLE_FEE_BPS", Search, "fee-stack ceiling for admitted cycles"),
    v("ARBOT_START_TOKENS", Search, "start-token allowlist"),
    v("ARBOT_DEPTH_DIVISOR", Search, "pool-depth fraction treated as tradeable"),
    v("ARBOT_FILTER_UNFUNDABLE", Search, "drop cycles no flash provider can fund"),
    v("ARBOT_RESCAN_SAME_BLOCK", Search, "permit a second scan within one block"),

    // ---- Economics -> apex-econ --------------------------------------------
    v("ARBOT_COST_COMPETITION_BPS", Econ, "competition premium; superseded by the §21 competitor model"),
    v("ARBOT_COST_EXEC_BUFFER_BPS", Econ, "flat execution buffer in bps; §23 replaces it with TotalExecutionCost"),
    v("ARBOT_COST_FLASH_FEE_BPS", Econ, "flash premium; superseded by §19 FlashSourceQuote"),
    v("ARBOT_COST_GAS_BPS", Econ, "gas premium; superseded by §23 TotalExecutionCost"),
    v("ARBOT_COST_RISK_BPS", Econ, "flat risk premium in bps; §2 replaces it with scenario-conditioned EV"),
    v("ARBOT_ARB_FEE_CEILING_PPM", Econ, "Arbitrum per-byte data-fee ceiling; seeds the §23.2 data-fee model"),
    v("ARBOT_BID_PROFIT_FRACTION_BPS", Capture, "bid as a fraction of profit; §24.2 replaces with an empirical bid curve"),
    v("ARBOT_TIP_BPS", Capture, "priority tip; same replacement"),
    v("ARBOT_GAS_RESERVE_TXS", Capture, "per-signer gas reserve, in transactions (§2.8)"),

    // ---- Simulation -> apex-sim --------------------------------------------
    v("ARBOT_SIM_REVM", Sim, "enable in-process REVM fork simulation (Tier 2)"),
    v("ARBOT_SIM_REVM_LIVE", Sim, "REVM against live state"),
    v("ARBOT_SIM_REVM_TIMEOUT_MS", Sim, "REVM deadline; becomes part of the §29.3 simulation admission bound"),
    v("ARBOT_SIM_REVM_EXECUTOR_BYTECODE", Sim, "bytecode override for fork simulation"),
    v("ARBOT_SIM_PREFETCH", Sim, "prefetch accounts before simulating"),
    v("ARBOT_SIM_PREFETCH_MAX_ACCOUNTS", Sim, "bound on accounts prefetched before a fork simulation"),
    v("ARBOT_SIM_QUORUM_MODE", Sim, "best_effort / strict / off"),
    v("ARBOT_SIM_QUORUM_TIMEOUT_MS", Sim, "deadline for an independent verifier endpoint to answer"),
    v("ARBOT_SIM_CASCADE_DEPTH", Sim, "simulation cascade depth"),
    v("ARBOT_SIM_L1_FEE", Sim, "include L1 data fee in simulation"),
    v("ARBOT_L2_SIM_CEILING_MS", Sim, "L2 simulation time budget; feeds the §29.5 latency decomposition"),

    // ---- Chain / RPC -> apex-chain -----------------------------------------
    v("ARBOT_RPC_URL", Chain, "primary RPC; moves into the chain adapter"),
    v("ARBOT_BASE_RPC_HTTP", Chain, "Base HTTP endpoint; moves behind the chain adapter's failover client"),
    v("ARBOT_BLOCK_POLL_MS", Chain, "polling cadence for new heads; the slow path's clock, not the fast path's"),
    v("ARBOT_NEW_BLOCK_WAIT_MS", Chain, "how long a scan waits for a new head before proceeding"),
    v("ARBOT_SCAN_IDLE_SLEEP_MS", Chain, "backoff when a scan finds nothing; a polling artefact the event-driven path removes"),
    v("ARBOT_RPC_QUOTE_TIMEOUT_SECS", Chain, "per-quote RPC deadline; bounds the §29.3 outstanding-RPC budget"),
    v("ARBOT_UNIV3_QUEUE_WAIT_TIMEOUT_SECS", Venues, "UniV3 quoter queue wait"),
    v("ARBOT_UNIV3_TOTAL_DEADLINE_SECS", Venues, "UniV3 quoter total deadline"),
    v("ARBOT_CANDIDATE_CONCURRENCY", Econ, "candidate prep concurrency; a compute budget (§29)"),
    v("ARBOT_REQUIRE_CHAIN_COVERAGE", Config, "refuse to start without full chain coverage"),

    // ---- Dispatch -> apex-capture ------------------------------------------
    v("ARBOT_RELAY_PARALLEL_BLAST", Capture, "parallel relay blast; §24.2 gates this on proven incremental EV"),
    v("ARBOT_DISABLE_PRIVATE_RAW_FALLBACK", Capture, "disable raw-tx fallback on a private lane"),
    v("ARBOT_FORCE_ATTEMPT", Capture, "bypass gating and attempt; must not survive into the v4 capture path"),

    // ---- Process / operator -> apex-config ---------------------------------
    v("ARBOT_ENV", Config, "environment name (production guard)"),
    v("ARBOT_INTERACTIVE", Config, "interactive command listener"),
    v("ARBOT_NONINTERACTIVE", Config, "inverse of the above; one of the pair should die in Phase 17"),

    // ---- Removed features ---------------------------------------------------
    v("ARBOT_ENABLE_JIT", Retired, "JIT liquidity is excluded from production by §42; dies with Op.JIT_LP_* in Phase 5"),

    // ---- Diagnostics / research tooling ------------------------------------
    v("ARBOT_CENSUS", Tooling, "event-triggered census run"),
    v("ARBOT_CENSUS_PATH", Tooling, "where the census writes its JSONL samples"),
    v("ARBOT_CENSUS_PER_HOPS", Tooling, "census samples per hop count"),
    v("ARBOT_DUMP_CALLDATA", Tooling, "dump encoded calldata"),
    v("ARBOT_DUMP_CALLDATA_PATH", Tooling, "where encoded calldata dumps are written for inspection"),

    // ---- Test harness -------------------------------------------------------
    v("ARBOT_INTEGRATION_SMOKE", TestOnly, "integration smoke switch"),
    v("ARBOT_INTEGRATION_CHAIN", TestOnly, "which chain the integration smoke test runs against"),
    v("ARBOT_SMOKE_EXECUTOR", TestOnly, "executor address for the smoke test"),
    v("ARBOT_FORK_RPC_URL", TestOnly, "fork endpoint for tests"),
    v("ARBOT_TEST_ENV_FLAG_9", TestOnly, "fixture for util::env_flag"),
    v("ARBOT_TEST_ENV_PARSE_9", TestOnly, "fixture for util::env_parse_opt"),
    v("ARBOT_TEST_ENV_U256_9", TestOnly, "fixture for util::env_u256_opt"),

    // ======================================================================
    // Variables outside the `ARBOT_` prefix, added 2026-09-23.
    // ======================================================================
    v("PRIVATE_KEY", Secret, "the signing key itself; §18 replaces it with a signer pool and a KMS/keystore boundary"),
    v("ALCHEMY_KEY", Secret, "provider API key, interpolated into RPC URLs"),
    v("TAX_EXCHANGE_API_KEY", Secret, "exchange API key for fiat conversion"),
    v("BASE_RPC_URL", Chain, "single Base HTTP endpoint (superseded by the plural form)"),
    v("BASE_RPC_URLS", Chain, "Base HTTP endpoint list, the primary transport"),
    v("BASE_RPC_HTTP", Chain, "Base HTTP endpoint, a third spelling of the same thing"),
    v("BASE_FLASHBLOCK_HTTP_URL", Chain, "Flashblocks preconfirmation feed (§21)"),
    v("RPC_URL", Chain, "chain-agnostic HTTP endpoint, single form"),
    v("RPC_URLS", Chain, "chain-agnostic HTTP endpoint list"),
    v("RPC_MAX_BACKOFF_SECS", Chain, "provider backoff ceiling"),
    v("WS_CONNECT_TIMEOUT_SECS", Chain, "websocket connect timeout"),
    v("PRIVATE_RELAY_URL", Chain, "private submission lane (§24)"),
    v("PRIVATE_RELAY_URLS", Chain, "private submission lanes"),
    v("PRIVATE_RELAY_TIMEOUT_MS", Chain, "private lane timeout"),
    v("CHAIN", Config, "which chain this process trades; §23 makes it a typed ChainId"),
    v("CHAIN_LIST", Config, "chains enabled for a multi-chain run (§23)"),
    v("BASE_EXECUTOR_ADDRESS", Config, "deployed executor; §19 makes it a verified registry entry"),
    v("BASE_UNIV3_FACTORY", Config, "venue factory address; §6.3 admission verifies it on chain"),
    v("BASE_UNIV3_QUOTER", Config, "venue quoter address; same"),
    v("REGISTRY_PATH", Config, "path to registry.json, the venue and token registry"),
    v("REGISTRY_CACHE_PATH", Config, "parsed-registry cache"),
    v("REGISTRY_EXPECTED_HASH", Config, "content hash pin for the registry"),
    v("OPS_INPUTS_STRICT", Config, "refuse unknown keys in ops/inputs.yaml"),
    v("POOL_DATA_ROOT", Config, "pool inventory root; §32 makes it a manifest with a content hash"),
    v("OFFLINE_MODE", Config, "run the engine with no node attached, for replay"),
    v("MEV_ROLE", Config, "operator role selector for multi-process deployments"),
    v("UNIV2_LOAD_CONCURRENCY", Venues, "reserve-load fan-out"),
    v("UNIV3_BOOTSTRAP_MAX_PAIRS", Venues, "discovery bound"),
    v("UNIV3_BOOTSTRAP_MAX_POOLS", Venues, "discovery bound"),
    v("UNIV3_MAX_CONCURRENT_POOL_TASKS", Venues, "discovery fan-out"),
    v("UNIV3_MIN_FORCED_QUOTES", Venues, "forced-quote floor during bootstrap"),
    v("UNIV3_QUOTE_CONCURRENCY", Venues, "quoter fan-out"),
    v("SLIPSTREAM_BOOTSTRAP_MAX_PAIRS", Venues, "discovery bound"),
    v("SLIPSTREAM_BOOTSTRAP_MAX_POOLS", Venues, "discovery bound"),
    v("SLIPSTREAM_MAX_CONCURRENT_POOL_TASKS", Venues, "discovery fan-out"),
    v("SLIPSTREAM_MIN_FORCED_QUOTES", Venues, "forced-quote floor"),
    v("SOLIDLY_STATE_CONCURRENCY", Venues, "state-load fan-out"),
    v("ENABLE_UNIV4_FIXED_PRICE_QUOTES", Venues, "the V4 stub's gate; dies with the stub in Phase 11"),
    v("CB_MAX_CONSECUTIVE_FAILURES", Venues, "consecutive failures that trip the breaker"),
    v("CB_REVERT_MIN_SAMPLES", Venues, "minimum samples before a revert rate is actionable"),
    v("CB_REVERT_RATE_LIMIT", Venues, "circuit-breaker revert-rate ceiling before tripping"),
    v("CB_REVERT_WINDOW_SECS", Venues, "circuit-breaker revert measurement window"),
    v("CB_RPC_ERROR_LIMIT", Venues, "circuit-breaker RPC-error ceiling before tripping"),
    v("CB_RPC_ERROR_WINDOW_SECS", Venues, "circuit-breaker RPC-error measurement window"),
    v("LOW_LIQUIDITY_FACTORIES", Venues, "factories whose pools are treated as thin"),
    v("LOW_LIQUIDITY_LOOKBACK_BLOCKS", Venues, "thin-pool detection window"),
    v("LOW_LIQUIDITY_MAX_TOTAL_TOKENS", Venues, "thin-pool bound"),
    v("LOW_LIQUIDITY_PRICE_DEVIATION_BPS", Venues, "thin-pool price-deviation bound"),
    v("POOL_MONITOR_POLL_MS", Venues, "pool state poll cadence"),
    v("POOL_MONITOR_STALE_MS", Venues, "pool state staleness ceiling"),
    v("POOL_DEPTH_REFRESH_SECS", Venues, "third-party depth refresh; §18.5 bounds what that number may decide"),
    v("BELLMAN_MAX_RELAXATIONS", Search, "relaxation budget (§29 compute budget)"),
    v("MAX_BELLMAN_CYCLES", Search, "cycle-enumeration cap"),
    v("MAX_HOPS_CAP", Search, "hop ceiling; §13 replaces it with ComplexityCost"),
    v("DETECTION_HAIRCUT_BPS", Search, "Stage-1 rate haircut; now a field on Graph, read once at construction"),
    v("EDGE_SLIPPAGE_BPS", Search, "ceiling on the per-edge tolerance band"),
    v("EDGE_MIN_HEALTH_SCORE_BPS", Search, "edge admission score floor"),
    v("STRICT_HUB_INTERMEDIATES", Search, "restrict intermediate tokens to hubs"),
    v("STRICT_START_TOKEN_HUB_ONLY", Search, "restrict cycle starts to hubs"),
    v("DYNAMIC_TOP_TOKENS_30D", Search, "token universe from 30-day volume"),
    v("TOKEN_WHITELIST_MAX", Search, "universe size cap"),
    v("MIN_LIQUIDITY_TOKENS", Search, "token admission floor"),
    v("RANK_AERO_USD", Search, "ranking price stand-in for AERO when no feed is present"),
    v("RANK_CBTC_USD", Search, "ranking price stand-in for cbBTC when no feed is present"),
    v("HOT_POOL_ACTIVITY_LOOKBACK_BLOCKS", Search, "hot-pool ranking window"),
    v("HOT_POOL_FEE_AWARE", Search, "whether hot-pool ranking charges venue fees"),
    v("HOT_POOL_MAX_VOLUME_SAMPLES", Search, "hot-pool sampling bound"),
    v("HOT_POOL_RANK_BY_ACTIVITY", Search, "hot-pool ranking mode"),
    v("HOT_POOL_RANK_CONCURRENCY", Search, "hot-pool ranking fan-out"),
    v("HOT_POOL_RPC_TIMEOUT_MS", Search, "hot-pool RPC timeout"),
    v("HOT_POOL_SKIP_VOLUME", Search, "skip the volume pass when ranking hot pools"),
    v("HOT_POOL_VOLUME_TIMEOUT_MS", Search, "volume-pass timeout"),
    v("BACKRUN_MINED_POLL_MS", Search, "backrun mined-poll cadence (Phase 13)"),
    v("BACKRUN_MIN_PRICE_IMPACT_BPS", Search, "backrun trigger floor"),
    v("BACKRUN_MONITOR", Search, "which mempool/log monitor supplies backrun targets (Phase 13)"),
    v("BACKRUN_MONITOR_ENABLED", Search, "backrun master switch"),
    v("BACKRUN_POLL_INTERVAL_MS", Search, "backrun poll cadence"),
    v("BACKRUN_POST_STATE", Search, "whether to rebuild state after a backrun target"),
    v("MAX_QUOTE_BLOCK_LAG", State, "how stale a quote may be; §5 makes it a StateVersion comparison"),
    v("BASE_AMOUNT_WEI", Econ, "default probe size when no per-token sizing exists"),
    v("PROFIT_MARGIN_BPS", Econ, "minimum margin a candidate must clear"),
    v("CROSS_CHAIN_PROFIT_BPS", Econ, "minimum margin for a cross-chain candidate"),
    v("COMPETITION_EMA_ALPHA", Econ, "competition-pressure smoothing (§9)"),
    v("CONGESTION_EMA_ALPHA", Econ, "congestion smoothing"),
    v("MAX_GAS_PRICE_CONGESTION_BPS", Econ, "gas-price ceiling under congestion"),
    v("NATIVE_PRICE_CONCURRENCY", Econ, "native-price fan-out"),
    v("NATIVE_PRICE_TTL_SECS", Econ, "native-price staleness ceiling"),
    v("NATIVE_USD_PRICE", Econ, "manual native/USD override when no feed is available"),
    v("AAVE_FLASH_FEE_BPS", Econ, "flash-loan fee (§14 makes it a measured cost term)"),
    v("ERC3156_FEE_BPS", Econ, "ERC-3156 flash fee"),
    v("ERC3156_LENDER", Econ, "ERC-3156 flash-loan lender address"),
    v("SIZE_SEARCH_EXPAND_ITERS", Econ, "sizing search expansion budget"),
    v("SIZE_SEARCH_REFINE_ITERS", Econ, "sizing search refinement budget"),
    v("SIPHON_BPS", Econ, "fraction of realised profit withdrawn from the executor"),
    v("SIPHON_TARGET_ADDRESS", Econ, "withdrawal destination; unset today (R-11)"),
    v("REINVEST_BPS", Econ, "fraction of realised profit retained as working capital"),
    v("TAX_RESERVE_BPS", Econ, "fraction of realised profit set aside for tax"),
    v("TAX_STABLECOIN_SYMBOL", Econ, "denomination the tax reserve is held in"),
    v("TAX_WALLET_ADDRESS", Econ, "destination address for the tax reserve"),
    v("TAX_EXCHANGE_API_URL", Econ, "endpoint used to convert the tax reserve to fiat"),
    v("SHADOW_MODE", Capture, "run without dispatching (§35 red/blue)"),
    v("SHADOW_TAG", Capture, "label distinguishing one shadow run from another (§35)"),
    v("SHADOW_LOG_PATH", Capture, "where shadow-mode output is written (§35)"),
    v("SHADOW_EXECUTOR_MAX_SLIPPAGE_BPS", Capture, "shadow executor bound"),
    v("PROMETHEUS_PORT", Observability, "port the metrics endpoint listens on (§26)"),
    v("RUST_LOG", Observability, "tracing filter directives for the whole process"),
    v("CANDIDATE_LOG_PATH", Observability, "candidate decision log (§27)"),
    v("CANDIDATE_LOG_BUFFER", Observability, "candidate log channel depth"),
    v("ACCOUNTING_ENABLED", Observability, "P&L accounting master switch (§26)"),
    v("ACCOUNTING_DIR", Observability, "accounting output root directory (§26)"),
    v("ACCOUNTING_TRADE_LOG", Observability, "per-trade log"),
    v("ACCOUNTING_EVENT_LOG", Observability, "accounting event log path (§26 P&L attribution)"),
    v("ACCOUNTING_DAILY_SUMMARY", Observability, "daily P&L rollup output path (§26)"),
    v("ALERT_WEBHOOK_URL", Observability, "webhook the alert router posts to (§26)"),
    v("ALERT_DAILY_NET_TARGET_WEI", Observability, "daily net-profit target the alerting compares against"),
    v("ALERT_REVERT_RATE_THRESHOLD", Observability, "revert-rate threshold that fires an alert"),
    v("ALERT_RPC_ERROR_RATE_THRESHOLD", Observability, "RPC-error-rate threshold that fires an alert"),
    v("CHAOS_BROADCAST_DELAY_MS", TestOnly, "fault injection"),
    v("CHAOS_DISABLE_WS", TestOnly, "fault injection"),
    v("CHAOS_PUBLIC_REJECT_BPS", TestOnly, "fault injection"),
    v("CHAOS_RELAY_REJECT_BPS", TestOnly, "fault injection"),
    v("CHAOS_WS_GAP_SECS", TestOnly, "fault injection"),
    v("JIT_LP_ENABLED", Retired, "JIT liquidity is out of scope (§1.4)"),
    v("JIT_MIN_AMOUNT_WEI", Retired, "JIT position floor; JIT liquidity is out of scope (§1.4)"),
    v("JIT_SEED_BPS", Retired, "JIT seed size; JIT liquidity is out of scope (§1.4)"),
    v("JIT_TICK_RANGE", Retired, "JIT position width; JIT liquidity is out of scope (§1.4)"),
    v("SANDWICH_MONITOR", Retired, "sandwich target monitor; sandwich.rs is deleted (§4)"),
    v("SANDWICH_MONITOR_ENABLED", Retired, "sandwich monitor switch; sandwich.rs is deleted (§4)"),
    v("PUBLIC_MEMPOOL_JITTER_BPS", Retired, "apply_public_mempool_jitter is deleted (§4)"),
    v("BRIDGE_MAX_TIME_SECS", Retired, "bridge.rs is deleted (§4)"),
    v("BRIDGE_ROUTES", Retired, "inline bridge route table; bridge.rs is deleted (§4)"),
    v("BRIDGE_ROUTES_FILE", Retired, "bridge route table path; bridge.rs is deleted (§4)"),

    // ---- found by the call-form scan, added 2026-09-23 --------------------
    v("APP_ENV", Config, "deployment environment label read at boot"),
    v("NODE_ENV", Config, "deployment environment label, second spelling"),
    v("RUN_MODE", Config, "process run mode selector read at boot"),
    v("DOTENV_FILE", Config, "which .env file dotenvy loads at startup"),
    v("ARB_RPC_URL", Chain, "Arbitrum HTTP endpoint (§23 chain expansion)"),
    v("ARB_WS_RPC_URLS", Chain, "Arbitrum websocket endpoints (§23)"),
    v("ARB_TOKENS", Search, "Arbitrum token universe seed list (§23)"),
    v("BASE_BALANCER_VAULT", Config, "Balancer vault address on Base; §6.3 verifies it on chain"),
    v("BASE_BAL_VAULT", Config, "Balancer vault address, second spelling"),
    v("BASE_BAL_FLASHLOAN_TOKENS", Econ, "tokens Balancer will flash-lend on Base"),
    v("ETH_BAL_VAULT", Config, "Balancer vault address on Ethereum"),
    v("ETH_UNIV3_ROUTER", Config, "Uniswap V3 router on Ethereum; §6.3 verifies it"),
    v("ETH_UNIV3_VALIDATION_AMOUNT_WEI", Venues, "probe size used to validate a UniV3 pool at bootstrap"),
    v("CYCLE_SEARCH_TIMEOUT_MS", Search, "wall-clock budget for one cycle search (§29)"),
    v("MAX_CANDIDATE_PATHS", Search, "cap on candidate paths kept per scan"),
    v("MAX_HOPS", Search, "hop ceiling; §13 replaces it with ComplexityCost"),
    v("MIN_HOPS", Search, "hop floor for candidate admission"),
    v("EDGE_PRUNE_LIQUIDITY_WEIGHT", Search, "edge-pruning score weight for liquidity"),
    v("EDGE_PRUNE_MAX_SLIPPAGE_BPS", Search, "edge-pruning slippage ceiling"),
    v("EDGE_PRUNE_MIN_SCORE", Search, "edge-pruning admission score floor"),
    v("EDGE_PRUNE_PROFIT_WEIGHT", Search, "edge-pruning score weight for profit"),
    v("EDGE_PRUNE_SLIPPAGE_WEIGHT", Search, "edge-pruning score weight for slippage"),
    v("QUOTE_BUDGET_MS", Sim, "wall-clock budget for quoting a candidate (§29)"),
    v("SIMULATION_BUDGET_MS", Sim, "wall-clock budget for simulation (§20, §29)"),
    v("REGISTRY_FILE", Config, "registry path, second spelling"),
    v("REGISTRY_URL", Config, "remote registry source"),
    v("REGISTRY_IPFS_CID", Config, "registry content id for IPFS retrieval"),
    v("REGISTRY_IPFS_GATEWAY", Config, "IPFS gateway used to fetch the registry"),
    v("REGISTRY_IPNS", Config, "IPNS name resolving to the current registry"),
    v("TEST_EXECUTOR_ADDRESS", TestOnly, "executor address used by integration fixtures"),
    v("TEST_PERMIT2", TestOnly, "Permit2 address used by fixtures"),
    v("TEST_PERMIT2_ADDRESS", TestOnly, "Permit2 address, second spelling, used by fixtures"),
    v("TEST_RPC_KEY", TestOnly, "provider key used only by integration fixtures"),
    v("PINTEST_EXECUTOR_CODEHASH", TestOnly, "executor codehash pinned by a deployment test"),
];

/// Lookup used by the coverage test.
pub fn lookup(name: &str) -> Option<&'static LegacyEnvVar> {
    LEGACY_ENV_VARS.iter().find(|e| e.name == name)
}

pub fn count_for(destination: Destination) -> usize {
    LEGACY_ENV_VARS.iter().filter(|e| e.destination == destination).count()
}
