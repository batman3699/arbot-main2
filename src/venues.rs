use anyhow::{anyhow, ensure, Context, Result};
use ethers::{prelude::*, providers::JsonRpcClient};
use std::{
    collections::{HashMap, HashSet},
    fs,
    future::Future,
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{OnceCell, Semaphore},
    task::JoinSet,
    time::timeout,
};

use crate::discovery::LowLiquidityPool;
use crate::graph::{Edge, Graph, VenueEdge};
use crate::hot_path::HotPathCache;
use crate::pool_store::{PoolRecord, ResolvedUniV2PoolCfg};
use crate::quote_balancer::BalQuote;
use crate::quote_curve::CurveQuote;
use crate::quote_solidly::{
    quote_exact_input_from_state as quote_solidly_exact_input, SolidlyPairState,
};
use crate::quote_univ2::{load_pair_state, quote_exact_input_from_state, UniV2PairState};
use crate::quote_univ3::{UniQuoter, UniV3ValidationConfig, FEE_TIERS};
use crate::quote_univ4::quote_fixed_price_exact_input;
use crate::util::{
    apply_slippage, compute_edge_weight, decimal_ratio, u256_to_decimal, NativePrice, TradeSizing,
};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, MathematicalOps};
use serde::de::DeserializeOwned;
use tracing::{debug, error, info, warn};

const ESTIMATED_GAS_UNIV3: u64 = 140_000;
const ESTIMATED_GAS_BAL: u64 = 155_000;
const ESTIMATED_GAS_CURVE: u64 = 180_000;
const ESTIMATED_GAS_UNIV2: u64 = 130_000;
const ESTIMATED_GAS_SOLIDLYV2: u64 = 135_000;
const ESTIMATED_GAS_UNIV4: u64 = 160_000;
const DEFAULT_RPC_QUOTE_TIMEOUT_SECS: u64 = 8;
const DEFAULT_QUEUE_WAIT_TIMEOUT_SECS: u64 = 8;
const DEFAULT_UNIV3_TOTAL_DEADLINE_SECS: u64 = 20;
const RPC_QUOTE_TIMEOUT_ENV: &str = "ARBOT_RPC_QUOTE_TIMEOUT_SECS";
const QUEUE_WAIT_TIMEOUT_ENV: &str = "ARBOT_UNIV3_QUEUE_WAIT_TIMEOUT_SECS";
const UNIV3_TOTAL_DEADLINE_ENV: &str = "ARBOT_UNIV3_TOTAL_DEADLINE_SECS";

fn parse_rpc_quote_timeout_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_RPC_QUOTE_TIMEOUT_SECS)
}

fn parse_timeout_secs(raw: Option<&str>, default_secs: u64) -> u64 {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_secs)
}

fn rpc_quote_timeout() -> Duration {
    Duration::from_secs(parse_rpc_quote_timeout_secs(
        std::env::var(RPC_QUOTE_TIMEOUT_ENV).ok().as_deref(),
    ))
}

fn queue_wait_timeout() -> Duration {
    Duration::from_secs(parse_timeout_secs(
        std::env::var(QUEUE_WAIT_TIMEOUT_ENV).ok().as_deref(),
        DEFAULT_QUEUE_WAIT_TIMEOUT_SECS,
    ))
}

fn univ3_total_deadline_timeout() -> Duration {
    Duration::from_secs(parse_timeout_secs(
        std::env::var(UNIV3_TOTAL_DEADLINE_ENV).ok().as_deref(),
        DEFAULT_UNIV3_TOTAL_DEADLINE_SECS,
    ))
}

abigen!(
    IBalancerPoolIdLookup,
    r#"[
        function getPoolId() external view returns (bytes32)
    ]"#,
);

fn native_price_for(token: Address, prices: &HashMap<Address, NativePrice>) -> NativePrice {
    prices
        .get(&token)
        .copied()
        .unwrap_or_else(NativePrice::unit)
}

fn edge_health_score_bps(edge: &Edge, current_block: U64, max_block_lag: U64) -> u32 {
    let liquidity_score = ((u256_to_decimal(edge.max_input) + Decimal::ONE).ln()
        * Decimal::from(1200u32))
    .to_u32()
    .unwrap_or(0)
    .min(10_000);
    let slippage_score =
        if edge.tolerance_bps == 0 || edge.observed_slippage_bps <= edge.tolerance_bps {
            10_000u32
        } else {
            let overage = edge
                .observed_slippage_bps
                .saturating_sub(edge.tolerance_bps);
            10_000u32.saturating_sub(overage.saturating_mul(10_000) / edge.tolerance_bps)
        };
    let freshness_score = match edge.quote_block {
        Some(quote_block) if max_block_lag > U64::zero() => {
            let lag = current_block.saturating_sub(quote_block).as_u64();
            let max_lag = max_block_lag.as_u64();
            10_000u32.saturating_sub(((lag.min(max_lag) as u32) * 10_000) / max_lag.max(1) as u32)
        }
        Some(quote_block) if quote_block == current_block => 10_000,
        Some(_) => 0,
        None => 0,
    };

    // Composite edge health score: liquidity depth (50%), slippage quality (35%), quote freshness (15%).
    ((liquidity_score * 50) + (slippage_score * 35) + (freshness_score * 15)) / 100
}

fn apply_pruning(
    edge: &mut Edge,
    min_edge_max_input: U256,
    token_whitelist: &HashSet<Address>,
    min_edge_health_score_bps: u32,
    current_block: U64,
    max_block_lag: U64,
) {
    if !token_whitelist.contains(&edge.from) || !token_whitelist.contains(&edge.to) {
        edge.active = false;
        return;
    }

    if edge.max_input < min_edge_max_input {
        edge.active = false;
        return;
    }

    if min_edge_health_score_bps > 0 {
        let health = edge_health_score_bps(edge, current_block, max_block_lag);
        if health < min_edge_health_score_bps {
            edge.active = false;
        }
    }
}

