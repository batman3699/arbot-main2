use anyhow::{anyhow, Context, Result};
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;
use ethers::types::{I256, U256, U64};
use rand::seq::index::sample;
use rand::thread_rng;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use rust_decimal::MathematicalOps;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::pool_store::PoolRecord;
use crate::quote_univ2::load_pair_state;
use crate::util::u256_to_decimal;

mod univ2_events {
    use ethers::prelude::abigen;
    abigen!(
        UniV2PairEvents,
        r#"[event Swap(address indexed sender, uint amount0In, uint amount1In, uint amount0Out, uint amount1Out, address indexed to)]"#,
    );
}

mod univ3_events {
    use ethers::prelude::abigen;
    abigen!(
        UniV3PoolEvents,
        r#"[event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)]"#,
    );
}

abigen!(
    UniV3PoolReader,
    r#"[function liquidity() external view returns (uint128)]"#,
);

#[derive(Clone, Debug)]
pub struct HotPoolConfig {
    pub min_liquidity_tokens: f64,
    pub max_hot_pools: usize,
    pub max_cold_pools: usize,
    pub event_sampling_rate: f64,
    pub event_sampling_blocks: u64,
    pub refresh_interval: Duration,
}

#[derive(Clone, Debug)]
struct PoolScore {
    pool: PoolRecord,
    liquidity_score: Decimal,
    volume_score: Decimal,
}

impl PoolScore {
    fn score_tuple(&self) -> (Decimal, Decimal) {
        (self.liquidity_score, self.volume_score)
    }
}

fn token_amount(reserve: U256, decimals: u8) -> Decimal {
    if reserve.is_zero() {
        return Decimal::ZERO;
    }
    let scaled = u256_to_decimal(reserve);
    let scale = Decimal::from(10u64)
        .checked_powu(decimals as u64)
        .unwrap_or(Decimal::ZERO);
    if scale.is_zero() {
        return Decimal::ZERO;
    }
    scaled.checked_div(scale).unwrap_or(Decimal::ZERO)
}

fn pool_sample_indices(total: usize, rate: f64) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    let clamped = rate.clamp(0.0, 1.0);
    if clamped <= 0.0 {
        return Vec::new();
    }
    let count = ((total as f64) * clamped).ceil() as usize;
    if count >= total {
        return (0..total).collect();
    }
    sample(&mut thread_rng(), total, count).into_vec()
}

fn abs_i256(value: I256) -> U256 {
    if value.is_negative() {
        (!value.into_raw()).saturating_add(U256::one())
    } else {
        value.into_raw()
    }
}

async fn univ2_volume<C>(
    provider: Arc<Provider<C>>,
    pool: Address,
    from_block: U64,
    to_block: U64,
) -> Result<U256>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = univ2_events::UniV2PairEvents::new(pool, provider);
    let events = contract
        .event::<univ2_events::SwapFilter>()
        .from_block(from_block)
        .to_block(to_block)
        .query()
        .await
        .context("query univ2 swap logs")?;
    let mut total = U256::zero();
    for event in events {
        total = total
            .saturating_add(event.amount_0_in)
            .saturating_add(event.amount_1_in)
            .saturating_add(event.amount_0_out)
            .saturating_add(event.amount_1_out);
    }
    Ok(total)
}

async fn univ3_volume<C>(
    provider: Arc<Provider<C>>,
    pool: Address,
    from_block: U64,
    to_block: U64,
) -> Result<U256>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = univ3_events::UniV3PoolEvents::new(pool, provider);
    let events = contract
        .event::<univ3_events::SwapFilter>()
        .from_block(from_block)
        .to_block(to_block)
        .query()
        .await
        .context("query univ3 swap logs")?;
    let mut total = U256::zero();
    for event in events {
        total = total
            .saturating_add(abs_i256(event.amount_0))
            .saturating_add(abs_i256(event.amount_1));
    }
    Ok(total)
}

async fn univ2_liquidity_score<C>(
    provider: Arc<Provider<C>>,
    record: &PoolRecord,
    token_decimals: &HashMap<Address, u8>,
) -> Result<Option<Decimal>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let Some(state) = load_pair_state(provider, record.pool).await? else {
        return Ok(None);
    };
    let decimals0 = token_decimals.get(&state.token0).copied().unwrap_or(18);
    let decimals1 = token_decimals.get(&state.token1).copied().unwrap_or(18);
    let reserve0_tokens = token_amount(state.reserve0, decimals0);
    let reserve1_tokens = token_amount(state.reserve1, decimals1);
    Ok(Some(
        reserve0_tokens
            .checked_add(reserve1_tokens)
            .unwrap_or(Decimal::ZERO),
    ))
}

