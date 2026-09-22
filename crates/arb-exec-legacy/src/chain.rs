#![allow(dead_code)]

use crate::ops_inputs::{ChainInputs, FlashloanKind, GasModel, OpsChainOverrides, VenueKind};
use crate::quote_univ3::UniV3ValidationConfig;
use crate::registry::{parse_address, RegistryChain};
use crate::util::{coerce_http_url, parse_endpoint_list};
use anyhow::{anyhow, ensure, Context, Result};
use ethers::providers::Middleware;
use ethers::types::{Address, BlockNumber, Bytes, NameOrAddress, TransactionRequest, U256};
use ethers::utils::id;
use hex_literal::hex;
use std::{fs, path::PathBuf, str::FromStr};
use tracing::warn;

#[derive(Clone, Debug)]
pub struct ChainCfg {
    #[allow(dead_code)]
    pub name: String,
    pub env_prefix: String,
    pub chain_id: u64,
    pub rpc: String,
    pub rpc_fallbacks: Vec<String>,
    pub univ3_quoter: Address,
    pub univ3_factory: Address,
    #[allow(dead_code)]
    pub univ3_router: Address,
    pub bal_vault: Address,
    pub aave_pool: Option<Address>,
    pub bal_flashloan_tokens: Option<Vec<Address>>,
    pub aave_flashloan_tokens: Option<Vec<Address>>,
    pub erc3156_flashloan_tokens: Option<Vec<Address>>,
    pub univ2_flashloan_tokens: Option<Vec<Address>>,
    pub univ3_flashloan_tokens: Option<Vec<Address>>,
    pub tokens: Vec<Address>,
    pub ws_endpoints: Vec<String>,
    pub univ3_validation: Option<UniV3ValidationConfig>,
    #[allow(dead_code)]
    pub registry_version: Option<String>,
    pub gas_model: GasModel,
    pub gas_rpc_method: Option<String>,
    pub arbitrum_l1_per_byte_wei: Option<U256>,
    pub arbitrum_l1_per_byte_max_deviation_bps: Option<u32>,
}