fn filter_stale_edges(edges: &mut Vec<Edge>, current_block: U64, max_block_lag: U64) -> usize {
    if edges.is_empty() {
        return 0;
    }
    let strict = max_block_lag == U64::zero();
    let mut removed = 0usize;
    edges.retain(|edge| {
        let Some(quote_block) = edge.quote_block else {
            if strict {
                removed = removed.saturating_add(1);
                return false;
            }
            return true;
        };
        if strict && quote_block != current_block {
            removed = removed.saturating_add(1);
            return false;
        }
        if current_block.saturating_sub(quote_block) > max_block_lag {
            removed = removed.saturating_add(1);
            return false;
        }
        true
    });
    removed
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct BalPoolCfg {
    #[serde(rename = "poolId")]
    pub pool_id: String,
    #[serde(rename = "tokenIn")]
    pub token_in: String,
    #[serde(rename = "tokenOut")]
    pub token_out: String,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct CurvePoolCfg {
    pub pool: String,
    #[serde(rename = "tokenIn")]
    pub token_in: String,
    #[serde(rename = "tokenOut")]
    pub token_out: String,
    pub selector: String,
    pub i: i128,
    pub j: i128,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct UniV2PoolCfg {
    pub pair: String,
    #[serde(rename = "tokenIn")]
    pub token_in: String,
    #[serde(rename = "tokenOut")]
    pub token_out: String,
    #[serde(default = "UniV2PoolCfg::default_fee_bps", rename = "feeBps")]
    pub fee_bps: u32,
}

impl UniV2PoolCfg {
    fn default_fee_bps() -> u32 {
        30
    }
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct SolidlyV2PoolCfg {
    pub pair: String,
    #[serde(rename = "tokenIn")]
    pub token_in: String,
    #[serde(rename = "tokenOut")]
    pub token_out: String,
    #[serde(default, rename = "stable")]
    pub stable: bool,
    #[serde(default = "SolidlyV2PoolCfg::default_fee_bps", rename = "feeBps")]
    pub fee_bps: u32,
}

impl SolidlyV2PoolCfg {
    fn default_fee_bps() -> u32 {
        30
    }
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct Univ4PoolCfg {
    #[serde(rename = "poolManager")]
    pub pool_manager: String,
    #[serde(rename = "tokenIn")]
    pub token_in: String,
    #[serde(rename = "tokenOut")]
    pub token_out: String,
    #[serde(rename = "fee")]
    pub fee: u32,
    #[serde(rename = "tickSpacing")]
    pub tick_spacing: i32,
    #[serde(rename = "hooks")]
    pub hooks: String,
    #[serde(rename = "sqrtPriceX96")]
    pub sqrt_price_x96: String,
}

#[derive(Clone)]
struct ResolvedBalPoolCfg {
    pool_id: [u8; 32],
    token_in: Address,
    token_out: Address,
    pool_address_hint: Option<Address>,
}

#[derive(Clone)]
struct ResolvedCurvePoolCfg {
    pool: Address,
    token_in: Address,
    token_out: Address,
    selector: [u8; 4],
    i: i128,
    j: i128,
}

#[derive(Clone)]
struct ResolvedSolidlyV2PoolCfg {
    pair: Address,
    token_in: Address,
    token_out: Address,
    stable: bool,
    fee_bps: u32,
}

#[derive(Clone)]
struct ResolvedUniv4PoolCfg {
    pool_manager: Address,
    token_in: Address,
    token_out: Address,
    token0: Address,
    token1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
    sqrt_price_x96: U256,
}

#[derive(Clone, Copy, Debug)]
struct QuoteComputation {
    amount_in: U256,
    amount_out: U256,
    slippage_bps: u32,
}

fn profit_value(quote: &QuoteComputation) -> Decimal {
    u256_to_decimal(quote.amount_out)
        .checked_sub(u256_to_decimal(quote.amount_in))
        .unwrap_or(Decimal::ZERO)
}

fn best_quote(candidates: &[QuoteComputation]) -> Option<&QuoteComputation> {
    candidates.iter().max_by(|a, b| {
        profit_value(a)
            .cmp(&profit_value(b))
            .then(a.amount_in.cmp(&b.amount_in))
    })
}

fn probe_amount(amount_in: U256) -> U256 {
    if amount_in.is_zero() {
        return U256::zero();
    }
    let thousand = U256::from(1000u64);
    let mut probe = amount_in / thousand;
    if probe.is_zero() {
        probe = U256::one();
    }
    probe
}

fn slippage_from_samples(
    amount_in: U256,
    amount_out: U256,
    probe_in: U256,
    probe_out: U256,
) -> u32 {
    if amount_in.is_zero() || amount_out.is_zero() || probe_in.is_zero() || probe_out.is_zero() {
        return u32::MAX;
    }
    let spot_rate = decimal_ratio(probe_out, probe_in).unwrap_or(Decimal::ZERO);
    if spot_rate.is_zero() || spot_rate.is_sign_negative() {
        return u32::MAX;
    }
    let actual_rate = decimal_ratio(amount_out, amount_in).unwrap_or(Decimal::ZERO);
    if actual_rate.is_zero() || actual_rate.is_sign_negative() {
        return u32::MAX;
    }
    let slippage = match spot_rate
        .checked_sub(actual_rate)
        .and_then(|diff| diff.checked_div(spot_rate))
    {
        Some(value) => value,
        None => return u32::MAX,
    };
    if slippage.is_sign_negative() || slippage.is_zero() {
        return 0;
    }
    let Some(multiplier) = Decimal::from_i32(10_000) else {
        warn!("Invalid Decimal conversion while computing slippage bps");
        return u32::MAX;
    };

    slippage
        .checked_mul(multiplier)
        .and_then(|v| v.ceil().to_u32())
        .unwrap_or(u32::MAX)
}

fn adjust_trade_size_sync<F>(
    base_amount: U256,
    threshold_bps: u32,
    mut quote_fn: F,
) -> Result<Option<QuoteComputation>>
where
    F: FnMut(U256) -> Result<Option<QuoteComputation>>,
{
    if base_amount.is_zero() {
        return Ok(None);
    }

    let mut evaluated: Vec<QuoteComputation> = Vec::new();
    let mut eval_cached = HashMap::new();
    let max_amount = base_amount
        .saturating_mul(U256::from(10u64))
        .max(base_amount);

    let mut evaluate = |amount: U256,
                        cache: &mut HashMap<U256, Option<QuoteComputation>>|
     -> Result<Option<QuoteComputation>> {
        if let Some(cached) = cache.get(&amount) {
            return Ok(*cached);
        }
        let quote = quote_fn(amount)?
            .filter(|q| q.slippage_bps <= threshold_bps)
            .map(|mut q| {
                q.amount_in = amount;
                q
            });
        cache.insert(amount, quote);
        Ok(quote)
    };

    // Seed with a small probe to estimate slippage curve.
    let mut current = probe_amount(base_amount).max(U256::one());
    let mut prev_profit = Decimal::MIN;
    let mut bracket: Option<(U256, U256)> = None;
    let mut last_amount: Option<U256> = None;
    for _ in 0..12 {
        if current.is_zero() || current > max_amount {
            break;
        }
        if let Some(q) = evaluate(current, &mut eval_cached)? {
            let profit = profit_value(&q);
            evaluated.push(q);
            if let Some(prev) = last_amount {
                if profit < prev_profit {
                    bracket = Some((prev, current));
                    break;
                }
            }
            last_amount = Some(current);
            prev_profit = profit;
        } else if let Some(prev) = last_amount {
            let lower = (prev / U256::from(2u64)).max(U256::one());
            bracket = Some((lower, current));
            break;
        }
        let next = current.saturating_mul(U256::from(2u64));
        if next <= current {
            break;
        }
        current = next;
    }

    // If we observed a decline, refine with ternary search in the bracket.
    if let Some((mut low, mut high)) = bracket {
        for _ in 0..8 {
            if high <= low {
                break;
            }
            let span = high.saturating_sub(low);
            let step = span / U256::from(3u64);
            if step.is_zero() {
                break;
            }
            let m1 = low.saturating_add(step);
            let m2 = high.saturating_sub(step);
            let q1 = evaluate(m1, &mut eval_cached)?;
            let q2 = evaluate(m2, &mut eval_cached)?;
            if let Some(q) = q1 {
                evaluated.push(q);
            }
            if let Some(q) = q2 {
                evaluated.push(q);
            }
            match (q1.as_ref(), q2.as_ref()) {
                (Some(p1), Some(p2)) => {
                    if profit_value(p1) < profit_value(p2) {
                        low = m1;
                    } else {
                        high = m2;
                    }
                }
                (Some(_), None) => {
                    high = m2;
                }
                (None, Some(_)) => {
                    low = m1;
                }
                (None, None) => break,
            }
        }
    }

    // Fallback: if nothing evaluated, attempt geometric reduction like before.
    if evaluated.is_empty() {
        let mut amount = base_amount;
        for _ in 0..8 {
            if amount.is_zero() {
                break;
            }
            if let Some(q) = evaluate(amount, &mut eval_cached)? {
                evaluated.push(q);
                break;
            }
            let next = amount * U256::from(3u64) / U256::from(4u64);
            if next >= amount {
                break;
            }
            amount = next;
        }
    }

    Ok(best_quote(&evaluated).copied())
}

fn is_balancer_small_trade_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("BAL#500") || msg.contains("42414c23353030")
}

async fn adjust_trade_size_async<F, Fut>(
    base_amount: U256,
    threshold_bps: u32,
    mut quote_fn: F,
) -> Result<Option<QuoteComputation>>
where
    F: FnMut(U256) -> Fut,
    Fut: Future<Output = Result<Option<QuoteComputation>>> + Send,
{
    if base_amount.is_zero() {
        return Ok(None);
    }

    let mut evaluated: Vec<QuoteComputation> = Vec::new();
    let mut eval_cached = HashMap::new();
    let max_amount = base_amount
        .saturating_mul(U256::from(10u64))
        .max(base_amount);

    // Start from a probe and expand until profit declines.
    let mut current = probe_amount(base_amount).max(U256::one());
    let mut prev_profit = Decimal::MIN;
    let mut bracket: Option<(U256, U256)> = None;
    let mut last_amount: Option<U256> = None;
    for _ in 0..12 {
        if current.is_zero() || current > max_amount {
            break;
        }
        if let Some(q) =
            evaluate_quote_async(current, threshold_bps, &mut eval_cached, &mut quote_fn).await?
        {
            let profit = profit_value(&q);
            evaluated.push(q);
            if let Some(prev) = last_amount {
                if profit < prev_profit {
                    bracket = Some((prev, current));
                    break;
                }
            }
            last_amount = Some(current);
            prev_profit = profit;
        } else if let Some(prev) = last_amount {
            let lower = (prev / U256::from(2u64)).max(U256::one());
            bracket = Some((lower, current));
            break;
        }
        let next = current.saturating_mul(U256::from(2u64));
        if next <= current {
            break;
        }
        current = next;
    }

    if let Some((mut low, mut high)) = bracket {
        for _ in 0..8 {
            if high <= low {
                break;
            }
            let span = high.saturating_sub(low);
            let step = span / U256::from(3u64);
            if step.is_zero() {
                break;
            }
            let m1 = low.saturating_add(step);
            let m2 = high.saturating_sub(step);
            let q1 =
                evaluate_quote_async(m1, threshold_bps, &mut eval_cached, &mut quote_fn).await?;
            let q2 =
                evaluate_quote_async(m2, threshold_bps, &mut eval_cached, &mut quote_fn).await?;
            if let Some(q) = q1 {
                evaluated.push(q);
            }
            if let Some(q) = q2 {
                evaluated.push(q);
            }
            match (q1.as_ref(), q2.as_ref()) {
                (Some(p1), Some(p2)) => {
                    if profit_value(p1) < profit_value(p2) {
                        low = m1;
                    } else {
                        high = m2;
                    }
                }
                (Some(_), None) => {
                    high = m2;
                }
                (None, Some(_)) => {
                    low = m1;
                }
                (None, None) => break,
            }
        }
    }

    if evaluated.is_empty() {
        let mut amount = base_amount;
        for _ in 0..8 {
            if amount.is_zero() {
                break;
            }
            if let Some(q) =
                evaluate_quote_async(amount, threshold_bps, &mut eval_cached, &mut quote_fn).await?
            {
                evaluated.push(q);
                break;
            }
            let next = amount * U256::from(3u64) / U256::from(4u64);
            if next >= amount {
                break;
            }
            amount = next;
        }
    }

    Ok(best_quote(&evaluated).copied())
}

async fn evaluate_quote_async<F, Fut>(
    amount: U256,
    threshold_bps: u32,
    cache: &mut HashMap<U256, Option<QuoteComputation>>,
    quote_fn: &mut F,
) -> Result<Option<QuoteComputation>>
where
    F: FnMut(U256) -> Fut,
    Fut: Future<Output = Result<Option<QuoteComputation>>> + Send,
{
    if let Some(cached) = cache.get(&amount) {
        return Ok(*cached);
    }
    let quote = quote_fn(amount)
        .await?
        .filter(|q| q.slippage_bps <= threshold_bps)
        .map(|mut q| {
            q.amount_in = amount;
            q
        });
    cache.insert(amount, quote);
    Ok(quote)
}

pub fn env_var_with_fallback(primary: &str, fallback: &str) -> Option<(String, String)> {
    std::env::var(primary)
        .map(|value| (value, primary.to_string()))
        .or_else(|_| std::env::var(fallback).map(|value| (value, fallback.to_string())))
        .ok()
}

pub(crate) fn parse_pool_configs<T>(raw: &str, source: &str) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    match try_parse_pool_configs(raw, source) {
        Ok(pools) => Ok(pools),
        Err(initial_error) => {
            if let Some((contents, derived_source)) = maybe_load_config_file(raw, source)? {
                try_parse_pool_configs(&contents, &derived_source)
            } else {
                Err(initial_error)
            }
        }
    }
}

fn try_parse_pool_configs<T>(raw: &str, source: &str) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    json5::from_str(raw).with_context(|| {
        format!(
            "failed to parse {} as JSON/JSON5. Ensure pool addresses, token addresses, and selectors are quoted strings (e.g. \"0xabc...\").",
            source
        )
    })
}

fn maybe_load_config_file(raw: &str, source: &str) -> Result<Option<(String, String)>> {
    let mut trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if let Some(stripped) = trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        trimmed = stripped;
    } else if trimmed.len() >= 2
        && trimmed.as_bytes().first() == Some(&39)
        && trimmed.as_bytes().last() == Some(&39)
    {
        trimmed = &trimmed[1..trimmed.len() - 1];
    }

    let trimmed = trimmed.strip_prefix('@').unwrap_or(trimmed);
    let path = if let Some(stripped) = trimmed.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(stripped)
        } else {
            PathBuf::from(trimmed)
        }
    } else {
        PathBuf::from(trimmed)
    };

    if path.is_file() {
        let contents = fs::read_to_string(&path).with_context(|| {
            format!(
                "failed to read config file `{}` referenced by {source}",
                path.display()
            )
        })?;
        let derived_source = format!("config file `{}` referenced by {source}", path.display());
        Ok(Some((contents, derived_source)))
    } else {
        Ok(None)
    }
}

fn parse_u256(raw: &str) -> Result<U256> {
    let trimmed = raw.trim();
    if let Some(stripped) = trimmed.strip_prefix("0x") {
        U256::from_str_radix(stripped, 16)
            .with_context(|| format!("invalid hex U256 value `{raw}`"))
    } else {
        U256::from_dec_str(trimmed).with_context(|| format!("invalid decimal U256 value `{raw}`"))
    }
}

fn parse_curve_selector(raw: &str) -> Result<[u8; 4]> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(trimmed)
        .with_context(|| format!("selector `{raw}` is not valid hexadecimal"))?;
    ensure!(
        bytes.len() >= 4,
        "selector `{raw}` must decode to at least 4 bytes (8 hex characters)"
    );
    let mut selector = [0u8; 4];
    selector.copy_from_slice(&bytes[..4]);
    Ok(selector)
}

fn parse_balancer_pool_id(raw: &str) -> Result<[u8; 32]> {
    let trimmed = raw.trim();
    let body = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let normalized = if body.len() == 63 {
        let mut padded = String::with_capacity(64);
        padded.push('0');
        padded.push_str(body);
        padded
    } else {
        body.to_string()
    };

    ensure!(
        normalized.len() == 64,
        "Invalid input length: expected 64 hex chars (32 bytes), got {}",
        normalized.len()
    );

    let bytes = hex::decode(&normalized).with_context(|| {
        format!("invalid Balancer pool id `{raw}`; expected hexadecimal 32-byte value")
    })?;
    let mut pool_id = [0u8; 32];
    pool_id.copy_from_slice(&bytes);
    Ok(pool_id)
}

fn resolve_bal_pools(raw: &str, source: &str) -> Result<Vec<ResolvedBalPoolCfg>> {
    let pools: Vec<BalPoolCfg> = parse_pool_configs(raw, source)?;
    let mut resolved = Vec::new();
    for pool_cfg in pools {
        let pool_id_raw = pool_cfg.pool_id.trim();
        let (pool_id, pool_address_hint) = match parse_balancer_pool_id(pool_id_raw) {
            Ok(pool_id) => (pool_id, None),
            Err(err) => {
                if let Ok(pool_addr) = Address::from_str(pool_id_raw) {
                    warn!(
                        target: "venue::balancer",
                        pool = %format!("0x{}", hex::encode(pool_addr)),
                        source = %source,
                        "Balancer pool configured with pool address instead of poolId; resolving via getPoolId()"
                    );
                    ([0u8; 32], Some(pool_addr))
                } else {
                    warn!(
                        target: "venue::balancer",
                        error = %err,
                        pool_id = %pool_id_raw,
                        source = %source,
                        "invalid Balancer pool id; skipping"
                    );
                    continue;
                }
            }
        };
        let token_in_raw = pool_cfg.token_in.trim();
        let token_in = match Address::from_str(token_in_raw) {
            Ok(token_in) => token_in,
            Err(err) => {
                warn!(
                    target: "venue::balancer",
                    error = %err,
                    token_in = %token_in_raw,
                    source = %source,
                    "invalid Balancer tokenIn; skipping"
                );
                continue;
            }
        };
        let token_out_raw = pool_cfg.token_out.trim();
        let token_out = match Address::from_str(token_out_raw) {
            Ok(token_out) => token_out,
            Err(err) => {
                warn!(
                    target: "venue::balancer",
                    error = %err,
                    token_out = %token_out_raw,
                    source = %source,
                    "invalid Balancer tokenOut; skipping"
                );
                continue;
            }
        };
        resolved.push(ResolvedBalPoolCfg {
            pool_id,
            token_in,
            token_out,
            pool_address_hint,
        });
    }
    Ok(resolved)
}