async fn univ3_liquidity_score<C>(
    provider: Arc<Provider<C>>,
    record: &PoolRecord,
) -> Result<Decimal>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = UniV3PoolReader::new(record.pool, provider);
    let liquidity = contract
        .liquidity()
        .call()
        .await
        .context("read univ3 liquidity")?;
    Ok(Decimal::from_u128(liquidity).unwrap_or(Decimal::ZERO))
}

async fn current_block<C>(provider: Arc<Provider<C>>) -> U64
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    provider.get_block_number().await.unwrap_or_default()
}

pub async fn rank_univ2_pools<C>(
    provider: Arc<Provider<C>>,
    cold_pools: &[PoolRecord],
    token_decimals: &HashMap<Address, u8>,
    config: &HotPoolConfig,
) -> Result<Vec<PoolRecord>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let started_at = Instant::now();
    let rpc_timeout = hot_pool_rpc_timeout();
    let mut scores = Vec::new();
    let capped = cold_pools
        .iter()
        .take(config.max_cold_pools.max(1))
        .cloned()
        .collect::<Vec<_>>();

    let block = current_block(provider.clone()).await;
    let from_block = block.saturating_sub(U64::from(config.event_sampling_blocks));
    let sample_indices = pool_sample_indices(capped.len(), config.event_sampling_rate);
    let mut sampled_volume: HashMap<Address, Decimal> = HashMap::new();
    let mut liquidity_failures = 0usize;

    for idx in sample_indices {
        let record = &capped[idx];
        match timeout(
            rpc_timeout,
            univ2_volume(provider.clone(), record.pool, from_block, block),
        )
        .await
        {
            Err(_) => {
                warn!(
                    timeout_ms = rpc_timeout.as_millis() as u64,
                    pool = %format!("0x{}", hex::encode(record.pool)),
                    "timed out loading univ2 swap volume"
                );
            }
            Ok(Err(err)) => {
                warn!(
                    error = %err,
                    pool = %format!("0x{}", hex::encode(record.pool)),
                    "failed to load univ2 swap volume"
                );
            }
            Ok(Ok(volume)) => {
                let volume_dec = u256_to_decimal(volume);
                sampled_volume.insert(record.pool, volume_dec);
            }
        }
    }

    for record in capped.iter() {
        let Some(liquidity_score) = (match timeout(
            rpc_timeout,
            univ2_liquidity_score(provider.clone(), record, token_decimals),
        )
        .await
        {
            Err(_) => {
                liquidity_failures = liquidity_failures.saturating_add(1);
                warn!(
                    timeout_ms = rpc_timeout.as_millis() as u64,
                    pool = %format!("0x{}", hex::encode(record.pool)),
                    "timed out loading univ2 liquidity"
                );
                None
            }
            Ok(Err(err)) => {
                liquidity_failures = liquidity_failures.saturating_add(1);
                warn!(
                    error = %err,
                    pool = %format!("0x{}", hex::encode(record.pool)),
                    "failed to load univ2 liquidity"
                );
                None
            }
            Ok(Ok(value)) => value,
        }) else {
            continue;
        };
        if config.min_liquidity_tokens > 0.0 {
            let min = Decimal::from_f64(config.min_liquidity_tokens).unwrap_or(Decimal::ZERO);
            if liquidity_score < min {
                continue;
            }
        }
        let volume_score = sampled_volume
            .get(&record.pool)
            .cloned()
            .unwrap_or(Decimal::ZERO);
        scores.push(PoolScore {
            pool: record.clone(),
            liquidity_score,
            volume_score,
        });
    }

    scores.sort_by_key(|score| Reverse(score.score_tuple()));
    let hot = scores
        .into_iter()
        .take(config.max_hot_pools.max(1))
        .map(|score| score.pool)
        .collect::<Vec<_>>();
    if hot.is_empty() && liquidity_failures > 0 {
        return Err(anyhow!(
            "univ2 ranking aborted: {liquidity_failures} liquidity RPC calls failed"
        ));
    }
    info!(
        cold_pool_records = capped.len(),
        sampled_pools = sampled_volume.len(),
        hot_pools = hot.len(),
        elapsed_ms = started_at.elapsed().as_millis() as u64,
        timeout_ms = rpc_timeout.as_millis() as u64,
        "completed univ2 hot pool ranking"
    );
    Ok(hot)
}

