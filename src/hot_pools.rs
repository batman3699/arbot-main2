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
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use futures_util::{stream, StreamExt};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::pool_store::PoolRecord;
use crate::quote_univ2::load_pair_state;
use crate::util::{u256_to_decimal, IERC20};

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

#[derive(Clone, Debug, Default)]
pub struct UniV3RankContext {
    pub hub_tokens: Vec<Address>,
    pub hub_usd_prices: HashMap<Address, f64>,
    pub token_decimals: HashMap<Address, u8>,
    pub pinned_pairs: Vec<(Address, Address)>,
}

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

fn volume_sample_indices(total: usize, rate: f64) -> Vec<usize> {
    let max_samples = hot_pool_max_volume_samples();
    let mut indices = pool_sample_indices(total, rate);
    if indices.len() > max_samples {
        indices.truncate(max_samples);
    }
    indices
}

fn hot_pool_max_volume_samples() -> usize {
    std::env::var("HOT_POOL_MAX_VOLUME_SAMPLES")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(64)
        .clamp(16, 512)
}

fn hot_pool_skip_volume() -> bool {
    std::env::var("HOT_POOL_SKIP_VOLUME")
        .ok()
        .map(|raw| {
            matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
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

fn canonical_pair(a: Address, b: Address) -> (Address, Address) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// All undirected pairs among configured hub tokens (e.g. WETH/USDC).
pub fn build_pinned_hub_pairs(hub_tokens: &[Address]) -> Vec<(Address, Address)> {
    let mut pairs = Vec::new();
    for i in 0..hub_tokens.len() {
        for j in (i + 1)..hub_tokens.len() {
            pairs.push(canonical_pair(hub_tokens[i], hub_tokens[j]));
        }
    }
    pairs
}

/// Prefer stable hubs for USD accounting, matching offline rank_base_pools.py.
pub fn hub_priority_order(hub_tokens: &[Address]) -> Vec<Address> {
    const PREFERRED: &[&str] = &[
        "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", // USDC
        "0x4200000000000000000000000000000000000006", // WETH
        "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf", // cbBTC
        "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca", // USDbC
        "0x50c5725949a6f0c72e6c4a641f24049a917db0cb", // DAI
        "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22", // cbETH
        "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452", // wstETH
        "0x940181a94a35a4569e4529a3cdfb74e38fd98631", // AERO
    ];
    let hub_set: HashSet<Address> = hub_tokens.iter().copied().collect();
    let mut ordered = Vec::new();
    for raw in PREFERRED {
        if let Ok(addr) = Address::from_str(raw) {
            if hub_set.contains(&addr) {
                ordered.push(addr);
            }
        }
    }
    for hub in hub_tokens {
        if !ordered.contains(hub) {
            ordered.push(*hub);
        }
    }
    ordered
}

fn pick_hub_token(token0: Address, token1: Address, hub_priority: &[Address]) -> Option<Address> {
    for hub in hub_priority {
        if *hub == token0 || *hub == token1 {
            return Some(*hub);
        }
    }
    None
}

fn pool_matches_pair(record: &PoolRecord, a: Address, b: Address) -> bool {
    (record.token0 == a && record.token1 == b) || (record.token0 == b && record.token1 == a)
}

fn collect_pinned_pools(cold: &[PoolRecord], pinned_pairs: &[(Address, Address)]) -> Vec<PoolRecord> {
    let mut seen = HashSet::new();
    let mut pinned = Vec::new();
    for (a, b) in pinned_pairs {
        for record in cold {
            if pool_matches_pair(record, *a, *b) && seen.insert(record.pool) {
                pinned.push(record.clone());
            }
        }
    }
    pinned
}

fn finalize_hot_with_pins(
    scored_hot: Vec<PoolRecord>,
    pinned: Vec<PoolRecord>,
    max_hot: usize,
) -> Vec<PoolRecord> {
    let max_hot = max_hot.max(1);
    let mut hot = Vec::with_capacity(max_hot);
    let mut seen = HashSet::new();
    for record in pinned {
        if seen.insert(record.pool) {
            hot.push(record);
        }
    }
    for record in scored_hot {
        if hot.len() >= max_hot {
            break;
        }
        if seen.insert(record.pool) {
            hot.push(record);
        }
    }
    hot
}

async fn univ3_hub_usd_liquidity_score<C>(
    provider: Arc<Provider<C>>,
    record: &PoolRecord,
    rank_ctx: &UniV3RankContext,
) -> Result<Option<Decimal>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let hub = match pick_hub_token(record.token0, record.token1, &rank_ctx.hub_tokens) {
        Some(hub) => hub,
        None => return Ok(None),
    };
    let price = match rank_ctx
        .hub_usd_prices
        .get(&hub)
        .copied()
        .filter(|value| value.is_finite() && *value > 0.0)
    {
        Some(price) => price,
        None => return Ok(None),
    };
    let decimals = rank_ctx.token_decimals.get(&hub).copied().unwrap_or(18);
    let contract = IERC20::new(hub, provider);
    let balance = contract
        .balance_of(record.pool)
        .call()
        .await
        .context("read hub token balanceOf(pool)")?;
    if balance.is_zero() {
        return Ok(None);
    }
    let tokens = token_amount(balance, decimals);
    let usd = tokens.checked_mul(
        Decimal::from_f64(price).unwrap_or(Decimal::ZERO),
    ).unwrap_or(Decimal::ZERO);
    if usd.is_zero() {
        return Ok(None);
    }
    Ok(Some(usd))
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
    let volume_timeout = hot_pool_volume_timeout();
    let mut scores = Vec::new();
    let capped = cold_pools
        .iter()
        .take(config.max_cold_pools.max(1))
        .cloned()
        .collect::<Vec<_>>();

    let mut liquidity_failures = 0usize;
    let concurrency = hot_pool_rank_concurrency();

    // Liquidity scoring first: volume log queries are expensive and should
    // only run on the top liquidity candidates, not random cold-pool indices.
    let token_decimals = Arc::new(token_decimals.clone());
    let min_liquidity_tokens = config.min_liquidity_tokens;
    let liquidity_outcomes: Vec<(PoolRecord, Option<Decimal>, bool)> =
        stream::iter(capped.iter().cloned().map(|record| {
            let provider = provider.clone();
            let token_decimals = token_decimals.clone();
            async move {
                match timeout(
                    rpc_timeout,
                    univ2_liquidity_score(provider, &record, token_decimals.as_ref()),
                )
                .await
                {
                    Err(_) => {
                        warn!(
                            timeout_ms = rpc_timeout.as_millis() as u64,
                            pool = %format!("0x{}", hex::encode(record.pool)),
                            "timed out loading univ2 liquidity"
                        );
                        (record, None, true)
                    }
                    Ok(Err(err)) => {
                        warn!(
                            error = %err,
                            pool = %format!("0x{}", hex::encode(record.pool)),
                            "failed to load univ2 liquidity"
                        );
                        (record, None, true)
                    }
                    Ok(Ok(value)) => (record, value, false),
                }
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut liquidity_ranked: Vec<(PoolRecord, Decimal)> = Vec::new();
    for (record, liquidity_value, failed) in liquidity_outcomes {
        if failed {
            liquidity_failures = liquidity_failures.saturating_add(1);
            continue;
        }
        let Some(liquidity_score) = liquidity_value else {
            continue;
        };
        if min_liquidity_tokens > 0.0 {
            let min = Decimal::from_f64(min_liquidity_tokens).unwrap_or(Decimal::ZERO);
            if liquidity_score < min {
                continue;
            }
        }
        liquidity_ranked.push((record, liquidity_score));
    }
    liquidity_ranked.sort_by_key(|(_, liquidity)| Reverse(*liquidity));

    let sampled_volume = if hot_pool_skip_volume() {
        HashMap::new()
    } else {
        let block = current_block(provider.clone()).await;
        let from_block = block.saturating_sub(U64::from(config.event_sampling_blocks));
        let volume_candidates: Vec<PoolRecord> = {
            let probe_cap = hot_pool_max_volume_samples()
                .min(volume_sample_indices(liquidity_ranked.len(), config.event_sampling_rate).len())
                .max(1)
                .min(liquidity_ranked.len());
            liquidity_ranked
                .iter()
                .take(probe_cap)
                .map(|(record, _)| record.clone())
                .collect()
        };
        let volume_pairs: Vec<(Address, Decimal)> =
            stream::iter(volume_candidates.into_iter().map(|record| {
                let pool = record.pool;
                let provider = provider.clone();
                async move {
                    match timeout(volume_timeout, univ2_volume(provider, pool, from_block, block))
                        .await
                    {
                        Err(_) => {
                            warn!(
                                timeout_ms = volume_timeout.as_millis() as u64,
                                pool = %format!("0x{}", hex::encode(pool)),
                                "timed out loading univ2 swap volume"
                            );
                            None
                        }
                        Ok(Err(err)) => {
                            warn!(
                                error = %err,
                                pool = %format!("0x{}", hex::encode(pool)),
                                "failed to load univ2 swap volume"
                            );
                            None
                        }
                        Ok(Ok(volume)) => Some((pool, u256_to_decimal(volume))),
                    }
                }
            }))
            .buffer_unordered(concurrency)
            .filter_map(|entry| async move { entry })
            .collect()
            .await;
        volume_pairs.into_iter().collect()
    };

    for (record, liquidity_score) in liquidity_ranked {
        let volume_score = sampled_volume
            .get(&record.pool)
            .cloned()
            .unwrap_or(Decimal::ZERO);
        scores.push(PoolScore {
            pool: record,
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
    rank_ctx: &UniV3RankContext,
) -> Result<Vec<PoolRecord>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let started_at = Instant::now();
    let rpc_timeout = hot_pool_rpc_timeout();
    let volume_timeout = hot_pool_volume_timeout();
    let mut scores = Vec::new();
    let pinned = collect_pinned_pools(cold_pools, &rank_ctx.pinned_pairs);
    let capped = cold_pools
        .iter()
        .take(config.max_cold_pools.max(1))
        .cloned()
        .collect::<Vec<_>>();

    let mut liquidity_failures = 0usize;
    let concurrency = hot_pool_rank_concurrency();
    let rank_ctx = Arc::new(rank_ctx.clone());

    // Hub-side USD liquidity scoring; volume probes only on top candidates.
    let liquidity_outcomes: Vec<(PoolRecord, Option<Decimal>, bool)> =
        stream::iter(capped.iter().cloned().map(|record| {
            let provider = provider.clone();
            let rank_ctx = rank_ctx.clone();
            async move {
                match timeout(
                    rpc_timeout,
                    univ3_hub_usd_liquidity_score(provider, &record, rank_ctx.as_ref()),
                )
                .await
                {
                    Err(_) => {
                        warn!(
                            timeout_ms = rpc_timeout.as_millis() as u64,
                            pool = %format!("0x{}", hex::encode(record.pool)),
                            "timed out loading univ3 liquidity"
                        );
                        (record, None, true)
                    }
                    Ok(Err(err)) => {
                        warn!(
                            error = %err,
                            pool = %format!("0x{}", hex::encode(record.pool)),
                            "failed to load univ3 liquidity"
                        );
                        (record, None, true)
                    }
                    Ok(Ok(value)) => (record, value, false),
                }
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let min_liquidity_tokens = config.min_liquidity_tokens;
    let mut liquidity_ranked: Vec<(PoolRecord, Decimal)> = Vec::new();
    for (record, liquidity_value, failed) in liquidity_outcomes {
        if failed {
            liquidity_failures = liquidity_failures.saturating_add(1);
            continue;
        }
        let Some(liquidity_score) = liquidity_value else {
            continue;
        };
        if min_liquidity_tokens > 0.0 {
            let min = Decimal::from_f64(min_liquidity_tokens).unwrap_or(Decimal::ZERO);
            if liquidity_score < min {
                continue;
            }
        }
        liquidity_ranked.push((record, liquidity_score));
    }
    liquidity_ranked.sort_by_key(|(_, liquidity)| Reverse(*liquidity));

    let sampled_volume = if hot_pool_skip_volume() {
        HashMap::new()
    } else {
        let block = current_block(provider.clone()).await;
        let from_block = block.saturating_sub(U64::from(config.event_sampling_blocks));
        let volume_candidates: Vec<PoolRecord> = {
            let probe_cap = hot_pool_max_volume_samples()
                .min(volume_sample_indices(liquidity_ranked.len(), config.event_sampling_rate).len())
                .max(1)
                .min(liquidity_ranked.len());
            liquidity_ranked
                .iter()
                .take(probe_cap)
                .map(|(record, _)| record.clone())
                .collect()
        };
        let volume_pairs: Vec<(Address, Decimal)> =
            stream::iter(volume_candidates.into_iter().map(|record| {
                let pool = record.pool;
                let provider = provider.clone();
                async move {
                    match timeout(volume_timeout, univ3_volume(provider, pool, from_block, block))
                        .await
                    {
                        Err(_) => {
                            warn!(
                                timeout_ms = volume_timeout.as_millis() as u64,
                                pool = %format!("0x{}", hex::encode(pool)),
                                "timed out loading univ3 swap volume"
                            );
                            None
                        }
                        Ok(Err(err)) => {
                            warn!(
                                error = %err,
                                pool = %format!("0x{}", hex::encode(pool)),
                                "failed to load univ3 swap volume"
                            );
                            None
                        }
                        Ok(Ok(volume)) => Some((pool, u256_to_decimal(volume))),
                    }
                }
            }))
            .buffer_unordered(concurrency)
            .filter_map(|entry| async move { entry })
            .collect()
            .await;
        volume_pairs.into_iter().collect()
    };

    for (record, liquidity_score) in liquidity_ranked {
        let volume_score = sampled_volume
            .get(&record.pool)
            .cloned()
            .unwrap_or(Decimal::ZERO);
        scores.push(PoolScore {
            pool: record,
            liquidity_score,
            volume_score,
        });
    }

    scores.sort_by_key(|score| Reverse(score.score_tuple()));
    let scored_hot = scores
        .into_iter()
        .take(config.max_hot_pools.max(1))
        .map(|score| score.pool)
        .collect::<Vec<_>>();
    let pinned_count = pinned.len();
    let hot = finalize_hot_with_pins(scored_hot, pinned, config.max_hot_pools);
    if hot.is_empty() && liquidity_failures > 0 {
        return Err(anyhow!(
            "univ3 ranking aborted: {liquidity_failures} liquidity RPC calls failed"
        ));
    }
    info!(
        cold_pool_records = capped.len(),
        sampled_pools = sampled_volume.len(),
        pinned_pools = pinned_count,
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
        .unwrap_or(800)
        .max(100);
    Duration::from_millis(ms)
}

fn hot_pool_volume_timeout() -> Duration {
    let ms = std::env::var("HOT_POOL_VOLUME_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(600)
        .max(100);
    Duration::from_millis(ms)
}

/// Bounded fan-out for hot-pool ranking RPC. Ranking issues one liquidity probe
/// per cold pool (plus sampled volume queries); doing these serially makes
/// startup and the periodic refresh scale linearly with the universe size. We
/// run them concurrently with a bounded window so a large universe still ranks
/// in seconds without overwhelming the RPC endpoint.
fn hot_pool_rank_concurrency() -> usize {
    std::env::var("HOT_POOL_RANK_CONCURRENCY")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(32)
        .clamp(1, 128)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Address;

    #[test]
    fn build_pinned_hub_pairs_covers_all_hub_combinations() {
        let hubs = vec![
            Address::from_low_u64_be(1),
            Address::from_low_u64_be(2),
            Address::from_low_u64_be(3),
        ];
        let pairs = build_pinned_hub_pairs(&hubs);
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn finalize_hot_with_pins_keeps_hub_pairs_first() {
        let pinned_pool = PoolRecord {
            pool: Address::from_low_u64_be(1),
            token0: Address::from_low_u64_be(2),
            token1: Address::from_low_u64_be(3),
            fee: 500,
            created_block: 1,
            hub_usd_liquidity: None,
        };
        let scored_pool = PoolRecord {
            pool: Address::from_low_u64_be(9),
            token0: Address::from_low_u64_be(4),
            token1: Address::from_low_u64_be(5),
            fee: 500,
            created_block: 2,
            hub_usd_liquidity: Some(1_000_000.0),
        };
        let hot = finalize_hot_with_pins(vec![scored_pool], vec![pinned_pool.clone()], 1);
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].pool, pinned_pool.pool);
    }

    #[test]
    fn pool_matches_pair_is_direction_insensitive() {
        let record = PoolRecord {
            pool: Address::from_low_u64_be(1),
            token0: Address::from_low_u64_be(2),
            token1: Address::from_low_u64_be(3),
            fee: 500,
            created_block: 1,
            hub_usd_liquidity: None,
        };
        assert!(pool_matches_pair(
            &record,
            Address::from_low_u64_be(3),
            Address::from_low_u64_be(2)
        ));
    }
}
