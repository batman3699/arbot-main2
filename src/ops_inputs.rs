#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use ethers::types::Address;
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct OpsInputs {
    #[serde(default)]
    pub chains: Vec<ChainInputs>,
    #[serde(default)]
    pub universe: UniverseConfig,
    #[serde(default)]
    pub risk: RiskConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
}

#[derive(Debug, Clone)]
pub struct OpsChainOverrides {
    pub chain_name: String,
    pub chain_id: Option<u64>,
    pub env_prefix: Option<String>,
    pub rpc_http_urls: Vec<String>,
    pub rpc_ws_urls: Vec<String>,
    pub univ3_quoter: Option<String>,
    pub univ3_factory: Option<String>,
    pub univ3_router: Option<String>,
    pub balancer_vault: Option<String>,
    pub aave_pool: Option<String>,
    pub bal_flashloan_tokens: Vec<String>,
    pub aave_flashloan_tokens: Vec<String>,
    pub erc3156_flashloan_tokens: Vec<String>,
    pub univ2_flashloan_tokens: Vec<String>,
    pub univ3_flashloan_tokens: Vec<String>,
    pub executor_address: Option<String>,
    pub executor_owner: Option<String>,
    pub permit2_address: Option<String>,
    pub broadcast_mode: Option<BroadcastMode>,
    pub broadcast_private_relays: Vec<String>,
    pub broadcast_private_method_policy: Option<PrivateRelayMethodPolicy>,
    pub broadcast_public_jitter_bps: Option<u32>,
    pub health: Option<HealthConfig>,
    pub gas_model: Option<GasModel>,
    pub gas_rpc_method: Option<String>,
    pub arbitrum_l1_per_byte_wei: Option<String>,
    pub arbitrum_l1_per_byte_max_deviation_bps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AavePoolProbeTarget {
    pub source: String,
    pub pool: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChainInputs {
    #[serde(default)]
    pub chain_name: String,
    #[serde(default)]
    pub chain_id: u64,
    #[serde(default)]
    pub env_prefix: String,
    #[serde(default)]
    pub rpc_http_urls: Vec<String>,
    #[serde(default)]
    pub rpc_ws_urls: Vec<String>,
    #[serde(default)]
    pub gas_model: Option<GasModel>,
    #[serde(default)]
    pub gas_rpc_method: Option<String>,
    #[serde(default)]
    pub arbitrum_l1_per_byte_wei: Option<String>,
    #[serde(default)]
    pub arbitrum_l1_per_byte_max_deviation_bps: Option<u32>,
    #[serde(default)]
    pub executor_address: String,
    #[serde(default)]
    pub executor_owner: Option<String>,
    #[serde(default)]
    pub permit2_address: String,
    #[serde(default)]
    pub venues: Vec<VenueConfig>,
    #[serde(default)]
    pub flashloans: Vec<FlashloanConfig>,
    #[serde(default)]
    pub broadcast: Option<BroadcastConfig>,
    #[serde(default)]
    pub health: Option<HealthConfig>,
    #[serde(default, flatten)]
    pub extras: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GasModel {
    #[serde(rename = "eip1559")]
    Eip1559,
    #[serde(rename = "legacy")]
    Legacy,
    #[serde(rename = "arbitrum")]
    Arbitrum,
    #[serde(rename = "op_stack")]
    OpStack,
    #[serde(rename = "linea_estimateGas")]
    LineaEstimateGas,
    #[serde(rename = "custom_rpc_method")]
    CustomRpcMethod,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct VenueConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: Option<VenueKind>,
    #[serde(default)]
    pub factory: Option<String>,
    #[serde(default)]
    pub router: Option<String>,
    #[serde(default)]
    pub quoter: Option<String>,
    #[serde(default)]
    pub pool_manager: Option<String>,
    #[serde(default)]
    pub vault: Option<String>,
    #[serde(default)]
    pub registry: Option<String>,
    #[serde(default)]
    pub fee_tiers: Option<Vec<u32>>,
    #[serde(default)]
    pub fee_bps: Option<u32>,
    #[serde(default)]
    pub pool_init_code_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VenueKind {
    Univ2Like,
    Univ3Like,
    Univ4,
    CurveLike,
    BalancerLike,
    SolidlyV2Like,
    GenericRouter,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FlashloanConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<FlashloanKind>,
    #[serde(default)]
    pub pool: Option<String>,
    #[serde(default)]
    pub vault: Option<String>,
    #[serde(default)]
    pub lender: Option<String>,
    #[serde(default)]
    pub fee_bps: Option<u32>,
    #[serde(default)]
    pub factory: Option<String>,
    #[serde(default)]
    pub max_loan_assets: Vec<String>,
    #[serde(default)]
    pub allowlist_tokens: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlashloanKind {
    AaveV3Like,
    BalancerVaultLike,
    Erc3156Like,
    Univ2Flashswap,
    Univ3Flash,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BroadcastConfig {
    #[serde(default)]
    pub mode: Option<BroadcastMode>,
    #[serde(default)]
    pub private_relays: Vec<String>,
    #[serde(default)]
    pub private_method_policy: Option<PrivateRelayMethodPolicy>,
    #[serde(default)]
    pub public_mempool_jitter_bps: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateRelayMethodPolicy {
    BundleOnly,
    BundleThenPrivateRaw,
}

impl PrivateRelayMethodPolicy {
    pub fn allows_private_raw_fallback(&self) -> bool {
        matches!(self, Self::BundleThenPrivateRaw)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HealthConfig {
    #[serde(default)]
    pub ema_alpha: Option<f64>,
    #[serde(default)]
    pub max_reject_rate_ema: Option<f64>,
    #[serde(default)]
    pub max_latency_ms_ema: Option<f64>,
    #[serde(default)]
    pub min_success_rate_ema: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BroadcastMode {
    Public,
    Private,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UniverseConfig {
    #[serde(default)]
    pub token_seeds: Vec<TokenSeed>,
    #[serde(default)]
    pub token_blacklist: Vec<String>,
    #[serde(default)]
    pub hub_tokens: Vec<String>,
    #[serde(default)]
    pub min_pool_liquidity_usd: Option<f64>,
    #[serde(default)]
    pub min_pool_liquidity_native: Option<f64>,
    #[serde(default)]
    pub min_pool_liquidity_tokens: Option<f64>,
    #[serde(default)]
    pub max_hot_pools_per_chain_per_venue: Option<usize>,
    #[serde(default)]
    pub max_cold_pools_stored: Option<usize>,
    #[serde(default)]
    pub max_edges_hot: Option<usize>,
    #[serde(default)]
    pub edge_prune_min_score: Option<f64>,
    #[serde(default)]
    pub edge_prune_max_slippage_bps: Option<u32>,
    #[serde(default)]
    pub edge_prune_liquidity_weight: Option<f64>,
    #[serde(default)]
    pub edge_prune_profit_weight: Option<f64>,
    #[serde(default)]
    pub edge_prune_slippage_weight: Option<f64>,
    #[serde(default)]
    pub topk_per_token: Option<usize>,
    #[serde(default)]
    pub max_hops: Option<usize>,
    #[serde(default)]
    pub cycle_candidate_cap_per_block: Option<usize>,
    #[serde(default)]
    pub time_budget_ms: TimeBudgetMs,
    #[serde(default)]
    pub event_sampling_rate: Option<f64>,
    #[serde(default)]
    pub event_sampling_block_window: Option<u64>,
    #[serde(default)]
    pub hot_pool_refresh_secs: Option<u64>,
    #[serde(default)]
    pub dynamic_top_tokens_30d: Option<usize>,
    #[serde(default)]
    pub include_usd_stablecoins: Option<bool>,
    #[serde(default)]
    pub include_weth: Option<bool>,
    #[serde(default)]
    pub include_wbtc: Option<bool>,
    #[serde(default)]
    pub pair_prune_min_liquidity_usd: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TokenSeed {
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub decimals: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TimeBudgetMs {
    #[serde(default)]
    pub search: Option<u64>,
    #[serde(default, alias = "quote")]
    pub quoting: Option<u64>,
    #[serde(default)]
    pub simulation: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RiskConfig {
    #[serde(default)]
    pub per_chain: Vec<RiskChainConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RiskChainConfig {
    #[serde(default)]
    pub chain_name: String,
    #[serde(default)]
    pub min_net_profit_usd: Option<f64>,
    #[serde(default)]
    pub min_net_profit_native: Option<f64>,
    #[serde(default)]
    pub max_gas_units_per_tx: Option<u64>,
    #[serde(default)]
    pub max_fee_per_gas_cap: Option<String>,
    #[serde(default)]
    pub max_slippage_bps: Option<u32>,
    #[serde(default)]
    pub max_price_impact_bps: Option<u32>,
    #[serde(default)]
    pub must_simulate_before_send: Option<bool>,
    #[serde(default)]
    pub revert_penalty_model: Option<RevertPenaltyModel>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertPenaltyKind {
    Flat,
    Linear,
    Exponential,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RevertPenaltyModel {
    #[serde(default)]
    pub kind: Option<RevertPenaltyKind>,
    #[serde(default)]
    pub flat_bps: Option<u32>,
    #[serde(default)]
    pub linear_base_bps: Option<u32>,
    #[serde(default)]
    pub linear_per_gas_bps: Option<u32>,
    #[serde(default)]
    pub exponential_base_bps: Option<u32>,
    #[serde(default)]
    pub exponential_factor: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub enable_backrun: Vec<String>,
    #[serde(default)]
    pub enable_liquidations: Vec<String>,
    #[serde(default)]
    pub liquidation_markets: Vec<LiquidationMarketsConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LiquidationMarketsConfig {
    #[serde(default)]
    pub chain_name: String,
    #[serde(default)]
    pub aave_v3: Option<AaveV3Addresses>,
    #[serde(default)]
    pub compound_v3: Option<CompoundV3Addresses>,
    #[serde(default)]
    pub markets: Vec<LiquidationMarketSpec>,
    #[serde(default)]
    pub candidate_ttl_secs: Option<u64>,
    #[serde(default)]
    pub candidate_min_score: Option<u32>,
    #[serde(default)]
    pub candidate_max: Option<usize>,
    #[serde(default)]
    pub revert_threshold: Option<u32>,
    #[serde(default)]
    pub revert_cooldown_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AaveV3Addresses {
    #[serde(default)]
    pub pool: String,
    #[serde(default)]
    pub data_provider: String,
    #[serde(default)]
    pub price_oracle: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CompoundV3Addresses {
    #[serde(default)]
    pub comet: String,
    #[serde(default)]
    pub rewards: String,
    #[serde(default)]
    pub configurator: String,
    #[serde(default)]
    pub oracle: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiquidationMarketKind {
    Aave,
    Compound,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LiquidationMarketSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<LiquidationMarketKind>,
    #[serde(default)]
    pub adapter: String,
    #[serde(default)]
    pub flash_loan_pool: String,
    #[serde(default)]
    pub debt_token: String,
    #[serde(default)]
    pub collateral_token: String,
    #[serde(default)]
    pub bonus_bps: Option<u32>,
    #[serde(default)]
    pub collateral_exchange_rate_bps: Option<u32>,
    #[serde(default)]
    pub estimated_gas: Option<u64>,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub receive_atoken: Option<bool>,
    #[serde(default)]
    pub max_repay_wei: Option<String>,
}

pub fn load_ops_inputs(path: impl AsRef<Path>) -> Result<OpsInputs> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let expanded = expand_env_vars(&raw);
    let inputs = parse_ops_inputs(&expanded)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let strict = ops_inputs_strict();
    let active_chains = active_chain_names_from_env();
    inputs.validate(path, strict, active_chains.as_deref())?;
    Ok(inputs)
}

/// Chains listed in CHAIN_LIST / CHAIN; when set, strict validation applies only to these targets.
fn active_chain_names_from_env() -> Option<Vec<String>> {
    let mut targets: Vec<String> = std::env::var("CHAIN_LIST")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(|chain| chain.trim().to_ascii_lowercase())
                .filter(|chain| !chain.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if targets.is_empty() {
        if let Some(chain) = std::env::var("CHAIN")
            .ok()
            .map(|chain| chain.trim().to_ascii_lowercase())
            .filter(|chain| !chain.is_empty())
        {
            targets.push(chain);
        }
    }

    if targets.is_empty() {
        None
    } else {
        Some(targets)
    }
}

pub fn parse_ops_inputs(raw: &str) -> Result<OpsInputs> {
    serde_yaml::from_str(raw).context("parse ops inputs yaml")
}

fn expand_env_vars(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && matches!(chars.peek(), Some('{')) {
            chars.next();
            let mut key = String::new();
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
                key.push(next);
            }

            if key.is_empty() {
                result.push_str("${}");
                continue;
            }

            match std::env::var(&key) {
                Ok(value) => result.push_str(&value),
                Err(_) => {
                    result.push_str("${");
                    result.push_str(&key);
                    result.push('}');
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

impl OpsInputs {
    pub fn chain_inputs(&self, chain_name: &str) -> Option<&ChainInputs> {
        self.chains
            .iter()
            .find(|chain| chain.chain_name.eq_ignore_ascii_case(chain_name))
    }

    pub fn venue_inputs(&self, chain_name: &str, venue_name: &str) -> Option<&VenueConfig> {
        let chain = self.chain_inputs(chain_name)?;
        chain
            .venues
            .iter()
            .find(|venue| venue.name.eq_ignore_ascii_case(venue_name))
    }

    pub fn chain_overrides(&self, chain_name: &str) -> Option<OpsChainOverrides> {
        let chain = self
            .chains
            .iter()
            .find(|chain| chain.chain_name.eq_ignore_ascii_case(chain_name))?;
        let executor_address_override =
            prefixed_extra_value(chain, "EXECUTOR_ADDRESS").or_else(|| {
                if is_missing_value(&chain.executor_address) {
                    None
                } else {
                    Some(chain.executor_address.clone())
                }
            });
        let executor_owner_override = prefixed_extra_value(chain, "EXECUTOR_OWNER").or_else(|| {
            chain
                .executor_owner
                .as_ref()
                .filter(|value| !is_missing_value(value))
                .cloned()
        });
        let permit2_override = prefixed_extra_value(chain, "PERMIT2_ADDRESS").or_else(|| {
            if is_missing_value(&chain.permit2_address) {
                None
            } else {
                Some(chain.permit2_address.clone())
            }
        });

        let univ3 = chain
            .venues
            .iter()
            .find(|venue| matches!(venue.kind, Some(VenueKind::Univ3Like)));
        let balancer_venue = chain
            .venues
            .iter()
            .find(|venue| matches!(venue.kind, Some(VenueKind::BalancerLike)));
        let aave = chain
            .flashloans
            .iter()
            .find(|loan| matches!(loan.kind, Some(FlashloanKind::AaveV3Like)));
        let balancer_loan = chain
            .flashloans
            .iter()
            .find(|loan| matches!(loan.kind, Some(FlashloanKind::BalancerVaultLike)));
        let erc3156 = chain
            .flashloans
            .iter()
            .find(|loan| matches!(loan.kind, Some(FlashloanKind::Erc3156Like)));
        let univ2 = chain
            .flashloans
            .iter()
            .find(|loan| matches!(loan.kind, Some(FlashloanKind::Univ2Flashswap)));
        let univ3_loan = chain
            .flashloans
            .iter()
            .find(|loan| matches!(loan.kind, Some(FlashloanKind::Univ3Flash)));

        Some(OpsChainOverrides {
            chain_name: chain.chain_name.clone(),
            chain_id: if chain.chain_id == 0 {
                None
            } else {
                Some(chain.chain_id)
            },
            env_prefix: if chain.env_prefix.trim().is_empty() {
                None
            } else {
                Some(chain.env_prefix.clone())
            },
            rpc_http_urls: chain.rpc_http_urls.clone(),
            rpc_ws_urls: chain.rpc_ws_urls.clone(),
            univ3_quoter: univ3.and_then(|venue| venue.quoter.clone()),
            univ3_factory: univ3.and_then(|venue| venue.factory.clone()),
            univ3_router: univ3.and_then(|venue| venue.router.clone()),
            balancer_vault: balancer_loan
                .and_then(|loan| loan.vault.clone())
                .or_else(|| balancer_venue.and_then(|venue| venue.vault.clone())),
            aave_pool: aave.and_then(|loan| loan.pool.clone()),
            bal_flashloan_tokens: balancer_loan
                .map(|loan| loan.allowlist_tokens.clone())
                .unwrap_or_default(),
            aave_flashloan_tokens: aave
                .map(|loan| loan.allowlist_tokens.clone())
                .unwrap_or_default(),
            erc3156_flashloan_tokens: erc3156
                .map(|loan| loan.allowlist_tokens.clone())
                .unwrap_or_default(),
            univ2_flashloan_tokens: univ2
                .map(|loan| loan.allowlist_tokens.clone())
                .unwrap_or_default(),
            univ3_flashloan_tokens: univ3_loan
                .map(|loan| loan.allowlist_tokens.clone())
                .unwrap_or_default(),
            executor_address: executor_address_override,
            executor_owner: executor_owner_override,
            permit2_address: permit2_override,
            broadcast_mode: chain.broadcast.as_ref().and_then(|cfg| cfg.mode.clone()),
            broadcast_private_relays: chain
                .broadcast
                .as_ref()
                .map(|cfg| cfg.private_relays.clone())
                .unwrap_or_default(),
            broadcast_private_method_policy: chain
                .broadcast
                .as_ref()
                .and_then(|cfg| cfg.private_method_policy.clone()),
            broadcast_public_jitter_bps: chain
                .broadcast
                .as_ref()
                .and_then(|cfg| cfg.public_mempool_jitter_bps),
            health: chain.health.clone(),
            gas_model: chain.gas_model.clone(),
            gas_rpc_method: chain
                .gas_rpc_method
                .as_ref()
                .filter(|value| !is_missing_value(value))
                .cloned(),
            arbitrum_l1_per_byte_wei: chain
                .arbitrum_l1_per_byte_wei
                .as_ref()
                .filter(|value| !is_missing_value(value))
                .cloned(),
            arbitrum_l1_per_byte_max_deviation_bps: chain.arbitrum_l1_per_byte_max_deviation_bps,
        })
    }

    pub fn liquidations_enabled_for(&self, chain_name: &str) -> bool {
        self.features
            .enable_liquidations
            .iter()
            .any(|name| name.eq_ignore_ascii_case(chain_name))
    }

    pub fn liquidation_markets_for(&self, chain_name: &str) -> Option<&LiquidationMarketsConfig> {
        self.features
            .liquidation_markets
            .iter()
            .find(|entry| entry.chain_name.eq_ignore_ascii_case(chain_name))
    }

    /// Fail closed when liquidations are enabled but scaffold placeholder addresses remain.
    pub fn validate_liquidation_safety(&self, feature_liquidations_enabled: bool) -> Result<()> {
        let mut violations = Vec::new();
        let mut record = |path: &str, address: &str| {
            if !address.trim().is_empty() && liquidation_address_is_known_placeholder(address) {
                violations.push(format!("{path}={address}"));
            }
        };

        for entry in &self.features.liquidation_markets {
            if !self.liquidations_enabled_for(&entry.chain_name) {
                continue;
            }
            let chain = entry.chain_name.as_str();
            if let Some(aave) = entry.aave_v3.as_ref() {
                record(
                    &format!("features.liquidation_markets[{chain}].aave_v3.pool"),
                    &aave.pool,
                );
                record(
                    &format!("features.liquidation_markets[{chain}].aave_v3.data_provider"),
                    &aave.data_provider,
                );
                record(
                    &format!("features.liquidation_markets[{chain}].aave_v3.price_oracle"),
                    &aave.price_oracle,
                );
            }
            if let Some(compound) = entry.compound_v3.as_ref() {
                record(
                    &format!("features.liquidation_markets[{chain}].compound_v3.comet"),
                    &compound.comet,
                );
                record(
                    &format!("features.liquidation_markets[{chain}].compound_v3.rewards"),
                    &compound.rewards,
                );
                record(
                    &format!("features.liquidation_markets[{chain}].compound_v3.configurator"),
                    &compound.configurator,
                );
                record(
                    &format!("features.liquidation_markets[{chain}].compound_v3.oracle"),
                    &compound.oracle,
                );
            }
            for (idx, market) in entry.markets.iter().enumerate() {
                let prefix = format!("features.liquidation_markets[{chain}].markets[{idx}]");
                record(&format!("{prefix}.adapter"), &market.adapter);
                record(&format!("{prefix}.flash_loan_pool"), &market.flash_loan_pool);
                record(&format!("{prefix}.debt_token"), &market.debt_token);
                record(
                    &format!("{prefix}.collateral_token"),
                    &market.collateral_token,
                );
            }
        }

        if violations.is_empty() {
            return Ok(());
        }

        if feature_liquidations_enabled {
            return Err(anyhow!(
                "FEATURE_LIQUIDATIONS is enabled but liquidation config contains known placeholder addresses: {}",
                violations.join(", ")
            ));
        }

        Ok(())
    }

    pub fn configured_aave_pool_probe_targets(&self, chain_name: &str) -> Vec<AavePoolProbeTarget> {
        let mut seen = HashSet::new();
        let mut targets = Vec::new();

        if let Some(chain) = self
            .chains
            .iter()
            .find(|chain| chain.chain_name.eq_ignore_ascii_case(chain_name))
        {
            for (idx, flashloan) in chain.flashloans.iter().enumerate() {
                if !matches!(flashloan.kind, Some(FlashloanKind::AaveV3Like)) {
                    continue;
                }
                if let Some(pool) = flashloan
                    .pool
                    .as_ref()
                    .filter(|pool| !is_missing_value(pool))
                {
                    let normalized = pool.to_ascii_lowercase();
                    if seen.insert(normalized) {
                        targets.push(AavePoolProbeTarget {
                            source: format!("chains[{}].flashloans[{idx}].pool", chain.chain_name),
                            pool: pool.clone(),
                        });
                    }
                }
            }
        }

        if self.liquidations_enabled_for(chain_name) {
            let Some(liquidation) = self.liquidation_markets_for(chain_name) else {
                return targets;
            };

            if let Some(aave) = liquidation.aave_v3.as_ref() {
                if !is_missing_value(&aave.pool) {
                    let normalized = aave.pool.to_ascii_lowercase();
                    if seen.insert(normalized) {
                        targets.push(AavePoolProbeTarget {
                            source: format!(
                                "features.liquidation_markets[{}].aave_v3.pool",
                                liquidation.chain_name
                            ),
                            pool: aave.pool.clone(),
                        });
                    }
                }
            }
            for (idx, market) in liquidation.markets.iter().enumerate() {
                if matches!(market.kind, Some(LiquidationMarketKind::Aave))
                    && !is_missing_value(&market.flash_loan_pool)
                {
                    let normalized = market.flash_loan_pool.to_ascii_lowercase();
                    if seen.insert(normalized) {
                        targets.push(AavePoolProbeTarget {
                            source: format!(
                                "features.liquidation_markets[{}].markets[{idx}].flash_loan_pool",
                                liquidation.chain_name
                            ),
                            pool: market.flash_loan_pool.clone(),
                        });
                    }
                }
            }
        }

        targets
    }

    fn validate(&self, path: &Path, strict: bool, active_chains: Option<&[String]>) -> Result<()> {
        let mut missing = Vec::new();
        let mut chain_name_map = HashSet::new();
        let mut chain_id_map = HashSet::new();
        let mut env_prefix_map = HashSet::new();

        if self.chains.is_empty() {
            push_missing(path, "chains", &mut missing, strict);
        }

        for (idx, chain) in self.chains.iter().enumerate() {
            if let Some(active) = active_chains {
                if !active
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&chain.chain_name))
                {
                    continue;
                }
            }

            let chain_key = chain_key(idx, &chain.chain_name);

            if is_missing_value(&chain.chain_name) {
                push_missing(
                    path,
                    &format!("{chain_key}.chain_name"),
                    &mut missing,
                    strict,
                );
            } else if !chain_name_map.insert(chain.chain_name.clone()) {
                return Err(anyhow!(
                    "duplicate chain_name `{}` in {}",
                    chain.chain_name,
                    path.display()
                ));
            }

            if chain.chain_id == 0 {
                push_missing(path, &format!("{chain_key}.chain_id"), &mut missing, strict);
            } else if !chain_id_map.insert(chain.chain_id) {
                return Err(anyhow!(
                    "duplicate chain_id {} in {}",
                    chain.chain_id,
                    path.display()
                ));
            }

            if is_missing_value(&chain.env_prefix) {
                push_missing(
                    path,
                    &format!("{chain_key}.env_prefix"),
                    &mut missing,
                    strict,
                );
            } else if !env_prefix_map.insert(chain.env_prefix.clone()) {
                return Err(anyhow!(
                    "duplicate env_prefix `{}` in {}",
                    chain.env_prefix,
                    path.display()
                ));
            }

            if chain.rpc_http_urls.is_empty() {
                push_missing(
                    path,
                    &format!("{chain_key}.rpc_http_urls"),
                    &mut missing,
                    strict,
                );
            } else {
                for (idx, url) in chain.rpc_http_urls.iter().enumerate() {
                    if is_missing_value(url) {
                        push_missing(
                            path,
                            &format!("{chain_key}.rpc_http_urls[{idx}]"),
                            &mut missing,
                            strict,
                        );
                    } else if !url.starts_with("http://") && !url.starts_with("https://") {
                        return Err(anyhow!(
                            "{} rpc_http_urls[{idx}] must start with http(s)://",
                            chain_key
                        ));
                    }
                }
            }

            if chain.rpc_ws_urls.is_empty() {
                push_missing(
                    path,
                    &format!("{chain_key}.rpc_ws_urls"),
                    &mut missing,
                    strict,
                );
            } else {
                for (idx, url) in chain.rpc_ws_urls.iter().enumerate() {
                    if is_missing_value(url) {
                        push_missing(
                            path,
                            &format!("{chain_key}.rpc_ws_urls[{idx}]"),
                            &mut missing,
                            strict,
                        );
                    } else if !url.starts_with("ws://") && !url.starts_with("wss://") {
                        return Err(anyhow!(
                            "{} rpc_ws_urls[{idx}] must start with ws(s)://",
                            chain_key
                        ));
                    }
                }
            }

            match &chain.gas_model {
                Some(GasModel::CustomRpcMethod) => {
                    if chain
                        .gas_rpc_method
                        .as_ref()
                        .map(|value| is_missing_value(value))
                        .unwrap_or(true)
                    {
                        push_missing(
                            path,
                            &format!("{chain_key}.gas_rpc_method"),
                            &mut missing,
                            strict,
                        );
                    }
                }
                Some(_) => {}
                None => {
                    push_missing(
                        path,
                        &format!("{chain_key}.gas_model"),
                        &mut missing,
                        strict,
                    );
                }
            }
            if matches!(chain.gas_model, Some(GasModel::Arbitrum)) {
                if let Some(value) = chain.arbitrum_l1_per_byte_wei.as_ref() {
                    if is_missing_value(value) {
                        push_missing(
                            path,
                            &format!("{chain_key}.arbitrum_l1_per_byte_wei"),
                            &mut missing,
                            strict,
                        );
                    } else {
                        parse_u256(value, &format!("{chain_key}.arbitrum_l1_per_byte_wei"))?;
                    }
                }
                if let Some(bps) = chain.arbitrum_l1_per_byte_max_deviation_bps {
                    if bps > 10_000 {
                        return Err(anyhow!(
                            "{chain_key}.arbitrum_l1_per_byte_max_deviation_bps must be <= 10000"
                        ));
                    }
                }
            }

            if !is_missing_value(&chain.executor_address) {
                parse_address(
                    &chain.executor_address,
                    &format!("{chain_key}.executor_address"),
                )?;
            } else if let Some(value) = prefixed_extra_value(chain, "EXECUTOR_ADDRESS") {
                parse_address(&value, &format!("{chain_key}.executor_address"))?;
            } else {
                push_missing(
                    path,
                    &format!("{chain_key}.executor_address"),
                    &mut missing,
                    strict,
                );
            }

            if let Some(owner) = chain.executor_owner.as_ref() {
                if is_missing_value(owner) {
                    push_missing(
                        path,
                        &format!("{chain_key}.executor_owner"),
                        &mut missing,
                        strict,
                    );
                } else {
                    parse_address(owner, &format!("{chain_key}.executor_owner"))?;
                }
            } else if let Some(owner) = prefixed_extra_value(chain, "EXECUTOR_OWNER") {
                parse_address(&owner, &format!("{chain_key}.executor_owner"))?;
            }

            if !is_missing_value(&chain.permit2_address) {
                parse_address(
                    &chain.permit2_address,
                    &format!("{chain_key}.permit2_address"),
                )?;
            } else if let Some(value) = prefixed_extra_value(chain, "PERMIT2_ADDRESS") {
                parse_address(&value, &format!("{chain_key}.permit2_address"))?;
            } else {
                push_missing(
                    path,
                    &format!("{chain_key}.permit2_address"),
                    &mut missing,
                    strict,
                );
            }

            if chain.venues.is_empty() {
                push_missing(path, &format!("{chain_key}.venues"), &mut missing, strict);
            }

            for (venue_idx, venue) in chain.venues.iter().enumerate() {
                let venue_key = format!("{chain_key}.venues[{venue_idx}]");
                if is_missing_value(&venue.name) {
                    push_missing(path, &format!("{venue_key}.name"), &mut missing, strict);
                }
                let kind = match &venue.kind {
                    Some(kind) => kind,
                    None => {
                        push_missing(path, &format!("{venue_key}.kind"), &mut missing, strict);
                        continue;
                    }
                };
                match kind {
                    VenueKind::Univ2Like => {
                        require_address(
                            path,
                            &venue_key,
                            "factory",
                            venue.factory.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        require_address(
                            path,
                            &venue_key,
                            "router",
                            venue.router.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        require_fee_bps(
                            path,
                            &venue_key,
                            "fee_bps",
                            venue.fee_bps,
                            &mut missing,
                            strict,
                        )?;
                        require_init_code_hash(
                            path,
                            &venue_key,
                            venue.pool_init_code_hash.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    VenueKind::Univ3Like => {
                        require_address(
                            path,
                            &venue_key,
                            "factory",
                            venue.factory.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        require_address(
                            path,
                            &venue_key,
                            "router",
                            venue.router.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        require_address(
                            path,
                            &venue_key,
                            "quoter",
                            venue.quoter.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        let tiers = venue.fee_tiers.as_ref().filter(|tiers| !tiers.is_empty());
                        if tiers.is_none() {
                            push_missing(
                                path,
                                &format!("{venue_key}.fee_tiers"),
                                &mut missing,
                                strict,
                            );
                        }
                    }
                    VenueKind::Univ4 => {
                        require_address(
                            path,
                            &venue_key,
                            "pool_manager",
                            venue.pool_manager.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    VenueKind::CurveLike => {
                        require_address(
                            path,
                            &venue_key,
                            "registry",
                            venue.registry.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    VenueKind::BalancerLike => {
                        require_address(
                            path,
                            &venue_key,
                            "vault",
                            venue.vault.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    VenueKind::SolidlyV2Like => {
                        require_address(
                            path,
                            &venue_key,
                            "factory",
                            venue.factory.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        require_address(
                            path,
                            &venue_key,
                            "router",
                            venue.router.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        require_fee_bps(
                            path,
                            &venue_key,
                            "fee_bps",
                            venue.fee_bps,
                            &mut missing,
                            strict,
                        )?;
                        require_init_code_hash(
                            path,
                            &venue_key,
                            venue.pool_init_code_hash.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    VenueKind::GenericRouter => {
                        require_address(
                            path,
                            &venue_key,
                            "router",
                            venue.router.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                }
            }

            if chain.flashloans.is_empty() {
                push_missing(
                    path,
                    &format!("{chain_key}.flashloans"),
                    &mut missing,
                    strict,
                );
            }

            for (fl_idx, fl) in chain.flashloans.iter().enumerate() {
                let fl_key = format!("{chain_key}.flashloans[{fl_idx}]");
                let kind = match &fl.kind {
                    Some(kind) => kind,
                    None => {
                        push_missing(path, &format!("{fl_key}.kind"), &mut missing, strict);
                        continue;
                    }
                };

                match kind {
                    FlashloanKind::AaveV3Like => {
                        require_address(
                            path,
                            &fl_key,
                            "pool",
                            fl.pool.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    FlashloanKind::BalancerVaultLike => {
                        require_address(
                            path,
                            &fl_key,
                            "vault",
                            fl.vault.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    FlashloanKind::Erc3156Like => {
                        require_address(
                            path,
                            &fl_key,
                            "lender",
                            fl.lender.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                        require_fee_bps(
                            path,
                            &fl_key,
                            "fee_bps",
                            fl.fee_bps,
                            &mut missing,
                            strict,
                        )?;
                    }
                    FlashloanKind::Univ2Flashswap => {
                        require_address(
                            path,
                            &fl_key,
                            "factory",
                            fl.factory.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                    FlashloanKind::Univ3Flash => {
                        require_address(
                            path,
                            &fl_key,
                            "factory",
                            fl.factory.as_ref(),
                            &mut missing,
                            strict,
                        )?;
                    }
                }

                if fl.max_loan_assets.is_empty() {
                    push_missing(
                        path,
                        &format!("{fl_key}.max_loan_assets"),
                        &mut missing,
                        strict,
                    );
                } else {
                    for (asset_idx, asset) in fl.max_loan_assets.iter().enumerate() {
                        if is_missing_value(asset) {
                            push_missing(
                                path,
                                &format!("{fl_key}.max_loan_assets[{asset_idx}]"),
                                &mut missing,
                                strict,
                            );
                        } else {
                            parse_address(
                                asset,
                                &format!("{fl_key}.max_loan_assets[{asset_idx}]"),
                            )?;
                        }
                    }
                }

                for (asset_idx, asset) in fl.allowlist_tokens.iter().enumerate() {
                    if is_missing_value(asset) {
                        push_missing(
                            path,
                            &format!("{fl_key}.allowlist_tokens[{asset_idx}]"),
                            &mut missing,
                            strict,
                        );
                    } else {
                        parse_address(asset, &format!("{fl_key}.allowlist_tokens[{asset_idx}]"))?;
                    }
                }
            }

            if let Some(broadcast) = chain.broadcast.as_ref() {
                if let Some(BroadcastMode::Private) = broadcast.mode {
                    if broadcast.private_relays.is_empty() {
                        push_missing(
                            path,
                            &format!("{chain_key}.broadcast.private_relays"),
                            &mut missing,
                            strict,
                        );
                    } else {
                        for (relay_idx, relay) in broadcast.private_relays.iter().enumerate() {
                            if is_missing_value(relay) {
                                push_missing(
                                    path,
                                    &format!("{chain_key}.broadcast.private_relays[{relay_idx}]"),
                                    &mut missing,
                                    strict,
                                );
                            }
                        }

                        if broadcast.private_method_policy.is_none()
                            && broadcast
                                .private_relays
                                .iter()
                                .any(|relay| is_generic_private_relay_url(relay))
                        {
                            return Err(anyhow!(
                                "{chain_key}.broadcast.private_relays contains generic RPC endpoints; set {chain_key}.broadcast.private_method_policy to a supported policy"
                            ));
                        }
                    }
                }
                if let Some(jitter) = broadcast.public_mempool_jitter_bps {
                    if jitter > 10_000 {
                        return Err(anyhow!(
                            "{chain_key}.broadcast.public_mempool_jitter_bps must be <= 10000"
                        ));
                    }
                }
            }

            if let Some(health) = chain.health.as_ref() {
                validate_health_config(path, &format!("{chain_key}.health"), health)?;
            }
        }

        if self.universe.token_seeds.is_empty() {
            push_missing(path, "universe.token_seeds", &mut missing, strict);
        }
        for (idx, seed) in self.universe.token_seeds.iter().enumerate() {
            let seed_key = format!("universe.token_seeds[{idx}]");
            if is_missing_value(&seed.address) {
                push_missing(path, &format!("{seed_key}.address"), &mut missing, strict);
            } else {
                parse_address(&seed.address, &format!("{seed_key}.address"))?;
            }
            if let Some(decimals) = seed.decimals {
                if decimals > 36 {
                    return Err(anyhow!("{seed_key}.decimals must be <= 36"));
                }
            }
        }
        for (idx, token) in self.universe.token_blacklist.iter().enumerate() {
            if is_missing_value(token) {
                push_missing(
                    path,
                    &format!("universe.token_blacklist[{idx}]"),
                    &mut missing,
                    strict,
                );
            } else {
                parse_address(token, &format!("universe.token_blacklist[{idx}]"))?;
            }
        }
        for (idx, token) in self.universe.hub_tokens.iter().enumerate() {
            if is_missing_value(token) {
                push_missing(
                    path,
                    &format!("universe.hub_tokens[{idx}]"),
                    &mut missing,
                    strict,
                );
            } else {
                parse_address(token, &format!("universe.hub_tokens[{idx}]"))?;
            }
        }

        if self.universe.min_pool_liquidity_usd.is_none()
            && self.universe.min_pool_liquidity_native.is_none()
            && self.universe.min_pool_liquidity_tokens.is_none()
        {
            push_missing(
                path,
                "universe.min_pool_liquidity_usd|min_pool_liquidity_native|min_pool_liquidity_tokens",
                &mut missing,
                strict,
            );
        }
        if let Some(value) = self.universe.min_pool_liquidity_usd {
            ensure_positive(value, "universe.min_pool_liquidity_usd")?;
        }
        if let Some(value) = self.universe.min_pool_liquidity_native {
            ensure_positive(value, "universe.min_pool_liquidity_native")?;
        }
        if let Some(value) = self.universe.min_pool_liquidity_tokens {
            ensure_positive(value, "universe.min_pool_liquidity_tokens")?;
        }
        if let Some(value) = self.universe.edge_prune_min_score {
            if value < 0.0 {
                return Err(anyhow!(
                    "universe.edge_prune_min_score must be >= 0 (got {value})"
                ));
            }
        }
        if let Some(value) = self.universe.edge_prune_max_slippage_bps {
            if value > 10_000 {
                return Err(anyhow!(
                    "universe.edge_prune_max_slippage_bps must be <= 10_000 (got {value})"
                ));
            }
        }
        if let Some(value) = self.universe.edge_prune_liquidity_weight {
            if value < 0.0 {
                return Err(anyhow!(
                    "universe.edge_prune_liquidity_weight must be >= 0 (got {value})"
                ));
            }
        }
        if let Some(value) = self.universe.edge_prune_profit_weight {
            if value < 0.0 {
                return Err(anyhow!(
                    "universe.edge_prune_profit_weight must be >= 0 (got {value})"
                ));
            }
        }
        if let Some(value) = self.universe.edge_prune_slippage_weight {
            if value < 0.0 {
                return Err(anyhow!(
                    "universe.edge_prune_slippage_weight must be >= 0 (got {value})"
                ));
            }
        }

        require_usize(
            path,
            "universe.max_hot_pools_per_chain_per_venue",
            self.universe.max_hot_pools_per_chain_per_venue,
            &mut missing,
            strict,
        )?;
        require_usize(
            path,
            "universe.max_cold_pools_stored",
            self.universe.max_cold_pools_stored,
            &mut missing,
            strict,
        )?;
        require_usize(
            path,
            "universe.max_edges_hot",
            self.universe.max_edges_hot,
            &mut missing,
            strict,
        )?;
        require_usize(
            path,
            "universe.topk_per_token",
            self.universe.topk_per_token,
            &mut missing,
            strict,
        )?;
        if let Some(max_hops) = self.universe.max_hops {
            if max_hops != 6 {
                return Err(anyhow!("universe.max_hops must be set to 6"));
            }
        } else {
            push_missing(path, "universe.max_hops", &mut missing, strict);
        }
        require_usize(
            path,
            "universe.cycle_candidate_cap_per_block",
            self.universe.cycle_candidate_cap_per_block,
            &mut missing,
            strict,
        )?;
        require_u64(
            path,
            "universe.time_budget_ms.search",
            self.universe.time_budget_ms.search,
            &mut missing,
            strict,
        )?;
        require_u64(
            path,
            "universe.time_budget_ms.quoting",
            self.universe.time_budget_ms.quoting,
            &mut missing,
            strict,
        )?;
        require_u64(
            path,
            "universe.time_budget_ms.simulation",
            self.universe.time_budget_ms.simulation,
            &mut missing,
            strict,
        )?;
        if let Some(rate) = self.universe.event_sampling_rate {
            if !(0.0..=1.0).contains(&rate) || rate == 0.0 {
                return Err(anyhow!(
                    "universe.event_sampling_rate must be > 0 and <= 1 (got {rate})"
                ));
            }
        } else {
            push_missing(path, "universe.event_sampling_rate", &mut missing, strict);
        }
        require_u64(
            path,
            "universe.event_sampling_block_window",
            self.universe.event_sampling_block_window,
            &mut missing,
            strict,
        )?;
        require_u64(
            path,
            "universe.hot_pool_refresh_secs",
            self.universe.hot_pool_refresh_secs,
            &mut missing,
            strict,
        )?;
        if let Some(value) = self.universe.dynamic_top_tokens_30d {
            if value == 0 {
                return Err(anyhow!(
                    "universe.dynamic_top_tokens_30d must be >= 1 (got {value})"
                ));
            }
        }
        if let Some(value) = self.universe.pair_prune_min_liquidity_usd {
            ensure_positive(value, "universe.pair_prune_min_liquidity_usd")?;
        }

        if self.risk.per_chain.is_empty() {
            push_missing(path, "risk.per_chain", &mut missing, strict);
        }

        let chain_names: HashSet<String> = self
            .chains
            .iter()
            .filter(|chain| !is_missing_value(&chain.chain_name))
            .map(|chain| chain.chain_name.clone())
            .collect();

        for (idx, cfg) in self.risk.per_chain.iter().enumerate() {
            let risk_key = format!("risk.per_chain[{idx}]");
            if is_missing_value(&cfg.chain_name) {
                push_missing(
                    path,
                    &format!("{risk_key}.chain_name"),
                    &mut missing,
                    strict,
                );
            } else if !chain_names.contains(&cfg.chain_name) {
                return Err(anyhow!(
                    "{risk_key}.chain_name `{}` is not present in chains list",
                    cfg.chain_name
                ));
            }

            if cfg.min_net_profit_usd.is_none() && cfg.min_net_profit_native.is_none() {
                push_missing(
                    path,
                    &format!("{risk_key}.min_net_profit_usd|min_net_profit_native"),
                    &mut missing,
                    strict,
                );
            }
            if let Some(value) = cfg.min_net_profit_usd {
                ensure_positive(value, &format!("{risk_key}.min_net_profit_usd"))?;
            }
            if let Some(value) = cfg.min_net_profit_native {
                ensure_positive(value, &format!("{risk_key}.min_net_profit_native"))?;
            }

            require_u64(
                path,
                &format!("{risk_key}.max_gas_units_per_tx"),
                cfg.max_gas_units_per_tx,
                &mut missing,
                strict,
            )?;
            if let Some(value) = cfg.max_fee_per_gas_cap.as_ref() {
                if is_missing_value(value) {
                    push_missing(
                        path,
                        &format!("{risk_key}.max_fee_per_gas_cap"),
                        &mut missing,
                        strict,
                    );
                } else {
                    parse_u256(value, &format!("{risk_key}.max_fee_per_gas_cap"))?;
                }
            } else {
                push_missing(
                    path,
                    &format!("{risk_key}.max_fee_per_gas_cap"),
                    &mut missing,
                    strict,
                );
            }

            require_u32(
                path,
                &format!("{risk_key}.max_slippage_bps"),
                cfg.max_slippage_bps,
                10_000,
                &mut missing,
                strict,
            )?;
            require_u32(
                path,
                &format!("{risk_key}.max_price_impact_bps"),
                cfg.max_price_impact_bps,
                10_000,
                &mut missing,
                strict,
            )?;

            match cfg.must_simulate_before_send {
                Some(_) => {}
                None => {
                    push_missing(
                        path,
                        &format!("{risk_key}.must_simulate_before_send"),
                        &mut missing,
                        strict,
                    );
                }
            }

            let model = match &cfg.revert_penalty_model {
                Some(model) => model,
                None => {
                    push_missing(
                        path,
                        &format!("{risk_key}.revert_penalty_model"),
                        &mut missing,
                        strict,
                    );
                    continue;
                }
            };
            let kind = match model.kind {
                Some(ref kind) => kind,
                None => {
                    push_missing(
                        path,
                        &format!("{risk_key}.revert_penalty_model.kind"),
                        &mut missing,
                        strict,
                    );
                    continue;
                }
            };
            match kind {
                RevertPenaltyKind::Flat => {
                    require_u32(
                        path,
                        &format!("{risk_key}.revert_penalty_model.flat_bps"),
                        model.flat_bps,
                        10_000,
                        &mut missing,
                        strict,
                    )?;
                }
                RevertPenaltyKind::Linear => {
                    require_u32(
                        path,
                        &format!("{risk_key}.revert_penalty_model.linear_base_bps"),
                        model.linear_base_bps,
                        10_000,
                        &mut missing,
                        strict,
                    )?;
                    require_u32(
                        path,
                        &format!("{risk_key}.revert_penalty_model.linear_per_gas_bps"),
                        model.linear_per_gas_bps,
                        10_000,
                        &mut missing,
                        strict,
                    )?;
                }
                RevertPenaltyKind::Exponential => {
                    require_u32(
                        path,
                        &format!("{risk_key}.revert_penalty_model.exponential_base_bps"),
                        model.exponential_base_bps,
                        10_000,
                        &mut missing,
                        strict,
                    )?;
                    require_u32(
                        path,
                        &format!("{risk_key}.revert_penalty_model.exponential_factor"),
                        model.exponential_factor,
                        1000,
                        &mut missing,
                        strict,
                    )?;
                }
            }
        }

        let mut liquidation_map: HashMap<&str, &LiquidationMarketsConfig> = HashMap::new();
        for entry in &self.features.liquidation_markets {
            if !is_missing_value(&entry.chain_name) {
                liquidation_map.insert(entry.chain_name.as_str(), entry);
            }
        }

        for (idx, chain_name) in self.features.enable_backrun.iter().enumerate() {
            if is_missing_value(chain_name) {
                push_missing(
                    path,
                    &format!("features.enable_backrun[{idx}]"),
                    &mut missing,
                    strict,
                );
            } else if !chain_names.contains(chain_name) {
                return Err(anyhow!(
                    "features.enable_backrun[{idx}] `{chain_name}` not present in chains list"
                ));
            }
        }

        for (idx, chain_name) in self.features.enable_liquidations.iter().enumerate() {
            if is_missing_value(chain_name) {
                push_missing(
                    path,
                    &format!("features.enable_liquidations[{idx}]"),
                    &mut missing,
                    strict,
                );
                continue;
            }
            if !chain_names.contains(chain_name) {
                return Err(anyhow!(
                    "features.enable_liquidations[{idx}] `{chain_name}` not present in chains list"
                ));
            }
            let Some(entry) = liquidation_map.get(chain_name.as_str()) else {
                push_missing(
                    path,
                    &format!("features.liquidation_markets[{chain_name}]"),
                    &mut missing,
                    strict,
                );
                continue;
            };
            let entry = *entry;
            if entry.aave_v3.is_none() && entry.compound_v3.is_none() {
                push_missing(
                    path,
                    &format!("features.liquidation_markets[{chain_name}]"),
                    &mut missing,
                    strict,
                );
            }
            if let Some(aave) = entry.aave_v3.as_ref() {
                require_address(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].aave_v3"),
                    "pool",
                    Some(&aave.pool),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].aave_v3"),
                    "data_provider",
                    Some(&aave.data_provider),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].aave_v3"),
                    "price_oracle",
                    Some(&aave.price_oracle),
                    &mut missing,
                    strict,
                )?;
            }

            let chain_aave_flashloan_pool = self
                .chains
                .iter()
                .find(|chain| chain.chain_name.eq_ignore_ascii_case(chain_name))
                .and_then(|chain| {
                    chain
                        .flashloans
                        .iter()
                        .find(|loan| matches!(loan.kind, Some(FlashloanKind::AaveV3Like)))
                })
                .and_then(|loan| loan.pool.as_ref())
                .filter(|pool| !is_missing_value(pool));

            if let (Some(liquidation_aave), Some(flashloan_aave)) =
                (entry.aave_v3.as_ref(), chain_aave_flashloan_pool)
            {
                if !liquidation_aave.pool.eq_ignore_ascii_case(flashloan_aave) {
                    return Err(anyhow!(
                        "features.liquidation_markets[{chain_name}].aave_v3.pool ({}) must match chains[{chain_name}].flashloans[aave_v3].pool ({})",
                        liquidation_aave.pool,
                        flashloan_aave
                    ));
                }
            }
            if let Some(compound) = entry.compound_v3.as_ref() {
                require_address(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].compound_v3"),
                    "comet",
                    Some(&compound.comet),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].compound_v3"),
                    "rewards",
                    Some(&compound.rewards),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].compound_v3"),
                    "configurator",
                    Some(&compound.configurator),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].compound_v3"),
                    "oracle",
                    Some(&compound.oracle),
                    &mut missing,
                    strict,
                )?;
            }

            if entry.markets.is_empty() {
                push_missing(
                    path,
                    &format!("features.liquidation_markets[{chain_name}].markets"),
                    &mut missing,
                    strict,
                );
            }

            for (market_idx, market) in entry.markets.iter().enumerate() {
                let market_key =
                    format!("features.liquidation_markets[{chain_name}].markets[{market_idx}]");
                if market.kind.is_none() {
                    push_missing(path, &format!("{market_key}.kind"), &mut missing, strict);
                }
                if matches!(market.kind, Some(LiquidationMarketKind::Aave))
                    && entry.aave_v3.is_none()
                {
                    return Err(anyhow!(
                        "{market_key}.kind is aave but aave_v3 addresses are missing in {path}",
                        path = path.display()
                    ));
                }
                if matches!(market.kind, Some(LiquidationMarketKind::Compound))
                    && entry.compound_v3.is_none()
                {
                    return Err(anyhow!(
                        "{market_key}.kind is compound but compound_v3 addresses are missing in {path}",
                        path = path.display()
                    ));
                }
                require_address(
                    path,
                    &market_key,
                    "adapter",
                    Some(&market.adapter),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &market_key,
                    "flash_loan_pool",
                    Some(&market.flash_loan_pool),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &market_key,
                    "debt_token",
                    Some(&market.debt_token),
                    &mut missing,
                    strict,
                )?;
                require_address(
                    path,
                    &market_key,
                    "collateral_token",
                    Some(&market.collateral_token),
                    &mut missing,
                    strict,
                )?;
                if let Some(raw) = market.max_repay_wei.as_ref() {
                    parse_u256(raw, &format!("{market_key}.max_repay_wei"))?;
                }
                if let Some(aave) = entry.aave_v3.as_ref() {
                    if !market.flash_loan_pool.eq_ignore_ascii_case(&aave.pool) {
                        return Err(anyhow!(
                            "{market_key}.flash_loan_pool ({}) must match features.liquidation_markets[{chain_name}].aave_v3.pool ({})",
                            market.flash_loan_pool,
                            aave.pool,
                        ));
                    }
                }
            }
        }

        if strict && !missing.is_empty() {
            let mut message = String::from("MISSING INPUTS");
            for field in missing {
                message.push_str("\n- ");
                message.push_str(&field);
            }
            return Err(anyhow!(message));
        }

        Ok(())
    }
}

fn chain_key(idx: usize, name: &str) -> String {
    if is_missing_value(name) {
        format!("chains[{idx}]")
    } else {
        format!("chains[{name}]")
    }
}

fn push_missing(path: &Path, field: &str, missing: &mut Vec<String>, strict: bool) {
    if strict {
        missing.push(format!("{}: {field}", path.display()));
    }
}

fn is_missing_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    let upper = trimmed.to_ascii_uppercase();
    upper.contains("REPLACE")
        || upper.contains("CHANGE_ME")
        || upper.contains("TODO")
        || trimmed.contains('<')
        || trimmed.contains('>')
        || trimmed.contains("${")
}

/// Local helper for ingest binary compatibility (ingest includes this file without `chain`).
fn liquidation_address_is_known_placeholder(address: &str) -> bool {
    const KNOWN: &[&str] = &["0xCfDAdA7D65e8e5B2564BDed4d79e0c084d595e90"];
    let normalized = address.trim().to_ascii_lowercase();
    !normalized.is_empty()
        && KNOWN
            .iter()
            .any(|candidate| normalized == candidate.trim().to_ascii_lowercase())
}

fn prefixed_extra_value(chain: &ChainInputs, suffix: &str) -> Option<String> {
    let prefix = chain.env_prefix.trim();
    if prefix.is_empty() {
        return None;
    }
    let target = format!("{prefix}_{suffix}").to_ascii_uppercase();
    chain.extras.iter().find_map(|(key, value)| {
        if key.to_ascii_uppercase() == target {
            yaml_value_to_string(value)
        } else {
            None
        }
    })
}

fn yaml_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_address(value: &str, field: &str) -> Result<Address> {
    let addr = Address::from_str(value)
        .with_context(|| format!("{field} must be a valid address (got `{value}`)"))?;
    if addr == Address::zero() {
        return Err(anyhow!("{field} must not be the zero address"));
    }
    Ok(addr)
}

fn require_address(
    path: &Path,
    parent: &str,
    field: &str,
    value: Option<&String>,
    missing: &mut Vec<String>,
    strict: bool,
) -> Result<Option<Address>> {
    match value {
        Some(value) if !is_missing_value(value) => {
            parse_address(value, &format!("{parent}.{field}"))
                .map(Some)
                .with_context(|| parent.to_string())
        }
        Some(_) | None => {
            push_missing(path, &format!("{parent}.{field}"), missing, strict);
            Ok(None)
        }
    }
}

fn require_fee_bps(
    path: &Path,
    parent: &str,
    field: &str,
    value: Option<u32>,
    missing: &mut Vec<String>,
    strict: bool,
) -> Result<()> {
    match value {
        Some(value) => {
            if value > 10_000 {
                return Err(anyhow!("{parent}.{field} must be <= 10000 (got {value})"));
            }
            Ok(())
        }
        None => {
            push_missing(path, &format!("{parent}.{field}"), missing, strict);
            Ok(())
        }
    }
}

fn require_init_code_hash(
    path: &Path,
    parent: &str,
    value: Option<&String>,
    missing: &mut Vec<String>,
    strict: bool,
) -> Result<()> {
    match value {
        Some(value) if !is_missing_value(value) => {
            let trimmed = value.trim();
            if !trimmed.starts_with("0x") || trimmed.len() != 66 {
                return Err(anyhow!(
                    "{parent}.pool_init_code_hash must be 0x-prefixed 32-byte hex"
                ));
            }
            Ok(())
        }
        Some(_) | None => {
            push_missing(
                path,
                &format!("{parent}.pool_init_code_hash"),
                missing,
                strict,
            );
            Ok(())
        }
    }
}

fn require_usize(
    path: &Path,
    field: &str,
    value: Option<usize>,
    missing: &mut Vec<String>,
    strict: bool,
) -> Result<()> {
    match value {
        Some(value) if value > 0 => Ok(()),
        Some(_) | None => {
            push_missing(path, field, missing, strict);
            Ok(())
        }
    }
}

fn require_u64(
    path: &Path,
    field: &str,
    value: Option<u64>,
    missing: &mut Vec<String>,
    strict: bool,
) -> Result<()> {
    match value {
        Some(value) if value > 0 => Ok(()),
        Some(_) | None => {
            push_missing(path, field, missing, strict);
            Ok(())
        }
    }
}

fn require_u32(
    path: &Path,
    field: &str,
    value: Option<u32>,
    max: u32,
    missing: &mut Vec<String>,
    strict: bool,
) -> Result<()> {
    match value {
        Some(value) => {
            if value > max {
                return Err(anyhow!("{field} must be <= {max} (got {value})"));
            }
            Ok(())
        }
        None => {
            push_missing(path, field, missing, strict);
            Ok(())
        }
    }
}

fn ops_inputs_strict() -> bool {
    std::env::var("OPS_INPUTS_STRICT")
        .ok()
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn validate_health_config(path: &Path, field: &str, health: &HealthConfig) -> Result<()> {
    if let Some(alpha) = health.ema_alpha {
        if !(alpha > 0.0 && alpha <= 1.0) {
            return Err(anyhow!(
                "{field}.ema_alpha must be > 0 and <= 1 ({})",
                path.display()
            ));
        }
    }
    if let Some(reject) = health.max_reject_rate_ema {
        if !(0.0..=1.0).contains(&reject) {
            return Err(anyhow!(
                "{field}.max_reject_rate_ema must be within [0,1] ({})",
                path.display()
            ));
        }
    }
    if let Some(success) = health.min_success_rate_ema {
        if !(0.0..=1.0).contains(&success) {
            return Err(anyhow!(
                "{field}.min_success_rate_ema must be within [0,1] ({})",
                path.display()
            ));
        }
    }
    if let Some(latency) = health.max_latency_ms_ema {
        if latency <= 0.0 {
            return Err(anyhow!(
                "{field}.max_latency_ms_ema must be > 0 ({})",
                path.display()
            ));
        }
    }
    Ok(())
}

fn is_generic_private_relay_url(relay: &str) -> bool {
    let relay = relay.trim().to_ascii_lowercase();
    if relay.is_empty() {
        return false;
    }
    relay.contains("alchemy.com/v2/")
        || relay.contains("publicnode.com")
        || relay.contains("ankr.com")
        || relay.contains("infura.io")
        || relay.contains("drpc.org")
        || relay.contains("quicknode.com")
        || relay.contains("llamarpc.com")
        || relay.contains("blastapi.io")
}

fn parse_u256(value: &str, field: &str) -> Result<()> {
    value
        .parse::<ethers::types::U256>()
        .with_context(|| format!("{field} must be a valid U256 value"))?;
    Ok(())
}

fn ensure_positive(value: f64, field: &str) -> Result<()> {
    if value <= 0.0 {
        return Err(anyhow!("{field} must be > 0"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expand_env_vars, load_ops_inputs, parse_ops_inputs};

    #[test]
    fn expands_env_vars_in_yaml() {
        std::env::set_var("ALCHEMY_KEY", "test-key");
        let raw = r#"
chains:
  - chain_name: ethereum
    chain_id: 1
    env_prefix: ETH
    rpc_http_urls:
      - https://ethereum-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
    rpc_ws_urls:
      - wss://ethereum-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
"#;
        let expanded = expand_env_vars(raw);
        let cfg = parse_ops_inputs(&expanded).expect("parse expanded yaml");
        assert_eq!(
            cfg.chains[0].rpc_http_urls[0],
            "https://ethereum-mainnet.g.alchemy.com/v2/test-key"
        );
        std::env::remove_var("ALCHEMY_KEY");
    }

    #[test]
    fn chain_overrides_reads_balancer_vault_from_flashloans() {
        let raw = r#"
chains:
  - chain_name: base
    chain_id: 8453
    env_prefix: BASE
    rpc_http_urls:
      - http://localhost:8545
    rpc_ws_urls:
      - ws://localhost:8546
    venues:
      - name: balancer
        kind: balancer_like
        vault: "0x1111111111111111111111111111111111111111"
    flashloans:
      - kind: balancer_vault_like
        vault: "0x2222222222222222222222222222222222222222"
"#;
        let cfg = parse_ops_inputs(raw).expect("parse ops inputs");
        let overrides = cfg.chain_overrides("base").expect("chain overrides");
        assert_eq!(
            overrides.balancer_vault.as_deref(),
            Some("0x2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn chain_overrides_falls_back_to_balancer_venue_vault() {
        let raw = r#"
chains:
  - chain_name: ink
    chain_id: 57073
    env_prefix: INK
    rpc_http_urls:
      - http://localhost:8545
    rpc_ws_urls:
      - ws://localhost:8546
    venues:
      - name: balancer
        kind: balancer_like
        vault: "0x3333333333333333333333333333333333333333"
"#;
        let cfg = parse_ops_inputs(raw).expect("parse ops inputs");
        let overrides = cfg.chain_overrides("ink").expect("chain overrides");
        assert_eq!(
            overrides.balancer_vault.as_deref(),
            Some("0x3333333333333333333333333333333333333333")
        );
    }

    #[test]
    fn configured_aave_pool_probe_targets_collects_flashloan_and_enabled_liquidation_sources() {
        let raw = r#"
chains:
  - chain_name: arbitrum
    chain_id: 42161
    env_prefix: ARB
    rpc_http_urls: ["https://arb.example"]
    rpc_ws_urls: ["wss://arb.example"]
    flashloans:
      - kind: aave_v3_like
        pool: "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
features:
  enable_liquidations: [arbitrum]
  liquidation_markets:
    - chain_name: arbitrum
      aave_v3:
        pool: "0x9999999999999999999999999999999999999999"
        data_provider: "0x2222222222222222222222222222222222222222"
        price_oracle: "0x3333333333333333333333333333333333333333"
      markets:
        - kind: aave
          adapter: "0x4444444444444444444444444444444444444444"
          flash_loan_pool: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
          debt_token: "0x5555555555555555555555555555555555555555"
          collateral_token: "0x6666666666666666666666666666666666666666"
"#;
        let cfg = parse_ops_inputs(raw).expect("parse ops inputs");
        let targets = cfg.configured_aave_pool_probe_targets("arbitrum");
        assert_eq!(targets.len(), 3);
        assert_eq!(
            targets[0].source,
            "chains[arbitrum].flashloans[0].pool".to_string()
        );
        assert_eq!(
            targets[1].source,
            "features.liquidation_markets[arbitrum].aave_v3.pool".to_string()
        );
        assert_eq!(
            targets[2].source,
            "features.liquidation_markets[arbitrum].markets[0].flash_loan_pool".to_string()
        );
    }

    #[test]
    fn configured_aave_pool_probe_targets_skip_disabled_and_compound_liquidation_sources() {
        let raw = r#"
chains:
  - chain_name: arbitrum
    chain_id: 42161
    env_prefix: ARB
    rpc_http_urls: ["https://arb.example"]
    rpc_ws_urls: ["wss://arb.example"]
    flashloans:
      - kind: aave_v3_like
        pool: "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
features:
  liquidation_markets:
    - chain_name: arbitrum
      aave_v3:
        pool: "<missing>"
        data_provider: "0x2222222222222222222222222222222222222222"
        price_oracle: "0x3333333333333333333333333333333333333333"
      markets:
        - kind: compound
          adapter: "0x4444444444444444444444444444444444444444"
          flash_loan_pool: "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
          debt_token: "0x5555555555555555555555555555555555555555"
          collateral_token: "0x6666666666666666666666666666666666666666"
"#;
        let cfg = parse_ops_inputs(raw).expect("parse ops inputs");
        let targets = cfg.configured_aave_pool_probe_targets("arbitrum");
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].source,
            "chains[arbitrum].flashloans[0].pool".to_string()
        );
    }

    #[test]
    fn validate_requires_consistent_aave_flashloan_addresses() {
        let raw = r#"
chains:
  - chain_name: arbitrum
    chain_id: 42161
    env_prefix: ARB
    rpc_http_urls: ["https://arb.example"]
    rpc_ws_urls: ["wss://arb.example"]
    venues: []
    flashloans:
      - name: aave_v3
        kind: aave_v3_like
        pool: "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
features:
  enable_liquidations: [arbitrum]
  liquidation_markets:
    - chain_name: arbitrum
      aave_v3:
        pool: "0x1111111111111111111111111111111111111111"
        data_provider: "0x2222222222222222222222222222222222222222"
        price_oracle: "0x3333333333333333333333333333333333333333"
      markets:
        - name: aave_usdc_weth
          kind: aave
          adapter: "0x4444444444444444444444444444444444444444"
          flash_loan_pool: "0x1111111111111111111111111111111111111111"
          debt_token: "0x5555555555555555555555555555555555555555"
          collateral_token: "0x6666666666666666666666666666666666666666"
          bonus_bps: 800
          collateral_exchange_rate_bps: 10000
          estimated_gas: 450000
          selector: 1865448899
          receive_atoken: false
          max_repay_wei: "0"
"#;
        let path = std::env::temp_dir().join("ops_inputs_consistency_test.yaml");
        std::fs::write(&path, raw).expect("write temp ops inputs");
        let err = load_ops_inputs(&path).expect_err("validation should fail");
        let _ = std::fs::remove_file(&path);
        assert!(
            err.to_string()
                .contains("must match chains[arbitrum].flashloans[aave_v3].pool"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_private_mode_generic_relay_without_method_policy() {
        let raw = r#"
chains:
  - chain_name: base
    chain_id: 8453
    env_prefix: BASE
    rpc_http_urls:
      - http://localhost:8545
    rpc_ws_urls:
      - ws://localhost:8546
    executor_address: "0x1111111111111111111111111111111111111111"
    permit2_address: "0x000000000022D473030F116dDEE9F6B43aC78BA3"
    venues: []
    flashloans:
      - kind: erc3156_like
        lender: "0x2222222222222222222222222222222222222222"
        fee_bps: 9
        max_loan_assets: ["0x3333333333333333333333333333333333333333"]
    broadcast:
      mode: private
      private_relays:
        - https://base-mainnet.g.alchemy.com/v2/test
"#;
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), raw).expect("write ops yaml");
        let err = load_ops_inputs(file.path()).expect_err("generic relay must be rejected");
        assert!(err.to_string().contains("broadcast.private_method_policy"));
    }

    #[test]
    fn accepts_private_mode_generic_relay_with_method_policy() {
        let raw = r#"
chains:
  - chain_name: base
    chain_id: 8453
    env_prefix: BASE
    rpc_http_urls:
      - http://localhost:8545
    rpc_ws_urls:
      - ws://localhost:8546
    executor_address: "0x1111111111111111111111111111111111111111"
    permit2_address: "0x000000000022D473030F116dDEE9F6B43aC78BA3"
    venues: []
    flashloans:
      - kind: erc3156_like
        lender: "0x2222222222222222222222222222222222222222"
        fee_bps: 9
        max_loan_assets: ["0x3333333333333333333333333333333333333333"]
    broadcast:
      mode: private
      private_method_policy: bundle_then_private_raw
      private_relays:
        - https://base-mainnet.g.alchemy.com/v2/test
"#;
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), raw).expect("write ops yaml");
        load_ops_inputs(file.path()).expect("method policy should allow generic relay");
    }

    fn chain_overrides_accepts_prefixed_executor_address() {
        let raw = r#"
chains:
  - chain_name: ethereum
    chain_id: 1
    env_prefix: ETH
    rpc_http_urls:
      - http://localhost:8545
    rpc_ws_urls:
      - ws://localhost:8546
    gas_model: eip1559
    ETH_EXECUTOR_ADDRESS: "0x1111111111111111111111111111111111111111"
    ETH_PERMIT2_ADDRESS: "0x000000000022D473030F116dDEE9F6B43aC78BA3"
"#;
        let cfg = parse_ops_inputs(raw).expect("parse ops inputs");
        let overrides = cfg.chain_overrides("ethereum").expect("chain overrides");
        assert_eq!(
            overrides.executor_address.as_deref(),
            Some("0x1111111111111111111111111111111111111111")
        );
        assert_eq!(
            overrides.permit2_address.as_deref(),
            Some("0x000000000022D473030F116dDEE9F6B43aC78BA3")
        );
    }
    #[test]
    fn chain_overrides_extracts_provider_flashloan_allowlists() {
        let raw = r#"
chains:
  - chain_name: arbitrum
    chain_id: 42161
    env_prefix: ARB
    rpc_http_urls:
      - http://localhost:8545
    rpc_ws_urls:
      - ws://localhost:8546
    flashloans:
      - kind: aave_v3_like
        pool: "0x1111111111111111111111111111111111111111"
        allowlist_tokens:
          - "0x0000000000000000000000000000000000000001"
      - kind: erc3156_like
        lender: "0x2222222222222222222222222222222222222222"
        fee_bps: 9
        allowlist_tokens:
          - "0x0000000000000000000000000000000000000002"
      - kind: univ2_flashswap
        allowlist_tokens:
          - "0x0000000000000000000000000000000000000003"
      - kind: univ3_flash
        allowlist_tokens:
          - "0x0000000000000000000000000000000000000004"
"#;
        let cfg = parse_ops_inputs(raw).expect("parse ops inputs");
        let overrides = cfg.chain_overrides("arbitrum").expect("chain overrides");
        assert_eq!(
            overrides.aave_flashloan_tokens,
            vec!["0x0000000000000000000000000000000000000001"]
        );
        assert_eq!(
            overrides.erc3156_flashloan_tokens,
            vec!["0x0000000000000000000000000000000000000002"]
        );
        assert_eq!(
            overrides.univ2_flashloan_tokens,
            vec!["0x0000000000000000000000000000000000000003"]
        );
        assert_eq!(
            overrides.univ3_flashloan_tokens,
            vec!["0x0000000000000000000000000000000000000004"]
        );
    }

    #[test]
    fn validate_liquidation_safety_rejects_placeholders_when_feature_enabled() {
        let raw = r#"
chains: []
universe:
  token_seeds: []
features:
  enable_liquidations:
    - base
  liquidation_markets:
    - chain_name: base
      markets:
        - kind: aave
          adapter: "0xCfDAdA7D65e8e5B2564BDed4d79e0c084d595e90"
          flash_loan_pool: "0xA238Dd80C259a72e81d7e4664a9801593F98d1c5"
          debt_token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913"
          collateral_token: "0x4200000000000000000000000000000000000006"
"#;
        let cfg = parse_ops_inputs(raw).expect("parse ops inputs");
        let err = cfg
            .validate_liquidation_safety(true)
            .expect_err("placeholder liquidation addresses must fail when enabled");
        assert!(err.to_string().contains("placeholder addresses"));
    }

    #[test]
    fn validate_liquidation_safety_allows_placeholders_when_feature_disabled() {
        let cfg = load_ops_inputs("ops/inputs.yaml").expect("parse ops inputs");
        cfg.validate_liquidation_safety(false)
            .expect("disabled liquidations should not block startup");
    }
}