impl ChainCfg {
    pub fn rpc_endpoints(&self) -> Vec<String> {
        let mut urls = Vec::with_capacity(1 + self.rpc_fallbacks.len());
        urls.push(self.rpc.clone());
        urls.extend(self.rpc_fallbacks.clone());
        urls
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum InputSource {
    Ops,
    Env,
}

fn env_var(key: &str) -> Result<String> {
    std::env::var(key).map_err(|err| match err {
        std::env::VarError::NotPresent => {
            anyhow!("environment variable {key} is not set")
        }
        std::env::VarError::NotUnicode(_) => {
            anyhow!("environment variable {key} contains invalid UTF-8")
        }
    })
}

#[allow(dead_code)]
pub fn env_addr(key: &str) -> Result<Address> {
    let raw = env_var(key)?;
    parse_address(&raw, key)
        .with_context(|| format!("environment variable {key} must be a valid address"))
}

pub fn env_addr_optional(key: &str) -> Result<Option<Address>> {
    match std::env::var(key) {
        Ok(raw) => {
            if raw.trim().is_empty() {
                Ok(None)
            } else {
                parse_address(&raw, key)
                    .with_context(|| format!("environment variable {key} must be a valid address"))
                    .map(Some)
            }
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(anyhow!("environment variable {key} contains invalid UTF-8"))
        }
    }
}

fn env_addr_optional_with_aliases(
    primary_key: &str,
    alias_keys: &[String],
) -> Result<Option<Address>> {
    if let Some(value) = env_addr_optional(primary_key)? {
        return Ok(Some(value));
    }
    for alias in alias_keys {
        if let Some(value) = env_addr_optional(alias)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn prefixed_env_alias(
    primary_key: &str,
    canonical_suffix: &str,
    alias_suffix: &str,
) -> Option<String> {
    primary_key
        .strip_suffix(canonical_suffix)
        .map(|prefix| format!("{prefix}{alias_suffix}"))
}

pub fn env_str(key: &str) -> Result<String> {
    env_var(key)
}

const DEFAULT_UNIV3_FACTORY_BYTES: [u8; 20] = hex!("1f98431c8ad98523631ae4a59f267346ea31f984");
const DEFAULT_UNIV3_VALIDATION_AMOUNT_WEI: u64 = 1_000_000_000_000_000;

fn default_univ3_factory() -> Address {
    Address::from_slice(&DEFAULT_UNIV3_FACTORY_BYTES)
}

#[allow(dead_code)]
struct ChainEnvConfig {
    env_prefix: &'static str,
    chain_id: u64,
    rpc_env: &'static str,
    rpc_list_env: &'static str,
    ws_env: &'static str,
    quoter_env: &'static str,
    factory_env: &'static str,
    router_env: &'static str,
    bal_vault_env: &'static str,
    aave_pool_env: &'static str,
    bal_flashloan_env: &'static str,
    tokens_env: &'static str,
    gas_model_env: &'static str,
    gas_rpc_method_env: &'static str,
    arbitrum_l1_per_byte_env: &'static str,
    arbitrum_l1_per_byte_max_dev_env: &'static str,
}

fn build_chain_cfg_from_env(name: &str, config: ChainEnvConfig) -> Result<ChainCfg> {
    let (rpc, rpc_fallbacks) = parse_rpc_endpoints(config.rpc_env, config.rpc_list_env)?;
    let ws_endpoints = parse_ws_endpoints(config.ws_env);

    let cfg = ChainCfg {
        name: name.to_string(),
        env_prefix: config.env_prefix.into(),
        chain_id: config.chain_id,
        rpc,
        rpc_fallbacks,
        univ3_quoter: env_addr(config.quoter_env)?,
        univ3_factory: env_addr_optional(config.factory_env)?.unwrap_or_else(default_univ3_factory),
        univ3_router: env_addr_optional_with_aliases(
            config.router_env,
            &prefixed_env_alias(config.router_env, "_UNIV3_ROUTER", "_SWAPROUTER02")
                .into_iter()
                .collect::<Vec<_>>(),
        )?
        .ok_or_else(|| anyhow!("environment variable {} is not set", config.router_env))?,
        bal_vault: env_addr_optional_with_aliases(
            config.bal_vault_env,
            &prefixed_env_alias(config.bal_vault_env, "_BAL_VAULT", "_BALANCER_VAULT")
                .into_iter()
                .collect::<Vec<_>>(),
        )?
        .ok_or_else(|| anyhow!("environment variable {} is not set", config.bal_vault_env))?,
        aave_pool: env_addr_optional(config.aave_pool_env)?,
        bal_flashloan_tokens: parse_optional_token_list(config.bal_flashloan_env)?,
        // See `load_chain_from_registry`: without a list Aave is never offered,
        // because its gate is `.unwrap_or(false)` where Balancer's is
        // `.unwrap_or(true)`. Unset keeps the provider off.
        aave_flashloan_tokens: parse_optional_token_list(&format!(
            "{}_AAVE_FLASHLOAN_TOKENS",
            config.env_prefix
        ))?,
        erc3156_flashloan_tokens: None,
        univ2_flashloan_tokens: None,
        univ3_flashloan_tokens: None,
        tokens: parse_token_list(config.tokens_env)?,
        ws_endpoints,
        univ3_validation: parse_validation_override(name, config.env_prefix)?,
        registry_version: None,
        gas_model: parse_gas_model_from_env(config.gas_model_env)
            .unwrap_or_else(|| default_gas_model(name)),
        gas_rpc_method: std::env::var(config.gas_rpc_method_env)
            .ok()
            .filter(|value| !value.trim().is_empty()),
        arbitrum_l1_per_byte_wei: parse_u256_env(config.arbitrum_l1_per_byte_env)?,
        arbitrum_l1_per_byte_max_deviation_bps: parse_u32_env(
            config.arbitrum_l1_per_byte_max_dev_env,
        )?,
    };

    validate_non_placeholder_rpc_endpoints(&cfg)?;
    Ok(cfg)
}

/// Returns true when any supported environment flag marks the process as production.
pub fn production_mode_enabled() -> bool {
    ["ARBOT_ENV", "APP_ENV", "NODE_ENV", "RUN_MODE"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .any(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            normalized == "production" || normalized == "prod"
        })
}

/// Known scaffold/placeholder contract addresses that must never be used in live configs.
pub const KNOWN_PLACEHOLDER_ADDRESSES: &[&str] = &[
    "0xCfDAdA7D65e8e5B2564BDed4d79e0c084d595e90",
];

/// Returns true when `address` matches a known placeholder/scaffold contract address.
pub fn address_is_known_placeholder(address: &str) -> bool {
    let normalized = address.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    KNOWN_PLACEHOLDER_ADDRESSES
        .iter()
        .any(|candidate| normalized == candidate.trim().to_ascii_lowercase())
}

/// Returns true when a secret env value is an obvious template and unsafe for production.
pub fn secret_looks_placeholder(raw: &str) -> bool {
    let normalized = raw.trim().to_ascii_lowercase();
    normalized.is_empty()
        || normalized.contains("replace")
        || normalized.contains("changeme")
        || normalized.contains("your_")
        || normalized == "0x"
        || normalized == "0x0"
}

pub fn endpoint_looks_placeholder(endpoint: &str) -> bool {
    let normalized = endpoint.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized.contains("${") {
        return true;
    }

    let host = normalized
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(normalized.as_str())
        .split('/')
        .next()
        .unwrap_or_default()
        .split('@')
        .next_back()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();

    host == "localhost"
        || host == "127.0.0.1"
        || host == "0.0.0.0"
        || host == "::1"
        || host == "example.org"
        || host.ends_with(".example")
        || host.ends_with(".example.org")
}

fn validate_non_placeholder_rpc_endpoints(cfg: &ChainCfg) -> Result<()> {
    if !production_mode_enabled() {
        return Ok(());
    }

    let placeholders: Vec<String> = cfg
        .rpc_endpoints()
        .into_iter()
        .chain(cfg.ws_endpoints.iter().cloned())
        .filter(|endpoint| endpoint_looks_placeholder(endpoint))
        .collect();

    if placeholders.is_empty() {
        return Ok(());
    }

    Err(anyhow!(
        "production mode rejected placeholder rpc endpoint(s) for {}: {}",
        cfg.name,
        placeholders.join(", ")
    ))
}

async fn staticcall_selector<M: Middleware>(
    provider: &M,
    pool: Address,
    signature: &str,
) -> Result<Bytes>
where
    M::Error: 'static,
{
    let selector = &id(signature)[..4];
    let tx = TransactionRequest {
        to: Some(NameOrAddress::Address(pool)),
        data: Some(Bytes::from(selector.to_vec())),
        ..Default::default()
    };
    provider
        .call(&tx.into(), Some(BlockNumber::Latest.into()))
        .await
        .with_context(|| format!("Aave pool {pool:?} call {signature} failed"))
}

pub async fn probe_aave_pool_interface<M: Middleware>(
    provider: &M,
    pool: Address,
    source: &str,
) -> Result<()>
where
    M::Error: 'static,
{
    let provider_addr = staticcall_selector(provider, pool, "ADDRESSES_PROVIDER()").await?;
    ensure!(
        provider_addr.len() >= 32,
        "Aave pool probe failed for {source}: ADDRESSES_PROVIDER() returned {} bytes",
        provider_addr.len()
    );

    let premium = staticcall_selector(provider, pool, "FLASHLOAN_PREMIUM_TOTAL()").await?;
    ensure!(
        premium.len() >= 32,
        "Aave pool probe failed for {source}: FLASHLOAN_PREMIUM_TOTAL() returned {} bytes",
        premium.len()
    );

    Ok(())
}
pub fn load_chain_from_env(name: &str) -> Result<ChainCfg> {
    match name {
        "ethereum" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "ETH",
                chain_id: 1,
                rpc_env: "ETH_RPC_URL",
                rpc_list_env: "ETH_RPC_URLS",
                ws_env: "ETH_WS_RPC_URLS",
                quoter_env: "ETH_UNIV3_QUOTER",
                factory_env: "ETH_UNIV3_FACTORY",
                router_env: "ETH_UNIV3_ROUTER",
                bal_vault_env: "ETH_BAL_VAULT",
                aave_pool_env: "ETH_AAVE_POOL",
                bal_flashloan_env: "ETH_BAL_FLASHLOAN_TOKENS",
                tokens_env: "ETH_TOKENS",
                gas_model_env: "ETH_GAS_MODEL",
                gas_rpc_method_env: "ETH_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "ETH_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "ETH_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "arbitrum" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "ARB",
                chain_id: 42_161,
                rpc_env: "ARB_RPC_URL",
                rpc_list_env: "ARB_RPC_URLS",
                ws_env: "ARB_WS_RPC_URLS",
                quoter_env: "ARB_UNIV3_QUOTER",
                factory_env: "ARB_UNIV3_FACTORY",
                router_env: "ARB_UNIV3_ROUTER",
                bal_vault_env: "ARB_BAL_VAULT",
                aave_pool_env: "ARB_AAVE_POOL",
                bal_flashloan_env: "ARB_BAL_FLASHLOAN_TOKENS",
                tokens_env: "ARB_TOKENS",
                gas_model_env: "ARB_GAS_MODEL",
                gas_rpc_method_env: "ARB_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "ARB_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "ARB_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "optimism" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "OPT",
                chain_id: 10,
                rpc_env: "OPT_RPC_URL",
                rpc_list_env: "OPT_RPC_URLS",
                ws_env: "OPT_WS_RPC_URLS",
                quoter_env: "OPT_UNIV3_QUOTER",
                factory_env: "OPT_UNIV3_FACTORY",
                router_env: "OPT_UNIV3_ROUTER",
                bal_vault_env: "OPT_BAL_VAULT",
                aave_pool_env: "OPT_AAVE_POOL",
                bal_flashloan_env: "OPT_BAL_FLASHLOAN_TOKENS",
                tokens_env: "OPT_TOKENS",
                gas_model_env: "OPT_GAS_MODEL",
                gas_rpc_method_env: "OPT_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "OPT_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "OPT_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "base" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "BASE",
                chain_id: 8_453,
                rpc_env: "BASE_RPC_URL",
                rpc_list_env: "BASE_RPC_URLS",
                ws_env: "BASE_WS_RPC_URLS",
                quoter_env: "BASE_UNIV3_QUOTER",
                factory_env: "BASE_UNIV3_FACTORY",
                router_env: "BASE_UNIV3_ROUTER",
                bal_vault_env: "BASE_BAL_VAULT",
                aave_pool_env: "BASE_AAVE_POOL",
                bal_flashloan_env: "BASE_BAL_FLASHLOAN_TOKENS",
                tokens_env: "BASE_TOKENS",
                gas_model_env: "BASE_GAS_MODEL",
                gas_rpc_method_env: "BASE_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "BASE_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "BASE_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "polygon" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "POLYGON",
                chain_id: 137,
                rpc_env: "POLYGON_RPC_URL",
                rpc_list_env: "POLYGON_RPC_URLS",
                ws_env: "POLYGON_WS_RPC_URLS",
                quoter_env: "POLYGON_UNIV3_QUOTER",
                factory_env: "POLYGON_UNIV3_FACTORY",
                router_env: "POLYGON_UNIV3_ROUTER",
                bal_vault_env: "POLYGON_BAL_VAULT",
                aave_pool_env: "POLYGON_AAVE_POOL",
                bal_flashloan_env: "POLYGON_BAL_FLASHLOAN_TOKENS",
                tokens_env: "POLYGON_TOKENS",
                gas_model_env: "POLYGON_GAS_MODEL",
                gas_rpc_method_env: "POLYGON_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "POLYGON_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "POLYGON_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "abstract" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "ABSTRACT",
                chain_id: 2_741,
                rpc_env: "ABSTRACT_RPC_URL",
                rpc_list_env: "ABSTRACT_RPC_URLS",
                ws_env: "ABSTRACT_WS_RPC_URLS",
                quoter_env: "ABSTRACT_UNIV3_QUOTER",
                factory_env: "ABSTRACT_UNIV3_FACTORY",
                router_env: "ABSTRACT_UNIV3_ROUTER",
                bal_vault_env: "ABSTRACT_BAL_VAULT",
                aave_pool_env: "ABSTRACT_AAVE_POOL",
                bal_flashloan_env: "ABSTRACT_BAL_FLASHLOAN_TOKENS",
                tokens_env: "ABSTRACT_TOKENS",
                gas_model_env: "ABSTRACT_GAS_MODEL",
                gas_rpc_method_env: "ABSTRACT_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "ABSTRACT_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "ABSTRACT_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "ink" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "INK",
                chain_id: 763_373,
                rpc_env: "INK_RPC_URL",
                rpc_list_env: "INK_RPC_URLS",
                ws_env: "INK_WS_RPC_URLS",
                quoter_env: "INK_UNIV3_QUOTER",
                factory_env: "INK_UNIV3_FACTORY",
                router_env: "INK_UNIV3_ROUTER",
                bal_vault_env: "INK_BAL_VAULT",
                aave_pool_env: "INK_AAVE_POOL",
                bal_flashloan_env: "INK_BAL_FLASHLOAN_TOKENS",
                tokens_env: "INK_TOKENS",
                gas_model_env: "INK_GAS_MODEL",
                gas_rpc_method_env: "INK_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "INK_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "INK_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "linea" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "LINEA",
                chain_id: 59_144,
                rpc_env: "LINEA_RPC_URL",
                rpc_list_env: "LINEA_RPC_URLS",
                ws_env: "LINEA_WS_RPC_URLS",
                quoter_env: "LINEA_UNIV3_QUOTER",
                factory_env: "LINEA_UNIV3_FACTORY",
                router_env: "LINEA_UNIV3_ROUTER",
                bal_vault_env: "LINEA_BAL_VAULT",
                aave_pool_env: "LINEA_AAVE_POOL",
                bal_flashloan_env: "LINEA_BAL_FLASHLOAN_TOKENS",
                tokens_env: "LINEA_TOKENS",
                gas_model_env: "LINEA_GAS_MODEL",
                gas_rpc_method_env: "LINEA_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "LINEA_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "LINEA_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "mantle" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "MANTLE",
                chain_id: 5_000,
                rpc_env: "MANTLE_RPC_URL",
                rpc_list_env: "MANTLE_RPC_URLS",
                ws_env: "MANTLE_WS_RPC_URLS",
                quoter_env: "MANTLE_UNIV3_QUOTER",
                factory_env: "MANTLE_UNIV3_FACTORY",
                router_env: "MANTLE_UNIV3_ROUTER",
                bal_vault_env: "MANTLE_BAL_VAULT",
                aave_pool_env: "MANTLE_AAVE_POOL",
                bal_flashloan_env: "MANTLE_BAL_FLASHLOAN_TOKENS",
                tokens_env: "MANTLE_TOKENS",
                gas_model_env: "MANTLE_GAS_MODEL",
                gas_rpc_method_env: "MANTLE_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "MANTLE_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "MANTLE_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        "scroll" => build_chain_cfg_from_env(
            name,
            ChainEnvConfig {
                env_prefix: "SCROLL",
                chain_id: 534_352,
                rpc_env: "SCROLL_RPC_URL",
                rpc_list_env: "SCROLL_RPC_URLS",
                ws_env: "SCROLL_WS_RPC_URLS",
                quoter_env: "SCROLL_UNIV3_QUOTER",
                factory_env: "SCROLL_UNIV3_FACTORY",
                router_env: "SCROLL_UNIV3_ROUTER",
                bal_vault_env: "SCROLL_BAL_VAULT",
                aave_pool_env: "SCROLL_AAVE_POOL",
                bal_flashloan_env: "SCROLL_BAL_FLASHLOAN_TOKENS",
                tokens_env: "SCROLL_TOKENS",
                gas_model_env: "SCROLL_GAS_MODEL",
                gas_rpc_method_env: "SCROLL_GAS_RPC_METHOD",
                arbitrum_l1_per_byte_env: "SCROLL_ARBITRUM_L1_PER_BYTE_WEI",
                arbitrum_l1_per_byte_max_dev_env: "SCROLL_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS",
            },
        ),
        _ => Err(anyhow!("unknown chain")),
    }
}

pub fn load_chain_from_registry(
    name: &str,
    registry_version: Option<String>,
    registry_chain: &RegistryChain,
) -> Result<ChainCfg> {
    let env_prefix = registry_chain
        .env_prefix
        .clone()
        .unwrap_or_else(|| name.to_ascii_uppercase());

    let mut rpc_iter = registry_chain.rpc_http.iter();
    let rpc = rpc_iter
        .next()
        .cloned()
        .ok_or_else(|| anyhow!("registry for {name} missing rpc_http endpoint"))?;
    let rpc_fallbacks = rpc_iter.cloned().collect();

    let tokens = if registry_chain.tokens.is_empty() {
        let env_key = format!("{}_TOKENS", env_prefix);
        parse_optional_token_list(&env_key)?.unwrap_or_default()
    } else {
        registry_chain
            .tokens
            .iter()
            .map(|raw| parse_address(raw, "tokens"))
            .collect::<Result<Vec<_>>>()?
    };

    let validation = parse_validation_override(name, &env_prefix)?;
    // Read before `env_prefix` is moved into the struct below.
    let aave_flashloan_tokens =
        parse_optional_token_list(&format!("{env_prefix}_AAVE_FLASHLOAN_TOKENS"))?;

    let cfg = ChainCfg {
        name: name.to_string(),
        env_prefix,
        chain_id: registry_chain.chain_id,
        rpc,
        rpc_fallbacks,
        univ3_quoter: parse_address(
            registry_chain
                .univ3_quoter
                .as_deref()
                .ok_or_else(|| anyhow!("registry missing univ3 quoter for {name}"))?,
            "univ3_quoter",
        )?,
        univ3_factory: registry_chain
            .univ3_factory
            .as_deref()
            .map(|raw| parse_address(raw, "univ3_factory"))
            .transpose()?
            .unwrap_or_else(default_univ3_factory),
        univ3_router: parse_address(
            registry_chain
                .univ3_router
                .as_deref()
                .ok_or_else(|| anyhow!("registry missing univ3 router for {name}"))?,
            "univ3_router",
        )?,
        bal_vault: parse_address(
            registry_chain
                .bal_vault
                .as_deref()
                .ok_or_else(|| anyhow!("registry missing bal_vault for {name}"))?,
            "bal_vault",
        )?,
        aave_pool: registry_chain
            .aave_pool
            .as_deref()
            .map(|raw| parse_address(raw, "aave_pool"))
            .transpose()?,
        bal_flashloan_tokens: if registry_chain.bal_flashloan_tokens.is_empty() {
            None
        } else {
            Some(
                registry_chain
                    .bal_flashloan_tokens
                    .iter()
                    .map(|raw| parse_address(raw, "bal_flashloan_tokens"))
                    .collect::<Result<Vec<_>>>()?,
            )
        },
        // Aave was UNREACHABLE while this was a hardcoded `None`.
        // `flash_loan_quotes` gates it on `.unwrap_or(false)` -- unlike
        // Balancer's `.unwrap_or(true)` -- so no allowlist means the provider
        // is never offered, however well `aave_pool` is configured and probed
        // at startup. Measured 2026-09-03: `no_flashloan_provider` was 438 of
        // 850 prepared candidates, because only the two tokens on Base's
        // Balancer list could fund anything at all.
        //
        // Still `None` unless the operator sets the list. That preserves
        // today's behaviour exactly, and it should: enabling a provider that
        // charges a fee and has never executed is a decision to make
        // deliberately, not one to inherit from a default.
        aave_flashloan_tokens,
        erc3156_flashloan_tokens: None,
        univ2_flashloan_tokens: None,
        univ3_flashloan_tokens: None,
        tokens,
        ws_endpoints: registry_chain.rpc_ws.clone(),
        univ3_validation: validation,
        registry_version,
        gas_model: default_gas_model(name),
        gas_rpc_method: None,
        arbitrum_l1_per_byte_wei: None,
        arbitrum_l1_per_byte_max_deviation_bps: None,
    };

    validate_non_placeholder_rpc_endpoints(&cfg)?;
    Ok(cfg)
}

#[derive(Clone, Copy, Debug)]
enum ConfigSource {
    Ops,
    Env,
    Registry,
    Default,
}

pub fn load_chain_from_sources(
    name: &str,
    registry_version: Option<String>,
    registry_chain: Option<&RegistryChain>,
    ops_chain: Option<&OpsChainOverrides>,
) -> Result<ChainCfg> {
    let default_meta = default_chain_meta(name)?;
    let env_prefix = select_string(
        name,
        "env_prefix",
        ops_chain.and_then(|chain| chain.env_prefix.clone()),
        registry_chain.and_then(|chain| chain.env_prefix.clone()),
        Some(default_meta.env_prefix.to_string()),
    )?;

    let chain_id = select_u64(
        name,
        "chain_id",
        ops_chain.and_then(|chain| chain.chain_id),
        registry_chain.and_then(|chain| {
            if chain.chain_id == 0 {
                None
            } else {
                Some(chain.chain_id)
            }
        }),
        Some(default_meta.chain_id),
    )?;

    let env_keys = ChainEnvConfig {
        env_prefix: "",
        chain_id: chain_id.value,
        rpc_env: "",
        rpc_list_env: "",
        ws_env: "",
        quoter_env: "",
        factory_env: "",
        router_env: "",
        bal_vault_env: "",
        aave_pool_env: "",
        bal_flashloan_env: "",
        tokens_env: "",
        gas_model_env: "",
        gas_rpc_method_env: "",
        arbitrum_l1_per_byte_env: "",
        arbitrum_l1_per_byte_max_dev_env: "",
    };
    let env_prefix = env_prefix.value;
    let env_keys = env_keys.with_prefix(&env_prefix);

    let gas_model = select_gas_model(
        name,
        ops_chain.and_then(|chain| chain.gas_model.clone()),
        parse_gas_model_from_env(&env_keys.gas_model_env),
        default_gas_model(name),
    );
    let gas_rpc_method = select_string_optional(
        name,
        "gas_rpc_method",
        ops_chain.and_then(|chain| chain.gas_rpc_method.clone()),
        std::env::var(&env_keys.gas_rpc_method_env).ok(),
    );
    let arbitrum_l1_per_byte_wei = select_u256_optional(
        name,
        "arbitrum_l1_per_byte_wei",
        ops_chain.and_then(|chain| chain.arbitrum_l1_per_byte_wei.clone()),
        parse_u256_env(&env_keys.arbitrum_l1_per_byte_env)?,
    )?;
    let arbitrum_l1_per_byte_max_deviation_bps = select_u32_optional(
        name,
        "arbitrum_l1_per_byte_max_deviation_bps",
        ops_chain.and_then(|chain| chain.arbitrum_l1_per_byte_max_deviation_bps),
        parse_u32_env(&env_keys.arbitrum_l1_per_byte_max_dev_env)?,
    );

    let ops_rpc = ops_chain
        .filter(|chain| !chain.rpc_http_urls.is_empty())
        .map(|chain| chain.rpc_http_urls.clone());
    let env_rpc = parse_rpc_env_endpoints(&env_keys)?;
    let registry_rpc = registry_chain.map(|chain| chain.rpc_http.clone());

    let (rpc, rpc_fallbacks) =
        select_rpc_endpoints(name, "rpc_endpoints", ops_rpc, env_rpc, registry_rpc)?;

    let ops_ws = ops_chain
        .filter(|chain| !chain.rpc_ws_urls.is_empty())
        .map(|chain| chain.rpc_ws_urls.clone());
    let env_ws = parse_ws_env_endpoints(&env_keys);
    let registry_ws = registry_chain
        .map(|chain| chain.rpc_ws.clone())
        .filter(|urls| !urls.is_empty());
    let ws_endpoints = select_ws_endpoints(name, "ws_endpoints", ops_ws, env_ws, registry_ws);

    let univ3_quoter = select_address(
        name,
        "univ3_quoter",
        ops_chain.and_then(|chain| chain.univ3_quoter.clone()),
        env_addr_optional(&env_keys.quoter_env)?,
        registry_chain.and_then(|chain| chain.univ3_quoter.clone()),
        None,
    )?;

    let univ3_router = select_address(
        name,
        "univ3_router",
        ops_chain.and_then(|chain| chain.univ3_router.clone()),
        env_addr_optional_with_aliases(
            &env_keys.router_env,
            std::slice::from_ref(&env_keys.legacy_router_env),
        )?,
        registry_chain.and_then(|chain| chain.univ3_router.clone()),
        None,
    )?;

    let univ3_factory = select_address(
        name,
        "univ3_factory",
        ops_chain.and_then(|chain| chain.univ3_factory.clone()),
        env_addr_optional(&env_keys.factory_env)?,
        registry_chain.and_then(|chain| chain.univ3_factory.clone()),
        Some(default_univ3_factory()),
    )?;

    let bal_vault = select_optional_address(
        name,
        "bal_vault",
        ops_chain.and_then(|chain| chain.balancer_vault.clone()),
        env_addr_optional_with_aliases(
            &env_keys.bal_vault_env,
            std::slice::from_ref(&env_keys.legacy_bal_vault_env),
        )?,
        registry_chain.and_then(|chain| chain.bal_vault.clone()),
    )?;

    let aave_pool = select_optional_address(
        name,
        "aave_pool",
        ops_chain.and_then(|chain| chain.aave_pool.clone()),
        env_addr_optional(&env_keys.aave_pool_env)?,
        registry_chain.and_then(|chain| chain.aave_pool.clone()),
    )?;

    let mut bal_flashloan_tokens = select_token_list(
        name,
        "bal_flashloan_tokens",
        ops_chain
            .filter(|chain| !chain.bal_flashloan_tokens.is_empty())
            .map(|chain| chain.bal_flashloan_tokens.clone()),
        parse_optional_token_list(&env_keys.bal_flashloan_env)?,
        registry_chain
            .map(|chain| chain.bal_flashloan_tokens.clone())
            .filter(|tokens| !tokens.is_empty()),
    )?;

    if bal_vault.is_none() {
        ensure!(
            bal_flashloan_tokens
                .as_ref()
                .is_none_or(|tokens| tokens.is_empty()),
            "bal_flashloan_tokens requires bal_vault to be configured"
        );

        // Prevent implicitly enabling Balancer flashloans with an Address::zero vault.
        bal_flashloan_tokens = Some(Vec::new());
    }

    let aave_flashloan_tokens = select_token_list(
        name,
        "aave_flashloan_tokens",
        ops_chain
            .filter(|chain| !chain.aave_flashloan_tokens.is_empty())
            .map(|chain| chain.aave_flashloan_tokens.clone()),
        None,
        None,
    )?;

    let erc3156_flashloan_tokens = select_token_list(
        name,
        "erc3156_flashloan_tokens",
        ops_chain
            .filter(|chain| !chain.erc3156_flashloan_tokens.is_empty())
            .map(|chain| chain.erc3156_flashloan_tokens.clone()),
        None,
        None,
    )?;

    let univ2_flashloan_tokens = select_token_list(
        name,
        "univ2_flashloan_tokens",
        ops_chain
            .filter(|chain| !chain.univ2_flashloan_tokens.is_empty())
            .map(|chain| chain.univ2_flashloan_tokens.clone()),
        None,
        None,
    )?;

    let univ3_flashloan_tokens = select_token_list(
        name,
        "univ3_flashloan_tokens",
        ops_chain
            .filter(|chain| !chain.univ3_flashloan_tokens.is_empty())
            .map(|chain| chain.univ3_flashloan_tokens.clone()),
        None,
        None,
    )?;

    let tokens = {
        let env_key = &env_keys.tokens_env;
        let env_tokens = parse_optional_token_list(env_key)?;
        let registry_tokens = registry_chain
            .map(|chain| chain.tokens.clone())
            .filter(|tokens| !tokens.is_empty());
        select_token_list(name, "tokens", None, env_tokens, registry_tokens)?.unwrap_or_default()
    };

    let validation = parse_validation_override(name, &env_prefix)?;

    let cfg = ChainCfg {
        name: name.to_string(),
        env_prefix,
        chain_id: chain_id.value,
        rpc,
        rpc_fallbacks,
        univ3_quoter: univ3_quoter.value,
        univ3_factory: univ3_factory.value,
        univ3_router: univ3_router.value,
        bal_vault: bal_vault.unwrap_or_else(Address::zero),
        aave_pool,
        bal_flashloan_tokens,
        aave_flashloan_tokens,
        erc3156_flashloan_tokens,
        univ2_flashloan_tokens,
        univ3_flashloan_tokens,
        tokens,
        ws_endpoints,
        univ3_validation: validation,
        registry_version,
        gas_model,
        gas_rpc_method,
        arbitrum_l1_per_byte_wei,
        arbitrum_l1_per_byte_max_deviation_bps,
    };

    validate_non_placeholder_rpc_endpoints(&cfg)?;
    Ok(cfg)
}

/// Validate chain configuration before runtime launch.
///
/// Precedence for addresses matches load_chain_from_sources and runtime resolution:
/// ops inputs > registry > env vars.
pub fn validate_chain_cfg(
    cfg: &ChainCfg,
    ops_chain: Option<&OpsChainOverrides>,
    chain_inputs: Option<&ChainInputs>,
) -> Result<()> {
    let mut missing = Vec::new();

    let http_endpoints: Vec<String> = cfg
        .rpc_endpoints()
        .iter()
        .filter_map(|url| coerce_http_url(url))
        .collect();
    if http_endpoints.is_empty() {
        missing.push(format!(
            "rpc endpoints (set {}_RPC_URL(S) or ops inputs rpc_http_urls)",
            cfg.env_prefix
        ));
    }

    resolve_required_address_for_validation(
        cfg,
        "EXECUTOR_ADDRESS",
        &[],
        ops_chain.and_then(|chain| chain.executor_address.clone()),
        &mut missing,
    )?;
    resolve_required_address_for_validation(
        cfg,
        "PERMIT2_ADDRESS",
        &["PERMIT2"],
        ops_chain.and_then(|chain| chain.permit2_address.clone()),
        &mut missing,
    )?;

    let mut needs_univ3 = false;
    let mut needs_balancer_vault = false;
    let mut needs_aave_pool = false;
    let mut needs_erc3156 = false;

    if let Some(chain_inputs) = chain_inputs {
        for venue in chain_inputs.venues.iter() {
            let Some(kind) = venue.kind.as_ref() else {
                continue;
            };
            match kind {
                VenueKind::Univ2Like => {
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "factory",
                        venue.factory.as_ref(),
                        &mut missing,
                    )?;
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "router",
                        venue.router.as_ref(),
                        &mut missing,
                    )?;
                }
                VenueKind::Univ3Like => {
                    needs_univ3 = true;
                }
                VenueKind::Univ4 => {
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "pool_manager",
                        venue.pool_manager.as_ref(),
                        &mut missing,
                    )?;
                }
                VenueKind::CurveLike => {
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "registry",
                        venue.registry.as_ref(),
                        &mut missing,
                    )?;
                }
                VenueKind::BalancerLike => {
                    needs_balancer_vault = true;
                }
                VenueKind::SolidlyV2Like => {
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "factory",
                        venue.factory.as_ref(),
                        &mut missing,
                    )?;
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "router",
                        venue.router.as_ref(),
                        &mut missing,
                    )?;
                }
                VenueKind::SlipstreamLike => {
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "factory",
                        venue.factory.as_ref(),
                        &mut missing,
                    )?;
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "router",
                        venue.router.as_ref(),
                        &mut missing,
                    )?;
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "quoter",
                        venue.quoter.as_ref(),
                        &mut missing,
                    )?;
                }
                VenueKind::GenericRouter => {
                    require_ops_address(
                        cfg,
                        &venue.name,
                        "router",
                        venue.router.as_ref(),
                        &mut missing,
                    )?;
                }
            }
        }

        for flashloan in chain_inputs.flashloans.iter() {
            let Some(kind) = flashloan.kind.as_ref() else {
                continue;
            };
            match kind {
                FlashloanKind::AaveV3Like => needs_aave_pool = true,
                FlashloanKind::BalancerVaultLike => needs_balancer_vault = true,
                FlashloanKind::Erc3156Like => needs_erc3156 = true,
                FlashloanKind::Univ2Flashswap | FlashloanKind::Univ3Flash => {}
            }
        }
    }

    if needs_univ3 {
        require_cfg_address(cfg, "univ3_quoter", cfg.univ3_quoter, &mut missing)?;
        require_cfg_address(cfg, "univ3_router", cfg.univ3_router, &mut missing)?;
        require_cfg_address(cfg, "univ3_factory", cfg.univ3_factory, &mut missing)?;
    }
    if needs_balancer_vault {
        require_cfg_address(cfg, "bal_vault", cfg.bal_vault, &mut missing)?;
    }
    if needs_aave_pool && (cfg.aave_pool.is_none() || cfg.aave_pool == Some(Address::zero())) {
        missing.push(format!(
            "aave_pool (set {}_AAVE_POOL or ops inputs flashloans)",
            cfg.env_prefix
        ));
    }
    if needs_erc3156 {
        let lender_key = "ERC3156_LENDER";
        let mut ops_lenders = Vec::new();
        if let Some(chain_inputs) = chain_inputs {
            for (idx, flashloan) in chain_inputs.flashloans.iter().enumerate() {
                if matches!(flashloan.kind, Some(FlashloanKind::Erc3156Like)) {
                    if let Some(raw) = flashloan.lender.as_ref() {
                        if !raw.trim().is_empty() {
                            ops_lenders.push((
                                format!("{}.flashloans[{idx}].lender", cfg.name),
                                raw.clone(),
                            ));
                        }
                    }
                }
            }
        }

        if ops_lenders.is_empty() {
            let lender = std::env::var(lender_key)
                .ok()
                .filter(|value| !value.trim().is_empty());
            if let Some(raw) = lender {
                let addr = parse_address(&raw, lender_key)?;
                ensure!(!addr.is_zero(), "{lender_key} must not be the zero address");
            } else {
                missing.push(format!(
                    "{lender_key} (required for ERC3156 flashloans; set env var or ops inputs flashloans[].lender)"
                ));
            }
        } else {
            for (label, raw) in ops_lenders {
                let addr = parse_address(&raw, &label)?;
                ensure!(!addr.is_zero(), "{label} must not be the zero address");
            }
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "missing required configuration for {}: {}",
            cfg.name,
            missing.join(", ")
        ))
    }
}

