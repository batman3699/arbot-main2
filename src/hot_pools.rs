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
use crate::quote_univ2::{load_pair_state, load_pair_states_batched, UniV2PairState};
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
    /// Round-trip cost of routing through this pool, in bps. Supplied by the
    /// caller because the unit differs by venue: UniV3-style records store
    /// millionths (3000 => 30bps) while univ2/solidly records already store bps.
    fee_bps: Decimal,
    /// Absolute price drift over the lookback window, in bps. Zero when the
    /// probe could not read the pool.
    activity_bps: Decimal,
}

impl PoolScore {
    /// Ranking key, highest-first.
    ///
    /// Ordering is ACTIVITY-first by default. The previous key was
    /// `(liquidity, volume)` — lexicographic, so liquidity dominated and volume
    /// only ever broke exact ties. With `HOT_POOL_SKIP_VOLUME=1` (the shipped
    /// default) `volume_score` is always zero, making it pure liquidity
    /// ranking. That selects deep quiet pools over shallow active ones, which
    /// is backwards: arbitrage comes from flow, and a pool with no trades has
    /// no divergence to capture.
    ///
    /// Liquidity remains the secondary key, so among comparably active pools
    /// the deeper one still wins — and every candidate has already cleared the
    /// configured liquidity floor before reaching here, so this cannot select
    /// dust.
    fn score_tuple(&self) -> (Decimal, Decimal, Decimal, Decimal) {
        self.score_tuple_with(hot_pool_rank_by_activity(), hot_pool_fee_aware())
    }

    /// Pure ordering, with the mode passed in. Tests use this directly: reading
    /// the flag from the process environment made the ordering tests race with
    /// any other test that touches the same variable.
    /// Drift this pool can actually yield: price movement MINUS the fee charged
    /// to capture it.
    ///
    /// Ranking on raw drift is what filled the hot set with expensive pools. On
    /// Base 53% of the UniV3 inventory is the 100bps tier and 27% is 30bps, so
    /// depth-and-drift ranking systematically selected the costliest routes in
    /// the universe: a 3-hop cycle across 30bps pools owes 90bps in fees before
    /// it earns anything, which is precisely the ~99.5bps floor every measured
    /// candidate sat behind. A pool that moved 20bps but charges 100bps to trade
    /// offers nothing, and must not outrank a 1bps pool that moved 5bps.
    ///
    /// DEFAULT OFF pending further work. Enabling it measurably changed which
    /// edges survive the `max_edges_hot` cap, and every cycle Bellman-Ford then
    /// found anchored at a token with no flash-loan provider — 61 cycles found,
    /// 0 fundable, versus 56/56 before. The economics here are right; the
    /// interaction with edge-cap selection and fundable-anchor connectivity is
    /// not solved. Set `HOT_POOL_FEE_AWARE=1` to evaluate.
    fn net_activity_bps(&self, fee_aware: bool) -> Decimal {
        if !fee_aware {
            return self.activity_bps;
        }
        let net = self.activity_bps - self.fee_bps;
        if net.is_sign_negative() {
            Decimal::ZERO
        } else {
            net
        }
    }

    /// Cheapness, as a descending-sort key. Ties on net drift (very common —
    /// most pools score zero) fall through to the cheaper pool before the deeper
    /// one, because fee is a certain cost while depth is only an option.
    fn fee_headroom(&self, fee_aware: bool) -> Decimal {
        if !fee_aware {
            return Decimal::ZERO;
        }
        (Decimal::from(10_000u64) - self.fee_bps).max(Decimal::ZERO)
    }


    fn score_tuple_with(
        &self,
        activity_first: bool,
        fee_aware: bool,
    ) -> (Decimal, Decimal, Decimal, Decimal) {
        if activity_first {
            // Liquidity precedes fee_headroom deliberately. Ranking cheapness
            // above depth filled the hot set with dust 1bps pools and the graph
            // lost the token overlap needed to close ANY cycle — a measured run
            // produced zero candidates. Net-of-fee drift is the economics that
            // matters; cheapness only breaks ties between comparable pools.
            (
                self.net_activity_bps(fee_aware),
                self.liquidity_score,
                self.fee_headroom(fee_aware),
                self.volume_score,
            )
        } else {
            (
                self.liquidity_score,
                self.volume_score,
                self.activity_bps,
                self.fee_headroom(fee_aware),
            )
        }
    }
}

/// UniV3-style `fee` is millionths of the input (3000 => 0.30% => 30bps).
fn univ3_fee_to_bps(fee: u32) -> Decimal {
    Decimal::from(fee) / Decimal::from(100u64)
}

/// UniV2/solidly records already store bps (30 => 0.30%).
fn univ2_fee_to_bps(fee: u32) -> Decimal {
    Decimal::from(fee)
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

/// Blocks to look back when measuring pool ACTIVITY. ~150 Base blocks is ~5
/// minutes at 2s/block — long enough that a genuinely traded pool has moved,
/// short enough to reflect current conditions.
fn hot_pool_activity_lookback_blocks() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("HOT_POOL_ACTIVITY_LOOKBACK_BLOCKS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(150)
            .clamp(1, 10_000)
    })
}