fn resolve_curve_pools(raw: &str, source: &str) -> Result<Vec<ResolvedCurvePoolCfg>> {
    let pools: Vec<CurvePoolCfg> = parse_pool_configs(raw, source)?;
    pools
        .into_iter()
        .map(|pool_cfg| {
            let pool = Address::from_str(&pool_cfg.pool)
                .with_context(|| format!("invalid Curve pool `{}` in {source}", pool_cfg.pool))?;
            let token_in = Address::from_str(&pool_cfg.token_in).with_context(|| {
                format!("invalid Curve tokenIn `{}` in {source}", pool_cfg.token_in)
            })?;
            let token_out = Address::from_str(&pool_cfg.token_out).with_context(|| {
                format!(
                    "invalid Curve tokenOut `{}` in {source}",
                    pool_cfg.token_out
                )
            })?;
            let selector = parse_curve_selector(&pool_cfg.selector)
                .with_context(|| format!("invalid selector for curve pool {}", pool_cfg.pool))?;
            Ok(ResolvedCurvePoolCfg {
                pool,
                token_in,
                token_out,
                selector,
                i: pool_cfg.i,
                j: pool_cfg.j,
            })
        })
        .collect()
}

#[allow(dead_code)]
pub fn resolve_univ2_pools(raw: &str, source: &str) -> Result<Vec<ResolvedUniV2PoolCfg>> {
    let pools: Vec<UniV2PoolCfg> = parse_pool_configs(raw, source)?;
    pools
        .into_iter()
        .map(|pool_cfg| {
            let pair = Address::from_str(&pool_cfg.pair)
                .with_context(|| format!("invalid UniV2 pair `{}` in {source}", pool_cfg.pair))?;
            let token_in = Address::from_str(&pool_cfg.token_in).with_context(|| {
                format!("invalid UniV2 tokenIn `{}` in {source}", pool_cfg.token_in)
            })?;
            let token_out = Address::from_str(&pool_cfg.token_out).with_context(|| {
                format!(
                    "invalid UniV2 tokenOut `{}` in {source}",
                    pool_cfg.token_out
                )
            })?;
            Ok(ResolvedUniV2PoolCfg {
                pair,
                token_in,
                token_out,
                fee_bps: pool_cfg.fee_bps,
            })
        })
        .collect()
}

fn resolve_solidly_pools(raw: &str, source: &str) -> Result<Vec<ResolvedSolidlyV2PoolCfg>> {
    let pools: Vec<SolidlyV2PoolCfg> = parse_pool_configs(raw, source)?;
    pools
        .into_iter()
        .map(|pool_cfg| {
            let pair = Address::from_str(&pool_cfg.pair).with_context(|| {
                format!("invalid SolidlyV2 pair `{}` in {source}", pool_cfg.pair)
            })?;
            let token_in = Address::from_str(&pool_cfg.token_in).with_context(|| {
                format!(
                    "invalid SolidlyV2 tokenIn `{}` in {source}",
                    pool_cfg.token_in
                )
            })?;
            let token_out = Address::from_str(&pool_cfg.token_out).with_context(|| {
                format!(
                    "invalid SolidlyV2 tokenOut `{}` in {source}",
                    pool_cfg.token_out
                )
            })?;
            Ok(ResolvedSolidlyV2PoolCfg {
                pair,
                token_in,
                token_out,
                stable: pool_cfg.stable,
                fee_bps: pool_cfg.fee_bps,
            })
        })
        .collect()
}

fn resolve_univ4_pools(raw: &str, source: &str) -> Result<Vec<ResolvedUniv4PoolCfg>> {
    let pools: Vec<Univ4PoolCfg> = parse_pool_configs(raw, source)?;
    pools
        .into_iter()
        .map(|pool_cfg| {
            let pool_manager = Address::from_str(&pool_cfg.pool_manager).with_context(|| {
                format!(
                    "invalid UniV4 poolManager `{}` in {source}",
                    pool_cfg.pool_manager
                )
            })?;
            let token_in = Address::from_str(&pool_cfg.token_in).with_context(|| {
                format!("invalid UniV4 tokenIn `{}` in {source}", pool_cfg.token_in)
            })?;
            let token_out = Address::from_str(&pool_cfg.token_out).with_context(|| {
                format!(
                    "invalid UniV4 tokenOut `{}` in {source}",
                    pool_cfg.token_out
                )
            })?;
            let hooks = Address::from_str(&pool_cfg.hooks)
                .with_context(|| format!("invalid UniV4 hooks `{}` in {source}", pool_cfg.hooks))?;
            let sqrt_price_x96 = parse_u256(&pool_cfg.sqrt_price_x96)?;
            let (token0, token1) = if token_in < token_out {
                (token_in, token_out)
            } else {
                (token_out, token_in)
            };
            Ok(ResolvedUniv4PoolCfg {
                pool_manager,
                token_in,
                token_out,
                token0,
                token1,
                fee: pool_cfg.fee,
                tick_spacing: pool_cfg.tick_spacing,
                hooks,
                sqrt_price_x96,
            })
        })
        .collect()
}

fn token_decimals_or_default(token_decimals: &HashMap<Address, u8>, token: Address) -> u8 {
    token_decimals.get(&token).copied().unwrap_or(18)
}

fn is_liquid(reserve: U256, decimals: u8, min_tokens: f64) -> bool {
    if min_tokens <= 0.0 {
        return true;
    }
    if reserve.is_zero() {
        return false;
    }
    let min_tokens_dec = Decimal::from_f64(min_tokens).unwrap_or(Decimal::ZERO);
    if min_tokens_dec.is_zero() || min_tokens_dec.is_sign_negative() {
        return true;
    }
    let scaled = u256_to_decimal(reserve);
    if scaled.is_zero() || scaled.is_sign_negative() {
        return false;
    }
    let divisor = Decimal::from(10u64)
        .checked_powu(decimals as u64)
        .unwrap_or(Decimal::MAX);
    let tokens = scaled.checked_div(divisor).unwrap_or(Decimal::ZERO);

    tokens >= min_tokens_dec
}

#[derive(Clone)]
struct EdgeBuildContext {
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
    gas_price: U256,
    native_token_prices: Arc<HashMap<Address, NativePrice>>,
    block_number: U64,
}

struct Univ3EdgeContext<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    edge_ctx: EdgeBuildContext,
    allowed_fee_tiers: Option<Arc<HashSet<u32>>>,
    quoter: Arc<UniQuoter<C>>,
    hot_paths: Arc<HotPathCache>,
    quote_semaphore: Arc<Semaphore>,
    quote_concurrency_limit: usize,
    token_whitelist: Arc<HashSet<Address>>,
    chain_env_prefix: String,
}

fn semaphore_inflight(concurrency_limit: usize, semaphore: &Semaphore) -> usize {
    concurrency_limit.saturating_sub(semaphore.available_permits())
}

fn classify_provider_url(url: &str) -> &'static str {
    let normalized = url.trim().to_ascii_lowercase();
    if normalized.contains("127.0.0.1") || normalized.contains("localhost") {
        "local_fork"
    } else {
        "upstream"
    }
}

fn classify_provider_for_chain(chain_env_prefix: &str) -> &'static str {
    let first_url_from_list = |raw: &str| -> Option<String> {
        raw.split(',')
            .map(str::trim)
            .find(|value| !value.is_empty())
            .map(str::to_string)
    };

    let prefixed_rpc = format!("{chain_env_prefix}_RPC_URL");
    if let Ok(url) = std::env::var(prefixed_rpc) {
        return classify_provider_url(&url);
    }

    let prefixed_rpcs = format!("{chain_env_prefix}_RPC_URLS");
    if let Ok(urls) = std::env::var(prefixed_rpcs) {
        if let Some(first) = first_url_from_list(&urls) {
            return classify_provider_url(&first);
        }
    }

    if let Ok(url) = std::env::var("RPC_URL") {
        return classify_provider_url(&url);
    }

    if let Ok(urls) = std::env::var("RPC_URLS") {
        if let Some(first) = first_url_from_list(&urls) {
            return classify_provider_url(&first);
        }
    }

    "unknown"
}