fn resolve_required_address_for_validation(
    cfg: &ChainCfg,
    suffix: &str,
    legacy_suffixes: &[&str],
    ops_value: Option<String>,
    missing: &mut Vec<String>,
) -> Result<()> {
    let env_key = format!("{}_{}", cfg.env_prefix, suffix);
    let mut env_keys = Vec::with_capacity(1 + legacy_suffixes.len());
    env_keys.push(env_key.clone());
    env_keys.extend(
        legacy_suffixes
            .iter()
            .map(|legacy| format!("{}_{}", cfg.env_prefix, legacy)),
    );
    let env_value = env_keys.iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    });
    if ops_value.is_some() && env_value.is_some() {
        warn!(
            chain = cfg.name.as_str(),
            env_prefix = cfg.env_prefix.as_str(),
            field = suffix,
            chosen = ?InputSource::Ops,
            "config source conflict; using ops inputs value",
        );
    }

    if let Some(raw) = ops_value {
        let addr = parse_address(&raw, &format!("{}.{}", cfg.name, suffix))?;
        ensure!(
            !addr.is_zero(),
            "{}.{} must not be the zero address",
            cfg.name,
            suffix
        );
        return Ok(());
    }
    if let Some(raw) = env_value {
        let addr = parse_address(&raw, &env_key)?;
        ensure!(!addr.is_zero(), "{env_key} must not be the zero address");
        return Ok(());
    }

    if legacy_suffixes.is_empty() {
        missing.push(format!("{suffix} (set {env_key} or ops inputs)"));
    } else {
        missing.push(format!(
            "{suffix} (set {env_key} or {} or ops inputs)",
            legacy_suffixes
                .iter()
                .map(|legacy| format!("{}_{}", cfg.env_prefix, legacy))
                .collect::<Vec<_>>()
                .join("/")
        ));
    }
    Ok(())
}