/// Set `HOT_POOL_RANK_BY_ACTIVITY=0` to fall back to the old liquidity-first
/// ordering.
/// Rank pools by drift NET of the fee needed to capture it. Off by default —
/// see `net_activity_bps`.
fn hot_pool_fee_aware() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("HOT_POOL_FEE_AWARE")
            .map(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

fn hot_pool_rank_by_activity() -> bool {
    std::env::var("HOT_POOL_RANK_BY_ACTIVITY")
        .map(|raw| !matches!(raw.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true)
}

/// Measure how much each pool's PRICE MOVED over the lookback window, in bps.
///
/// Arbitrage comes from flow, not from depth: a pool nobody trades has no
/// divergence to capture. Ranking hot pools by liquidity systematically selects
/// deep quiet pools over shallow active ones — measured on Base, the WETH/cbETH
/// 0.05% pool (liquidity 1.15e22) had a byte-identical `sqrtPriceX96` across 100
/// blocks while the 0.01% pool (liquidity 3.7e19, ~300x smaller) moved 0.45 bps.
/// The engine kept the dead one and dropped the live one.
///
/// Implemented as two `Multicall3` batches — one at `head`, one at
/// `head - lookback` — so the whole candidate set costs ~2 round-trips rather
/// than 2 per pool. Pools that fail either read score zero and fall back to
/// liquidity ordering rather than being dropped.
async fn probe_pool_activity_bps<C>(
    provider: &Arc<Provider<C>>,
    pools: &[Address],
    head: U64,
    lookback: u64,
) -> HashMap<Address, Decimal>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut out: HashMap<Address, Decimal> = HashMap::new();
    if pools.is_empty() || head.as_u64() <= lookback {
        return out;
    }
    let past = U64::from(head.as_u64().saturating_sub(lookback));
    // slot0() — first 32 bytes of the return are sqrtPriceX96.
    let sel = {
        let h = ethers::utils::keccak256(b"slot0()");
        vec![h[0], h[1], h[2], h[3]]
    };

    const POOLS_PER_BATCH: usize = 120;
    for chunk in pools.chunks(POOLS_PER_BATCH) {
        let calls: Vec<(Address, Vec<u8>)> = chunk.iter().map(|p| (*p, sel.clone())).collect();
        // Sequential, not concurrent: firing both batches at once doubles the
        // instantaneous request rate, and on a rate-limited endpoint that is
        // enough to fail BOTH. A failed probe is invisible in the ranking — the
        // pools simply score zero activity and fall back to liquidity order —
        // so it must at least be logged.
        let now = crate::quote_cl::multicall3_aggregate3(provider, &calls, head).await;
        let then = crate::quote_cl::multicall3_aggregate3(provider, &calls, past).await;
        let (now, then) = match (now, then) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                warn!(
                    target: "hot_pools",
                    pools = chunk.len(),
                    head = head.as_u64(),
                    past = past.as_u64(),
                    head_err = a.err().map(|e| e.to_string()).unwrap_or_default(),
                    past_err = b.err().map(|e| e.to_string()).unwrap_or_default(),
                    "activity probe batch failed; these pools fall back to liquidity ranking"
                );
                continue;
            }
        };
        for (i, pool) in chunk.iter().enumerate() {
            let (Some(Some(a)), Some(Some(b))) = (now.get(i), then.get(i)) else {
                continue;
            };
            if a.len() < 32 || b.len() < 32 {
                continue;
            }
            let p_now = u256_to_decimal(U256::from_big_endian(&a[..32]));
            let p_then = u256_to_decimal(U256::from_big_endian(&b[..32]));
            if p_now.is_zero() || p_then.is_zero() {
                continue;
            }
            // |price / price_then - 1| in bps. sqrtPrice moves as the square
            // root of price, so this understates by ~2x uniformly — fine for a
            // ranking signal, which only needs to be monotone.
            let ratio = match p_now.checked_div(p_then) {
                Some(r) => r,
                None => continue,
            };
            let drift = (ratio - Decimal::ONE).abs() * Decimal::from(10_000u64);
            out.insert(*pool, drift);
        }
    }
    out
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

/// Reserve-sum liquidity for an already-loaded pair. Shared by the batched and
/// per-pair paths so both score identically.
fn univ2_liquidity_from_state(
    state: &UniV2PairState,
    token_decimals: &HashMap<Address, u8>,
) -> Decimal {
    let decimals0 = token_decimals.get(&state.token0).copied().unwrap_or(18);
    let decimals1 = token_decimals.get(&state.token1).copied().unwrap_or(18);
    let reserve0_tokens = token_amount(state.reserve0, decimals0);
    let reserve1_tokens = token_amount(state.reserve1, decimals1);
    reserve0_tokens
        .checked_add(reserve1_tokens)
        .unwrap_or(Decimal::ZERO)
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
    Ok(Some(univ2_liquidity_from_state(&state, token_decimals)))
}

/// Score every univ2 candidate's liquidity, batched.
///
/// The per-pair [`load_pair_state`] issues three SEQUENTIAL, UNPINNED `eth_call`s
/// per pool. At `max_cold_pools` scale that is thousands of independent
/// round-trips, and — as `load_pair_states_batched` already documents for the
/// quoting path — that does not merely cost latency, it costs COVERAGE:
/// whichever pools happen to be throttled that scan are dropped from the
/// universe. Measured against a live endpoint the unpinned reads also returned
/// `BlockOutOfRangeError`, which discarded the pool outright.
///
/// So batch first, exactly as the quoting path does: ~one Multicall3 per 64
/// pairs, every read pinned to a single block. Only pairs the batch could not
/// return fall back to the per-pair path, which is where the old failure modes
/// still live — but now for a handful of pools rather than all of them.
/// Mirrors `PAIRS_PER_BATCH` in [`load_pair_states_batched`]; used only to size
/// the timeout budget below.
const UNIV2_PAIRS_PER_BATCH: usize = 64;