async fn bootstrap_univ3_pools_from_tokens<C>(ctx: &Univ3EdgeContext<C>) -> Vec<PoolRecord>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut tokens: Vec<Address> = ctx.token_whitelist.iter().copied().collect();
    tokens.sort_unstable();
    if tokens.len() < 2 {
        return Vec::new();
    }

    let fees: Vec<u32> = ctx
        .allowed_fee_tiers
        .as_ref()
        .map(|tiers| {
            let mut v: Vec<u32> = tiers.iter().copied().collect();
            v.sort_unstable();
            v
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| FEE_TIERS.to_vec());

    let max_pairs = std::env::var("UNIV3_BOOTSTRAP_MAX_PAIRS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(256);
    let max_pools = std::env::var("UNIV3_BOOTSTRAP_MAX_POOLS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(512);

    let mut discovered = Vec::new();
    let mut pair_checks = 0usize;
    'outer: for i in 0..tokens.len() {
        for j in (i + 1)..tokens.len() {
            if pair_checks >= max_pairs || discovered.len() >= max_pools {
                break 'outer;
            }
            pair_checks = pair_checks.saturating_add(1);
            let token_a = tokens[i];
            let token_b = tokens[j];
            for fee in &fees {
                if discovered.len() >= max_pools {
                    break 'outer;
                }
                match ctx.quoter.pool_address(token_a, token_b, *fee).await {
                    Ok(Some(pool)) => discovered.push(PoolRecord {
                        pool,
                        token0: token_a,
                        token1: token_b,
                        fee: *fee,
                        created_block: 0,
                    }),
                    Ok(None) => {}
                    Err(err) => {
                        debug!(
                            target: "venue::univ3",
                            error = %err,
                            token0 = %format!("0x{}", hex::encode(token_a)),
                            token1 = %format!("0x{}", hex::encode(token_b)),
                            fee = *fee,
                            "UniV3 bootstrap pool discovery failed"
                        );
                    }
                }
            }
        }
    }

    if !discovered.is_empty() {
        info!(
            target: "venue::univ3",
            pair_checks,
            discovered_pools = discovered.len(),
            "Bootstrapped UniV3 pools from token whitelist"
        );
    }

    discovered
}

fn reserve_forced_discovery_quote(
    should_quote: bool,
    forced_discovery_quotes_used: &AtomicUsize,
    min_forced_discovery_quotes: usize,
) -> bool {
    if should_quote {
        return true;
    }
    let forced_slot = forced_discovery_quotes_used.fetch_add(1, Ordering::Relaxed);
    forced_slot < min_forced_discovery_quotes
}

async fn collect_univ3_edges<C>(
    hot_pools: &[PoolRecord],
    ctx: &Univ3EdgeContext<C>,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    struct Univ3Stats {
        hot_path_skips: AtomicUsize,
        forced_discovery_quotes: AtomicUsize,
        fee_tier_skips: AtomicUsize,
        quote_attempts: AtomicUsize,
        quote_success: AtomicUsize,
        quote_failures: AtomicUsize,
        quote_timeouts: AtomicUsize,
        queue_wait_timeouts: AtomicUsize,
        semaphore_acquire_attempts: AtomicUsize,
        semaphore_acquired: AtomicUsize,
        semaphore_releases: AtomicUsize,
        total_deadline_timeouts: AtomicUsize,
        max_inflight_quotes: AtomicUsize,
        max_queue_wait_ms: AtomicUsize,
        max_permit_hold_ms: AtomicUsize,
    }

    impl Default for Univ3Stats {
        fn default() -> Self {
            Self {
                hot_path_skips: AtomicUsize::new(0),
                forced_discovery_quotes: AtomicUsize::new(0),
                fee_tier_skips: AtomicUsize::new(0),
                quote_attempts: AtomicUsize::new(0),
                quote_success: AtomicUsize::new(0),
                quote_failures: AtomicUsize::new(0),
                quote_timeouts: AtomicUsize::new(0),
                queue_wait_timeouts: AtomicUsize::new(0),
                semaphore_acquire_attempts: AtomicUsize::new(0),
                semaphore_acquired: AtomicUsize::new(0),
                semaphore_releases: AtomicUsize::new(0),
                total_deadline_timeouts: AtomicUsize::new(0),
                max_inflight_quotes: AtomicUsize::new(0),
                max_queue_wait_ms: AtomicUsize::new(0),
                max_permit_hold_ms: AtomicUsize::new(0),
            }
        }
    }

    let source_pools: Vec<PoolRecord> = if hot_pools.is_empty() {
        bootstrap_univ3_pools_from_tokens(ctx).await
    } else {
        hot_pools.to_vec()
    };
    if source_pools.is_empty() {
        return Ok(Vec::new());
    }

    let stats = Arc::new(Univ3Stats::default());
    let gas_price = ctx.edge_ctx.gas_price;
    let allowed_fee_tiers = ctx.allowed_fee_tiers.as_ref().and_then(|tiers| {
        if tiers.is_empty() {
            None
        } else {
            Some(Arc::clone(tiers))
        }
    });
    let min_forced_discovery_quotes = std::env::var("UNIV3_MIN_FORCED_QUOTES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(8);
    let forced_discovery_quotes_used = Arc::new(AtomicUsize::new(0));
    let provider_class = classify_provider_for_chain(&ctx.chain_env_prefix);
    let max_pool_tasks = std::env::var("UNIV3_MAX_CONCURRENT_POOL_TASKS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(ctx.quote_concurrency_limit.max(1));

    let mut join_set: JoinSet<Result<Vec<Edge>>> = JoinSet::new();
    let mut edges = Vec::new();
    let mut pool_tasks_spawned = 0usize;
    for pool in source_pools.iter().cloned() {
        while join_set.len() >= max_pool_tasks {
            if let Some(res) = join_set.join_next().await {
                edges.extend(res??);
            }
        }
        if let Some(tiers) = &allowed_fee_tiers {
            if !tiers.contains(&pool.fee) {
                stats.fee_tier_skips.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        let profile_in = ctx
            .edge_ctx
            .base_profiles
            .as_ref()
            .get(&pool.token0)
            .copied()
            .unwrap_or(ctx.edge_ctx.default_profile);
        let profile_out = ctx
            .edge_ctx
            .base_profiles
            .as_ref()
            .get(&pool.token1)
            .copied()
            .unwrap_or(ctx.edge_ctx.default_profile);
        let quoter = Arc::clone(&ctx.quoter);
        let hot_paths = Arc::clone(&ctx.hot_paths);
        let stats = Arc::clone(&stats);
        let native_token_prices = Arc::clone(&ctx.edge_ctx.native_token_prices);
        let quote_semaphore = Arc::clone(&ctx.quote_semaphore);
        let block_number = ctx.edge_ctx.block_number;
        let forced_discovery_quotes_used = Arc::clone(&forced_discovery_quotes_used);
        let chain_env_prefix = ctx.chain_env_prefix.clone();
        let quote_concurrency_limit = ctx.quote_concurrency_limit;
        join_set.spawn(async move {
            let mut local_edges = Vec::new();
            let directions = [
                (pool.token0, pool.token1, profile_in),
                (pool.token1, pool.token0, profile_out),
            ];
            for (token_in, token_out, profile) in directions {
                let chain_env_prefix_for_direction = chain_env_prefix.clone();
                let should_quote = hot_paths.should_quote(token_in, token_out, pool.fee).await;
                let quote_allowed = reserve_forced_discovery_quote(
                    should_quote,
                    forced_discovery_quotes_used.as_ref(),
                    min_forced_discovery_quotes,
                );
                if !quote_allowed {
                    stats.hot_path_skips.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if !should_quote {
                    stats
                        .forced_discovery_quotes
                        .fetch_add(1, Ordering::Relaxed);
                }
                let base_amount_in = profile.base_amount;
                let tolerance_bps = profile.slippage_tolerance_bps;
                let path = vec![(token_in, None), (token_out, Some(pool.fee))];
                let hot_paths_quote = Arc::clone(&hot_paths);
                let quote_semaphore_fee = Arc::clone(&quote_semaphore);
                let quote = adjust_trade_size_async(base_amount_in, tolerance_bps, {
                    let quoter = quoter.clone();
                    let path_clone = path.clone();
                    let stats = Arc::clone(&stats);
                    move |amount: U256| {
                        let quoter = quoter.clone();
                        let path_inner = path_clone.clone();
                        let stats = Arc::clone(&stats);
                        let quote_semaphore = Arc::clone(&quote_semaphore_fee);
                        let provider_class = provider_class;
                        let chain_env_prefix = chain_env_prefix_for_direction.clone();
                        async move {
                            if amount.is_zero() {
                                return Ok(None);
                            }
                            stats.quote_attempts.fetch_add(1, Ordering::Relaxed);
                            let quote_started_at = Instant::now();
                            let queue_wait_window = queue_wait_timeout();
                            let rpc_timeout_window = rpc_quote_timeout();
                            let total_deadline_window = univ3_total_deadline_timeout();
                            let quote_eval = async {
                                let wait_started_at = Instant::now();
                                let permits_before = quote_semaphore.available_permits();
                                stats
                                    .semaphore_acquire_attempts
                                    .fetch_add(1, Ordering::Relaxed);
                                let permit = match timeout(
                                    queue_wait_window,
                                    quote_semaphore.clone().acquire_owned(),
                                )
                                .await
                                {
                                    Ok(Ok(permit)) => permit,
                                    Ok(Err(_)) => {
                                        return Err(anyhow!("univ3 quote semaphore closed"));
                                    }
                                    Err(_) => {
                                        stats.queue_wait_timeouts.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::univ3",
                                            chain = %chain_env_prefix,
                                            venue = "uniswap_v3",
                                            token_in = %format!("0x{}", hex::encode(token_in)),
                                            token_out = %format!("0x{}", hex::encode(token_out)),
                                            fee = pool.fee,
                                            amount = %amount,
                                            timeout_secs = queue_wait_window.as_secs(),
                                            queue_wait_ms = wait_started_at.elapsed().as_millis() as u64,
                                            permits_available_before = permits_before,
                                            provider_class,
                                            "UniV3 quote stalled before network submission (semaphore wait timeout)"
                                        );
                                        return Ok(None);
                                    }
                                };
                                let queue_wait_ms = wait_started_at.elapsed().as_millis() as usize;
                                stats
                                    .max_queue_wait_ms
                                    .fetch_max(queue_wait_ms, Ordering::Relaxed);
                                stats.semaphore_acquired.fetch_add(1, Ordering::Relaxed);
                                let permits_available_after_acquire =
                                    quote_semaphore.available_permits();
                                let inflight_quotes = semaphore_inflight(
                                    quote_concurrency_limit,
                                    quote_semaphore.as_ref(),
                                );
                                stats
                                    .max_inflight_quotes
                                    .fetch_max(inflight_quotes, Ordering::Relaxed);

                                let hold_started_at = Instant::now();
                                let rpc_started_at = Instant::now();
                                let result = timeout(
                                    rpc_timeout_window,
                                    quoter.quote_path(path_inner.clone(), amount, block_number),
                                )
                                .await;
                                let hold_elapsed_ms = hold_started_at.elapsed().as_millis() as usize;
                                stats
                                    .max_permit_hold_ms
                                    .fetch_max(hold_elapsed_ms, Ordering::Relaxed);
                                drop(permit);
                                stats.semaphore_releases.fetch_add(1, Ordering::Relaxed);
                                debug!(
                                    target: "venue::univ3",
                                    chain = %chain_env_prefix,
                                    venue = "uniswap_v3",
                                    token_in = %format!("0x{}", hex::encode(token_in)),
                                    token_out = %format!("0x{}", hex::encode(token_out)),
                                    fee = pool.fee,
                                    amount = %amount,
                                    queue_wait_ms,
                                    hold_ms = hold_elapsed_ms,
                                    inflight_quotes,
                                    permits_available_after_release = quote_semaphore.available_permits(),
                                    "UniV3 quote permit released"
                                );

                                let rpc_elapsed_ms = rpc_started_at.elapsed().as_millis() as u64;
                                let total_elapsed_ms = quote_started_at.elapsed().as_millis() as u64;

                                match result {
                                    Ok(Ok(out)) if out > U256::zero() => Ok(Some(out)),
                                    Ok(Ok(_)) => {
                                        stats.quote_failures.fetch_add(1, Ordering::Relaxed);
                                        Ok(None)
                                    }
                                    Ok(Err(err)) => {
                                        stats.quote_failures.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::univ3",
                                            chain = %chain_env_prefix,
                                            venue = "uniswap_v3",
                                            provider_class,
                                            error = %err,
                                            token_in = %format!("0x{}", hex::encode(token_in)),
                                            token_out = %format!("0x{}", hex::encode(token_out)),
                                            fee = pool.fee,
                                            amount = %amount,
                                            queue_wait_ms,
                                            rpc_elapsed_ms,
                                            elapsed_ms = total_elapsed_ms,
                                            permits_available_after_acquire,
                                            "UniV3 quote failed"
                                        );
                                        Ok(None)
                                    }
                                    Err(_) => {
                                        stats.quote_timeouts.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::univ3",
                                            chain = %chain_env_prefix,
                                            venue = "uniswap_v3",
                                            provider_class,
                                            token_in = %format!("0x{}", hex::encode(token_in)),
                                            token_out = %format!("0x{}", hex::encode(token_out)),
                                            fee = pool.fee,
                                            amount = %amount,
                                            timeout_secs = rpc_timeout_window.as_secs(),
                                            queue_wait_ms = queue_wait_ms as u64,
                                            rpc_elapsed_ms,
                                            elapsed_ms = total_elapsed_ms,
                                            permits_available_after_acquire,
                                            "UniV3 quote timed out"
                                        );
                                        Ok(None)
                                    }
                                }
                            };
                            let out = match timeout(total_deadline_window, quote_eval).await {
                                Ok(result) => match result? {
                                    Some(value) => value,
                                    None => return Ok(None),
                                },
                                Err(_) => {
                                    stats.total_deadline_timeouts.fetch_add(1, Ordering::Relaxed);
                                    warn!(
                                        target: "venue::univ3",
                                        chain = %chain_env_prefix,
                                        venue = "uniswap_v3",
                                        token_in = %format!("0x{}", hex::encode(token_in)),
                                        token_out = %format!("0x{}", hex::encode(token_out)),
                                        fee = pool.fee,
                                        amount = %amount,
                                        timeout_secs = total_deadline_window.as_secs(),
                                        elapsed_ms = quote_started_at.elapsed().as_millis() as u64,
                                        provider_class,
                                        "UniV3 quote exceeded total request deadline"
                                    );
                                    return Ok(None);
                                }
                            };
                            let probe_in = probe_amount(amount);
                            let probe_out = if probe_in == amount {
                                out
                            } else {
                                let probe_wait_started_at = Instant::now();
                                stats
                                    .semaphore_acquire_attempts
                                    .fetch_add(1, Ordering::Relaxed);
                                let permits_before = quote_semaphore.available_permits();
                                let permit = match timeout(
                                    queue_wait_window,
                                    quote_semaphore.clone().acquire_owned(),
                                )
                                .await
                                {
                                    Ok(Ok(permit)) => permit,
                                    Ok(Err(_)) => {
                                        return Err(anyhow!("univ3 quote semaphore closed"));
                                    }
                                    Err(_) => {
                                        stats.queue_wait_timeouts.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::univ3",
                                            chain = %chain_env_prefix,
                                            venue = "uniswap_v3",
                                            token_in = %format!("0x{}", hex::encode(token_in)),
                                            token_out = %format!("0x{}", hex::encode(token_out)),
                                            fee = pool.fee,
                                            amount = %probe_in,
                                            timeout_secs = queue_wait_window.as_secs(),
                                            queue_wait_ms = probe_wait_started_at.elapsed().as_millis() as u64,
                                            permits_available_before = permits_before,
                                            provider_class,
                                            "UniV3 probe quote stalled before network submission (semaphore wait timeout)"
                                        );
                                        return Ok(None);
                                    }
                                };
                                stats.semaphore_acquired.fetch_add(1, Ordering::Relaxed);
                                let probe_queue_wait_ms =
                                    probe_wait_started_at.elapsed().as_millis() as usize;
                                stats
                                    .max_queue_wait_ms
                                    .fetch_max(probe_queue_wait_ms, Ordering::Relaxed);
                                let probe_result = timeout(
                                    rpc_quote_timeout(),
                                    quoter.quote_path(path_inner.clone(), probe_in, block_number),
                                )
                                .await;
                                let probe_hold_ms = probe_wait_started_at.elapsed().as_millis() as usize;
                                stats
                                    .max_permit_hold_ms
                                    .fetch_max(probe_hold_ms, Ordering::Relaxed);
                                drop(permit);
                                stats.semaphore_releases.fetch_add(1, Ordering::Relaxed);

                                match probe_result {
                                    Ok(Ok(value)) => value,
                                    Ok(Err(err)) => {
                                        stats.quote_failures.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::univ3",
                                            error = %err,
                                            token_in = %format!("0x{}", hex::encode(token_in)),
                                            token_out = %format!("0x{}", hex::encode(token_out)),
                                            fee = pool.fee,
                                            amount = %probe_in,
                                            "UniV3 probe quote failed"
                                        );
                                        U256::zero()
                                    }
                                    Err(_) => {
                                        stats.quote_timeouts.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::univ3",
                                            token_in = %format!("0x{}", hex::encode(token_in)),
                                            token_out = %format!("0x{}", hex::encode(token_out)),
                                            fee = pool.fee,
                                            amount = %probe_in,
                                            timeout_secs = rpc_quote_timeout().as_secs(),
                                            "UniV3 probe quote timed out"
                                        );
                                        U256::zero()
                                    }
                                }
                            };
                            if probe_out.is_zero() {
                                return Ok(None);
                            }
                            let slippage_bps =
                                slippage_from_samples(amount, out, probe_in, probe_out);
                            stats.quote_success.fetch_add(1, Ordering::Relaxed);
                            Ok(Some(QuoteComputation {
                                amount_in: amount,
                                amount_out: out,
                                slippage_bps,
                            }))
                        }
                    }
                })
                .await?;

                let Some(quote) = quote else {
                    hot_paths_quote
                        .as_ref()
                        .record_failure(token_in, token_out, pool.fee)
                        .await;
                    continue;
                };

                if quote.slippage_bps > tolerance_bps {
                    hot_paths_quote
                        .as_ref()
                        .record_failure(token_in, token_out, pool.fee)
                        .await;
                    continue;
                }

                let protected_out = apply_slippage(quote.amount_out, tolerance_bps);
                if protected_out.is_zero() {
                    hot_paths_quote
                        .as_ref()
                        .record_failure(token_in, token_out, pool.fee)
                        .await;
                    continue;
                }

                let weight = compute_edge_weight(
                    protected_out,
                    quote.amount_in,
                    ESTIMATED_GAS_UNIV3,
                    gas_price,
                    quote.amount_in,
                    native_price_for(token_in, &native_token_prices),
                );
                let edge = Edge {
                    from: token_in,
                    to: token_out,
                    rate_num: quote.amount_out,
                    rate_den: quote.amount_in,
                    venue: VenueEdge::UniV3 {
                        path: path.clone(),
                        pool: pool.pool,
                        fee: pool.fee,
                    },
                    estimated_gas: ESTIMATED_GAS_UNIV3,
                    weight,
                    max_input: quote.amount_in,
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: Some(block_number),
                    active: true,
                };
                local_edges.push(edge);
            }
            Ok(local_edges)
        });
        pool_tasks_spawned = pool_tasks_spawned.saturating_add(1);
    }

    while let Some(res) = join_set.join_next().await {
        edges.extend(res??);
    }

    info!(
        target: "venue::univ3",
        pools = source_pools.len(),
        built_edges = edges.len(),
        hot_path_skips = stats.hot_path_skips.load(Ordering::Relaxed),
        forced_discovery_quotes = stats.forced_discovery_quotes.load(Ordering::Relaxed),
        fee_tier_skips = stats.fee_tier_skips.load(Ordering::Relaxed),
        quote_attempts = stats.quote_attempts.load(Ordering::Relaxed),
        quote_success = stats.quote_success.load(Ordering::Relaxed),
        quote_failures = stats.quote_failures.load(Ordering::Relaxed),
        quote_timeouts = stats.quote_timeouts.load(Ordering::Relaxed),
        queue_wait_timeouts = stats.queue_wait_timeouts.load(Ordering::Relaxed),
        semaphore_acquire_attempts = stats.semaphore_acquire_attempts.load(Ordering::Relaxed),
        semaphore_acquired = stats.semaphore_acquired.load(Ordering::Relaxed),
        semaphore_releases = stats.semaphore_releases.load(Ordering::Relaxed),
        total_deadline_timeouts = stats.total_deadline_timeouts.load(Ordering::Relaxed),
        max_inflight_quotes = stats.max_inflight_quotes.load(Ordering::Relaxed),
        max_queue_wait_ms = stats.max_queue_wait_ms.load(Ordering::Relaxed),
        max_permit_hold_ms = stats.max_permit_hold_ms.load(Ordering::Relaxed),
        max_pool_tasks,
        pool_tasks_spawned,
        "UniV3 hot pool summary",
    );
    Ok(edges)
}