fn require_cfg_address(
    cfg: &ChainCfg,
    label: &str,
    addr: Address,
    missing: &mut Vec<String>,
) -> Result<()> {
    if addr.is_zero() {
        missing.push(format!("{} (resolved for {})", label, cfg.name));
    }
    Ok(())
}

fn require_ops_address(
    cfg: &ChainCfg,
    venue: &str,
    field: &str,
    value: Option<&String>,
    missing: &mut Vec<String>,
) -> Result<()> {
    let Some(raw) = value.filter(|value| !value.trim().is_empty()) else {
        missing.push(format!("{}.venues[{}].{}", cfg.name, venue, field));
        return Ok(());
    };
    let addr = parse_address(raw, &format!("{}.venues[{}].{}", cfg.name, venue, field))?;
    ensure!(
        !addr.is_zero(),
        "{}.venues[{}].{} must not be the zero address",
        cfg.name,
        venue,
        field
    );
    Ok(())
}

fn parse_rpc_endpoints(primary_key: &str, list_key: &str) -> Result<(String, Vec<String>)> {
    if let Ok(raw) = std::env::var(list_key) {
        let mut urls = parse_endpoint_list(&raw);
        if urls.is_empty() {
            return Err(anyhow!(
                "environment variable {list_key} must provide at least one rpc endpoint",
            ));
        }
        let primary = urls.remove(0);
        Ok((primary, urls))
    } else {
        let primary_raw = env_str(primary_key)?;
        let mut urls = parse_endpoint_list(&primary_raw);
        let primary = if urls.is_empty() {
            primary_raw
        } else {
            urls.remove(0)
        };

        let fallback_key = format!("{primary_key}_FALLBACKS");
        let mut fallbacks = std::env::var(&fallback_key)
            .ok()
            .map(|raw| parse_endpoint_list(&raw))
            .unwrap_or_default();
        fallbacks.splice(0..0, urls);

        Ok((primary, fallbacks))
    }
}

fn parse_rpc_env_endpoints(env: &ChainEnvConfigOwned) -> Result<Option<(String, Vec<String>)>> {
    let list_var = std::env::var(&env.rpc_list_env).ok();
    let primary_var = std::env::var(&env.rpc_env).ok();

    if list_var.is_none() && primary_var.is_none() {
        return Ok(None);
    }

    let (rpc, rpc_fallbacks) = if let Some(raw) = list_var {
        let mut urls = parse_endpoint_list(&raw);
        if urls.is_empty() {
            return Err(anyhow!(
                "environment variable {} must provide at least one rpc endpoint",
                env.rpc_list_env
            ));
        }
        let primary = urls.remove(0);
        (primary, urls)
    } else {
        let primary_raw = env_str(&env.rpc_env)?;
        let mut urls = parse_endpoint_list(&primary_raw);
        let primary = if urls.is_empty() {
            primary_raw
        } else {
            urls.remove(0)
        };
        let fallback_key = format!("{}_FALLBACKS", env.rpc_env);
        let mut fallbacks = std::env::var(&fallback_key)
            .ok()
            .map(|raw| parse_endpoint_list(&raw))
            .unwrap_or_default();
        fallbacks.splice(0..0, urls);
        (primary, fallbacks)
    };

    Ok(Some((rpc, rpc_fallbacks)))
}

fn select_rpc_endpoints(
    chain: &str,
    field: &str,
    ops: Option<Vec<String>>,
    env: Option<(String, Vec<String>)>,
    registry: Option<Vec<String>>,
) -> Result<(String, Vec<String>)> {
    let ops = ops.map(expand_endpoint_list);
    let registry = registry.map(expand_endpoint_list);
    let ops_present = ops.as_ref().map(|rpc| !rpc.is_empty()).unwrap_or(false);
    let env_present = env.is_some();
    let registry_present = registry
        .as_ref()
        .map(|rpc| !rpc.is_empty())
        .unwrap_or(false);
    let sources = sources_present(ops_present, env_present, registry_present, false);

    let (rpc, rpc_fallbacks) = if let Some(rpc) = ops {
        let mut iter = rpc.into_iter();
        let primary = iter
            .next()
            .ok_or_else(|| anyhow!("ops inputs missing rpc_http_urls for {chain}"))?;
        let fallbacks = iter.collect();
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        (primary, fallbacks)
    } else if let Some(rpc) = registry {
        let mut iter = rpc.into_iter();
        let primary = iter
            .next()
            .ok_or_else(|| anyhow!("registry missing rpc_http endpoint for {chain}"))?;
        let fallbacks = iter.collect();
        warn_conflict(chain, field, ConfigSource::Registry, &sources);
        (primary, fallbacks)
    } else if let Some((rpc, fallbacks)) = env {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        (rpc, fallbacks)
    } else {
        return Err(anyhow!("no rpc endpoints configured for {chain}"));
    };

    Ok((rpc, rpc_fallbacks))
}

fn parse_ws_env_endpoints(env: &ChainEnvConfigOwned) -> Option<Vec<String>> {
    std::env::var(&env.ws_env)
        .ok()
        .map(|raw| parse_endpoint_list(&raw))
}

fn expand_endpoint_list(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .flat_map(|entry| parse_endpoint_list(&entry))
        .collect()
}

