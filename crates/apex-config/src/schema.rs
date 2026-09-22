//! The `ops/inputs.yaml` schema, written fresh.
//!
//! Not moved from `ops_inputs.rs`. That module imports `crate::util`, and
//! `registry.rs`/`config_validation.rs` import `venues` (5,646 LOC), `chain` and
//! `bridge` -- all of which stay in `arb-exec-legacy`, which itself must depend
//! on this crate. Moving them would make `apex-config` depend back on the legacy
//! crate and Cargo would reject the cycle.
//!
//! General rule this establishes for the migration: never move a module with
//! upward dependencies. Build its replacement fresh and adapt the old one to
//! feed it, then differential the two (Task 0.4a).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RawConfig {
    pub chains: Vec<RawChain>,
    pub universe: RawUniverse,
    pub risk: RawRisk,
    #[serde(default)]
    pub features: serde_yaml::Value,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RawChain {
    pub chain_name: String,
    pub chain_id: u64,
    pub env_prefix: String,
    #[serde(default)]
    pub rpc_http_urls: Vec<String>,
    #[serde(default)]
    pub rpc_ws_urls: Vec<String>,
    #[serde(default)]
    pub gas_model: Option<String>,
    #[serde(default)]
    pub executor_address: Option<String>,
    #[serde(default)]
    pub executor_owner: Option<String>,
    #[serde(default)]
    pub permit2_address: Option<String>,
    #[serde(default)]
    pub venues: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub flashloans: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub broadcast: Option<RawBroadcast>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RawBroadcast {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub private_relays: Vec<String>,
    #[serde(default)]
    pub private_method_policy: Option<String>,
    #[serde(default)]
    pub public_mempool_jitter_bps: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RawUniverse {
    pub min_pool_liquidity_usd: u64,
    pub max_hops: u8,
    #[serde(default)]
    pub pair_prune_min_liquidity_usd: Option<u64>,
    #[serde(default)]
    pub max_hot_pools_per_chain_per_venue: Option<u32>,
    #[serde(default)]
    pub max_edges_hot: Option<u32>,
    #[serde(default)]
    pub topk_per_token: Option<u32>,
    #[serde(default)]
    pub event_sampling_rate: Option<f64>,
    #[serde(default)]
    pub hub_tokens: Vec<String>,
    #[serde(default)]
    pub token_blacklist: Vec<String>,
    /// §Chain-port note: this list is global and Base-only today. Kept as-is
    /// here; making it per-chain is Phase 15 work, not a config rename.
    #[serde(default)]
    pub token_seeds: Vec<serde_yaml::Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RawRisk {
    pub per_chain: Vec<RawRiskChain>,
}

/// Mirrors what `risk_policy.rs` actually enforces on the trading path. That
/// module exists because `risk.per_chain` was once parsed and never consulted --
/// "safety theater" in its own words -- so every field here has a consumer.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RawRiskChain {
    pub chain_name: String,
    #[serde(default)]
    pub min_net_profit_usd: Option<f64>,
    #[serde(default)]
    pub min_net_profit_native: Option<f64>,
    #[serde(default)]
    pub max_gas_units_per_tx: Option<u64>,
    /// Wei, as a STRING.
    ///
    /// Not a numeric type: wei values routinely exceed `u64::MAX`, and
    /// `serde_yaml` 0.9's untyped `Value` cannot represent one. The production
    /// file already carries such a value (`ethereum: 21000000000000000000`),
    /// which is why the legacy loader declares this `Option<String>` too and
    /// defers to `U256` parsing in `risk_policy.rs`. Keeping the same
    /// representation is also what lets Task 0.4a differential the two planes.
    #[serde(default)]
    pub max_fee_per_gas_cap: Option<String>,
    #[serde(default)]
    pub max_slippage_bps: Option<u32>,
    #[serde(default)]
    pub max_price_impact_bps: Option<u32>,
    #[serde(default)]
    pub must_simulate_before_send: Option<bool>,
    #[serde(default)]
    pub revert_penalty_model: Option<serde_yaml::Value>,
}