async fn collect_balancer_edges<C>(
    provider: Arc<Provider<C>>,
    bal_vault: Address,
    chain_env_prefix: &str,
    ctx: &EdgeBuildContext,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut edges = Vec::new();
    let bal = Arc::new(BalQuote::new(provider.clone(), bal_vault));
    let bal_env = format!("{}_BAL_POOLS", chain_env_prefix);
    if let Some((raw, source)) = env_var_with_fallback(&bal_env, "BAL_POOLS") {
        let pools = resolve_bal_pools(&raw, &source)?;
        let mut resolved_pool_ids: HashMap<Address, [u8; 32]> = HashMap::new();

        for pool in pools {
            let resolved_pool_id = if let Some(pool_addr) = pool.pool_address_hint {
                if let Some(cached) = resolved_pool_ids.get(&pool_addr).copied() {
                    cached
                } else {
                    let resolver = IBalancerPoolIdLookup::new(pool_addr, provider.clone());
                    match resolver.get_pool_id().call().await {
                        Ok(value) => {
                            resolved_pool_ids.insert(pool_addr, value);
                            value
                        }
                        Err(err) => {
                            warn!(
                                target: "venue::balancer",
                                error = %err,
                                pool = %format!("0x{}", hex::encode(pool_addr)),
                                source = %source,
                                "failed to resolve Balancer pool address to poolId; skipping"
                            );
                            continue;
                        }
                    }
                }
            } else {
                pool.pool_id
            };

            let pool_id = H256::from(resolved_pool_id);
            let profile = ctx
                .base_profiles
                .as_ref()
                .get(&pool.token_in)
                .copied()
                .unwrap_or(ctx.default_profile);
            let base_amount_in = profile.base_amount;
            let tolerance_bps = profile.slippage_tolerance_bps;
            let failed = Arc::new(AtomicBool::new(false));
            let quote = adjust_trade_size_async(base_amount_in, tolerance_bps, {
                let bal = bal.clone();
                let failed = failed.clone();
                move |amount: U256| {
                    let bal = bal.clone();
                    let failed = failed.clone();
                    async move {
                        if failed.load(Ordering::Relaxed) {
                            return Ok(None);
                        }
                        if amount.is_zero() {
                            return Ok(None);
                        }
                        let quote_future = bal.quote_single_given_in(
                            pool_id,
                            pool.token_in,
                            pool.token_out,
                            amount,
                            ctx.block_number,
                        );
                        let out = match timeout(rpc_quote_timeout(), quote_future).await {
                            Ok(Ok(out)) if out > U256::zero() => out,
                            Ok(Ok(_)) => return Ok(None),
                            Ok(Err(err)) => {
                                if is_balancer_small_trade_error(&err) {
                                    debug!(
                                        target: "venue::balancer",
                                        error = %err,
                                        pool_id = %format!("0x{}", hex::encode(pool_id)),
                                        token_in = %format!("0x{}", hex::encode(pool.token_in)),
                                        token_out = %format!("0x{}", hex::encode(pool.token_out)),
                                        amount = %amount,
                                        "Balancer rejected tiny trade; retrying with larger size"
                                    );
                                    return Ok(None);
                                }
                                if !failed.swap(true, Ordering::Relaxed) {
                                    warn!(
                                        target: "venue::balancer",
                                        error = %err,
                                        pool_id = %format!("0x{}", hex::encode(pool_id)),
                                        token_in = %format!("0x{}", hex::encode(pool.token_in)),
                                        token_out = %format!("0x{}", hex::encode(pool.token_out)),
                                        amount = %amount,
                                        "Balancer quote failed; disabling pool for this run"
                                    );
                                }
                                return Ok(None);
                            }
                            Err(_) => {
                                warn!(
                                    target: "venue::balancer",
                                    pool_id = %format!("0x{}", hex::encode(pool_id)),
                                    token_in = %format!("0x{}", hex::encode(pool.token_in)),
                                    token_out = %format!("0x{}", hex::encode(pool.token_out)),
                                    amount = %amount,
                                    timeout_secs = rpc_quote_timeout().as_secs(),
                                    "Balancer quote timed out"
                                );
                                return Ok(None);
                            }
                        };
                        let probe_in = probe_amount(amount);
                        let probe_out = if probe_in == amount {
                            out
                        } else {
                            let probe_future = bal.quote_single_given_in(
                                pool_id,
                                pool.token_in,
                                pool.token_out,
                                probe_in,
                                ctx.block_number,
                            );
                            match timeout(rpc_quote_timeout(), probe_future).await {
                                Ok(Ok(value)) => value,
                                Ok(Err(err)) => {
                                    warn!(
                                        target: "venue::balancer",
                                        error = %err,
                                        pool_id = %format!("0x{}", hex::encode(pool_id)),
                                        token_in = %format!("0x{}", hex::encode(pool.token_in)),
                                        token_out = %format!("0x{}", hex::encode(pool.token_out)),
                                        amount = %probe_in,
                                        "Balancer probe quote failed"
                                    );
                                    U256::zero()
                                }
                                Err(_) => {
                                    warn!(
                                        target: "venue::balancer",
                                        pool_id = %format!("0x{}", hex::encode(pool_id)),
                                        token_in = %format!("0x{}", hex::encode(pool.token_in)),
                                        token_out = %format!("0x{}", hex::encode(pool.token_out)),
                                        amount = %probe_in,
                                        timeout_secs = rpc_quote_timeout().as_secs(),
                                        "Balancer probe quote timed out"
                                    );
                                    U256::zero()
                                }
                            }
                        };
                        if probe_out.is_zero() {
                            return Ok(None);
                        }
                        let slippage_bps = slippage_from_samples(amount, out, probe_in, probe_out);
                        Ok(Some(QuoteComputation {
                            amount_in: amount,
                            amount_out: out,
                            slippage_bps,
                        }))
                    }
                }
            })
            .await?;

            if let Some(quote) = quote {
                if quote.slippage_bps > tolerance_bps {
                    continue;
                }
                let protected_out = apply_slippage(quote.amount_out, tolerance_bps);
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(
                    protected_out,
                    quote.amount_in,
                    ESTIMATED_GAS_BAL,
                    ctx.gas_price,
                    quote.amount_in,
                    native_price_for(pool.token_in, &ctx.native_token_prices),
                );
                let edge = Edge {
                    from: pool.token_in,
                    to: pool.token_out,
                    rate_num: quote.amount_out,
                    rate_den: quote.amount_in,
                    venue: VenueEdge::Balancer {
                        pool_id: resolved_pool_id,
                        token_in: pool.token_in,
                        token_out: pool.token_out,
                    },
                    estimated_gas: ESTIMATED_GAS_BAL,
                    weight,
                    max_input: quote.amount_in,
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: Some(ctx.block_number),
                    active: true,
                };
                edges.push(edge);
            }
        }
    }
    Ok(edges)
}