fn select_ws_endpoints(
    chain: &str,
    field: &str,
    ops: Option<Vec<String>>,
    env: Option<Vec<String>>,
    registry: Option<Vec<String>>,
) -> Vec<String> {
    let ops = ops.map(expand_endpoint_list);
    let env = env.map(expand_endpoint_list);
    let registry = registry.map(expand_endpoint_list);
    let ops_present = ops.as_ref().map(|urls| !urls.is_empty()).unwrap_or(false);
    let env_present = env.as_ref().map(|urls| !urls.is_empty()).unwrap_or(false);
    let registry_present = registry
        .as_ref()
        .map(|urls| !urls.is_empty())
        .unwrap_or(false);
    let sources = sources_present(ops_present, env_present, registry_present, false);

    if let Some(urls) = ops.filter(|urls| !urls.is_empty()) {
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return urls;
    }
    if let Some(urls) = registry.filter(|urls| !urls.is_empty()) {
        warn_conflict(chain, field, ConfigSource::Registry, &sources);
        return urls;
    }
    if let Some(urls) = env.filter(|urls| !urls.is_empty()) {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        return urls;
    }
    Vec::new()
}

struct ChainEnvConfigOwned {
    rpc_env: String,
    rpc_list_env: String,
    ws_env: String,
    quoter_env: String,
    factory_env: String,
    router_env: String,
    legacy_router_env: String,
    bal_vault_env: String,
    legacy_bal_vault_env: String,
    aave_pool_env: String,
    bal_flashloan_env: String,
    tokens_env: String,
    gas_model_env: String,
    gas_rpc_method_env: String,
    arbitrum_l1_per_byte_env: String,
    arbitrum_l1_per_byte_max_dev_env: String,
}

impl ChainEnvConfig {
    fn with_prefix(&self, prefix: &str) -> ChainEnvConfigOwned {
        ChainEnvConfigOwned {
            rpc_env: format!("{prefix}_RPC_URL"),
            rpc_list_env: format!("{prefix}_RPC_URLS"),
            ws_env: format!("{prefix}_WS_RPC_URLS"),
            quoter_env: format!("{prefix}_UNIV3_QUOTER"),
            factory_env: format!("{prefix}_UNIV3_FACTORY"),
            router_env: format!("{prefix}_UNIV3_ROUTER"),
            legacy_router_env: format!("{prefix}_SWAPROUTER02"),
            bal_vault_env: format!("{prefix}_BAL_VAULT"),
            legacy_bal_vault_env: format!("{prefix}_BALANCER_VAULT"),
            aave_pool_env: format!("{prefix}_AAVE_POOL"),
            bal_flashloan_env: format!("{prefix}_BAL_FLASHLOAN_TOKENS"),
            tokens_env: format!("{prefix}_TOKENS"),
            gas_model_env: format!("{prefix}_GAS_MODEL"),
            gas_rpc_method_env: format!("{prefix}_GAS_RPC_METHOD"),
            arbitrum_l1_per_byte_env: format!("{prefix}_ARBITRUM_L1_PER_BYTE_WEI"),
            arbitrum_l1_per_byte_max_dev_env: format!(
                "{prefix}_ARBITRUM_L1_PER_BYTE_MAX_DEVIATION_BPS"
            ),
        }
    }
}

struct Selected<T> {
    value: T,
    source: ConfigSource,
}

fn select_string(
    chain: &str,
    field: &str,
    ops: Option<String>,
    registry: Option<String>,
    default: Option<String>,
) -> Result<Selected<String>> {
    let ops_present = ops
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let registry_present = registry
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let default_present = default.is_some();
    let sources = sources_present(ops_present, false, registry_present, default_present);

    if let Some(value) = ops.filter(|value| !value.trim().is_empty()) {
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Ok(Selected {
            value,
            source: ConfigSource::Ops,
        });
    }
    if let Some(value) = registry.filter(|value| !value.trim().is_empty()) {
        warn_conflict(chain, field, ConfigSource::Registry, &sources);
        return Ok(Selected {
            value,
            source: ConfigSource::Registry,
        });
    }
    if let Some(value) = default {
        warn_conflict(chain, field, ConfigSource::Default, &sources);
        return Ok(Selected {
            value,
            source: ConfigSource::Default,
        });
    }
    Err(anyhow!("missing required field {field} for {chain}"))
}

fn select_string_optional(
    chain: &str,
    field: &str,
    ops: Option<String>,
    env: Option<String>,
) -> Option<String> {
    let ops_present = ops
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let env_present = env
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let sources = sources_present(ops_present, env_present, false, false);
    if let Some(value) = ops.filter(|value| !value.trim().is_empty()) {
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Some(value);
    }
    if let Some(value) = env.filter(|value| !value.trim().is_empty()) {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        return Some(value);
    }
    None
}

fn select_gas_model(
    chain: &str,
    ops: Option<GasModel>,
    env: Option<GasModel>,
    default: GasModel,
) -> GasModel {
    let ops_present = ops.is_some();
    let env_present = env.is_some();
    let sources = sources_present(ops_present, env_present, false, true);
    if let Some(model) = ops {
        warn_conflict(chain, "gas_model", ConfigSource::Ops, &sources);
        return model;
    }
    if let Some(model) = env {
        warn_conflict(chain, "gas_model", ConfigSource::Env, &sources);
        return model;
    }
    warn_conflict(chain, "gas_model", ConfigSource::Default, &sources);
    default
}

fn select_u256_optional(
    chain: &str,
    field: &str,
    ops: Option<String>,
    env: Option<U256>,
) -> Result<Option<U256>> {
    let ops_present = ops
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let env_present = env.is_some();
    let sources = sources_present(ops_present, env_present, false, false);
    if let Some(value) = ops.filter(|value| !value.trim().is_empty()) {
        let parsed =
            U256::from_dec_str(&value).with_context(|| format!("invalid {field} value"))?;
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Ok(Some(parsed));
    }
    if let Some(value) = env {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        return Ok(Some(value));
    }
    Ok(None)
}

fn select_u32_optional(
    chain: &str,
    field: &str,
    ops: Option<u32>,
    env: Option<u32>,
) -> Option<u32> {
    let ops_present = ops.is_some();
    let env_present = env.is_some();
    let sources = sources_present(ops_present, env_present, false, false);
    if let Some(value) = ops {
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Some(value);
    }
    if let Some(value) = env {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        return Some(value);
    }
    None
}

fn select_u64(
    chain: &str,
    field: &str,
    ops: Option<u64>,
    registry: Option<u64>,
    default: Option<u64>,
) -> Result<Selected<u64>> {
    let ops_present = ops.is_some();
    let registry_present = registry.is_some();
    let default_present = default.is_some();
    let sources = sources_present(ops_present, false, registry_present, default_present);

    if let Some(value) = ops {
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Ok(Selected {
            value,
            source: ConfigSource::Ops,
        });
    }
    if let Some(value) = registry {
        warn_conflict(chain, field, ConfigSource::Registry, &sources);
        return Ok(Selected {
            value,
            source: ConfigSource::Registry,
        });
    }
    if let Some(value) = default {
        warn_conflict(chain, field, ConfigSource::Default, &sources);
        return Ok(Selected {
            value,
            source: ConfigSource::Default,
        });
    }
    Err(anyhow!("missing required field {field} for {chain}"))
}

fn select_address(
    chain: &str,
    field: &str,
    ops: Option<String>,
    env: Option<Address>,
    registry: Option<String>,
    default: Option<Address>,
) -> Result<Selected<Address>> {
    let ops_present = ops
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let env_present = env.is_some();
    let registry_present = registry
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let default_present = default.is_some();
    let sources = sources_present(ops_present, env_present, registry_present, default_present);

    if let Some(value) = ops.filter(|value| !value.trim().is_empty()) {
        let addr = parse_address(&value, field)?;
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Ok(Selected {
            value: addr,
            source: ConfigSource::Ops,
        });
    }
    if let Some(value) = registry.filter(|value| !value.trim().is_empty()) {
        let addr = parse_address(&value, field)?;
        warn_conflict(chain, field, ConfigSource::Registry, &sources);
        return Ok(Selected {
            value: addr,
            source: ConfigSource::Registry,
        });
    }
    if let Some(addr) = env {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        return Ok(Selected {
            value: addr,
            source: ConfigSource::Env,
        });
    }
    if let Some(addr) = default {
        warn_conflict(chain, field, ConfigSource::Default, &sources);
        return Ok(Selected {
            value: addr,
            source: ConfigSource::Default,
        });
    }
    Err(anyhow!("missing required field {field} for {chain}"))
}

fn select_optional_address(
    chain: &str,
    field: &str,
    ops: Option<String>,
    env: Option<Address>,
    registry: Option<String>,
) -> Result<Option<Address>> {
    let ops_present = ops
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let env_present = env.is_some();
    let registry_present = registry
        .as_ref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let sources = sources_present(ops_present, env_present, registry_present, false);

    if let Some(value) = ops.filter(|value| !value.trim().is_empty()) {
        let addr = parse_address(&value, field)?;
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Ok(Some(addr));
    }
    if let Some(value) = registry.filter(|value| !value.trim().is_empty()) {
        let addr = parse_address(&value, field)?;
        warn_conflict(chain, field, ConfigSource::Registry, &sources);
        return Ok(Some(addr));
    }
    if let Some(addr) = env {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        return Ok(Some(addr));
    }
    Ok(None)
}

fn select_token_list(
    chain: &str,
    field: &str,
    ops: Option<Vec<String>>,
    env: Option<Vec<Address>>,
    registry: Option<Vec<String>>,
) -> Result<Option<Vec<Address>>> {
    let ops_present = ops.as_ref().map(|value| !value.is_empty()).unwrap_or(false);
    let env_present = env.as_ref().map(|value| !value.is_empty()).unwrap_or(false);
    let registry_present = registry
        .as_ref()
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    let sources = sources_present(ops_present, env_present, registry_present, false);

    if let Some(tokens) = ops.filter(|tokens| !tokens.is_empty()) {
        let parsed = tokens
            .iter()
            .map(|raw| parse_address(raw, field))
            .collect::<Result<Vec<_>>>()?;
        warn_conflict(chain, field, ConfigSource::Ops, &sources);
        return Ok(Some(parsed));
    }
    if let Some(tokens) = registry.filter(|tokens| !tokens.is_empty()) {
        let parsed = tokens
            .iter()
            .map(|raw| parse_address(raw, field))
            .collect::<Result<Vec<_>>>()?;
        warn_conflict(chain, field, ConfigSource::Registry, &sources);
        return Ok(Some(parsed));
    }
    if let Some(tokens) = env.filter(|tokens| !tokens.is_empty()) {
        warn_conflict(chain, field, ConfigSource::Env, &sources);
        return Ok(Some(tokens));
    }
    Ok(None)
}

fn sources_present(ops: bool, env: bool, registry: bool, default: bool) -> Vec<ConfigSource> {
    let mut sources = Vec::new();
    if ops {
        sources.push(ConfigSource::Ops);
    }
    if registry {
        sources.push(ConfigSource::Registry);
    }
    if env {
        sources.push(ConfigSource::Env);
    }
    if default {
        sources.push(ConfigSource::Default);
    }
    sources
}

fn warn_conflict(chain: &str, field: &str, chosen: ConfigSource, sources: &[ConfigSource]) {
    if sources.len() > 1 {
        warn!(
            chain = chain,
            field = field,
            chosen = ?chosen,
            sources = ?sources,
            "config source conflict; using highest-precedence value",
        );
    }
}

struct ChainMeta {
    env_prefix: &'static str,
    chain_id: u64,
}

