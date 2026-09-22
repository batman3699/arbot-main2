//! Where every legacy `ARBOT_*` variable goes.
//!
//! Blueprint §2.4 forbids late configuration lookup on the dispatch path, and
//! this repository reads 84 distinct `ARBOT_*` variables at call sites
//! throughout the hot path. Retiring them is not one change -- each belongs to
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
];

/// Lookup used by the coverage test.
pub fn lookup(name: &str) -> Option<&'static LegacyEnvVar> {
    LEGACY_ENV_VARS.iter().find(|e| e.name == name)
}

pub fn count_for(destination: Destination) -> usize {
    LEGACY_ENV_VARS.iter().filter(|e| e.destination == destination).count()
}