async fn collect_curve_edges<C>(
    provider: Arc<Provider<C>>,
    chain_env_prefix: &str,
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
    gas_price: U256,
    native_token_prices: Arc<HashMap<Address, NativePrice>>,
    block_number: U64,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut edges = Vec::new();
    let curve = Arc::new(CurveQuote::new(provider.clone()));
    let curve_env = format!("{}_CURVE_POOLS", chain_env_prefix);
    if let Some((raw, source)) = env_var_with_fallback(&curve_env, "CURVE_POOLS") {
        let pools = resolve_curve_pools(&raw, &source)?;
        for pool_cfg in pools {
            let profile = base_profiles
                .as_ref()
                .get(&pool_cfg.token_in)
                .copied()
                .unwrap_or(default_profile);
            let base_amount_in = profile.base_amount;
            let tolerance_bps = profile.slippage_tolerance_bps;
            let curve = Arc::clone(&curve);
            let quote = adjust_trade_size_async(base_amount_in, tolerance_bps, {
                move |amount: U256| {
                    let curve = Arc::clone(&curve);
                    async move {
                        if amount.is_zero() {
                            return Ok(None);
                        }
                        let quote_future = curve.quote_get_dy(
                            pool_cfg.pool,
                            pool_cfg.i,
                            pool_cfg.j,
                            amount,
                            block_number,
                        );
                        let out = match timeout(rpc_quote_timeout(), quote_future).await {
                            Ok(Ok(out)) if out > U256::zero() => out,
                            Ok(Ok(_)) => return Ok(None),
                            Ok(Err(err)) => {
                                warn!(
                                    target: "venue::curve",
                                    error = %err,
                                    pool = %format!("0x{}", hex::encode(pool_cfg.pool)),
                                    token_in = %format!("0x{}", hex::encode(pool_cfg.token_in)),
                                    token_out = %format!("0x{}", hex::encode(pool_cfg.token_out)),
                                    amount = %amount,
                                    "Curve quote failed"
                                );
                                return Ok(None);
                            }
                            Err(_) => {
                                warn!(
                                    target: "venue::curve",
                                    pool = %format!("0x{}", hex::encode(pool_cfg.pool)),
                                    token_in = %format!("0x{}", hex::encode(pool_cfg.token_in)),
                                    token_out = %format!("0x{}", hex::encode(pool_cfg.token_out)),
                                    amount = %amount,
                                    timeout_secs = rpc_quote_timeout().as_secs(),
                                    "Curve quote timed out"
                                );
                                return Ok(None);
                            }
                        };
                        let probe_in = probe_amount(amount);
                        let probe_out = if probe_in == amount {
                            out
                        } else {
                            let probe_future = curve.quote_get_dy(
                                pool_cfg.pool,
                                pool_cfg.i,
                                pool_cfg.j,
                                probe_in,
                                block_number,
                            );
                            match timeout(rpc_quote_timeout(), probe_future).await {
                                Ok(Ok(value)) => value,
                                Ok(Err(err)) => {
                                    warn!(
                                        target: "venue::curve",
                                        error = %err,
                                        pool = %format!("0x{}", hex::encode(pool_cfg.pool)),
                                        token_in = %format!("0x{}", hex::encode(pool_cfg.token_in)),
                                        token_out = %format!("0x{}", hex::encode(pool_cfg.token_out)),
                                        amount = %probe_in,
                                        "Curve probe quote failed"
                                    );
                                    U256::zero()
                                }
                                Err(_) => {
                                    warn!(
                                        target: "venue::curve",
                                        pool = %format!("0x{}", hex::encode(pool_cfg.pool)),
                                        token_in = %format!("0x{}", hex::encode(pool_cfg.token_in)),
                                        token_out = %format!("0x{}", hex::encode(pool_cfg.token_out)),
                                        amount = %probe_in,
                                        timeout_secs = rpc_quote_timeout().as_secs(),
                                        "Curve probe quote timed out"
                                    );
                                    U256::zero()
                                }
                            }
                        };
                        if probe_out.is_zero() {
                            return Ok(None);
                        }
                        let slippage_bps =
                            slippage_from_samples(amount, out, probe_in, probe_out);
                        Ok(Some(QuoteComputation {
                            amount_in: amount,
                            amount_out: out,
                            slippage_bps,
                        }))
                    }
                }
            })
            .await?;

            if let Some(quote) = quote {
                if quote.slippage_bps > tolerance_bps {
                    continue;
                }
                let protected_out = apply_slippage(quote.amount_out, tolerance_bps);
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(
                    protected_out,
                    quote.amount_in,
                    ESTIMATED_GAS_CURVE,
                    gas_price,
                    quote.amount_in,
                    native_price_for(pool_cfg.token_in, &native_token_prices),
                );
                let edge = Edge {
                    from: pool_cfg.token_in,
                    to: pool_cfg.token_out,
                    rate_num: quote.amount_out,
                    rate_den: quote.amount_in,
                    venue: VenueEdge::Curve {
                        pool: pool_cfg.pool,
                        selector: pool_cfg.selector,
                        i: pool_cfg.i,
                        j: pool_cfg.j,
                    },
                    estimated_gas: ESTIMATED_GAS_CURVE,
                    weight,
                    max_input: quote.amount_in,
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: Some(block_number),
                    active: true,
                };
                edges.push(edge);
            }
        }
    }
    Ok(edges)
}

#[allow(clippy::too_many_arguments)]
async fn collect_univ2_edges<C>(
    provider: Arc<Provider<C>>,
    pool_monitor: Option<Arc<crate::ingestion::PoolMonitor<C>>>,
    hot_pools: &[ResolvedUniV2PoolCfg],
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
    gas_price: U256,
    native_token_prices: Arc<HashMap<Address, NativePrice>>,
    token_decimals: Arc<HashMap<Address, u8>>,
    min_liquidity_tokens: f64,
    block_number: U64,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut edges = Vec::new();
    if hot_pools.is_empty() {
        return Ok(edges);
    }

    let mut cached_states: Vec<(ResolvedUniV2PoolCfg, UniV2PairState, Option<U64>)> = Vec::new();
    for pool in hot_pools.iter().cloned() {
        let snapshot = if let Some(monitor) = &pool_monitor {
            monitor.state_with_block(pool.pair).await
        } else {
            None
        };

        match snapshot {
            Some((state, last_block)) => cached_states.push((pool, state, last_block)),
            None => match load_pair_state(provider.clone(), pool.pair).await {
                Ok(Some(state)) => cached_states.push((pool, state, Some(block_number))),
                Ok(None) => {
                    debug!(
                        pair = %format!("0x{}", hex::encode(pool.pair)),
                        "Skipping UniV2 pair with unsupported interface"
                    );
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        pair = %format!("0x{}", hex::encode(pool.pair)),
                        "Failed to load UniV2 state"
                    );
                }
            },
        }
    }

    for (pool, state, last_block) in cached_states {
        let quote_block = last_block.or(Some(block_number));
        let (reserve_in, reserve_out) = match state.reserves_for(pool.token_in) {
            Some(reserves) => reserves,
            None => continue,
        };

        let decimals_in = token_decimals_or_default(token_decimals.as_ref(), pool.token_in);
        let decimals_out = token_decimals_or_default(token_decimals.as_ref(), pool.token_out);
        if !is_liquid(reserve_in, decimals_in, min_liquidity_tokens)
            || !is_liquid(reserve_out, decimals_out, min_liquidity_tokens)
        {
            continue;
        }

        let expected_token_out = if pool.token_in == state.token0 {
            state.token1
        } else if pool.token_in == state.token1 {
            state.token0
        } else {
            continue;
        };
        if expected_token_out != pool.token_out {
            continue;
        }

        let profile = base_profiles
            .as_ref()
            .get(&pool.token_in)
            .copied()
            .unwrap_or(default_profile);
        let base_amount_in = profile.base_amount;
        let tolerance_bps = profile.slippage_tolerance_bps;
        let quote = match adjust_trade_size_sync(base_amount_in, tolerance_bps, |amount| {
            let quote = quote_exact_input_from_state(&state, pool.token_in, amount, pool.fee_bps)?;
            Ok(quote.map(|q| QuoteComputation {
                amount_in: amount,
                amount_out: q.amount_out,
                slippage_bps: q.price_impact_bps,
            }))
        }) {
            Ok(q) => q,
            Err(err) => {
                warn!(
                    error = %err,
                    pair = %format!("0x{}", hex::encode(pool.pair)),
                    "Failed to quote UniV2 pair"
                );
                continue;
            }
        };

        if let Some(quote) = quote {
            if quote.slippage_bps > tolerance_bps {
                continue;
            }
            let protected_out = apply_slippage(quote.amount_out, tolerance_bps);
            if protected_out.is_zero() {
                continue;
            }
            let weight = compute_edge_weight(
                protected_out,
                quote.amount_in,
                ESTIMATED_GAS_UNIV2,
                gas_price,
                quote.amount_in,
                native_price_for(pool.token_in, &native_token_prices),
            );
            let edge = Edge {
                from: pool.token_in,
                to: pool.token_out,
                rate_num: quote.amount_out,
                rate_den: quote.amount_in,
                venue: VenueEdge::UniV2 {
                    pair: pool.pair,
                    token_out: pool.token_out,
                    token0: state.token0,
                    token1: state.token1,
                    reserve_in,
                    reserve_out,
                    fee_bps: pool.fee_bps,
                },
                estimated_gas: ESTIMATED_GAS_UNIV2,
                weight,
                max_input: quote.amount_in,
                tolerance_bps,
                observed_slippage_bps: quote.slippage_bps,
                quote_block,
                active: true,
            };
            edges.push(edge);
        }
    }
    Ok(edges)
}

async fn collect_solidly_edges<C>(
    provider: Arc<Provider<C>>,
    chain_env_prefix: &str,
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
    gas_price: U256,
    native_token_prices: Arc<HashMap<Address, NativePrice>>,
    token_decimals: Arc<HashMap<Address, u8>>,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut edges = Vec::new();
    let env_key = format!("{chain_env_prefix}_SOLIDLY_V2_POOLS");
    if let Some((raw, source)) = env_var_with_fallback(&env_key, "SOLIDLY_V2_POOLS") {
        let pools = resolve_solidly_pools(&raw, &source)?;
        for pool in pools {
            let state = match load_pair_state(provider.clone(), pool.pair).await? {
                Some(state) => state,
                None => continue,
            };
            let profile = base_profiles
                .as_ref()
                .get(&pool.token_in)
                .copied()
                .unwrap_or(default_profile);
            let base_amount_in = profile.base_amount;
            let tolerance_bps = profile.slippage_tolerance_bps;
            // Stable-pool math requires real token decimals to normalize reserves to
            // 1e18. Fail closed for stable pools when decimals are unknown rather than
            // quoting on a wrong (defaulted) scale.
            let decimals0 = token_decimals.get(&state.token0).copied();
            let decimals1 = token_decimals.get(&state.token1).copied();
            if pool.stable && (decimals0.is_none() || decimals1.is_none()) {
                warn!(
                    target: "venue::solidly",
                    pair = ?pool.pair,
                    "skipping stable Solidly pool: missing token decimals for correct stableswap quote"
                );
                continue;
            }
            let decimals0 = decimals0.unwrap_or(18);
            let decimals1 = decimals1.unwrap_or(18);
            let solidly_state = SolidlyPairState {
                token0: state.token0,
                token1: state.token1,
                reserve0: state.reserve0,
                reserve1: state.reserve1,
                stable: pool.stable,
                decimals0,
                decimals1,
            };
            let quote = adjust_trade_size_sync(base_amount_in, tolerance_bps, |amount| {
                let quote =
                    quote_solidly_exact_input(&solidly_state, pool.token_in, amount, pool.fee_bps)?;
                Ok(quote.map(|q| QuoteComputation {
                    amount_in: amount,
                    amount_out: q.amount_out,
                    slippage_bps: q.price_impact_bps,
                }))
            })?;

            if let Some(quote) = quote {
                if quote.slippage_bps > tolerance_bps {
                    continue;
                }
                let protected_out = apply_slippage(quote.amount_out, tolerance_bps);
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(
                    protected_out,
                    quote.amount_in,
                    ESTIMATED_GAS_SOLIDLYV2,
                    gas_price,
                    quote.amount_in,
                    native_price_for(pool.token_in, &native_token_prices),
                );
                let (reserve_in, reserve_out) = match solidly_state.reserves_for(pool.token_in) {
                    Some(reserves) => reserves,
                    None => continue,
                };
                let edge = Edge {
                    from: pool.token_in,
                    to: pool.token_out,
                    rate_num: quote.amount_out,
                    rate_den: quote.amount_in,
                    venue: VenueEdge::SolidlyV2 {
                        pair: pool.pair,
                        token_out: pool.token_out,
                        token0: solidly_state.token0,
                        token1: solidly_state.token1,
                        stable: pool.stable,
                        reserve_in,
                        reserve_out,
                        fee_bps: pool.fee_bps,
                        decimals0,
                        decimals1,
                    },
                    estimated_gas: ESTIMATED_GAS_SOLIDLYV2,
                    weight,
                    max_input: quote.amount_in,
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: None,
                    active: true,
                };
                edges.push(edge);
            }
        }
    }
    Ok(edges)
}