fn default_chain_meta(name: &str) -> Result<ChainMeta> {
    match name {
        "ethereum" => Ok(ChainMeta {
            env_prefix: "ETH",
            chain_id: 1,
        }),
        "arbitrum" => Ok(ChainMeta {
            env_prefix: "ARB",
            chain_id: 42_161,
        }),
        "optimism" => Ok(ChainMeta {
            env_prefix: "OPT",
            chain_id: 10,
        }),
        "base" => Ok(ChainMeta {
            env_prefix: "BASE",
            chain_id: 8_453,
        }),
        "polygon" => Ok(ChainMeta {
            env_prefix: "POLYGON",
            chain_id: 137,
        }),
        "abstract" => Ok(ChainMeta {
            env_prefix: "ABSTRACT",
            chain_id: 2_741,
        }),
        "ink" => Ok(ChainMeta {
            env_prefix: "INK",
            chain_id: 763_373,
        }),
        "linea" => Ok(ChainMeta {
            env_prefix: "LINEA",
            chain_id: 59_144,
        }),
        "mantle" => Ok(ChainMeta {
            env_prefix: "MANTLE",
            chain_id: 5_000,
        }),
        "scroll" => Ok(ChainMeta {
            env_prefix: "SCROLL",
            chain_id: 534_352,
        }),
        _ => Err(anyhow!("unknown chain")),
    }
}

fn default_gas_model(name: &str) -> GasModel {
    match name {
        "arbitrum" => GasModel::Arbitrum,
        "optimism" | "base" | "ink" | "mantle" => GasModel::OpStack,
        "linea" => GasModel::LineaEstimateGas,
        "abstract" => GasModel::CustomRpcMethod,
        _ => GasModel::Eip1559,
    }
}

fn parse_gas_model_from_env(key: &str) -> Option<GasModel> {
    std::env::var(key)
        .ok()
        .and_then(|raw| match raw.to_ascii_lowercase().as_str() {
            "eip1559" => Some(GasModel::Eip1559),
            "legacy" => Some(GasModel::Legacy),
            "arbitrum" => Some(GasModel::Arbitrum),
            "op_stack" => Some(GasModel::OpStack),
            "linea_estimateGas" | "linea_estimategas" => Some(GasModel::LineaEstimateGas),
            "custom_rpc_method" => Some(GasModel::CustomRpcMethod),
            _ => None,
        })
}

fn parse_u256_env(key: &str) -> Result<Option<U256>> {
    match std::env::var(key) {
        Ok(raw) => {
            let parsed =
                U256::from_dec_str(&raw).with_context(|| format!("invalid {key} value"))?;
            Ok(Some(parsed))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(anyhow!("environment variable {key} contains invalid UTF-8"))
        }
    }
}

fn parse_u32_env(key: &str) -> Result<Option<u32>> {
    match std::env::var(key) {
        Ok(raw) => {
            let parsed = raw
                .parse::<u32>()
                .with_context(|| format!("invalid {key} value"))?;
            Ok(Some(parsed))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(anyhow!("environment variable {key} contains invalid UTF-8"))
        }
    }
}

fn parse_ws_endpoints(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|raw| parse_endpoint_list(&raw))
        .unwrap_or_default()
}

fn parse_token_list(key: &str) -> Result<Vec<Address>> {
    let raw = match std::env::var(key) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(Vec::new()),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(anyhow!(
                "environment variable {key} contains invalid UTF-8 and cannot be parsed"
            ))
        }
    };
    let contents = maybe_expand_env_file(&raw, key)?;
    parse_token_list_contents(&contents, key)
}

fn parse_optional_token_list(key: &str) -> Result<Option<Vec<Address>>> {
    match std::env::var(key) {
        Ok(value) => {
            let contents = maybe_expand_env_file(&value, key)?;
            let tokens = parse_token_list_contents(&contents, key)?;
            Ok(Some(tokens))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(anyhow!(
            "environment variable {key} contains invalid UTF-8 and cannot be parsed"
        )),
    }
}

fn default_univ3_validation(chain: &str) -> Option<UniV3ValidationConfig> {
    let addr = |raw: &[u8; 20]| Address::from_slice(raw);
    match chain {
        "ethereum" => Some(UniV3ValidationConfig {
            token_in: addr(&hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")),
            token_out: addr(&hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")),
            fee: 500,
            amount_in: U256::from(DEFAULT_UNIV3_VALIDATION_AMOUNT_WEI),
        }),
        "arbitrum" => Some(UniV3ValidationConfig {
            token_in: addr(&hex!("82af49447d8a07e3bd95bd0d56f35241523fbab1")),
            token_out: addr(&hex!("af88d065e77c8cc2239327c5edb3a432268e5831")),
            fee: 500,
            amount_in: U256::from(DEFAULT_UNIV3_VALIDATION_AMOUNT_WEI),
        }),
        "optimism" => Some(UniV3ValidationConfig {
            token_in: addr(&hex!("4200000000000000000000000000000000000006")),
            token_out: addr(&hex!("0b2c639c533813f4aa9d7837caf62653d097ff85")),
            fee: 500,
            amount_in: U256::from(DEFAULT_UNIV3_VALIDATION_AMOUNT_WEI),
        }),
        "base" => Some(UniV3ValidationConfig {
            token_in: addr(&hex!("4200000000000000000000000000000000000006")),
            token_out: addr(&hex!("833589fcd6edb6e08f4c7c32d4f71b54bda02913")),
            fee: 500,
            amount_in: U256::from(DEFAULT_UNIV3_VALIDATION_AMOUNT_WEI),
        }),
        "polygon" => Some(UniV3ValidationConfig {
            token_in: addr(&hex!("0d500b1d8e8ef31e21c99d1db9a6444d3adf1270")),
            token_out: addr(&hex!("2791bca1f2de4661ed88a30c99a7a9449aa84174")),
            fee: 500,
            amount_in: U256::from(DEFAULT_UNIV3_VALIDATION_AMOUNT_WEI),
        }),
        _ => None,
    }
}

fn parse_validation_override(
    chain: &str,
    env_prefix: &str,
) -> Result<Option<UniV3ValidationConfig>> {
    let path_key = format!("{env_prefix}_UNIV3_VALIDATION_PATH");
    let amount_key = format!("{env_prefix}_UNIV3_VALIDATION_AMOUNT_WEI");
    if let Ok(raw) = std::env::var(&path_key) {
        let parts: Vec<_> = raw
            .split([',', ':', '>', ' '])
            .filter(|s| !s.is_empty())
            .collect();
        ensure!(
            parts.len() == 2 || parts.len() == 3,
            "{path_key} must be tokenIn,fee,tokenOut (3 parts)"
        );
        let token_in = parse_address(parts[0], &path_key)?;
        let (fee, token_out) = if parts.len() == 2 {
            warn!("{path_key} provided without fee; defaulting to 500");
            (500, parse_address(parts[1], &path_key)?)
        } else {
            let fee = parts[1]
                .parse::<u32>()
                .with_context(|| format!("{path_key} fee must be u32"))?;
            (fee, parse_address(parts[2], &path_key)?)
        };
        let amount_in = std::env::var(&amount_key)
            .ok()
            .and_then(|raw| U256::from_dec_str(&raw).ok())
            .unwrap_or_else(|| U256::from(1_000_000u64));

        return Ok(Some(UniV3ValidationConfig {
            token_in,
            token_out,
            fee,
            amount_in,
        }));
    }

    Ok(default_univ3_validation(chain))
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct TokenInfo {
    pub addr: Address,
    pub decimals: u8,
}

pub(crate) fn parse_token_list_contents(raw: &str, key: &str) -> Result<Vec<Address>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if trimmed.starts_with('[') {
        let entries: Vec<String> = json5::from_str(trimmed).with_context(|| {
            format!(
                "environment variable {key} referenced JSON/JSON5 token list that could not be parsed"
            )
        })?;
        return parse_addresses(entries, key);
    }

    parse_addresses(
        trimmed
            .split([',', '\n'])
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.starts_with('#'))
            .map(str::to_string),
        key,
    )
}

fn parse_addresses<I>(entries: I, key: &str) -> Result<Vec<Address>>
where
    I: IntoIterator<Item = String>,
{
    let mut addresses = Vec::new();
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        addresses.push(Address::from_str(trimmed).with_context(|| {
            format!("failed to parse address '{trimmed}' in environment variable {key}")
        })?);
    }
    Ok(addresses)
}

fn maybe_expand_env_file(raw: &str, key: &str) -> Result<String> {
    let mut trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    if let Some(stripped) = trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        trimmed = stripped;
    } else if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        trimmed = &trimmed[1..trimmed.len() - 1];
    }

    if let Some(stripped) = trimmed.strip_prefix('@') {
        let path = resolve_env_path(stripped);
        let contents = fs::read_to_string(&path).with_context(|| {
            format!(
                "failed to read token list from `{}` referenced by environment variable {key}",
                path.display()
            )
        })?;
        Ok(contents)
    } else {
        Ok(trimmed.to_string())
    }
}