/// Overall ceiling for the batched liquidity read.
///
/// The batches run sequentially, so the budget scales with their count.
/// `multicall3` has no timeout of its own; without this a stalled endpoint would
/// hang the refresh loop indefinitely, which the per-pair path it replaced could
/// not do.
///
/// The cap is deliberately generous — expiry means falling back to per-pair
/// reads, which cost 3 round-trips per pool and are strictly SLOWER than the
/// batch. So this is a guard against a hung endpoint, not a latency target: it
/// must not fire on one that is merely slow. At the shipped `max_cold_pools`
/// (2500 => 40 batches) the scaled budget is 32s and the cap does not bind, and
/// even that is a small fraction of the 300s refresh interval.
fn univ2_batch_budget(pairs: usize, per_batch: Duration) -> Duration {
    let batches = pairs
        .div_ceil(UNIV2_PAIRS_PER_BATCH)
        .clamp(1, u32::MAX as usize) as u32;
    per_batch
        .saturating_mul(batches)
        .clamp(per_batch, Duration::from_secs(60))
}

async fn score_univ2_liquidity<C>(
    provider: &Arc<Provider<C>>,
    candidates: &[PoolRecord],
    token_decimals: &Arc<HashMap<Address, u8>>,
    rpc_timeout: Duration,
    concurrency: usize,
) -> Vec<(PoolRecord, Option<Decimal>, bool)>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    if candidates.is_empty() {
        return Vec::new();
    }
    let block = current_block(provider.clone()).await;
    let batched = if block.is_zero() {
        // No head to pin to; the batch would read at block 0 and fail wholesale.
        warn!(
            target: "hot_pools",
            "could not read head block; univ2 liquidity falls back to per-pair reads"
        );
        HashMap::new()
    } else {
        let pairs: Vec<Address> = candidates.iter().map(|record| record.pool).collect();
        let budget = univ2_batch_budget(pairs.len(), rpc_timeout);
        match timeout(
            budget,
            load_pair_states_batched(provider.clone(), &pairs, block),
        )
        .await
        {
            Ok(states) => states,
            Err(_) => {
                // Every pool falls through to the per-pair path below, which is
                // exactly the pre-batching behaviour — degraded, not stalled.
                warn!(
                    target: "hot_pools",
                    budget_ms = budget.as_millis() as u64,
                    pairs = pairs.len(),
                    "batched univ2 liquidity read timed out; falling back to per-pair"
                );
                HashMap::new()
            }
        }
    };

    let mut outcomes: Vec<(PoolRecord, Option<Decimal>, bool)> =
        Vec::with_capacity(candidates.len());
    let mut fallback: Vec<PoolRecord> = Vec::new();
    for record in candidates {
        match batched.get(&record.pool) {
            Some(state) => outcomes.push((
                record.clone(),
                Some(univ2_liquidity_from_state(state, token_decimals)),
                false,
            )),
            None => fallback.push(record.clone()),
        }
    }

    if !fallback.is_empty() {
        info!(
            target: "hot_pools",
            batched = outcomes.len(),
            fallback = fallback.len(),
            block = block.as_u64(),
            "univ2 liquidity batched; reading the remainder per-pair"
        );
        let recovered: Vec<(PoolRecord, Option<Decimal>, bool)> =
            stream::iter(fallback.into_iter().map(|record| {
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
        outcomes.extend(recovered);
    }
    outcomes
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

/// Hub-side USD liquidity computed offline by `rank_base_pools.py` and stored on
/// the record.
///
/// Every pool in a ranked inventory file carries this — the Base UniV3 file is
/// literally the ranker's output, filtered to pools above its USD floor — so
/// reading it here removes one `balanceOf` round-trip per pool from the ranking
/// path. That matters twice over: the RPC fan-out was the single largest cost in
/// ranking (~17-22s for 1.5k pools), and every one of its failure modes silently
/// deleted a pool from the universe.
fn offline_hub_usd_liquidity(record: &PoolRecord) -> Option<Decimal> {
    let usd = crate::pool_store::trusted_hub_usd_liquidity(record)?;
    Decimal::from_f64(usd).filter(|value| !value.is_zero())
}

/// Outcome of the liquidity stage, retained so drops are counted rather than
/// vanishing. `Ok(None)` used to mean "silently discard this pool": no log, no
/// counter, no way to tell a genuinely dry pool from an unreadable one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LiquidityDropStats {
    /// RPC failed or timed out. Logged per pool at the call site.
    failed: usize,
    /// Liquidity could not be determined at all — no hub token, no USD price for
    /// the hub, or a zero hub balance.
    unmeasured: usize,
    /// Measured, but below the configured floor.
    below_floor: usize,
}

impl LiquidityDropStats {
    fn dropped(&self) -> usize {
        self.failed
            .saturating_add(self.unmeasured)
            .saturating_add(self.below_floor)
    }
}

/// Apply the liquidity floor and sort highest-first, tallying every pool that
/// does not survive. Extracted from both rank paths so the accounting is shared
/// and directly testable — the previous inline loop dropped pools through two
/// bare `continue`s that no counter or log ever observed.
fn retain_ranked_liquidity(
    outcomes: Vec<(PoolRecord, Option<Decimal>, bool)>,
    min_liquidity_tokens: f64,
) -> (Vec<(PoolRecord, Decimal)>, LiquidityDropStats) {
    let mut stats = LiquidityDropStats::default();
    let min = if min_liquidity_tokens > 0.0 {
        Decimal::from_f64(min_liquidity_tokens).unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };
    let mut ranked: Vec<(PoolRecord, Decimal)> = Vec::with_capacity(outcomes.len());
    for (record, liquidity_value, failed) in outcomes {
        if failed {
            stats.failed = stats.failed.saturating_add(1);
            continue;
        }
        let Some(liquidity_score) = liquidity_value else {
            stats.unmeasured = stats.unmeasured.saturating_add(1);
            continue;
        };
        if liquidity_score < min {
            stats.below_floor = stats.below_floor.saturating_add(1);
            continue;
        }
        ranked.push((record, liquidity_score));
    }
    // Cheap pools first, then depth within each group.
    //
    // Ranking on depth alone is what buried the only pools arbitrage can use.
    // Base's depth sits in the expensive tiers -- 1,778,343 of the factory's
    // 1,881,808 univ3 pools are the 1% tier -- so sorting by liquidity selects
    // a ~44.5 bps mean fee per hop, an ~89 bps hurdle on a 2-hop, while the
    // cross-venue pairs that actually clear cost 1.1-8 bps round-trip. Measured
    // 2026-09-06: restricting a census to cheap AND deep pools moved the 3-hop
    // median gross from -173 bps to -7.4 bps.
    //
    // This only bites once a list is truncated (`.take(max_hot_pools)` below,
    // 1500 today against 1,126 records), so it is a guard against a future
    // ingest quietly re-burying them rather than a change to today's set.
    //
    // A fee is a hurdle paid on every hop of every attempt while depth only
    // caps size, so it gates rather than weighting: cheap-and-shallow still
    // loses to cheap-and-deep, but expensive-and-deep can no longer crowd the
    // cheap cohort out. The depth floor above already removed dust, and the
    // cheap cohort is small -- ~75 pools across Base at a $100k floor.
    ranked.sort_by_key(|(record, liquidity)| {
        (Reverse(is_cheap_enough_to_arb(record)), Reverse(*liquidity))
    });
    (ranked, stats)
}

/// Fee ceiling, in ppm, under which a pool is worth preferring.
///
/// 500 ppm is 5 bps a hop: a 2-hop round trip at or under 10 bps, which is the
/// band the measured cross-venue pairs occupy (1.09 to 8.00 bps).
fn arb_fee_ceiling_ppm() -> u32 {
    crate::util::env_parse_opt::<u32>("ARBOT_ARB_FEE_CEILING_PPM").unwrap_or(500)
}

/// Whether this pool's fee is known AND low enough to be worth preferring.
///
/// Reads `fee_ppm_onchain` and NEVER `record.fee`. That field is the venue's
/// pool key -- a fee tier on univ3, a TICK SPACING on Slipstream -- so treating
/// it as a cost would mark every spacing-1 or spacing-100 Slipstream pool
/// "cheap" regardless of the 212-8000 ppm it actually charges. An unknown fee is
/// not cheap: a builder that could not determine the fee has earned no
/// preference for its pools.
fn is_cheap_enough_to_arb(record: &PoolRecord) -> bool {
    record
        .fee_ppm_onchain
        .is_some_and(|ppm| ppm > 0 && ppm <= arb_fee_ceiling_ppm())
}

/// Log the pools the liquidity stage removed. A drop here is invisible in the
/// final `hot_pools=` count — the universe just silently shrinks — so it has to
/// be reported even when nothing failed outright.
fn log_liquidity_drops(venue: &str, candidates: usize, stats: &LiquidityDropStats) {
    if stats.dropped() == 0 {
        return;
    }
    warn!(
        target: "hot_pools",
        venue,
        candidates,
        ranked = candidates.saturating_sub(stats.dropped()),
        rpc_failed = stats.failed,
        unmeasured = stats.unmeasured,
        below_floor = stats.below_floor,
        "dropped pools during liquidity scoring"
    );
}

async fn univ3_hub_usd_liquidity_score<C>(
    provider: Arc<Provider<C>>,
    record: &PoolRecord,
    rank_ctx: &UniV3RankContext,
) -> Result<Option<Decimal>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    // Offline ranking already priced this pool's hub side. Trust it and skip the
    // RPC entirely; the live read below is the fallback for records that predate
    // ranking or carry a corrupt value.
    if let Some(offline) = offline_hub_usd_liquidity(record) {
        return Ok(Some(offline));
    }
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

    let concurrency = hot_pool_rank_concurrency();

    // Liquidity scoring first: volume log queries are expensive and should
    // only run on the top liquidity candidates, not random cold-pool indices.
    let token_decimals = Arc::new(token_decimals.clone());
    let min_liquidity_tokens = config.min_liquidity_tokens;
    let liquidity_outcomes = score_univ2_liquidity(
        &provider,
        &capped,
        &token_decimals,
        rpc_timeout,
        concurrency,
    )
    .await;

    let candidate_count = capped.len();
    let (liquidity_ranked, drop_stats) =
        retain_ranked_liquidity(liquidity_outcomes, min_liquidity_tokens);
    log_liquidity_drops("univ2", candidate_count, &drop_stats);

    // Measure which of the surviving candidates actually MOVE. Two batched
    // Multicall3 reads for the whole set, so this costs ~2 round-trips rather
    // than scaling with pool count.
    let activity: HashMap<Address, Decimal> = if hot_pool_rank_by_activity() {
        let candidates: Vec<Address> = liquidity_ranked.iter().map(|(r, _)| r.pool).collect();
        let head = current_block(provider.clone()).await;
        let probed = probe_pool_activity_bps(
            &provider,
            &candidates,
            head,
            hot_pool_activity_lookback_blocks(),
        )
        .await;
        let moving = probed.values().filter(|d| !d.is_zero()).count();
        info!(
            target: "hot_pools",
            candidates = candidates.len(),
            probed = probed.len(),
            moving,
            lookback_blocks = hot_pool_activity_lookback_blocks(),
            "ranked hot pools by price activity"
        );
        probed
    } else {
        HashMap::new()
    };

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
        let activity_bps = activity
            .get(&record.pool)
            .cloned()
            .unwrap_or(Decimal::ZERO);
        scores.push(PoolScore {
            fee_bps: univ2_fee_to_bps(record.fee),
            pool: record,
            liquidity_score,
            volume_score,
            activity_bps,
        });
    }

    scores.sort_by_key(|score| Reverse(score.score_tuple()));
    let hot = scores
        .into_iter()
        .take(config.max_hot_pools.max(1))
        .map(|score| score.pool)
        .collect::<Vec<_>>();
    if hot.is_empty() && drop_stats.failed > 0 {
        return Err(anyhow!(
            "univ2 ranking aborted: {} liquidity RPC calls failed",
            drop_stats.failed
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
    let candidate_count = capped.len();
    let (liquidity_ranked, drop_stats) =
        retain_ranked_liquidity(liquidity_outcomes, min_liquidity_tokens);
    log_liquidity_drops("univ3", candidate_count, &drop_stats);

    // Measure which of the surviving candidates actually MOVE. Two batched
    // Multicall3 reads for the whole set, so this costs ~2 round-trips rather
    // than scaling with pool count.
    let activity: HashMap<Address, Decimal> = if hot_pool_rank_by_activity() {
        let candidates: Vec<Address> = liquidity_ranked.iter().map(|(r, _)| r.pool).collect();
        let head = current_block(provider.clone()).await;
        let probed = probe_pool_activity_bps(
            &provider,
            &candidates,
            head,
            hot_pool_activity_lookback_blocks(),
        )
        .await;
        let moving = probed.values().filter(|d| !d.is_zero()).count();
        info!(
            target: "hot_pools",
            candidates = candidates.len(),
            probed = probed.len(),
            moving,
            lookback_blocks = hot_pool_activity_lookback_blocks(),
            "ranked hot pools by price activity"
        );
        probed
    } else {
        HashMap::new()
    };

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
        let activity_bps = activity
            .get(&record.pool)
            .cloned()
            .unwrap_or(Decimal::ZERO);
        scores.push(PoolScore {
            fee_bps: univ3_fee_to_bps(record.fee),
            pool: record,
            liquidity_score,
            volume_score,
            activity_bps,
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
    if hot.is_empty() && drop_stats.failed > 0 {
        return Err(anyhow!(
            "univ3 ranking aborted: {} liquidity RPC calls failed",
            drop_stats.failed
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
    fn score_for(liq: u64, vol: u64, act: u64) -> PoolScore {
        score_for_fee(liq, vol, act, 0)
    }

    fn score_for_fee(liq: u64, vol: u64, act: u64, fee_bps: u64) -> PoolScore {
        PoolScore {
            fee_bps: Decimal::from(fee_bps),
            pool: PoolRecord {
                pool: Address::from_low_u64_be(liq + vol + act + 1),
                token0: Address::from_low_u64_be(1),
                token1: Address::from_low_u64_be(2),
                fee: 500,
                created_block: 0,
                hub_usd_liquidity: None,
                hub_symbol: None,
                fee_ppm_onchain: None,
            },
            liquidity_score: Decimal::from(liq),
            volume_score: Decimal::from(vol),
            activity_bps: Decimal::from(act),
        }
    }

    #[test]
    fn active_shallow_pool_outranks_deep_dead_pool() {
        // The exact case measured on Base: WETH/cbETH 0.05% held 1.15e22 of
        // liquidity and a byte-identical price across 100 blocks, while the
        // 0.01% pool held ~300x less and actually moved. Liquidity-first
        // ranking kept the dead one. Arbitrage needs flow, not depth.
        let dead_deep = score_for(1_000_000, 0, 0);
        let live_shallow = score_for(1_000, 0, 45);
        let mut v = [dead_deep, live_shallow.clone()];
        v.sort_by_key(|s| Reverse(s.score_tuple_with(true, true)));
        assert_eq!(
            v[0].liquidity_score, live_shallow.liquidity_score,
            "the pool that MOVES must rank first"
        );
    }

    #[test]
    fn liquidity_still_breaks_ties_between_equally_active_pools() {
        let shallow = score_for(1_000, 0, 20);
        let deep = score_for(1_000_000, 0, 20);
        let mut v = [shallow, deep.clone()];
        v.sort_by_key(|s| Reverse(s.score_tuple_with(true, true)));
        assert_eq!(
            v[0].liquidity_score, deep.liquidity_score,
            "equal activity must fall back to depth"
        );
    }

    #[test]
    fn activity_ranking_can_be_disabled() {
        // Escape hatch back to the old liquidity-first behaviour.
        let dead_deep = score_for(1_000_000, 0, 0);
        let live_shallow = score_for(1_000, 0, 45);
        let mut v = [live_shallow, dead_deep.clone()];
        v.sort_by_key(|s| Reverse(s.score_tuple_with(false, true)));
        assert_eq!(
            v[0].liquidity_score, dead_deep.liquidity_score,
            "with the flag off, depth must win again"
        );
    }

    use super::*;
    use ethers::types::Address;

    fn record_with_liquidity(id: u64, hub_usd_liquidity: Option<f64>) -> PoolRecord {
        // Ranked output always names the hub it measured. The unnamed-hub case
        // has its own helper below.
        PoolRecord {
            pool: Address::from_low_u64_be(id),
            token0: Address::from_low_u64_be(1),
            token1: Address::from_low_u64_be(2),
            fee: 500,
            created_block: id,
            hub_usd_liquidity,
            hub_symbol: hub_usd_liquidity.map(|_| "WETH".to_string()),
            fee_ppm_onchain: None,
        }
    }

    fn record_with_unnamed_hub(id: u64, hub_usd_liquidity: Option<f64>) -> PoolRecord {
        PoolRecord {
            hub_symbol: None,
            fee_ppm_onchain: None,
            ..record_with_liquidity(id, hub_usd_liquidity)
        }
    }

    /// A number without the hub it was measured against is not a hub-side
    /// measurement, however plausible it looks.
    ///
    /// The writers that omit `hub_symbol` never identify a hub token: they put
    /// GeckoTerminal's `reserve_in_usd` -- whole-pool TVL -- into a field that
    /// means the USD value of the hub token's balance. Audited on-chain
    /// 2026-09-06 over the 32 aerodrome_slipstream_gauge records: 23 were
    /// overstated by the expected 1-4x whole-pool-vs-hub-side factor, and 9
    /// were fabricated. The pool claiming $2,364,678,243 holds $0.04 of WETH.
    /// Being the largest numbers in their venue, they ranked FIRST.
    ///
    /// Refusing them is not a loss: `univ3_hub_usd_liquidity_score` falls
    /// through to a live `balanceOf`, which is the correct value.
    #[test]
    fn a_liquidity_number_without_its_hub_is_not_trusted() {
        for plausible in [1_008.39, 250_000.0, 5_695_613.95, 59_959_477.32] {
            assert!(
                offline_hub_usd_liquidity(&record_with_liquidity(1, Some(plausible))).is_some(),
                "{plausible} names its hub and must be trusted"
            );
            assert_eq!(
                offline_hub_usd_liquidity(&record_with_unnamed_hub(1, Some(plausible))),
                None,
                "{plausible} without a hub symbol must fall through to the live read"
            );
        }
    }

    #[test]
    fn offline_hub_liquidity_is_read_from_the_record() {
        // The Base UniV3 inventory is the offline ranker's own output: every one
        // of its 584 records carries the hub-side USD it was ranked on. Reading
        // it here is what removes the per-pool balanceOf round-trip.
        let record = record_with_liquidity(1, Some(61_947_688.71));
        assert_eq!(
            offline_hub_usd_liquidity(&record),
            Decimal::from_f64(61_947_688.71),
            "a priced record must score without touching the RPC"
        );
    }

    #[test]
    fn offline_hub_liquidity_rejects_corrupt_values() {
        // Matches pool_store's sanitizer: raw UniV3 liquidity() scores leak in as
        // absurd "USD" values and must fall through to the live read.
        for corrupt in [Some(0.0), Some(-1.0), Some(f64::NAN), Some(1e33), None] {
            assert_eq!(
                offline_hub_usd_liquidity(&record_with_liquidity(1, corrupt)),
                None,
                "corrupt value {corrupt:?} must not be trusted"
            );
        }
    }

    fn priced(id: u64, usd: f64, fee_ppm: Option<u32>) -> (PoolRecord, Option<Decimal>, bool) {
        let mut r = record_with_liquidity(id, Some(usd));
        r.fee_ppm_onchain = fee_ppm;
        let score = offline_hub_usd_liquidity(&r);
        (r, score, false)
    }

    /// A cheap pool outranks a deeper expensive one, because the fee is a hurdle
    /// paid on every attempt while depth only caps size.
    ///
    /// Sorting on depth alone is what buried the arb-viable cohort: Base's depth
    /// is in the 1% tier, so the top 50 by liquidity averaged 44.5 bps a hop --
    /// an ~89 bps hurdle on a 2-hop -- while the cross-venue pairs that clear
    /// cost 1.09 to 8.00 bps round-trip.
    #[test]
    fn a_cheap_pool_outranks_a_deeper_expensive_one() {
        let (ranked, _) = retain_ranked_liquidity(
            vec![
                priced(1, 50_000_000.0, Some(3_000)),  // deep, 30 bps
                priced(2, 250_000.0, Some(100)),       // shallow, 1 bp
                priced(3, 900_000.0, Some(500)),       // mid, 5 bps
            ],
            1.0,
        );
        let order: Vec<u64> = ranked.iter().map(|(r, _)| r.pool.to_low_u64_be()).collect();
        assert_eq!(
            order,
            vec![3, 2, 1],
            "cheap pools lead, deepest first within the cheap group; the 30 bps \
             pool ranks last however deep it is"
        );
    }

    /// An unknown fee is not a cheap fee.
    ///
    /// `fee_ppm_onchain` is absent on every record a fee-blind builder wrote, and
    /// those must not be promoted over pools whose cost was actually measured.
    /// Within the unknown group the old depth ordering is untouched.
    #[test]
    fn an_unknown_fee_earns_no_preference() {
        let (ranked, _) = retain_ranked_liquidity(
            vec![
                priced(1, 5_000_000.0, None),
                priced(2, 100_000.0, Some(100)),
                priced(3, 9_000_000.0, None),
            ],
            1.0,
        );
        let order: Vec<u64> = ranked.iter().map(|(r, _)| r.pool.to_low_u64_be()).collect();
        assert_eq!(order, vec![2, 3, 1], "measured-cheap first, then depth");
    }

    /// The tick-spacing trap: `record.fee` must never be read as a cost.
    ///
    /// On Slipstream that field is the POOL KEY, so a spacing of 100 would look
    /// like 1 bp while the pool charges 2500 ppm. Only the explicit
    /// `fee_ppm_onchain` counts.
    #[test]
    fn a_slipstream_pool_key_is_not_mistaken_for_a_cheap_fee() {
        // fee = 100 is the tick spacing; the pool really charges 2500 ppm.
        let mut spacing_looks_cheap = record_with_liquidity(1, Some(100_000.0));
        spacing_looks_cheap.fee = 100;
        spacing_looks_cheap.fee_ppm_onchain = Some(2_500);
        assert!(
            !is_cheap_enough_to_arb(&spacing_looks_cheap),
            "a spacing of 100 must not read as 1 bp when the pool charges 25 bps"
        );

        let mut genuinely_cheap = record_with_liquidity(2, Some(100_000.0));
        genuinely_cheap.fee = 2_000; // a wide spacing ...
        genuinely_cheap.fee_ppm_onchain = Some(90); // ... on a 0.9 bp pool
        assert!(
            is_cheap_enough_to_arb(&genuinely_cheap),
            "a wide spacing says nothing about the fee; 90 ppm is cheap"
        );
    }

    /// The cohort that was actually in the shipped inventory on 2026-09-06,
    /// pinned so the cap cannot drift back up.
    ///
    /// These four records claimed fabricated hub liquidity and were written by
    /// a path that also omitted `hub_symbol`. Under the old 1e12 cap the first
    /// two were rejected and the last two PASSED, ranking #1 and #2 in the
    /// hot-pool list -- garbage sorted to the top of the universe, which is
    /// exactly what the cap exists to prevent. Re-measured on-chain the four
    /// pools hold $262, $1.50, $0.02 and $0.02.
    #[test]
    fn the_cap_rejects_every_value_in_the_corrupted_cohort() {
        for fabricated in [3.2441739079683994e18, 1.729821609691e12, 8.21312811065e11] {
            assert_eq!(
                offline_hub_usd_liquidity(&record_with_liquidity(1, Some(fabricated))),
                None,
                "{fabricated:e} is not a pool's USD liquidity and must not rank"
            );
        }
    }

    /// The other half of the cap: it must not start rejecting real pools.
    /// $2.4e9 is the largest value in any inventory in this repo, and the
    /// largest single pool that has ever existed on any chain is single-digit
    /// billions -- all of which has to stay rankable.
    #[test]
    fn the_cap_still_admits_the_largest_pool_that_could_exist() {
        for real in [1_008.39, 59_959_477.32, 2_364_678_244.0, 9.9e10] {
            assert!(
                offline_hub_usd_liquidity(&record_with_liquidity(1, Some(real))).is_some(),
                "{real:e} is a plausible pool size and must still rank"
            );
        }
    }

    #[test]
    fn every_priced_pool_survives_liquidity_ranking() {
        // The reported bug: a 584-pool inventory ranked far fewer. Each record is
        // priced offline and clears the floor, so none may be discarded.
        let outcomes: Vec<(PoolRecord, Option<Decimal>, bool)> = (0..584u64)
            .map(|i| {
                let record = record_with_liquidity(i + 1, Some(10_000.0 + i as f64));
                let score = offline_hub_usd_liquidity(&record);
                (record, score, false)
            })
            .collect();
        let (ranked, stats) = retain_ranked_liquidity(outcomes, 1.0);
        assert_eq!(ranked.len(), 584, "all 584 priced pools must rank");
        assert_eq!(stats.dropped(), 0, "no pool may be dropped silently");
        assert!(
            ranked.windows(2).all(|w| w[0].1 >= w[1].1),
            "ranking must be highest-liquidity-first"
        );
    }

    /// The real Base UniV3 inventory, end to end. The synthetic case above
    /// proves the filter; this proves the shipped data actually clears it — the
    /// bug was that a 584-pool file ranked a fraction of itself.
    #[test]
    fn shipped_base_univ3_inventory_ranks_in_full() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data/base/uniswap_v3/pools.jsonl");
        let records = crate::pool_store::load_pool_records(&path).expect("load inventory");
        if records.is_empty() {
            return; // inventory not present in this checkout
        }
        let total = records.len();
        let records_for_report = records.clone();
        let outcomes: Vec<(PoolRecord, Option<Decimal>, bool)> = records
            .into_iter()
            .map(|record| {
                let score = offline_hub_usd_liquidity(&record);
                (record, score, false)
            })
            .collect();
        // Name the offenders. This reads a LIVE inventory that is regenerated
        // outside git, so a failure here is usually bad data rather than a
        // broken ranker -- and "1128 != 1130" alone sends you looking in the
        // wrong place. Measured 2026-09-06: four records carried fabricated
        // liquidity (3.2e18 USD among them) from a writer that also omitted
        // `hub_symbol`; two were caught by MAX_SANE_HUB_USD_LIQUIDITY and two
        // ranked #1 and #2 until the cap was tightened.
        let unrankable: Vec<String> = records_for_report
            .iter()
            .filter(|record| offline_hub_usd_liquidity(record).is_none())
            .map(|record| {
                format!(
                    "0x{} (hub_usd_liquidity: {:?})",
                    hex::encode(record.pool),
                    record.hub_usd_liquidity
                )
            })
            .collect();
        // 1 == universe.min_pool_liquidity_tokens from ops/inputs.yaml.
        let (ranked, stats) = retain_ranked_liquidity(outcomes, 1.0);
        assert_eq!(
            ranked.len(),
            total,
            "every pool in the shipped inventory must rank, dropped: {stats:?}; \
             unrankable records: {unrankable:?}"
        );
    }

    #[test]
    fn univ2_liquidity_sums_both_reserves_at_token_scale() {
        // 2 WETH (18dp) + 5000 USDC (6dp) => 5002 tokens.
        let state = UniV2PairState {
            token0: Address::from_low_u64_be(1),
            token1: Address::from_low_u64_be(2),
            reserve0: U256::from(2u64) * U256::exp10(18),
            reserve1: U256::from(5_000u64) * U256::exp10(6),
        };
        let decimals: HashMap<Address, u8> = [
            (Address::from_low_u64_be(1), 18u8),
            (Address::from_low_u64_be(2), 6u8),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            univ2_liquidity_from_state(&state, &decimals),
            Decimal::from(5_002u64)
        );
    }

    #[test]
    fn univ2_liquidity_defaults_unknown_decimals_to_18() {
        // An unlisted token must not be scored as if it were 0-decimal, which
        // would inflate it by 1e18 and let dust outrank real depth.
        let state = UniV2PairState {
            token0: Address::from_low_u64_be(1),
            token1: Address::from_low_u64_be(2),
            reserve0: U256::from(3u64) * U256::exp10(18),
            reserve1: U256::zero(),
        };
        assert_eq!(
            univ2_liquidity_from_state(&state, &HashMap::new()),
            Decimal::from(3u64)
        );
    }

    #[test]
    fn univ2_batch_budget_scales_with_batches_and_stays_capped() {
        let per_batch = Duration::from_millis(800);
        // Sequential batches, so the budget grows with their count...
        assert_eq!(univ2_batch_budget(0, per_batch), per_batch);
        assert_eq!(univ2_batch_budget(64, per_batch), per_batch);
        assert_eq!(univ2_batch_budget(65, per_batch), per_batch * 2);
        // The shipped max_cold_pools: 40 batches, and the cap must NOT bind here
        // — expiry falls back to per-pair reads, which are slower than the batch.
        assert_eq!(univ2_batch_budget(2_500, per_batch), per_batch * 40);
        // ...but it is still bounded, so a hung endpoint cannot stall the loop.
        assert_eq!(
            univ2_batch_budget(1_000_000, per_batch),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn liquidity_drops_are_counted_by_reason() {
        // Each drop path used to be a bare `continue`. They must now be
        // attributable, so a shrinking universe is visible instead of silent.
        let outcomes = vec![
            (record_with_liquidity(1, None), None, true),
            (record_with_liquidity(2, None), None, false),
            (record_with_liquidity(3, None), Some(Decimal::from(5u64)), false),
            (
                record_with_liquidity(4, None),
                Some(Decimal::from(5_000u64)),
                false,
            ),
        ];
        let (ranked, stats) = retain_ranked_liquidity(outcomes, 100.0);
        assert_eq!(ranked.len(), 1);
        assert_eq!(
            stats,
            LiquidityDropStats {
                failed: 1,
                unmeasured: 1,
                below_floor: 1,
            }
        );
    }

    #[test]
    fn zero_floor_keeps_every_measured_pool() {
        let outcomes = vec![
            (record_with_liquidity(1, None), Some(Decimal::ONE), false),
            (record_with_liquidity(2, None), Some(Decimal::ZERO), false),
        ];
        let (ranked, stats) = retain_ranked_liquidity(outcomes, 0.0);
        assert_eq!(ranked.len(), 2, "a zero floor must not filter anything");
        assert_eq!(stats.dropped(), 0);
    }

    #[test]
    fn drift_that_cannot_cover_its_own_fee_is_worthless() {
        // The measured Base failure: 53% of the UniV3 inventory is the 100bps
        // tier. A pool that moved 20bps but charges 100bps to trade yields
        // nothing, and must not outrank a cheap pool that actually moved.
        let expensive = score_for_fee(1_000_000, 0, 20, 100);
        let cheap = score_for_fee(1_000, 0, 5, 1);
        let mut v = [expensive, cheap.clone()];
        v.sort_by_key(|s| Reverse(s.score_tuple_with(true, true)));
        assert_eq!(
            v[0].fee_bps, cheap.fee_bps,
            "net-of-fee drift must beat raw drift the fee erases"
        );
    }

    #[test]
    fn net_activity_floors_at_zero_and_never_goes_negative() {
        // A negative net would sort BELOW an untradeable pool, inverting the
        // ordering among the many pools that score zero drift.
        assert_eq!(score_for_fee(1, 0, 5, 100).net_activity_bps(true), Decimal::ZERO);
        assert_eq!(score_for_fee(1, 0, 100, 100).net_activity_bps(true), Decimal::ZERO);
        assert_eq!(score_for_fee(1, 0, 130, 100).net_activity_bps(true), Decimal::from(30u64));
    }

    #[test]
    fn equal_net_drift_and_depth_prefers_the_cheaper_pool() {
        // Same depth: fee decides. (Cheapness must NOT outrank depth outright —
        // that emptied the graph of closable cycles.)
        let dear = score_for_fee(1_000, 0, 0, 100);
        let cheap = score_for_fee(1_000, 0, 0, 1);
        let mut v = [dear, cheap.clone()];
        v.sort_by_key(|s| Reverse(s.score_tuple_with(true, true)));
        assert_eq!(v[0].fee_bps, cheap.fee_bps, "cheaper pool wins the tie");
    }

    #[test]
    fn venue_fee_units_convert_to_the_same_scale() {
        // UniV3 stores millionths, univ2/solidly stores bps. Conflating them
        // would misprice every pool by 100x in one direction or the other.
        assert_eq!(univ3_fee_to_bps(3000), Decimal::from(30u64));
        assert_eq!(univ3_fee_to_bps(100), Decimal::ONE);
        assert_eq!(univ3_fee_to_bps(10_000), Decimal::from(100u64));
        assert_eq!(univ2_fee_to_bps(30), Decimal::from(30u64));
    }

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
            hub_symbol: None,
            fee_ppm_onchain: None,
        };
        let scored_pool = PoolRecord {
            pool: Address::from_low_u64_be(9),
            token0: Address::from_low_u64_be(4),
            token1: Address::from_low_u64_be(5),
            fee: 500,
            created_block: 2,
            hub_usd_liquidity: Some(1_000_000.0),
            // Names its hub, so the score is actually trusted -- otherwise this
            // would test that an UNSCORED pool loses to a pin, which is weaker
            // than what it is named for.
            hub_symbol: Some("WETH".to_string()),
            fee_ppm_onchain: None,
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
            hub_symbol: None,
            fee_ppm_onchain: None,
        };
        assert!(pool_matches_pair(
            &record,
            Address::from_low_u64_be(3),
            Address::from_low_u64_be(2)
        ));
    }
}