async fn collect_univ4_edges(
    chain_env_prefix: &str,
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
    gas_price: U256,
    native_token_prices: Arc<HashMap<Address, NativePrice>>,
) -> Result<Vec<Edge>> {
    let mut edges = Vec::new();
    let env_key = format!("{chain_env_prefix}_UNIV4_POOLS");
    if let Some((raw, source)) = env_var_with_fallback(&env_key, "UNIV4_POOLS") {
        // The UniV4 quote here is a FIXED-PRICE (zero price-impact) approximation:
        // it assumes infinite liquidity at spot and produces phantom profits for any
        // non-trivial size. A correct V4 quote needs concentrated-liquidity tick
        // crossing (on-chain Quoter/StateView). Until that exists, these edges are
        // OFF by default and must be explicitly opted into.
        let allow_fixed_price = std::env::var("ENABLE_UNIV4_FIXED_PRICE_QUOTES")
            .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if !allow_fixed_price {
            warn!(
                target: "venue::univ4",
                source = %source,
                "UniV4 pools configured but disabled: fixed-price quote is unsound (no tick crossing). \
                 Set ENABLE_UNIV4_FIXED_PRICE_QUOTES=1 to override (NOT recommended for live capital)."
            );
            return Ok(edges);
        }
        let pools = resolve_univ4_pools(&raw, &source)?;
        for pool in pools {
            let profile = base_profiles
                .as_ref()
                .get(&pool.token_in)
                .copied()
                .unwrap_or(default_profile);
            let base_amount_in = profile.base_amount;
            let tolerance_bps = profile.slippage_tolerance_bps;
            let zero_for_one = pool.token_in == pool.token0;
            let quote = adjust_trade_size_sync(base_amount_in, tolerance_bps, |amount| {
                let quote = quote_fixed_price_exact_input(
                    pool.sqrt_price_x96,
                    amount,
                    pool.fee,
                    zero_for_one,
                )?;
                Ok(quote.map(|q| QuoteComputation {
                    amount_in: amount,
                    amount_out: q.amount_out,
                    slippage_bps: 0,
                }))
            })?;

            if let Some(quote) = quote {
                if quote.slippage_bps > tolerance_bps {
                    continue;
                }
                let protected_out = apply_slippage(quote.amount_out, tolerance_bps);
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(
                    protected_out,
                    quote.amount_in,
                    ESTIMATED_GAS_UNIV4,
                    gas_price,
                    quote.amount_in,
                    native_price_for(pool.token_in, &native_token_prices),
                );
                let edge = Edge {
                    from: pool.token_in,
                    to: pool.token_out,
                    rate_num: quote.amount_out,
                    rate_den: quote.amount_in,
                    venue: VenueEdge::Univ4 {
                        pool_manager: pool.pool_manager,
                        token0: pool.token0,
                        token1: pool.token1,
                        fee: pool.fee,
                        tick_spacing: pool.tick_spacing,
                        hooks: pool.hooks,
                        sqrt_price_x96: pool.sqrt_price_x96,
                    },
                    estimated_gas: ESTIMATED_GAS_UNIV4,
                    weight,
                    max_input: quote.amount_in,
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: None,
                    active: true,
                };
                edges.push(edge);
            }
        }
    }
    Ok(edges)
}