fn resolve_env_path(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops_inputs::OpsChainOverrides;
    use once_cell::sync::Lazy;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    static CHAIN_ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn seed_chain_env(prefix: &str) {
        let rpc = format!("{prefix}_RPC_URL");
        let rpc_list = format!("{prefix}_RPC_URLS");
        let ws = format!("{prefix}_WS_RPC_URLS");
        let quoter = format!("{prefix}_UNIV3_QUOTER");
        let factory = format!("{prefix}_UNIV3_FACTORY");
        let router = format!("{prefix}_UNIV3_ROUTER");
        let legacy_router = format!("{prefix}_SWAPROUTER02");
        let bal_vault = format!("{prefix}_BAL_VAULT");
        let legacy_bal_vault = format!("{prefix}_BALANCER_VAULT");
        let aave_pool = format!("{prefix}_AAVE_POOL");
        let bal_flash_tokens = format!("{prefix}_BAL_FLASHLOAN_TOKENS");
        let tokens = format!("{prefix}_TOKENS");

        let endpoint = "https://example.org";
        std::env::set_var(&rpc, endpoint);
        std::env::remove_var(&rpc_list);
        std::env::set_var(&ws, "wss://example.org");
        std::env::set_var(&quoter, "0x0000000000000000000000000000000000000010");
        std::env::set_var(&factory, "0x0000000000000000000000000000000000000011");
        std::env::set_var(&router, "0x0000000000000000000000000000000000000012");
        std::env::remove_var(&legacy_router);
        std::env::set_var(&bal_vault, "0x0000000000000000000000000000000000000013");
        std::env::remove_var(&legacy_bal_vault);
        std::env::set_var(&aave_pool, "0x0000000000000000000000000000000000000014");
        std::env::set_var(
            &bal_flash_tokens,
            "0x0000000000000000000000000000000000000015",
        );
        std::env::set_var(
            &tokens,
            "0x0000000000000000000000000000000000000016,0x0000000000000000000000000000000000000017",
        );
    }

    fn clear_chain_env(prefix: &str) {
        for suffix in [
            "RPC_URL",
            "RPC_URLS",
            "WS_RPC_URLS",
            "UNIV3_QUOTER",
            "UNIV3_FACTORY",
            "UNIV3_ROUTER",
            "SWAPROUTER02",
            "BAL_VAULT",
            "BALANCER_VAULT",
            "AAVE_POOL",
            "BAL_FLASHLOAN_TOKENS",
            "UNIV3_VALIDATION_PATH",
            "UNIV3_VALIDATION_AMOUNT_WEI",
            "TOKENS",
        ] {
            std::env::remove_var(format!("{prefix}_{suffix}"));
        }
    }

    #[test]
    fn load_chain_from_env_supports_multi_chain_targets() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        let chains = vec![
            ("ethereum", "ETH", 1u64),
            ("arbitrum", "ARB", 42_161),
            ("optimism", "OPT", 10u64),
            ("base", "BASE", 8_453),
            ("polygon", "POLYGON", 137u64),
            ("abstract", "ABSTRACT", 2_741),
            ("ink", "INK", 763_373),
            ("linea", "LINEA", 59_144),
            ("mantle", "MANTLE", 5_000),
            ("scroll", "SCROLL", 534_352),
        ];

        for (name, prefix, chain_id) in chains {
            seed_chain_env(prefix);

            let cfg = load_chain_from_env(name).expect("chain should load from env");
            assert_eq!(cfg.chain_id, chain_id);
            assert_eq!(cfg.env_prefix, prefix);
            assert_eq!(cfg.rpc, "https://example.org");
            assert_eq!(cfg.ws_endpoints, vec!["wss://example.org".to_string()]);
            assert_eq!(cfg.tokens.len(), 2);

            clear_chain_env(prefix);
        }
    }

    #[test]
    fn endpoint_looks_placeholder_rejects_zero_bind_address() {
        assert!(endpoint_looks_placeholder("http://0.0.0.0:8546"));
    }

    #[test]
    fn address_is_known_placeholder_detects_scaffold_contract() {
        assert!(address_is_known_placeholder(
            "0xCfDAdA7D65e8e5B2564BDed4d79e0c084d595e90"
        ));
        assert!(!address_is_known_placeholder(
            "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2"
        ));
    }

    #[test]
    fn secret_looks_placeholder_detects_template_values() {
        assert!(secret_looks_placeholder("REPLACE_ME"));
        assert!(secret_looks_placeholder("REPLACE_WITH_NEW_ROTATED_KEY"));
        assert!(!secret_looks_placeholder(
            "0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356"
        ));
    }

    #[test]
    fn load_chain_from_env_rejects_placeholder_rpc_in_production_mode() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ETH");
        std::env::set_var("ARBOT_ENV", "production");

        let err = load_chain_from_env("ethereum").expect_err("placeholder rpc should be rejected");

        clear_chain_env("ETH");
        std::env::remove_var("ARBOT_ENV");

        assert!(
            format!("{err:#}").contains("production mode rejected placeholder rpc endpoint"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn load_chain_from_env_allows_placeholder_rpc_outside_production_mode() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ETH");
        std::env::remove_var("ARBOT_ENV");
        std::env::remove_var("APP_ENV");
        std::env::remove_var("NODE_ENV");
        std::env::remove_var("RUN_MODE");

        let cfg = load_chain_from_env("ethereum").expect("dev mode should allow placeholder rpc");
        clear_chain_env("ETH");

        assert_eq!(cfg.rpc, "https://example.org");
    }

    #[test]
    fn fork_dry_run_chain_id_mapping_stays_in_sync_with_chain_metadata() {
        let script = include_str!(concat!(env!("WORKSPACE_ROOT"), "/scripts/fork/run_integration_dry_run.sh"));
        let mut script_mapping = HashMap::<String, u64>::new();

        for line in script.lines() {
            let trimmed = line.trim();
            if let Some((name, rest)) = trimmed.split_once(") echo ") {
                if let Some(id_str) = rest.strip_suffix(" ;;") {
                    if let Ok(chain_id) = id_str.trim().parse::<u64>() {
                        script_mapping.insert(name.trim().to_string(), chain_id);
                    }
                }
            }
        }

        assert!(
            !script_mapping.is_empty(),
            "failed to parse dry-run chain mapping"
        );

        for (name, script_chain_id) in script_mapping {
            let meta = default_chain_meta(&name)
                .unwrap_or_else(|_| panic!("unsupported chain in dry-run mapping: {name}"));
            assert_eq!(
                script_chain_id, meta.chain_id,
                "chain_id drift for {name}: fork harness={script_chain_id}, runtime metadata={}",
                meta.chain_id
            );
        }
    }

    #[test]
    fn univ3_validation_can_be_overridden_via_env() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ETH");
        std::env::set_var(
            "ETH_UNIV3_VALIDATION_PATH",
            "0x0000000000000000000000000000000000000100,3000,0x0000000000000000000000000000000000000200",
        );
        std::env::set_var("ETH_UNIV3_VALIDATION_AMOUNT_WEI", "12345");

        let cfg = load_chain_from_env("ethereum").expect("chain should load from env");
        clear_chain_env("ETH");

        let validation = cfg.univ3_validation.expect("validation config present");
        assert_eq!(validation.fee, 3000);
        assert_eq!(validation.amount_in, U256::from(12345u64));
        let mut expected_in = [0u8; 20];
        expected_in[18] = 0x01;
        expected_in[19] = 0x00;
        let mut expected_out = [0u8; 20];
        expected_out[18] = 0x02;
        expected_out[19] = 0x00;
        assert_eq!(validation.token_in, Address::from(expected_in));
        assert_eq!(validation.token_out, Address::from(expected_out));
    }

    #[test]
    fn univ3_validation_allows_two_part_env_override() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ETH");
        std::env::set_var(
            "ETH_UNIV3_VALIDATION_PATH",
            "0x0000000000000000000000000000000000000100,0x0000000000000000000000000000000000000200",
        );

        let cfg = load_chain_from_env("ethereum").expect("chain should load from env");
        clear_chain_env("ETH");

        let validation = cfg.univ3_validation.expect("validation config present");
        assert_eq!(validation.fee, 500);
        let mut expected_in = [0u8; 20];
        expected_in[18] = 0x01;
        expected_in[19] = 0x00;
        let mut expected_out = [0u8; 20];
        expected_out[18] = 0x02;
        expected_out[19] = 0x00;
        assert_eq!(validation.token_in, Address::from(expected_in));
        assert_eq!(validation.token_out, Address::from(expected_out));
    }

    #[test]
    fn default_univ3_validation_uses_nontrivial_amount() {
        let chains = ["ethereum", "arbitrum", "optimism", "base", "polygon"];

        for chain in chains {
            let validation =
                default_univ3_validation(chain).expect("default validation should exist");
            assert_eq!(
                validation.amount_in,
                U256::from(DEFAULT_UNIV3_VALIDATION_AMOUNT_WEI)
            );
        }
    }

    #[test]
    fn registry_tokens_take_precedence_over_env() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        let registry_chain = RegistryChain {
            env_prefix: Some("ARB".into()),
            chain_id: 42_161,
            rpc_http: vec!["https://example.org".into()],
            univ3_quoter: Some("0x0000000000000000000000000000000000000010".into()),
            univ3_factory: Some("0x0000000000000000000000000000000000000011".into()),
            univ3_router: Some("0x0000000000000000000000000000000000000012".into()),
            bal_vault: Some("0x0000000000000000000000000000000000000013".into()),
            aave_pool: Some("0x0000000000000000000000000000000000000014".into()),
            tokens: vec![
                "0x0000000000000000000000000000000000000001".into(),
                "0x0000000000000000000000000000000000000002".into(),
            ],
            ..Default::default()
        };

        std::env::set_var(
            "ARB_TOKENS",
            "0x00000000000000000000000000000000000000AA,\
             0x00000000000000000000000000000000000000BB",
        );

        let cfg =
            load_chain_from_registry("arbitrum", Some("1.0.0".into()), &registry_chain).unwrap();

        std::env::remove_var("ARB_TOKENS");

        assert_eq!(cfg.tokens.len(), 2);
        assert_eq!(cfg.tokens[0], Address::from_low_u64_be(0x01));
        assert_eq!(cfg.tokens[1], Address::from_low_u64_be(0x02));
    }

    #[test]
    fn ops_chain_overrides_take_precedence() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ARB");

        let registry_chain = RegistryChain {
            env_prefix: Some("ARB".into()),
            chain_id: 42_161,
            rpc_http: vec!["https://registry.org".into()],
            rpc_ws: vec!["wss://registry.org".into()],
            univ3_quoter: Some("0x0000000000000000000000000000000000000010".into()),
            univ3_factory: Some("0x0000000000000000000000000000000000000011".into()),
            univ3_router: Some("0x0000000000000000000000000000000000000012".into()),
            bal_vault: Some("0x0000000000000000000000000000000000000013".into()),
            aave_pool: Some("0x0000000000000000000000000000000000000014".into()),
            tokens: vec!["0x00000000000000000000000000000000000000aa".into()],
            ..Default::default()
        };

        let ops_chain = OpsChainOverrides {
            univ2_flash_pool: None,
            univ2_flash_fee_bps: None,
            univ3_flash_pool: None,
            univ3_flash_fee_bps: None,
            chain_name: "arbitrum".into(),
            chain_id: Some(42_161),
            env_prefix: Some("ARB".into()),
            rpc_http_urls: vec!["https://ops.org".into(), "https://ops2.org".into()],
            rpc_ws_urls: vec!["wss://ops.org".into()],
            univ3_quoter: Some("0x0000000000000000000000000000000000000020".into()),
            univ3_factory: Some("0x0000000000000000000000000000000000000021".into()),
            univ3_router: Some("0x0000000000000000000000000000000000000022".into()),
            balancer_vault: Some("0x0000000000000000000000000000000000000023".into()),
            aave_pool: Some("0x0000000000000000000000000000000000000024".into()),
            bal_flashloan_tokens: vec![],
            aave_flashloan_tokens: vec![],
            erc3156_flashloan_tokens: vec![],
            univ2_flashloan_tokens: vec![],
            univ3_flashloan_tokens: vec![],
            executor_address: None,
            executor_owner: None,
            permit2_address: None,
            broadcast_mode: None,
            broadcast_private_relays: vec![],
            broadcast_private_method_policy: None,
            broadcast_public_jitter_bps: None,
            health: None,
            gas_model: Some(GasModel::Arbitrum),
            gas_rpc_method: None,
            arbitrum_l1_per_byte_wei: None,
            arbitrum_l1_per_byte_max_deviation_bps: None,
        };

        let cfg = load_chain_from_sources(
            "arbitrum",
            Some("1.0.0".into()),
            Some(&registry_chain),
            Some(&ops_chain),
        )
        .unwrap();

        clear_chain_env("ARB");

        assert_eq!(cfg.rpc, "https://ops.org");
        assert_eq!(cfg.rpc_fallbacks, vec!["https://ops2.org".to_string()]);
        assert_eq!(cfg.ws_endpoints, vec!["wss://ops.org".to_string()]);
        assert_eq!(cfg.univ3_quoter, Address::from_low_u64_be(0x20));
    }

    #[test]
    fn ops_rpc_urls_split_commas() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ARB");

        let registry_chain = RegistryChain {
            env_prefix: Some("ARB".into()),
            chain_id: 42_161,
            rpc_http: vec!["https://registry.org".into()],
            rpc_ws: vec!["wss://registry.org".into()],
            univ3_quoter: Some("0x0000000000000000000000000000000000000010".into()),
            univ3_factory: Some("0x0000000000000000000000000000000000000011".into()),
            univ3_router: Some("0x0000000000000000000000000000000000000012".into()),
            bal_vault: Some("0x0000000000000000000000000000000000000013".into()),
            aave_pool: Some("0x0000000000000000000000000000000000000014".into()),
            tokens: vec!["0x00000000000000000000000000000000000000aa".into()],
            ..Default::default()
        };

        let ops_chain = OpsChainOverrides {
            univ2_flash_pool: None,
            univ2_flash_fee_bps: None,
            univ3_flash_pool: None,
            univ3_flash_fee_bps: None,
            chain_name: "arbitrum".into(),
            chain_id: Some(42_161),
            env_prefix: Some("ARB".into()),
            rpc_http_urls: vec!["https://ops.org,https://ops2.org".into()],
            rpc_ws_urls: vec!["wss://ops.org,wss://ops2.org".into()],
            univ3_quoter: Some("0x0000000000000000000000000000000000000020".into()),
            univ3_factory: Some("0x0000000000000000000000000000000000000021".into()),
            univ3_router: Some("0x0000000000000000000000000000000000000022".into()),
            balancer_vault: Some("0x0000000000000000000000000000000000000023".into()),
            aave_pool: Some("0x0000000000000000000000000000000000000024".into()),
            bal_flashloan_tokens: vec![],
            aave_flashloan_tokens: vec![],
            erc3156_flashloan_tokens: vec![],
            univ2_flashloan_tokens: vec![],
            univ3_flashloan_tokens: vec![],
            executor_address: None,
            executor_owner: None,
            permit2_address: None,
            broadcast_mode: None,
            broadcast_private_relays: vec![],
            broadcast_private_method_policy: None,
            broadcast_public_jitter_bps: None,
            health: None,
            gas_model: Some(GasModel::Arbitrum),
            gas_rpc_method: None,
            arbitrum_l1_per_byte_wei: None,
            arbitrum_l1_per_byte_max_deviation_bps: None,
        };

        let cfg = load_chain_from_sources(
            "arbitrum",
            Some("1.0.0".into()),
            Some(&registry_chain),
            Some(&ops_chain),
        )
        .unwrap();

        clear_chain_env("ARB");

        assert_eq!(cfg.rpc, "https://ops.org");
        assert_eq!(cfg.rpc_fallbacks, vec!["https://ops2.org".to_string()]);
        assert_eq!(
            cfg.ws_endpoints,
            vec!["wss://ops.org".to_string(), "wss://ops2.org".to_string()]
        );
    }

    #[test]
    fn registry_overrides_env_when_ops_missing() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ARB");
        std::env::set_var("ARB_RPC_URL", "https://env.org");
        std::env::set_var("ARB_WS_RPC_URLS", "wss://env.org");
        std::env::set_var(
            "ARB_UNIV3_QUOTER",
            "0x00000000000000000000000000000000000000aa",
        );
        std::env::set_var(
            "ARB_UNIV3_ROUTER",
            "0x00000000000000000000000000000000000000bb",
        );
        std::env::set_var(
            "ARB_BAL_VAULT",
            "0x00000000000000000000000000000000000000cc",
        );

        let registry_chain = RegistryChain {
            env_prefix: Some("ARB".into()),
            chain_id: 42_161,
            rpc_http: vec!["https://registry.org".into()],
            rpc_ws: vec!["wss://registry.org".into()],
            univ3_quoter: Some("0x0000000000000000000000000000000000000010".into()),
            univ3_factory: Some("0x0000000000000000000000000000000000000011".into()),
            univ3_router: Some("0x0000000000000000000000000000000000000012".into()),
            bal_vault: Some("0x0000000000000000000000000000000000000013".into()),
            tokens: vec!["0x00000000000000000000000000000000000000aa".into()],
            ..Default::default()
        };

        let cfg = load_chain_from_sources(
            "arbitrum",
            Some("1.0.0".into()),
            Some(&registry_chain),
            None,
        )
        .unwrap();

        clear_chain_env("ARB");

        assert_eq!(cfg.rpc, "https://registry.org");
        assert_eq!(cfg.ws_endpoints, vec!["wss://registry.org".to_string()]);
        assert_eq!(cfg.univ3_quoter, Address::from_low_u64_be(0x10));
    }

    #[test]
    fn load_chain_from_sources_disables_balancer_flashloans_without_vault() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("BASE");
        std::env::remove_var("BASE_BAL_VAULT");
        std::env::remove_var("BASE_BALANCER_VAULT");
        std::env::remove_var("BASE_BAL_FLASHLOAN_TOKENS");

        let cfg = load_chain_from_sources("base", None, None, None).unwrap();

        clear_chain_env("BASE");

        assert_eq!(cfg.bal_vault, Address::zero());
        assert_eq!(cfg.bal_flashloan_tokens, Some(Vec::new()));
    }

    #[test]
    fn load_chain_from_sources_rejects_balancer_tokens_without_vault() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("BASE");
        std::env::remove_var("BASE_BAL_VAULT");
        std::env::remove_var("BASE_BALANCER_VAULT");

        let err = load_chain_from_sources("base", None, None, None)
            .expect_err("balancer tokens without vault should fail");

        clear_chain_env("BASE");

        assert!(format!("{err:#}").contains("bal_flashloan_tokens requires bal_vault"));
    }

    #[test]
    fn validate_chain_cfg_requires_executor_and_permit2() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        std::env::remove_var("TEST_EXECUTOR_ADDRESS");
        std::env::remove_var("TEST_PERMIT2_ADDRESS");

        let cfg = ChainCfg {
            name: "test".to_string(),
            env_prefix: "TEST".to_string(),
            chain_id: 1,
            rpc: "https://example.org".to_string(),
            rpc_fallbacks: vec![],
            univ3_quoter: Address::from_low_u64_be(1),
            univ3_factory: Address::from_low_u64_be(2),
            univ3_router: Address::from_low_u64_be(3),
            bal_vault: Address::from_low_u64_be(4),
            aave_pool: None,
            bal_flashloan_tokens: None,
            aave_flashloan_tokens: None,
            erc3156_flashloan_tokens: None,
            univ2_flashloan_tokens: None,
            univ3_flashloan_tokens: None,
            tokens: vec![],
            ws_endpoints: vec![],
            univ3_validation: None,
            registry_version: None,
            gas_model: GasModel::Eip1559,
            gas_rpc_method: None,
            arbitrum_l1_per_byte_wei: None,
            arbitrum_l1_per_byte_max_deviation_bps: None,
        };

        let err = validate_chain_cfg(&cfg, None, None).expect_err("validation should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("EXECUTOR_ADDRESS"));
        assert!(msg.contains("PERMIT2_ADDRESS"));
    }

    #[test]
    fn load_chain_from_env_supports_legacy_router_and_vault_env_keys() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        seed_chain_env("ETH");
        std::env::remove_var("ETH_UNIV3_ROUTER");
        std::env::set_var(
            "ETH_SWAPROUTER02",
            "0x00000000000000000000000000000000000000b1",
        );
        std::env::remove_var("ETH_BAL_VAULT");
        std::env::set_var(
            "ETH_BALANCER_VAULT",
            "0x00000000000000000000000000000000000000b2",
        );

        let cfg = load_chain_from_env("ethereum").expect("chain should load from env");
        clear_chain_env("ETH");

        assert_eq!(cfg.univ3_router, Address::from_low_u64_be(0xb1));
        assert_eq!(cfg.bal_vault, Address::from_low_u64_be(0xb2));
    }

    #[test]
    fn validate_chain_cfg_accepts_legacy_permit2_env_key() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        std::env::set_var(
            "TEST_EXECUTOR_ADDRESS",
            "0x00000000000000000000000000000000000000aa",
        );
        std::env::remove_var("TEST_PERMIT2_ADDRESS");
        std::env::set_var("TEST_PERMIT2", "0x00000000000000000000000000000000000000bb");

        let cfg = ChainCfg {
            name: "test".to_string(),
            env_prefix: "TEST".to_string(),
            chain_id: 1,
            rpc: "https://example.org".to_string(),
            rpc_fallbacks: vec![],
            univ3_quoter: Address::from_low_u64_be(1),
            univ3_factory: Address::from_low_u64_be(2),
            univ3_router: Address::from_low_u64_be(3),
            bal_vault: Address::from_low_u64_be(4),
            aave_pool: None,
            bal_flashloan_tokens: None,
            aave_flashloan_tokens: None,
            erc3156_flashloan_tokens: None,
            univ2_flashloan_tokens: None,
            univ3_flashloan_tokens: None,
            tokens: vec![],
            ws_endpoints: vec![],
            univ3_validation: None,
            registry_version: None,
            gas_model: GasModel::Eip1559,
            gas_rpc_method: None,
            arbitrum_l1_per_byte_wei: None,
            arbitrum_l1_per_byte_max_deviation_bps: None,
        };

        let result = validate_chain_cfg(&cfg, None, None);
        std::env::remove_var("TEST_EXECUTOR_ADDRESS");
        std::env::remove_var("TEST_PERMIT2");

        assert!(result.is_ok());
    }

    #[test]
    fn validate_chain_cfg_accepts_erc3156_lender_from_ops_inputs() {
        let _guard = CHAIN_ENV_LOCK.lock().unwrap();

        std::env::set_var(
            "TEST_EXECUTOR_ADDRESS",
            "0x00000000000000000000000000000000000000aa",
        );
        std::env::set_var(
            "TEST_PERMIT2_ADDRESS",
            "0x00000000000000000000000000000000000000bb",
        );
        std::env::remove_var("ERC3156_LENDER");

        let cfg = ChainCfg {
            name: "test".to_string(),
            env_prefix: "TEST".to_string(),
            chain_id: 1,
            rpc: "https://example.org".to_string(),
            rpc_fallbacks: vec![],
            univ3_quoter: Address::from_low_u64_be(1),
            univ3_factory: Address::from_low_u64_be(2),
            univ3_router: Address::from_low_u64_be(3),
            bal_vault: Address::from_low_u64_be(4),
            aave_pool: None,
            bal_flashloan_tokens: None,
            aave_flashloan_tokens: None,
            erc3156_flashloan_tokens: None,
            univ2_flashloan_tokens: None,
            univ3_flashloan_tokens: None,
            tokens: vec![],
            ws_endpoints: vec![],
            univ3_validation: None,
            registry_version: None,
            gas_model: GasModel::Eip1559,
            gas_rpc_method: None,
            arbitrum_l1_per_byte_wei: None,
            arbitrum_l1_per_byte_max_deviation_bps: None,
        };

        let chain_inputs = ChainInputs {
            chain_name: "test".to_string(),
            chain_id: 1,
            env_prefix: "TEST".to_string(),
            rpc_http_urls: vec!["https://example.org".to_string()],
            rpc_ws_urls: vec![],
            gas_model: None,
            gas_rpc_method: None,
            arbitrum_l1_per_byte_wei: None,
            arbitrum_l1_per_byte_max_deviation_bps: None,
            executor_address: "0x00000000000000000000000000000000000000aa".to_string(),
            executor_owner: None,
            permit2_address: "0x00000000000000000000000000000000000000bb".to_string(),
            venues: vec![],
            flashloans: vec![crate::ops_inputs::FlashloanConfig {
                name: Some("erc3156".to_string()),
                kind: Some(FlashloanKind::Erc3156Like),
                pool: None,
                vault: None,
                lender: Some("0x00000000000000000000000000000000000000cc".to_string()),
                fee_bps: Some(9),
                factory: None,
                max_loan_assets: vec![],
                allowlist_tokens: vec![],
            }],
            broadcast: None,
            health: None,
            extras: HashMap::new(),
        };

        validate_chain_cfg(&cfg, None, Some(&chain_inputs))
            .expect("ops inputs lender should satisfy ERC3156 validation");
    }

    #[test]
    fn expands_at_file_and_trims_quotes() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), "0x1,0x2").unwrap();
        let raw = format!(" '@{}' ", file.path().display());

        let expanded = maybe_expand_env_file(&raw, "TEST_KEY").unwrap();

        assert_eq!(expanded, "0x1,0x2");
    }

    #[test]
    fn strips_quotes_for_inline_values() {
        let expanded = maybe_expand_env_file(" \"0x1,0x2\" ", "TEST_KEY").unwrap();

        assert_eq!(expanded, "0x1,0x2");
    }
}