pub async fn rank_univ3_pools<C>(
    provider: Arc<Provider<C>>,
    cold_pools: &[PoolRecord],
    config: &HotPoolConfig,
) -> Result<Vec<PoolRecord>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let started_at = Instant::now();
    let rpc_timeout = hot_pool_rpc_timeout();
    let mut scores = Vec::new();
    let capped = cold_pools
        .iter()
        .take(config.max_cold_pools.max(1))
        .cloned()
        .collect::<Vec<_>>();

    let block = current_block(provider.clone()).await;
    let from_block = block.saturating_sub(U64::from(config.event_sampling_blocks));
    let sample_indices = pool_sample_indices(capped.len(), config.event_sampling_rate);
    let mut sampled_volume: HashMap<Address, Decimal> = HashMap::new();
    let mut liquidity_failures = 0usize;

    for idx in sample_indices {
        let record = &capped[idx];
        match timeout(
            rpc_timeout,
            univ3_volume(provider.clone(), record.pool, from_block, block),
        )
        .await
        {
            Err(_) => {
                warn!(
                    timeout_ms = rpc_timeout.as_millis() as u64,
                    pool = %format!("0x{}", hex::encode(record.pool)),
                    "timed out loading univ3 swap volume"
                );
            }
            Ok(Err(err)) => {
                warn!(
                    error = %err,
                    pool = %format!("0x{}", hex::encode(record.pool)),
                    "failed to load univ3 swap volume"
                );
            }
            Ok(Ok(volume)) => {
                let volume_dec = u256_to_decimal(volume);
                sampled_volume.insert(record.pool, volume_dec);
            }
        }
    }

    for record in capped.iter() {
        let liquidity_score =
            match timeout(rpc_timeout, univ3_liquidity_score(provider.clone(), record)).await {
                Err(_) => {
                    liquidity_failures = liquidity_failures.saturating_add(1);
                    warn!(
                        timeout_ms = rpc_timeout.as_millis() as u64,
                        pool = %format!("0x{}", hex::encode(record.pool)),
                        "timed out loading univ3 liquidity"
                    );
                    continue;
                }
                Ok(Err(err)) => {
                    liquidity_failures = liquidity_failures.saturating_add(1);
                    warn!(
                        error = %err,
                        pool = %format!("0x{}", hex::encode(record.pool)),
                        "failed to load univ3 liquidity"
                    );
                    continue;
                }
                Ok(Ok(value)) => value,
            };
        if config.min_liquidity_tokens > 0.0 {
            let min = Decimal::from_f64(config.min_liquidity_tokens).unwrap_or(Decimal::ZERO);
            if liquidity_score < min {
                continue;
            }
        }
        let volume_score = sampled_volume
            .get(&record.pool)
            .cloned()
            .unwrap_or(Decimal::ZERO);
        scores.push(PoolScore {
            pool: record.clone(),
            liquidity_score,
            volume_score,
        });
    }

    scores.sort_by_key(|score| Reverse(score.score_tuple()));
    let hot = scores
        .into_iter()
        .take(config.max_hot_pools.max(1))
        .map(|score| score.pool)
        .collect::<Vec<_>>();
    if hot.is_empty() && liquidity_failures > 0 {
        return Err(anyhow!(
            "univ3 ranking aborted: {liquidity_failures} liquidity RPC calls failed"
        ));
    }
    info!(
        cold_pool_records = capped.len(),
        sampled_pools = sampled_volume.len(),
        hot_pools = hot.len(),
        elapsed_ms = started_at.elapsed().as_millis() as u64,
        timeout_ms = rpc_timeout.as_millis() as u64,
        "completed univ3 hot pool ranking"
    );
    Ok(hot)
}

fn hot_pool_rpc_timeout() -> Duration {
    let ms = std::env::var("HOT_POOL_RPC_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(1_500)
        .max(100);
    Duration::from_millis(ms)
}

pub fn log_hot_pool_refresh(chain: &str, venue: &str, kind: &str, hot: usize) {
    info!(
        chain = %chain,
        venue = %venue,
        kind = %kind,
        hot_pools = hot,
        "refreshed hot pool list"
    );
}