#[allow(clippy::too_many_arguments)]
pub async fn populate_edges<C>(
    g: &mut Graph,
    provider: Arc<Provider<C>>,
    pool_monitor: Option<Arc<crate::ingestion::PoolMonitor<C>>>,
    quoter: Arc<UniQuoter<C>>,
    univ3_validation: Option<UniV3ValidationConfig>,
    univ3_fee_tiers: Option<Arc<HashSet<u32>>>,
    bal_vault: Address,
    chain_env_prefix: &str,
    chain_name: String,
    token_whitelist: &HashSet<Address>,
    default_base_amount: U256,
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    max_slippage_bps: u32,
    gas_price: U256,
    token_decimals: Arc<HashMap<Address, u8>>,
    native_token_prices: Arc<HashMap<Address, NativePrice>>,
    min_liquidity_tokens: f64,
    min_edge_max_input: U256,
    low_liquidity: &[LowLiquidityPool],
    hot_univ2_pools: &[ResolvedUniV2PoolCfg],
    hot_univ3_pools: &[PoolRecord],
    hot_paths: Arc<HotPathCache>,
    quote_semaphore: Arc<Semaphore>,
    block_number: U64,
    max_quote_block_lag: U64,
    quoter_validation_once: Arc<OnceCell<()>>,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    ensure!(
        max_slippage_bps <= 10_000,
        "EDGE_SLIPPAGE_BPS must be less than or equal to 10_000 (100%). Got {max_slippage_bps}."
    );

    let mut edges = Vec::new();
    let default_profile = TradeSizing::new(default_base_amount, max_slippage_bps);
    let min_edge_health_score_bps = std::env::var("EDGE_MIN_HEALTH_SCORE_BPS")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(0)
        .min(10_000);

    if let Some(validation) = univ3_validation {
        if quoter_validation_once.get().is_none() {
            let quoter = Arc::clone(&quoter);
            let chain = chain_name.clone();
            let env_prefix = chain_env_prefix.to_string();
            let chain_for_error = chain.clone();
            let env_for_error = env_prefix.clone();
            let validation_for_init = validation.clone();
            if let Err(err) = quoter_validation_once
                .get_or_try_init(|| {
                    let quoter = Arc::clone(&quoter);
                    let chain = chain.clone();
                    let env_prefix = env_prefix.clone();
                    let validation = validation_for_init.clone();
                    async move {
                        match quoter.validate(&validation).await {
                            Ok(amount_out) => {
                                info!(
                                        chain = %chain,
                                        env = %env_prefix,
                                    token_in = %format!("0x{}", hex::encode(validation.token_in)),
                                    token_out = %format!("0x{}", hex::encode(validation.token_out)),
                                    fee_bps = validation.fee,
                                    amount_in = %validation.amount_in,
                                    amount_out = %amount_out,
                                    "Validated UniV3 quoter"
                                );
                                Ok::<(), anyhow::Error>(())
                            }
                            Err(err) => {
                                let err_msg = err.to_string();
                                error!(
                                chain = %chain,
                                env = %env_prefix,
                                token_in = %format!("0x{}", hex::encode(validation.token_in)),
                                token_out = %format!("0x{}", hex::encode(validation.token_out)),
                                fee_bps = validation.fee,
                                amount_in = %validation.amount_in,
                                error = %err,
                                "UniV3 quoter validation failed"
                                );
                                Err(anyhow::anyhow!(
                                    "UniV3 quoter validation failed for chain {chain} ({env_prefix}): {err_msg}"
                                ))
                            }
                        }
                    }
                })
                .await
            {
                error!(
                    chain = %chain_for_error,
                    env = %env_for_error,
                    error = %err,
                    "UniV3 quoter validation initialization failed"
                );
                return Err(err.context(format!(
                    "UniV3 quoter validation initialization failed for chain {chain_for_error} ({env_for_error})"
                )));
            }
        }
    }
    let edge_ctx = EdgeBuildContext {
        base_profiles: base_profiles.clone(),
        default_profile,
        gas_price,
        native_token_prices: Arc::clone(&native_token_prices),
        block_number,
    };
    let univ3_ctx = Univ3EdgeContext {
        edge_ctx: edge_ctx.clone(),
        allowed_fee_tiers: univ3_fee_tiers,
        quoter: Arc::clone(&quoter),
        hot_paths: Arc::clone(&hot_paths),
        quote_semaphore: Arc::clone(&quote_semaphore),
        quote_concurrency_limit: quote_semaphore.available_permits(),
        token_whitelist: Arc::new(token_whitelist.clone()),
        chain_env_prefix: chain_env_prefix.to_string(),
    };
    type EdgeJoinResult = (
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
    );
    let (
        mut univ3_edges,
        mut bal_edges,
        mut curve_edges,
        mut univ2_edges,
        mut solidly_edges,
        mut univ4_edges,
    ): EdgeJoinResult = tokio::try_join!(
        collect_univ3_edges(hot_univ3_pools, &univ3_ctx),
        collect_balancer_edges(provider.clone(), bal_vault, chain_env_prefix, &edge_ctx),
        collect_curve_edges(
            provider.clone(),
            chain_env_prefix,
            base_profiles.clone(),
            default_profile,
            gas_price,
            Arc::clone(&native_token_prices),
            block_number,
        ),
        collect_univ2_edges(
            provider.clone(),
            pool_monitor.clone(),
            hot_univ2_pools,
            base_profiles.clone(),
            default_profile,
            gas_price,
            Arc::clone(&native_token_prices),
            token_decimals.clone(),
            min_liquidity_tokens,
            block_number,
        ),
        collect_solidly_edges(
            provider.clone(),
            chain_env_prefix,
            base_profiles.clone(),
            default_profile,
            gas_price,
            Arc::clone(&native_token_prices),
            token_decimals.clone(),
        ),
        collect_univ4_edges(
            chain_env_prefix,
            base_profiles.clone(),
            default_profile,
            gas_price,
            Arc::clone(&native_token_prices),
        ),
    )?;

    for edges in [
        &mut univ3_edges,
        &mut bal_edges,
        &mut curve_edges,
        &mut univ2_edges,
        &mut solidly_edges,
        &mut univ4_edges,
    ] {
        for edge in edges.iter_mut() {
            apply_pruning(
                edge,
                min_edge_max_input,
                token_whitelist,
                min_edge_health_score_bps,
                block_number,
                max_quote_block_lag,
            );
        }
    }

    let mut stale_edges = 0usize;
    for edges in [
        &mut univ3_edges,
        &mut bal_edges,
        &mut curve_edges,
        &mut univ2_edges,
    ] {
        let removed = filter_stale_edges(edges, block_number, max_quote_block_lag);
        stale_edges = stale_edges.saturating_add(removed);
    }
    if stale_edges > 0 {
        warn!(
            stale_edges,
            current_block = %block_number,
            max_block_lag = %max_quote_block_lag,
            "Quarantined stale quote edges"
        );
    }

    for edge in univ3_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }
    for edge in bal_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }
    for edge in curve_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }
    for edge in univ2_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }
    for edge in solidly_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }
    for edge in univ4_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }

    if !low_liquidity.is_empty() {
        for pool in low_liquidity {
            for &(token_in, token_out) in
                [(pool.token0, pool.token1), (pool.token1, pool.token0)].iter()
            {
                let profile = base_profiles
                    .as_ref()
                    .get(&token_in)
                    .copied()
                    .unwrap_or(default_profile);
                let base_amount_in = profile.base_amount;
                let tolerance_bps = profile.slippage_tolerance_bps;
                let quote = match adjust_trade_size_sync(base_amount_in, tolerance_bps, |amount| {
                    let quote =
                        quote_exact_input_from_state(&pool.state, token_in, amount, pool.fee_bps)?;
                    Ok(quote.map(|q| QuoteComputation {
                        amount_in: amount,
                        amount_out: q.amount_out,
                        slippage_bps: q.price_impact_bps,
                    }))
                }) {
                    Ok(q) => q,
                    Err(err) => {
                        warn!(
                            error = %err,
                            pair = %format!("0x{}", hex::encode(pool.pair)),
                            "Failed to quote low-liquidity pool"
                        );
                        continue;
                    }
                };

                if let Some(quote) = quote {
                    if quote.slippage_bps > tolerance_bps {
                        continue;
                    }
                    let protected_out = apply_slippage(quote.amount_out, tolerance_bps);
                    if protected_out.is_zero() {
                        continue;
                    }
                    let weight = compute_edge_weight(
                        protected_out,
                        quote.amount_in,
                        ESTIMATED_GAS_UNIV2,
                        gas_price,
                        quote.amount_in,
                        native_price_for(token_in, &native_token_prices),
                    );
                    let (reserve_in, reserve_out) = match pool.state.reserves_for(token_in) {
                        Some(reserves) => reserves,
                        None => continue,
                    };
                    let edge = Edge {
                        from: token_in,
                        to: token_out,
                        rate_num: quote.amount_out,
                        rate_den: quote.amount_in,
                        venue: VenueEdge::UniV2 {
                            pair: pool.pair,
                            token_out,
                            token0: pool.state.token0,
                            token1: pool.state.token1,
                            reserve_in,
                            reserve_out,
                            fee_bps: pool.fee_bps,
                        },
                        estimated_gas: ESTIMATED_GAS_UNIV2,
                        weight,
                        max_input: quote.amount_in,
                        tolerance_bps,
                        observed_slippage_bps: quote.slippage_bps,
                        quote_block: Some(block_number),
                        active: true,
                    };
                    let mut edge = edge;
                    apply_pruning(
                        &mut edge,
                        min_edge_max_input,
                        token_whitelist,
                        min_edge_health_score_bps,
                        block_number,
                        max_quote_block_lag,
                    );
                    info!(
                        pair = %format!("0x{}", hex::encode(pool.pair)),
                        deviation_bps = pool.price_deviation_bps,
                        "Adding low-liquidity edge"
                    );
                    g.add_edge(edge.clone());
                    edges.push(edge);
                    continue;
                }
            }
        }
    }

    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::{Address, U64};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_file(prefix: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}_{timestamp}.json5"))
    }

    #[test]
    fn rpc_quote_timeout_parser_uses_default_for_invalid_values() {
        assert_eq!(
            parse_rpc_quote_timeout_secs(None),
            DEFAULT_RPC_QUOTE_TIMEOUT_SECS
        );
        assert_eq!(
            parse_rpc_quote_timeout_secs(Some("0")),
            DEFAULT_RPC_QUOTE_TIMEOUT_SECS
        );
        assert_eq!(
            parse_rpc_quote_timeout_secs(Some("-2")),
            DEFAULT_RPC_QUOTE_TIMEOUT_SECS
        );
        assert_eq!(
            parse_rpc_quote_timeout_secs(Some("not-a-number")),
            DEFAULT_RPC_QUOTE_TIMEOUT_SECS
        );
    }

    #[test]
    fn rpc_quote_timeout_parser_accepts_positive_values() {
        assert_eq!(parse_rpc_quote_timeout_secs(Some("15")), 15);
    }

    #[test]
    fn classify_provider_prefers_prefixed_single_url() {
        std::env::set_var("BASE_RPC_URL", "http://127.0.0.1:8545");
        std::env::remove_var("BASE_RPC_URLS");
        std::env::remove_var("RPC_URL");
        std::env::remove_var("RPC_URLS");

        let class = classify_provider_for_chain("BASE");

        std::env::remove_var("BASE_RPC_URL");
        assert_eq!(class, "local_fork");
    }

    #[test]
    fn classify_provider_uses_prefixed_url_list_when_single_missing() {
        std::env::remove_var("BASE_RPC_URL");
        std::env::set_var(
            "BASE_RPC_URLS",
            " https://base.example , http://127.0.0.1:8545 ",
        );
        std::env::remove_var("RPC_URL");
        std::env::remove_var("RPC_URLS");

        let class = classify_provider_for_chain("BASE");

        std::env::remove_var("BASE_RPC_URLS");
        assert_eq!(class, "upstream");
    }

    #[test]
    fn classify_provider_uses_global_url_list_fallback() {
        std::env::remove_var("BASE_RPC_URL");
        std::env::remove_var("BASE_RPC_URLS");
        std::env::remove_var("RPC_URL");
        std::env::set_var("RPC_URLS", " , http://localhost:8545, https://base.example");

        let class = classify_provider_for_chain("BASE");

        std::env::remove_var("RPC_URLS");
        assert_eq!(class, "local_fork");
    }

    #[test]
    fn optimal_trade_size_prefers_peak_profit() {
        let base = U256::from(200u64);
        let tolerance = 100u32;

        let quote = adjust_trade_size_sync(base, tolerance, |amount| {
            let x = amount.as_u64() as f64;
            let profit = (20000.0 - (x - 100.0).powi(2)).max(0.0);
            if profit <= 0.0 {
                return Ok(None);
            }
            let out = amount + U256::from(profit as u64);
            Ok(Some(QuoteComputation {
                amount_in: amount,
                amount_out: out,
                slippage_bps: 50,
            }))
        })
        .expect("sizing should succeed");

        let quote = quote.expect("expected a quote");
        let amount = quote.amount_in.as_u64();
        assert!(
            (80..=120).contains(&amount),
            "amount {amount} not near optimum"
        );
    }

    #[test]
    fn sizing_falls_back_when_large_slippage() {
        let base = U256::from(100u64);
        let tolerance = 25u32;

        let quote = adjust_trade_size_sync(base, tolerance, |amount| {
            let slippage_bps = if amount <= U256::from(10u64) {
                10
            } else {
                50_000
            };
            if slippage_bps > tolerance {
                return Ok(None);
            }
            Ok(Some(QuoteComputation {
                amount_in: amount,
                amount_out: amount + U256::from(5u64),
                slippage_bps,
            }))
        })
        .expect("sizing should succeed");

        let quote = quote.expect("expected fallback quote");
        assert_eq!(quote.amount_in, U256::from(10u64));
    }

    #[test]
    fn balancer_min_trade_error_is_non_fatal() {
        let err = anyhow::anyhow!(
            "Contract call reverted with data: 0x08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000742414c2335303000000000000000000000000000000000000000000000000000"
        );

        assert!(is_balancer_small_trade_error(&err));

        let other = anyhow::anyhow!("some other revert");
        assert!(!is_balancer_small_trade_error(&other));
    }

    #[test]
    fn stale_edges_are_quarantined_by_block_lag() {
        fn edge_with_block(block: Option<U64>) -> Edge {
            Edge {
                from: Address::zero(),
                to: Address::zero(),
                rate_num: U256::one(),
                rate_den: U256::one(),
                venue: VenueEdge::UniV2 {
                    pair: Address::zero(),
                    token_out: Address::zero(),
                    token0: Address::zero(),
                    token1: Address::zero(),
                    reserve_in: U256::from(1u64),
                    reserve_out: U256::from(1u64),
                    fee_bps: 30,
                },
                estimated_gas: 0,
                weight: 0,
                max_input: U256::from(1u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: block,
                active: true,
            }
        }

        let current = U64::from(100u64);
        let mut edges = vec![edge_with_block(Some(U64::from(96u64)))];
        let removed = filter_stale_edges(&mut edges, current, U64::from(2u64));
        assert_eq!(removed, 1);
        assert!(edges.is_empty());

        let mut strict_edges = vec![edge_with_block(Some(current)), edge_with_block(None)];
        let removed = filter_stale_edges(&mut strict_edges, current, U64::zero());
        assert_eq!(removed, 1);
        assert_eq!(strict_edges.len(), 1);
        assert_eq!(strict_edges[0].quote_block, Some(current));
    }

    #[test]
    fn health_score_penalizes_stale_and_high_slippage_edges() {
        let mut edge = Edge {
            from: Address::zero(),
            to: Address::zero(),
            rate_num: U256::from(1000u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::UniV2 {
                pair: Address::zero(),
                token_out: Address::zero(),
                token0: Address::zero(),
                token1: Address::zero(),
                reserve_in: U256::from(1_000u64),
                reserve_out: U256::from(1_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000_000u64),
            tolerance_bps: 50,
            observed_slippage_bps: 50,
            quote_block: Some(U64::from(100u64)),
            active: true,
        };

        let healthy = edge_health_score_bps(&edge, U64::from(101u64), U64::from(10u64));
        edge.observed_slippage_bps = 500;
        edge.quote_block = Some(U64::from(80u64));
        let degraded = edge_health_score_bps(&edge, U64::from(101u64), U64::from(10u64));

        assert!(healthy > degraded);
    }
    #[test]
    fn parse_pool_configs_supports_file_paths() {
        let path = unique_temp_file("arbot_bal_pool_test");
        let json = r#"[
            {
                "poolId": "0x00000000000000000000000000000000000000000000000000000000000000aa",
                "tokenIn": "0x00000000000000000000000000000000000000a1",
                "tokenOut": "0x00000000000000000000000000000000000000b2"
            }
        ]"#;
        fs::write(&path, json).expect("failed to write temp config");

        let path_str = path.to_str().expect("temp path should be valid utf-8");

        let from_path = parse_pool_configs::<BalPoolCfg>(path_str, "test env var")
            .expect("should parse config from path");
        assert_eq!(from_path.len(), 1);
        assert_eq!(
            from_path[0].pool_id,
            "0x00000000000000000000000000000000000000000000000000000000000000aa"
        );

        let from_prefixed =
            parse_pool_configs::<BalPoolCfg>(&format!("@{path_str}"), "test env var")
                .expect("should parse config when prefixed with @");
        assert_eq!(from_prefixed.len(), 1);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn resolve_bal_pools_skips_invalid_entries() {
        let raw = r#"
        [
            {
                "poolId": "0x00000000000000000000000000000000000000000000000000000000000000aa",
                "tokenIn": "0x00000000000000000000000000000000000000a1",
                "tokenOut": "0x00000000000000000000000000000000000000b2"
            },
            {
                "poolId": "0x1234",
                "tokenIn": "0x00000000000000000000000000000000000000a1",
                "tokenOut": "0x00000000000000000000000000000000000000b2"
            }
        ]
        "#;

        let pools = resolve_bal_pools(raw, "test").expect("should parse pool list");
        assert_eq!(pools.len(), 1);
        assert_eq!(
            hex::encode(pools[0].pool_id),
            "00000000000000000000000000000000000000000000000000000000000000aa"
        );
    }

    #[test]
    fn resolve_bal_pools_accepts_63_char_pool_ids_by_padding() {
        let raw = r#"
        [
            {
                "poolId": "0x32296969ef14eb0c6d29669c550d4a044913023000200000000000000000064",
                "tokenIn": "0x00000000000000000000000000000000000000a1",
                "tokenOut": "0x00000000000000000000000000000000000000b2"
            }
        ]
        "#;

        let pools = resolve_bal_pools(raw, "test").expect("should parse pool list");
        assert_eq!(pools.len(), 1);
        assert_eq!(
            hex::encode(pools[0].pool_id),
            "032296969ef14eb0c6d29669c550d4a044913023000200000000000000000064"
        );
    }

    #[test]
    fn resolve_bal_pools_accepts_pool_address_hints() {
        let raw = r#"
        [
            {
                "poolId": "0x32296969Ef14EB0c6d29669C550D4a0449130230",
                "tokenIn": "0x00000000000000000000000000000000000000a1",
                "tokenOut": "0x00000000000000000000000000000000000000b2"
            }
        ]
        "#;

        let pools = resolve_bal_pools(raw, "test").expect("should parse pool list");
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].pool_id, [0u8; 32]);
        assert_eq!(
            pools[0].pool_address_hint,
            Some(
                "0x32296969Ef14EB0c6d29669C550D4a0449130230"
                    .parse()
                    .unwrap()
            )
        );
    }
    #[test]
    fn forced_discovery_quote_reserves_budget_for_skipped_pairs() {
        let used = AtomicUsize::new(0);
        assert!(reserve_forced_discovery_quote(false, &used, 2));
        assert!(reserve_forced_discovery_quote(false, &used, 2));
        assert!(!reserve_forced_discovery_quote(false, &used, 2));
    }

    #[test]
    fn forced_discovery_quote_does_not_consume_budget_for_hot_pairs() {
        let used = AtomicUsize::new(0);
        assert!(reserve_forced_discovery_quote(true, &used, 1));
        assert_eq!(used.load(Ordering::Relaxed), 0);
    }
}
