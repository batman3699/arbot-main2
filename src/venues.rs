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
use crate::metrics::Metrics;
use crate::pool_store::{PoolRecord, ResolvedUniV2PoolCfg};
use crate::quote_balancer::BalQuote;
use crate::quote_curve::CurveQuote;
use crate::quote_solidly::{
    quote_exact_input_from_state as quote_solidly_exact_input, SolidlyPairState,
};
use crate::quote_univ2::{load_pair_state, quote_exact_input_from_state, UniV2PairState};
use crate::quote_slipstream::SlipstreamQuoter;
use crate::quote_univ3::{UniQuoter, UniV3ValidationConfig, FEE_TIERS};
use futures_util::{stream, StreamExt};
use crate::quote_univ4::quote_fixed_price_exact_input;
use crate::util::{
    apply_slippage, compute_edge_weight, decimal_ratio, u256_to_decimal, TradeSizing,
};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, MathematicalOps};
use serde::de::DeserializeOwned;
use tracing::{debug, error, info, warn};

// Public so the fast path's constructed edges carry the SAME per-hop gas
// estimate as the scan's. Two numbers for one venue would make the two paths
// disagree about whether the same cycle clears its gas cost.
pub const ESTIMATED_GAS_UNIV3: u64 = 140_000;
const ESTIMATED_GAS_BAL: u64 = 155_000;
const ESTIMATED_GAS_CURVE: u64 = 180_000;
const ESTIMATED_GAS_UNIV2: u64 = 130_000;
pub const ESTIMATED_GAS_SOLIDLYV2: u64 = 135_000;
const ESTIMATED_GAS_UNIV4: u64 = 160_000;

fn univ2_load_concurrency() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("UNIV2_LOAD_CONCURRENCY")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .unwrap_or(32)
            .clamp(4, 128)
    })
}
// Production scan budgets run 150-250ms; a hung quote must never be able to
// pin a pool for multiple seconds. 2s matches the validated Base shadow
// override (ARBOT_RPC_QUOTE_TIMEOUT_SECS=2) and is now the fail-safe default.
const DEFAULT_RPC_QUOTE_TIMEOUT_SECS: u64 = 2;
const DEFAULT_QUEUE_WAIT_TIMEOUT_SECS: u64 = 2;
const DEFAULT_UNIV3_TOTAL_DEADLINE_SECS: u64 = 8;
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
    static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        Duration::from_secs(parse_rpc_quote_timeout_secs(
            std::env::var(RPC_QUOTE_TIMEOUT_ENV).ok().as_deref(),
        ))
    })
}

fn queue_wait_timeout() -> Duration {
    static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        Duration::from_secs(parse_timeout_secs(
            std::env::var(QUEUE_WAIT_TIMEOUT_ENV).ok().as_deref(),
            DEFAULT_QUEUE_WAIT_TIMEOUT_SECS,
        ))
    })
}

fn univ3_total_deadline_timeout() -> Duration {
    static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        Duration::from_secs(parse_timeout_secs(
            std::env::var(UNIV3_TOTAL_DEADLINE_ENV).ok().as_deref(),
            DEFAULT_UNIV3_TOTAL_DEADLINE_SECS,
        ))
    })
}

abigen!(
    IBalancerPoolIdLookup,
    r#"[
        function getPoolId() external view returns (bytes32)
    ]"#,
);

/// Price the size grid for one CL pool.
///
/// Prefers the multi-tick model when a ladder is available, and treats an
/// exhausted quote as "this pool cannot fill THIS size" — that grid point
/// becomes `None` and is simply not selectable, so sizing lands on the largest
/// size the pool can actually pay for. If no grid point is fillable the pool
/// yields no edge at all.
///
/// This used to call `quote_exact_input_single_tick` unconditionally. That model
/// holds liquidity constant — it assumes infinite depth — so a pool with almost
/// nothing on the output side still quoted a near-spot rate at any size, and the
/// edge entered the graph looking attractive. That is where phantom candidates
/// were born; `plan.rs` only caught them one stage later, after they had already
/// displaced real edges in ranking.
///
/// Measured: WETH/bsdETH 0xdea629c5587037d0925ff85f1961d95db62bedd6 (fee 500)
/// holds 248.9 WETH and 0.154 bsdETH. Single-tick quoted 2 WETH -> 1.904 bsdETH;
/// the on-chain quoter pays 0.0000169 and lands at MIN_SQRT_RATIO+1.
fn cl_grid_quote(
    state: &crate::cl_sim::ClPoolState,
    ladder: Option<&crate::cl_swap::TickLadder>,
    base_amount: U256,
    tolerance_bps: u32,
    zero_for_one: bool,
) -> Result<Option<QuoteComputation>> {
    let grid = univ3_size_grid(base_amount);
    if grid.is_empty() {
        return Ok(None);
    }
    let use_multi = crate::cl_sim::multi_tick_enabled() && ladder.is_some();

    // A pool cannot pay out more than it HOLDS of the output token. This is the
    // one bound that needs no model and cannot be argued with, and it catches
    // quotes that are wrong by any margin — including the ones no plausibility
    // heuristic would flag.
    //
    // Measured on Base: an edge claimed 0.01 SAPIEN -> 5.372e25 raw USDC. The
    // chain pays 327 raw USDC (fee tier 10000). That is ~1.6e23x, twenty-three
    // orders of magnitude. Such an edge does not merely misprice itself — it
    // blows `amount` up through `cycle_input_capacity`, flooring the projection
    // to zero and surfacing as `no_liquidity` on cycles that are perfectly
    // sound. 37/37 sampled `no_liquidity` rejections were this, at 3-5 hops.
    //
    // `None` (balance unknown) applies NO cap: fail open here rather than
    // silently deleting every edge on a degraded RPC. The capacity path already
    // fails closed separately.
    let balance_out = if zero_for_one {
        state.balance1
    } else {
        state.balance0
    }
    .filter(|b| !b.is_zero());

    let mut outs = Vec::with_capacity(grid.len());
    for amount in &grid {
        let out = match (use_multi, ladder) {
            (true, Some(ladder)) => {
                match crate::cl_swap::quote_exact_input_multi_tick(
                    state,
                    ladder,
                    *amount,
                    zero_for_one,
                    crate::cl_sim::cl_max_ticks_crossed(),
                ) {
                    // Exhausted means the pool ran out of liquidity before
                    // filling this size. Not a fillable grid point.
                    Some(q)
                        if q.exhausted || q.amount_out.is_zero() || q.is_unfillable(*amount) =>
                    {
                        None
                    }
                    Some(q) => Some(q.amount_out),
                    // `None` is "cannot answer" (e.g. ladder too short to even
                    // start), which is distinct from "cannot fill" — the
                    // single-tick estimate is still the best available there.
                    None => crate::cl_sim::quote_exact_input_single_tick(
                        state,
                        *amount,
                        zero_for_one,
                        state.fee_ppm,
                    )?,
                }
            }
            _ => crate::cl_sim::quote_exact_input_single_tick(
                state,
                *amount,
                zero_for_one,
                state.fee_ppm,
            )?,
        };
        // Discard any grid point claiming more output than the pool holds.
        let out = match (out, balance_out) {
            (Some(o), Some(bal)) if o > bal => {
                tracing::debug!(
                    target: "capacity",
                    amount_in = %amount,
                    claimed_out = %o,
                    pool_holds = %bal,
                    "quote exceeds the pool's balance of the output token; discarding"
                );
                None
            }
            (other, _) => other,
        };
        outs.push(out);
    }
    Ok(best_from_grid(&grid, &outs, tolerance_bps))
}


#[cfg(test)]
mod cl_grid_quote_discrimination_tests {
    use super::*;
    use ethers::types::U256;

    fn state(liq: u128) -> crate::cl_sim::ClPoolState {
        crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: liq,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            ..Default::default()
        }
    }

    /// The fix must DISCRIMINATE, not blanket-reject. A pool with depth on both
    /// sides still prices; a pool whose ladder is exhausted by the size does not.
    /// Without this pairing, "rejects the phantom" is indistinguishable from
    /// "rejects everything", which would silently starve the edge set.
    #[test]
    fn deep_pool_still_prices_while_exhausted_pool_is_rejected() {
        let _lock = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::cl_sim::MultiTickEnvGuard::set("1");

        let base = U256::from(1_000_000u64);

        // Deep: wide ladder, ample liquidity across it.
        let deep_state = state(1_000_000_000_000_000);
        let deep_ladder = crate::cl_swap::TickLadder::new(
            vec![(-600, 10_000_000_000_000), (600, -10_000_000_000_000)],
            -6000,
            6000,
        );
        let deep = cl_grid_quote(&deep_state, Some(&deep_ladder), base, 50, true)
            .expect("deep quote must not error");
        assert!(
            deep.is_some_and(|q| !q.amount_out.is_zero()),
            "a pool with real depth on both sides must still produce an edge"
        );

        // Shallow: ladder far too narrow for the same grid.
        let thin_state = state(1_000);
        let thin_ladder = crate::cl_swap::TickLadder::new(vec![(-60, 900)], -60, 60);
        let thin = cl_grid_quote(&thin_state, Some(&thin_ladder), base, 50, true)
            .expect("thin quote must not error");
        assert!(
            thin.is_none(),
            "a pool that cannot fill any grid size must yield no edge, not a phantom one"
        );
    }
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

fn pool_pair_in_whitelist(token_a: Address, token_b: Address, whitelist: &HashSet<Address>) -> bool {
    whitelist.contains(&token_a) && whitelist.contains(&token_b)
}

fn filter_hot_univ3_pools(
    pools: &[PoolRecord],
    whitelist: &HashSet<Address>,
) -> Vec<PoolRecord> {
    pools
        .iter()
        .filter(|pool| pool_pair_in_whitelist(pool.token0, pool.token1, whitelist))
        .cloned()
        .collect()
}

fn filter_hot_univ2_pools(
    pools: &[ResolvedUniV2PoolCfg],
    whitelist: &HashSet<Address>,
) -> Vec<ResolvedUniV2PoolCfg> {
    pools
        .iter()
        .filter(|pool| pool_pair_in_whitelist(pool.token_in, pool.token_out, whitelist))
        .cloned()
        .collect()
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

    // WARNING — `min_edge_max_input` (env `MIN_EDGE_MAX_INPUT_WEI`) is compared
    // against `edge.max_input`, which is in the edge's INPUT TOKEN raw units,
    // not wei. The name and default (zero, i.e. filter disabled) hide this. Set
    // it to a native-denominated value like 1e18 and every 6-decimal token edge
    // is silently deactivated — real USDC depth is ~1e12-1e13 raw units, so the
    // comparison is true for all of them and USDC disappears from the graph
    // with no log line. Same decimals-vs-units class as the flash-loan bounds
    // fixed in `compute_base_amounts`. Convert per-token before enabling this.
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

/// Share of a constant-product reserve treated as usable input. Beyond roughly a
/// third of the reserve the marginal rate collapses and the sizer rejects the
/// candidate anyway, so this bounds the search without deciding economics.
pub(crate) const EDGE_CAPACITY_RESERVE_BPS: u32 = 3_333;

/// How far past the probe size a quote-backed edge may be scaled when the probe
/// registered no measurable price impact. Deep pools are common on Base majors;
/// pinning capacity to the probe is what broke sizing in the first place.
const EDGE_CAPACITY_PROBE_MULTIPLIER: u64 = 256;

/// Capacity of a reserve-backed edge (UniV2 / Solidly), in `from`-token units.
pub(crate) fn edge_capacity_from_reserve(reserve_in: U256) -> U256 {
    reserve_in.saturating_mul(U256::from(EDGE_CAPACITY_RESERVE_BPS)) / U256::from(10_000u32)
}

/// VIRTUAL reserves of a CL pool at its current price. **NOT a depth measure.**
///
/// DO NOT use this to size trades or set `max_input`. `L/sqrt(P)` and
/// `L*sqrt(P)` describe the constant-product curve a CL pool is tangent to at
/// the current price, and that curve runs from zero to infinity — far outside
/// the ticks where liquidity actually exists. It therefore OVERSTATES what the
/// pool holds, without bound.
///
/// Measured on Base against real `balanceOf`:
///   WETH/USDC 0x6c561b446: virtual reserve0 562,148 WETH vs 10,046 held (56x);
///                          virtual reserve1 1.40B USDC vs 88.3M held (16x).
///   WETH/bsdETH 0xdea629c5: virtual reserve1 7.98e21 raw vs 0.154 bsdETH held.
///
/// Wiring this into `max_input` capped a 10,046-WETH pool at 187,364 WETH, let
/// sizing explore amounts the pool cannot honour, and took candidate production
/// from ~400 per 7min to ZERO (confirmed by A/B revert).
///
/// The only sound depth source is the pool's actual token balances — see
/// `edge_capacity_from_pool_balance`.
#[allow(dead_code)]
fn cl_virtual_reserves_unsafe_for_depth(
    state: &crate::cl_sim::ClPoolState,
) -> Option<(U256, U256)> {
    cl_virtual_reserves(state)
}

/// Capacity of an edge from the pool's REAL holding of the input token.
///
/// `balance_in` must come from `balanceOf(pool)` for the input token — ground
/// truth, and the same source used to validate the pool inventory. A pool can
/// never pay out more than it holds, and can never absorb input beyond what
/// drains the far side, so this is the honest bound. Uses the same
/// `EDGE_CAPACITY_RESERVE_BPS` fraction as the UniV2/Solidly path.
///
/// NOT YET WIRED: `ClPoolState` carries no balances, so they must first be
/// fetched alongside `slot0`/`liquidity` in `cl_sim::load_cl_pool_states_batched`
/// (which needs the pool's token addresses threaded in from the caller).
/// Until then CL edges keep the probe-derived capacity, which is wrong but is
/// at least the wrong value the rest of the pipeline is tuned around.
fn edge_capacity_from_pool_balance(balance_in: U256) -> U256 {
    edge_capacity_from_reserve(balance_in)
}

/// Virtual reserves of a concentrated-liquidity pool at its current price, in
/// raw token units, or `None` when the state cannot support the derivation.
///
/// A CL pool with active liquidity `L` at price `P` behaves locally like a
/// constant-product pool with reserves `L/sqrt(P)` and `L*sqrt(P)`. With
/// `sqrt_price_x96 = sqrt(P) * 2^96`:
///
/// ```text
/// reserve0 = L * 2^96 / sqrt_price_x96
/// reserve1 = L * sqrt_price_x96 / 2^96
/// ```
///
/// This is depth measured from POOL STATE, which is what `max_input` should have
/// been derived from all along — see `edge_capacity_from_cl_state`.
///
/// It describes the liquidity active at the current tick, so it is an
/// approximation for a swap that crosses ticks. That is the correct direction to
/// be wrong in: it under-states depth for a pool whose neighbouring ticks also
/// hold liquidity, and never invents capacity that is not there. The multi-tick
/// simulator and `is_unfillable` remain the authority on whether a specific size
/// actually fills.
fn cl_virtual_reserves(state: &crate::cl_sim::ClPoolState) -> Option<(U256, U256)> {
    if state.sqrt_price_x96.is_zero() || state.liquidity == 0 {
        return None;
    }
    let liquidity = U512::from(U256::from(state.liquidity));
    let sqrt_p = U512::from(state.sqrt_price_x96);
    let q96 = U512::from(1u64) << 96;

    let reserve0 = liquidity.checked_mul(q96)? / sqrt_p;
    let reserve1 = liquidity.checked_mul(sqrt_p)? >> 96;

    let narrow = |v: U512| -> Option<U256> {
        if v > U512::from(U256::MAX) {
            return Some(U256::MAX);
        }
        let mut buf = [0u8; 64];
        v.to_little_endian(&mut buf);
        Some(U256::from_little_endian(&buf[..32]))
    };
    Some((narrow(reserve0)?, narrow(reserve1)?))
}

/// Capacity of a concentrated-liquidity edge derived from POOL DEPTH rather than
/// from the probe we happened to quote at.
///
/// `zero_for_one` selects which side is the INPUT, so the cap is taken against
/// the reserve the trade actually spends into. Uses the same
/// `EDGE_CAPACITY_RESERVE_BPS` fraction as the UniV2/Solidly path, so CL and
/// constant-product edges are finally sized on the same basis.
fn edge_capacity_from_cl_state(
    state: &crate::cl_sim::ClPoolState,
    zero_for_one: bool,
) -> Option<U256> {
    let (reserve0, reserve1) = cl_virtual_reserves(state)?;
    let reserve_in = if zero_for_one { reserve0 } else { reserve1 };
    if reserve_in.is_zero() {
        return None;
    }
    Some(edge_capacity_from_reserve(reserve_in))
}

/// Capacity of a quote-backed edge (UniV3 / Slipstream / Balancer / Curve / V4),
/// in `from`-token units.
///
/// Fallback for variants that genuinely carry no depth state (Balancer, Curve,
/// V4). Price impact is locally linear in size, so a probe of `amount_in` that
/// moved the price `s` bps can absorb about `amount_in * tolerance / s` before it
/// moves `tolerance` bps. A probe with no measurable impact says the pool is deep
/// relative to the probe, not that the probe is the limit — fall back to a
/// generous multiple there.
///
/// NOT for UniV3/Slipstream: those carry `ClPoolState`, so their depth is
/// measurable and `edge_capacity_from_cl_state` must be preferred. Deriving their
/// capacity from the probe made `max_input` an artifact of the size we happened
/// to quote at rather than a fact about the pool. Measured on Base: a live
/// WETH-pair hop reported `max_input = 0.2087` tokens while the preceding hop
/// pushed 9,546 through it, collapsing cycle capacity below one raw unit of the
/// start token and rejecting the cycle as `no_liquidity` — 297 of 411 records.
///
/// Never returns less than the probe: an amount we already quoted successfully
/// is by construction within capacity.
fn edge_capacity_from_quote(quote: &QuoteComputation, tolerance_bps: u32) -> U256 {
    let probe = quote.amount_in;
    if probe.is_zero() {
        return U256::zero();
    }
    let ceiling = probe.saturating_mul(U256::from(EDGE_CAPACITY_PROBE_MULTIPLIER));
    if quote.slippage_bps == 0 || tolerance_bps == 0 {
        return ceiling;
    }
    let scaled =
        probe.saturating_mul(U256::from(tolerance_bps)) / U256::from(quote.slippage_bps);
    scaled.max(probe).min(ceiling)
}

/// Effective price of a quote, as `amount_out` per unit of `amount_in`.
///
/// Unit-safe ONLY across quotes for the same token pair, which is the only way
/// it is used: comparing grid points on one edge. It deliberately does not try
/// to express "profit" — a single swap converts A to B and has no profit until
/// it is closed by the rest of a cycle.
fn effective_rate(quote: &QuoteComputation) -> Decimal {
    let denom = u256_to_decimal(quote.amount_in);
    if denom.is_zero() {
        return Decimal::ZERO;
    }
    u256_to_decimal(quote.amount_out) / denom
}

/// Pick the size that represents this edge: the LARGEST one inside the slippage
/// tolerance the caller has already applied.
///
/// This previously ranked by `amount_out - amount_in`, subtracting a token_IN
/// amount from a token_OUT amount with no decimal normalisation. On any pair
/// where the input carries more decimals than the output — every
/// WETH->USDC/USDT/cbBTC edge on Base — that expression reduces to
/// `-amount_in`, so "most profitable" silently meant "smallest". Every such
/// edge was then rated at the `base/1000` probe: spot price with effectively no
/// slippage. Cycles looked profitable during detection and died in sizing,
/// which is precisely the `no_profitable_size` signature.
///
/// Every candidate here has already passed `slippage_bps <= tolerance`, so the
/// largest is the most executable size at an acceptable price — the
/// representative notional an edge weight is supposed to carry, rather than an
/// infinitesimal one that flatters the whole graph.
fn best_quote(candidates: &[QuoteComputation]) -> Option<&QuoteComputation> {
    candidates.iter().max_by(|a, b| {
        a.amount_in
            .cmp(&b.amount_in)
            // Same size quoted twice: prefer the better fill.
            .then(a.amount_out.cmp(&b.amount_out))
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

/// Fixed, log-ish-spaced grid of trade sizes for batched UniV3 sizing. The
/// smallest entry (a small probe) anchors the slippage estimate; the rest span
/// up to 10x the base amount. Deterministic multiplicative steps (no floats),
/// covering the same range the sequential adaptive search explored.
fn univ3_size_grid(base_amount: U256) -> Vec<U256> {
    if base_amount.is_zero() {
        return Vec::new();
    }
    let probe = probe_amount(base_amount).max(U256::one());
    let mut grid = vec![probe];

    // Dense geometric ladder from the probe up to 10x base.
    //
    // The old grid was [probe, base/4, base/2, base, 2b, 4b, 8b, 10b], which
    // left a 250x HOLE between the probe (base/1000) and base/4 — and
    // `base = depth/5`, so with a 10 WETH pool the tested sizes were
    // 0.002 then nothing until 0.5. Every size was either too small to clear
    // gas or already deep into price impact, and the profitable band in between
    // was never sampled. That is a systematic reason to report
    // `no_profitable_size` on a cycle that genuinely has an optimum: the
    // optimiser can only choose from points it was given, and it was never
    // given one near the peak.
    //
    // Doubling from the probe covers the full 10,000x span in ~14 points, so
    // consecutive sizes differ by 2x instead of 250x and the search always has
    // a sample within 2x of the true optimum.
    let ceiling = base_amount.saturating_mul(U256::from(10u64));
    let mut amount = probe;
    while amount < ceiling && grid.len() < 20 {
        amount = amount.saturating_mul(U256::from(2u64));
        if amount.is_zero() || amount > ceiling {
            break;
        }
        grid.push(amount);
    }

    // Keep the exact anchors the sizer and its tests reason about, so the
    // denser ladder is strictly additive rather than a replacement.
    for (num, den) in [(1u64, 1u64), (10, 1)] {
        let anchor = base_amount.saturating_mul(U256::from(num)) / U256::from(den);
        if !anchor.is_zero() {
            grid.push(anchor);
        }
    }

    grid.sort();
    grid.dedup();
    grid
}

/// Pick the most profitable grid point within the slippage tolerance, using the
/// smallest valid quote as the spot/probe reference (mirrors the sequential
/// path's `slippage_from_samples` semantics).
fn best_from_grid(
    amounts: &[U256],
    outs: &[Option<U256>],
    tolerance_bps: u32,
) -> Option<QuoteComputation> {
    let mut probe: Option<(U256, U256)> = None;
    for (amount, out) in amounts.iter().zip(outs.iter()) {
        if let Some(value) = out {
            if *value > U256::zero() {
                probe = Some((*amount, *value));
                break;
            }
        }
    }
    let (probe_in, probe_out) = probe?;
    let mut candidates = Vec::new();
    for (amount, out) in amounts.iter().zip(outs.iter()) {
        if let Some(value) = out {
            if value.is_zero() {
                continue;
            }
            let slippage_bps = slippage_from_samples(*amount, *value, probe_in, probe_out);
            if slippage_bps <= tolerance_bps {
                candidates.push(QuoteComputation {
                    amount_in: *amount,
                    amount_out: *value,
                    slippage_bps,
                });
            }
        }
    }
    best_quote(&candidates).copied()
}

/// Batched UniV3 sizing: quote the whole size grid in one Multicall3 round-trip
/// (one RTT instead of ~2N sequential quotes). Returns `Err` on transport
/// failure so the caller can fall back to the sequential adaptive search;
/// `Ok(None)` means the grid was quoted but no size is profitable within
/// tolerance (no fallback needed).
async fn univ3_grid_quote<C>(
    quoter: &UniQuoter<C>,
    path: Vec<(Address, Option<u32>)>,
    base_amount: U256,
    tolerance_bps: u32,
    block: U64,
    quote_semaphore: &Arc<Semaphore>,
) -> Result<Option<QuoteComputation>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let grid = univ3_size_grid(base_amount);
    if grid.is_empty() {
        return Ok(None);
    }
    let permit = match timeout(queue_wait_timeout(), quote_semaphore.clone().acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err(anyhow!("univ3 quote semaphore closed")),
        Err(_) => return Err(anyhow!("univ3 grid quote semaphore wait timed out")),
    };
    let result = timeout(rpc_quote_timeout(), quoter.quote_path_grid(path, &grid, block)).await;
    drop(permit);
    match result {
        Ok(Ok(outs)) => Ok(best_from_grid(&grid, &outs, tolerance_bps)),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(anyhow!("univ3 grid quote timed out")),
    }
}

/// Expansion-phase probe count for the trade-size search. Each probe is one
/// quote; on the async (UniV3 `eth_call`) path this dominates per-scan latency,
/// so it is tunable. Default preserves prior behavior (12).
fn size_search_expand_iters() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("SIZE_SEARCH_EXPAND_ITERS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(12)
            .clamp(3, 24)
    })
}

/// Refinement-phase iteration count (each does up to 2 quotes). Default 8.
fn size_search_refine_iters() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("SIZE_SEARCH_REFINE_ITERS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(8)
            .clamp(2, 16)
    })
}

/// Bounded concurrency for loading Solidly/Aerodrome pair state. Each pool needs
/// 3 RPCs (getReserves + token0 + token1); loading them concurrently (instead of
/// sequentially per directional entry) is the dominant solidly latency win.
fn solidly_state_concurrency() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("SOLIDLY_STATE_CONCURRENCY")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(32)
            .clamp(1, 128)
    })
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
    let mut bracket: Option<(U256, U256)> = None;
    let mut last_amount: Option<U256> = None;
    for _ in 0..12 {
        if current.is_zero() || current > max_amount {
            break;
        }
        if let Some(q) = evaluate(current, &mut eval_cached)? {
            // Expansion stops when a size stops being quotable within tolerance
            // (the `else` arm below), NOT on a decline in this value. It used to
            // break as soon as `profit_value` fell, but that quantity was
            // ~= -amount_in and therefore decreased on every single step, so the
            // search bracketed on iteration 2 every time and never explored the
            // range SIZE_SEARCH_EXPAND_ITERS was configured for.
            evaluated.push(q);
            last_amount = Some(current);
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
                    if effective_rate(p1) < effective_rate(p2) {
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
    let mut bracket: Option<(U256, U256)> = None;
    let mut last_amount: Option<U256> = None;
    for _ in 0..size_search_expand_iters() {
        if current.is_zero() || current > max_amount {
            break;
        }
        if let Some(q) =
            evaluate_quote_async(current, threshold_bps, &mut eval_cached, &mut quote_fn).await?
        {
            // Same correction as the sync path: bracket on tolerance failure,
            // not on a decline in a quantity that only ever declined.
            evaluated.push(q);
            last_amount = Some(current);
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
        for _ in 0..size_search_refine_iters() {
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
                    if effective_rate(p1) < effective_rate(p2) {
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
            "failed to parse {} as JSON/JSON5. Ensure addresses and hex fields are quoted strings (e.g. \"0xabc...\").",
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

/// Build deduplicated Solidly pair monitor entries from chain env config.
pub fn solidly_monitored_pools(chain_env_prefix: &str) -> Vec<crate::ingestion::MonitoredPool> {
    use crate::ingestion::{MonitoredPool, PoolMonitorKind};
    let env_key = format!("{chain_env_prefix}_SOLIDLY_V2_POOLS");
    let Some((raw, source)) = env_var_with_fallback(&env_key, "SOLIDLY_V2_POOLS") else {
        return Vec::new();
    };
    let pools = resolve_solidly_pools(&raw, &source).unwrap_or_default();
    let mut unique: HashMap<Address, MonitoredPool> = HashMap::new();
    for pool in pools {
        unique.entry(pool.pair).or_insert(MonitoredPool {
            pair: pool.pair,
            token_in: pool.token_in,
            token_out: pool.token_out,
            fee_bps: pool.fee_bps,
            stable: pool.stable,
            kind: PoolMonitorKind::Solidly,
        });
    }
    unique.into_values().collect()
}

pub fn merge_monitored_pools(
    univ2: Vec<crate::ingestion::MonitoredPool>,
    solidly: Vec<crate::ingestion::MonitoredPool>,
) -> Vec<crate::ingestion::MonitoredPool> {
    let mut unique: HashMap<Address, crate::ingestion::MonitoredPool> = HashMap::new();
    for pool in univ2.into_iter().chain(solidly) {
        unique.insert(pool.pair, pool);
    }
    unique.into_values().collect()
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeDigest {
    pub edges: usize,
    pub active_edges: usize,
    pub weight_sum: i128,
    pub weight_abs_sum: i128,
}

pub fn edge_digest(edges: &[Edge]) -> EdgeDigest {
    let mut digest = EdgeDigest {
        edges: edges.len(),
        ..Default::default()
    };
    for edge in edges.iter() {
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

pub fn edge_digest_changed_significantly(previous: Option<EdgeDigest>, current: EdgeDigest) -> bool {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopulateMode {
    Full,
    Incremental,
    Skipped,
}

#[derive(Clone, Debug, Default)]
pub struct PopulateOptions {
    pub touched_pools: HashSet<Address>,
    pub last_digest: Option<EdgeDigest>,
    pub cached_edges: Option<Vec<Edge>>,
}

#[derive(Clone)]
struct EdgeBuildContext {
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
    block_number: U64,
}

struct Univ3EdgeContext<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    edge_ctx: EdgeBuildContext,
    allowed_fee_tiers: Option<Arc<HashSet<u32>>>,
    quoter: Arc<UniQuoter<C>>,
    provider: Arc<Provider<C>>,
    hot_paths: Arc<HotPathCache>,
    quote_semaphore: Arc<Semaphore>,
    quote_concurrency_limit: usize,
    token_whitelist: Arc<HashSet<Address>>,
    chain_env_prefix: String,
    pool_filter: Option<HashSet<Address>>,
    /// Long-lived tick ladder cache, owned by the chain's `Runner` and shared
    /// across every `scan_once()` call so the epoch cache in
    /// `CachedTickSource` actually pays off across scans instead of being
    /// rebuilt (and thrown away) once per `populate_edges` call. One instance
    /// per chain/provider — never shared across chains, so it cannot serve
    /// one chain's tick data to another.
    tick_cache: Arc<crate::cl_ticks::CachedTickSource<crate::cl_ticks::RpcTickSource<C>>>,
}

pub const ESTIMATED_GAS_SLIPSTREAM: u64 = ESTIMATED_GAS_UNIV3;

struct SlipstreamEdgeContext<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    edge_ctx: EdgeBuildContext,
    allowed_tick_spacings: Option<Arc<HashSet<u32>>>,
    quoter: Arc<SlipstreamQuoter<C>>,
    provider: Arc<Provider<C>>,
    router: Address,
    hot_paths: Arc<HotPathCache>,
    quote_semaphore: Arc<Semaphore>,
    quote_concurrency_limit: usize,
    token_whitelist: Arc<HashSet<Address>>,
    chain_env_prefix: String,
    pool_filter: Option<HashSet<Address>>,
    /// See `Univ3EdgeContext::tick_cache` — same long-lived, per-chain cache.
    tick_cache: Arc<crate::cl_ticks::CachedTickSource<crate::cl_ticks::RpcTickSource<C>>>,
}

async fn slipstream_grid_quote<C>(
    quoter: &SlipstreamQuoter<C>,
    path: Vec<(Address, Option<u32>)>,
    base_amount: U256,
    tolerance_bps: u32,
    block: U64,
    quote_semaphore: &Arc<Semaphore>,
) -> Result<Option<QuoteComputation>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let grid = univ3_size_grid(base_amount);
    if grid.is_empty() {
        return Ok(None);
    }
    let permit = match timeout(queue_wait_timeout(), quote_semaphore.clone().acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err(anyhow!("slipstream quote semaphore closed")),
        Err(_) => return Err(anyhow!("slipstream grid quote semaphore wait timed out")),
    };
    let result = timeout(rpc_quote_timeout(), quoter.quote_path_grid(path, &grid, block)).await;
    drop(permit);
    match result {
        Ok(Ok(outs)) => Ok(best_from_grid(&grid, &outs, tolerance_bps)),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(anyhow!("slipstream grid quote timed out")),
    }
}

async fn bootstrap_slipstream_pools_from_tokens<C>(ctx: &SlipstreamEdgeContext<C>) -> Vec<PoolRecord>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut tokens: Vec<Address> = ctx.token_whitelist.iter().copied().collect();
    tokens.sort_unstable();
    if tokens.len() < 2 {
        return Vec::new();
    }

    let fees: Vec<u32> = ctx
        .allowed_tick_spacings
        .as_ref()
        .map(|tiers| {
            let mut v: Vec<u32> = tiers.iter().copied().collect();
            v.sort_unstable();
            v
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| crate::quote_slipstream::TICK_SPACINGS.to_vec());

    let max_pairs = std::env::var("SLIPSTREAM_BOOTSTRAP_MAX_PAIRS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(256);
    let max_pools = std::env::var("SLIPSTREAM_BOOTSTRAP_MAX_POOLS")
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
                        hub_usd_liquidity: None,
                    }),
                    Ok(None) => {}
                    Err(err) => {
                        debug!(
                            target: "venue::slipstream",
                            error = %err,
                            token0 = %format!("0x{}", hex::encode(token_a)),
                            token1 = %format!("0x{}", hex::encode(token_b)),
                            tick_spacing = *fee,
                            "Slipstream bootstrap pool discovery failed"
                        );
                    }
                }
            }
        }
    }

    if !discovered.is_empty() {
        info!(
            target: "venue::slipstream",
            pair_checks,
            discovered_pools = discovered.len(),
            "Bootstrapped Slipstream pools from token whitelist"
        );
    }

    discovered
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
                        hub_usd_liquidity: None,
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
    let source_pools: Vec<PoolRecord> = if let Some(filter) = &ctx.pool_filter {
        if filter.is_empty() {
            source_pools
        } else {
            source_pools
                .into_iter()
                .filter(|p| filter.contains(&p.pool))
                .collect()
        }
    } else {
        source_pools
    };
    if source_pools.is_empty() {
        return Ok(Vec::new());
    }

    let stats = Arc::new(Univ3Stats::default());
    let collector_started_at = Instant::now();
    let allowed_fee_tiers = ctx.allowed_fee_tiers.as_ref().and_then(|tiers| {
        if tiers.is_empty() {
            None
        } else {
            Some(Arc::clone(tiers))
        }
    });
    let min_forced_discovery_quotes = {
        static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *V.get_or_init(|| {
            std::env::var("UNIV3_MIN_FORCED_QUOTES")
                .ok()
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(8)
        })
    };
    let forced_discovery_quotes_used = Arc::new(AtomicUsize::new(0));
    let provider_class = classify_provider_for_chain(&ctx.chain_env_prefix);
    let max_pool_tasks = {
        static V: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
        (*V.get_or_init(|| {
            std::env::var("UNIV3_MAX_CONCURRENT_POOL_TASKS")
                .ok()
                .and_then(|raw| raw.parse::<usize>().ok())
                .filter(|value| *value > 0)
        }))
        .unwrap_or(ctx.quote_concurrency_limit.max(1))
    };

    // Spec §3.4: "All pool-state reads via Multicall3. Never N sequential RPC
    // round-trips per scan." Prefetch every pool's slot0/liquidity/tickSpacing/
    // fee in one batched call instead of letting each spawned task issue its own
    // four sequential reads. At 64 pools that is 256 round-trips collapsed to 2.
    let prefetched_cl_state = if crate::cl_sim::local_cl_quotes_enabled() {
        // token0/token1 ride along so the loader can read each pool's REAL
        // balances in the same batch — the only sound capacity source.
        let targets: Vec<(Address, Option<u32>, Address, Address)> = source_pools
            .iter()
            .map(|p| (p.pool, Some(p.fee), p.token0, p.token1))
            .collect();
        let started = Instant::now();
        let states = crate::cl_sim::load_cl_pool_states_batched(
            Arc::clone(&ctx.provider),
            &targets,
            ctx.edge_ctx.block_number,
        )
        .await;
        info!(
            target: "venue::univ3",
            pools = targets.len(),
            loaded = states.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "prefetched CL pool state via Multicall3"
        );
        Arc::new(states)
    } else {
        Arc::new(std::collections::HashMap::new())
    };

    // Ladders ride along with the state prefetch: same block, same pool set.
    // `tick_cache` is the chain's long-lived `CachedTickSource` (owned by
    // `Runner`, threaded through `populate_edges`), so its epoch cache
    // actually collapses repeat words across scans rather than being rebuilt
    // and discarded on every call. Built concurrently, bounded by the same
    // `max_pool_tasks` limit the per-pool quote loop below uses, so this
    // doesn't reintroduce the sequential-RPC pattern spec §3.4 forbids.
    // Skipped entirely when the flag is off, so this costs nothing until
    // enabled.
    let tick_ladders: Arc<HashMap<Address, Arc<crate::cl_swap::TickLadder>>> =
        if crate::cl_sim::multi_tick_enabled() {
            let words = crate::cl_sim::cl_ladder_words();
            let built = crate::cl_ticks::build_ladders_concurrent(
                Arc::clone(&ctx.tick_cache),
                &prefetched_cl_state,
                ctx.edge_ctx.block_number,
                words,
                max_pool_tasks,
            )
            .await;

            // Earn trust per pool. The local model is exact on nearly every pool
            // measured, but where a pool's own reported state is internally
            // inconsistent it can be 40,000+ bps out — and no local check
            // detects that, because the inputs look self-consistent. Only the
            // pool's own quoter is authoritative. Verdicts carry a TTL and only
            // a bounded number of pools are (re)measured per scan, so this costs
            // one quoter call per pool per TTL rather than reintroducing the
            // per-scan RPC the local path exists to avoid.
            let gate = crate::cl_parity_gate::gate();
            for pool in gate.due_for_check(built.keys().copied()) {
                let (Some(state), Some(ladder), Some(record)) = (
                    prefetched_cl_state.get(&pool),
                    built.get(&pool),
                    hot_pools.iter().find(|p| p.pool == pool),
                ) else {
                    continue;
                };
                let zero_for_one = record.token0 < record.token1;
                let (token_in, token_out) = if zero_for_one {
                    (record.token0, record.token1)
                } else {
                    (record.token1, record.token0)
                };
                // Measure at the same notional the quote loop uses for this
                // token, not a token amount: the divergence is size-dependent,
                // so a small probe would pass a pool that fails at trade size.
                let notional = ctx
                    .edge_ctx
                    .base_profiles
                    .as_ref()
                    .get(&token_in)
                    .copied()
                    .unwrap_or(ctx.edge_ctx.default_profile)
                    .base_amount;
                // Step down until the model can answer. A pool whose ladder
                // exhausts at the full notional still deserves a verdict; it may
                // model smaller trades exactly, and executed hop amounts are
                // often well below the base notional. Measuring only at full
                // size left most pools permanently unmeasured, hence untrusted,
                // hence with no ladder — silently forcing every hop back onto
                // the optimistic single-tick path.
                let mut verdict: Option<i64> = None;
                for size in crate::cl_parity_gate::measurement_sizes(notional) {
                    let Some(model) =
                        crate::cl_parity_gate::model_quote(state, ladder, size, zero_for_one)
                    else {
                        continue;
                    };
                    let Ok(on_chain) = ctx
                        .quoter
                        .quote_path(
                            // PoolRecord.fee is the venue's POOL KEY: a fee tier
                        // for univ3, a TICK SPACING for slipstream. Passing
                        // state.fee_ppm (the on-chain fee()) resolved no pool on
                        // slipstream, so no verdict was ever earned and all 24
                        // slipstream pools stayed untrusted with zero ladders —
                        // forcing one hop of every univ3+slipstream cycle onto
                        // the optimistic single-tick model.
                        vec![(token_in, None), (token_out, Some(record.fee))],
                            size,
                            ctx.edge_ctx.block_number,
                        )
                        .await
                    else {
                        // Transport failure is not evidence either way; stop
                        // rather than record a verdict we did not earn.
                        break;
                    };
                    verdict = crate::cl_parity_gate::divergence_bps(model, on_chain);
                    break;
                }
                gate.record(pool, verdict);
            }

            let trusted: HashMap<Address, Arc<crate::cl_swap::TickLadder>> =
                built.into_iter().filter(|(p, _)| gate.trusted(*p)).collect();
            let (ok, rejected, tracked) = gate.stats();
            info!(
                target: "ladder",
                pools_with_state = prefetched_cl_state.len(),
                hot_pools = hot_pools.len(),
                ladders = trusted.len(),
                trusted = ok,
                rejected,
                tracked,
                "CL parity gate applied"
            );
            Arc::new(trusted)
        } else {
            Arc::new(HashMap::new())
        };

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
        let provider = Arc::clone(&ctx.provider);
        let hot_paths = Arc::clone(&ctx.hot_paths);
        let stats = Arc::clone(&stats);
        let quote_semaphore = Arc::clone(&ctx.quote_semaphore);
        let block_number = ctx.edge_ctx.block_number;
        let forced_discovery_quotes_used = Arc::clone(&forced_discovery_quotes_used);
        let chain_env_prefix = ctx.chain_env_prefix.clone();
        let quote_concurrency_limit = ctx.quote_concurrency_limit;
        let prefetched_cl_state = Arc::clone(&prefetched_cl_state);
        let tick_ladders = Arc::clone(&tick_ladders);
        join_set.spawn(async move {
            let mut local_edges = Vec::new();
            // Prefer the Multicall3-prefetched state; only fall back to the
            // four sequential per-pool reads when this pool missed the batch.
            let cl_state = if !crate::cl_sim::local_cl_quotes_enabled() {
                None
            } else if let Some(state) = prefetched_cl_state.get(&pool.pool) {
                Some(state.clone())
            } else {
                match crate::cl_sim::load_cl_pool_state(
                    provider.clone(),
                    pool.pool,
                    block_number,
                    Some(pool.fee),
                )
                .await
                {
                    Ok(state) => state,
                    Err(err) => {
                        debug!(
                            target: "venue::univ3",
                            error = %err,
                            pool = %format!("0x{}", hex::encode(pool.pool)),
                            "CL pool state load failed; quoter fallback"
                        );
                        None
                    }
                }
            };
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
                // FAST PATH: local CL sim grid when state is loaded; else batched
                // QuoterV2 multicall. Fall back to quoter on local sim errors.
                let quote = if let Some(ref cl_state) = cl_state {
                    let zero_for_one = token_in == pool.token0;
                    match cl_grid_quote(
                        cl_state,
                        tick_ladders.get(&pool.pool).map(|l| l.as_ref()),
                        base_amount_in,
                        tolerance_bps,
                        zero_for_one,
                    ) {
                        Ok(Some(q)) => Ok(Some(q)),
                        Ok(None) => Ok(None),
                        Err(err) => {
                            debug!(
                                target: "venue::univ3",
                                error = %err,
                                pool = %format!("0x{}", hex::encode(pool.pool)),
                                "local CL grid failed; falling back to quoter"
                            );
                            univ3_grid_quote(
                                quoter.as_ref(),
                                path.clone(),
                                base_amount_in,
                                tolerance_bps,
                                block_number,
                                &quote_semaphore_fee,
                            )
                            .await
                        }
                    }
                } else {
                    univ3_grid_quote(
                        quoter.as_ref(),
                        path.clone(),
                        base_amount_in,
                        tolerance_bps,
                        block_number,
                        &quote_semaphore_fee,
                    )
                    .await
                };
                let quote = match quote {
                    Ok(opt) => {
                        stats.quote_attempts.fetch_add(1, Ordering::Relaxed);
                        if opt.is_some() {
                            stats.quote_success.fetch_add(1, Ordering::Relaxed);
                        }
                        opt
                    }
                    Err(_) => adjust_trade_size_async(base_amount_in, tolerance_bps, {
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
                            // Hoisted: the batched quote below needs BOTH samples.
                            let probe_in = probe_amount(amount);
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
                                // ONE round trip for both samples. slippage_from_samples
                                // needs a probe and the full size; quoting them
                                // separately cost 2 RPCs per fallback edge, and
                                // populate is ~960 calls/scan at ~600ms per round of 64.
                                let grid_amounts: Vec<U256> = if probe_in == amount {
                                    vec![amount]
                                } else {
                                    vec![probe_in, amount]
                                };
                                let result = timeout(
                                    rpc_timeout_window,
                                    quoter.quote_path_grid(
                                        path_inner.clone(),
                                        &grid_amounts,
                                        block_number,
                                    ),
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
                                    Ok(Ok(outs)) => {
                                        let out =
                                            outs.last().copied().flatten().unwrap_or_default();
                                        let probe_out =
                                            outs.first().copied().flatten().unwrap_or(out);
                                        if out > U256::zero() {
                                            Ok(Some((out, probe_out)))
                                        } else {
                                            stats.quote_failures.fetch_add(1, Ordering::Relaxed);
                                            Ok(None)
                                        }
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
                            let (out, probe_out) = match timeout(total_deadline_window, quote_eval).await {
                                Ok(result) => match result? {
                                    Some(pair) => pair,
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
                            // probe_out comes from the batched grid above; the separate
                            // probe round trip is gone.
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
                .await?,
                };

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

                // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
                if protected_out.is_zero() {
                    hot_paths_quote
                        .as_ref()
                        .record_failure(token_in, token_out, pool.fee)
                        .await;
                    continue;
                }

                let weight = compute_edge_weight(protected_out, quote.amount_in);
                let edge = Edge {
                    from: token_in,
                    to: token_out,
                    rate_num: quote.amount_out,
                    rate_den: quote.amount_in,
                    venue: VenueEdge::UniV3 {
                        path: path.clone(),
                        pool: pool.pool,
                        fee: pool.fee,
                        state: cl_state.clone(),
                    },
                    estimated_gas: ESTIMATED_GAS_UNIV3,
                    weight,
                    // Depth from POOL STATE when we have it; the probe-derived
                    // value only as a fallback. `max_input` bounded by the quote
                    // probe is a fact about our sampling, not about the pool.
                    // Capacity from the pool's REAL balance of the input token
                    // when we have it. Falls back to the probe-derived value
                    // per-edge when the balance read failed — never to the
                    // virtual L/sqrt(P) reserves, which overstate holdings by
                    // 16-56x on deep pools and unboundedly on thin ones.
                    max_input: cl_state
                        .as_ref()
                        .and_then(|st| {
                            let zero_for_one = token_in == pool.token0;
                            let balance_in = if zero_for_one {
                                st.balance0
                            } else {
                                st.balance1
                            };
                            balance_in
                                .filter(|b| !b.is_zero())
                                .map(edge_capacity_from_pool_balance)
                        })
                        .unwrap_or_else(|| edge_capacity_from_quote(&quote, tolerance_bps)),
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: Some(block_number),
                    active: true,
                    tick_ladder: tick_ladders.get(&pool.pool).cloned(),
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
        elapsed_ms = collector_started_at.elapsed().as_millis() as u64,
        "UniV3 hot pool summary",
    );
    Ok(edges)
}

async fn collect_slipstream_edges<C>(
    hot_pools: &[PoolRecord],
    ctx: &SlipstreamEdgeContext<C>,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    struct SlipstreamStats {
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

    impl Default for SlipstreamStats {
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
        bootstrap_slipstream_pools_from_tokens(ctx).await
    } else {
        hot_pools.to_vec()
    };
    let source_pools: Vec<PoolRecord> = if let Some(filter) = &ctx.pool_filter {
        if filter.is_empty() {
            source_pools
        } else {
            source_pools
                .into_iter()
                .filter(|p| filter.contains(&p.pool))
                .collect()
        }
    } else {
        source_pools
    };
    if source_pools.is_empty() {
        return Ok(Vec::new());
    }

    let stats = Arc::new(SlipstreamStats::default());
    let collector_started_at = Instant::now();
    let allowed_fee_tiers = ctx.allowed_tick_spacings.as_ref().and_then(|tiers| {
        if tiers.is_empty() {
            None
        } else {
            Some(Arc::clone(tiers))
        }
    });
    let min_forced_discovery_quotes = {
        static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *V.get_or_init(|| {
            std::env::var("SLIPSTREAM_MIN_FORCED_QUOTES")
                .ok()
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(8)
        })
    };
    let forced_discovery_quotes_used = Arc::new(AtomicUsize::new(0));
    let provider_class = classify_provider_for_chain(&ctx.chain_env_prefix);
    let max_pool_tasks = {
        static V: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
        (*V.get_or_init(|| {
            std::env::var("SLIPSTREAM_MAX_CONCURRENT_POOL_TASKS")
                .ok()
                .and_then(|raw| raw.parse::<usize>().ok())
                .filter(|value| *value > 0)
        }))
        .unwrap_or(ctx.quote_concurrency_limit.max(1))
    };

    // Same Multicall3 prefetch as the UniV3 collector (spec §3.4). Slipstream
    // pools expose the identical slot0/liquidity/tickSpacing/fee surface, so a
    // single batched read replaces four sequential calls per pool.
    // `fee` is read from chain here (no hint) because Slipstream keys pools by
    // tick spacing rather than a fee tier.
    let prefetched_cl_state = if crate::cl_sim::local_cl_quotes_enabled() {
        let targets: Vec<(Address, Option<u32>, Address, Address)> = source_pools
            .iter()
            .map(|p| (p.pool, None, p.token0, p.token1))
            .collect();
        let started = Instant::now();
        let states = crate::cl_sim::load_cl_pool_states_batched(
            Arc::clone(&ctx.provider),
            &targets,
            ctx.edge_ctx.block_number,
        )
        .await;
        info!(
            target: "venue::slipstream",
            pools = targets.len(),
            loaded = states.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "prefetched CL pool state via Multicall3"
        );
        Arc::new(states)
    } else {
        Arc::new(std::collections::HashMap::new())
    };

    // Ladders ride along with the state prefetch: same block, same pool set.
    // `tick_cache` is the chain's long-lived `CachedTickSource` (owned by
    // `Runner`, threaded through `populate_edges`), so its epoch cache
    // actually collapses repeat words across scans rather than being rebuilt
    // and discarded on every call. Built concurrently, bounded by the same
    // `max_pool_tasks` limit the per-pool quote loop below uses, so this
    // doesn't reintroduce the sequential-RPC pattern spec §3.4 forbids.
    // Skipped entirely when the flag is off, so this costs nothing until
    // enabled.
    let tick_ladders: Arc<HashMap<Address, Arc<crate::cl_swap::TickLadder>>> =
        if crate::cl_sim::multi_tick_enabled() {
            let words = crate::cl_sim::cl_ladder_words();
            let built = crate::cl_ticks::build_ladders_concurrent(
                Arc::clone(&ctx.tick_cache),
                &prefetched_cl_state,
                ctx.edge_ctx.block_number,
                words,
                max_pool_tasks,
            )
            .await;

            // Earn trust per pool. The local model is exact on nearly every pool
            // measured, but where a pool's own reported state is internally
            // inconsistent it can be 40,000+ bps out — and no local check
            // detects that, because the inputs look self-consistent. Only the
            // pool's own quoter is authoritative. Verdicts carry a TTL and only
            // a bounded number of pools are (re)measured per scan, so this costs
            // one quoter call per pool per TTL rather than reintroducing the
            // per-scan RPC the local path exists to avoid.
            let gate = crate::cl_parity_gate::gate();
            for pool in gate.due_for_check(built.keys().copied()) {
                let (Some(state), Some(ladder), Some(record)) = (
                    prefetched_cl_state.get(&pool),
                    built.get(&pool),
                    hot_pools.iter().find(|p| p.pool == pool),
                ) else {
                    continue;
                };
                let zero_for_one = record.token0 < record.token1;
                let (token_in, token_out) = if zero_for_one {
                    (record.token0, record.token1)
                } else {
                    (record.token1, record.token0)
                };
                // Measure at the same notional the quote loop uses for this
                // token, not a token amount: the divergence is size-dependent,
                // so a small probe would pass a pool that fails at trade size.
                let notional = ctx
                    .edge_ctx
                    .base_profiles
                    .as_ref()
                    .get(&token_in)
                    .copied()
                    .unwrap_or(ctx.edge_ctx.default_profile)
                    .base_amount;
                // Step down until the model can answer. A pool whose ladder
                // exhausts at the full notional still deserves a verdict; it may
                // model smaller trades exactly, and executed hop amounts are
                // often well below the base notional. Measuring only at full
                // size left most pools permanently unmeasured, hence untrusted,
                // hence with no ladder — silently forcing every hop back onto
                // the optimistic single-tick path.
                let mut verdict: Option<i64> = None;
                for size in crate::cl_parity_gate::measurement_sizes(notional) {
                    let Some(model) =
                        crate::cl_parity_gate::model_quote(state, ladder, size, zero_for_one)
                    else {
                        continue;
                    };
                    let Ok(on_chain) = ctx
                        .quoter
                        .quote_path(
                            // PoolRecord.fee is the venue's POOL KEY: a fee tier
                        // for univ3, a TICK SPACING for slipstream. Passing
                        // state.fee_ppm (the on-chain fee()) resolved no pool on
                        // slipstream, so no verdict was ever earned and all 24
                        // slipstream pools stayed untrusted with zero ladders —
                        // forcing one hop of every univ3+slipstream cycle onto
                        // the optimistic single-tick model.
                        vec![(token_in, None), (token_out, Some(record.fee))],
                            size,
                            ctx.edge_ctx.block_number,
                        )
                        .await
                    else {
                        // Transport failure is not evidence either way; stop
                        // rather than record a verdict we did not earn.
                        break;
                    };
                    verdict = crate::cl_parity_gate::divergence_bps(model, on_chain);
                    break;
                }
                gate.record(pool, verdict);
            }

            let trusted: HashMap<Address, Arc<crate::cl_swap::TickLadder>> =
                built.into_iter().filter(|(p, _)| gate.trusted(*p)).collect();
            let (ok, rejected, tracked) = gate.stats();
            info!(
                target: "ladder",
                pools_with_state = prefetched_cl_state.len(),
                hot_pools = hot_pools.len(),
                ladders = trusted.len(),
                trusted = ok,
                rejected,
                tracked,
                "CL parity gate applied"
            );
            Arc::new(trusted)
        } else {
            Arc::new(HashMap::new())
        };

    let mut join_set: JoinSet<Result<Vec<Edge>>> = JoinSet::new();
    let mut edges = Vec::new();
    let mut pool_tasks_spawned = 0usize;
    let slipstream_router = ctx.router;
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
        let provider = Arc::clone(&ctx.provider);
        let hot_paths = Arc::clone(&ctx.hot_paths);
        let stats = Arc::clone(&stats);
        let quote_semaphore = Arc::clone(&ctx.quote_semaphore);
        let block_number = ctx.edge_ctx.block_number;
        let forced_discovery_quotes_used = Arc::clone(&forced_discovery_quotes_used);
        let chain_env_prefix = ctx.chain_env_prefix.clone();
        let quote_concurrency_limit = ctx.quote_concurrency_limit;
        let prefetched_cl_state = Arc::clone(&prefetched_cl_state);
        let tick_ladders = Arc::clone(&tick_ladders);
        join_set.spawn(async move {
            let mut local_edges = Vec::new();
            let cl_state = if !crate::cl_sim::local_cl_quotes_enabled() {
                None
            } else if let Some(state) = prefetched_cl_state.get(&pool.pool) {
                Some(state.clone())
            } else {
                match crate::cl_sim::load_cl_pool_state(
                    provider.clone(),
                    pool.pool,
                    block_number,
                    None,
                )
                .await
                {
                    Ok(state) => state,
                    Err(err) => {
                        debug!(
                            target: "venue::slipstream",
                            error = %err,
                            pool = %format!("0x{}", hex::encode(pool.pool)),
                            "CL pool state load failed; quoter fallback"
                        );
                        None
                    }
                }
            };
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
                let quote = if let Some(ref cl_state) = cl_state {
                    let zero_for_one = token_in == pool.token0;
                    match cl_grid_quote(
                        cl_state,
                        tick_ladders.get(&pool.pool).map(|l| l.as_ref()),
                        base_amount_in,
                        tolerance_bps,
                        zero_for_one,
                    ) {
                        Ok(Some(q)) => Ok(Some(q)),
                        Ok(None) => Ok(None),
                        Err(err) => {
                            debug!(
                                target: "venue::slipstream",
                                error = %err,
                                pool = %format!("0x{}", hex::encode(pool.pool)),
                                "local CL grid failed; falling back to quoter"
                            );
                            slipstream_grid_quote(
                                quoter.as_ref(),
                                path.clone(),
                                base_amount_in,
                                tolerance_bps,
                                block_number,
                                &quote_semaphore_fee,
                            )
                            .await
                        }
                    }
                } else {
                    slipstream_grid_quote(
                        quoter.as_ref(),
                        path.clone(),
                        base_amount_in,
                        tolerance_bps,
                        block_number,
                        &quote_semaphore_fee,
                    )
                    .await
                };
                let quote = match quote {
                    Ok(opt) => {
                        stats.quote_attempts.fetch_add(1, Ordering::Relaxed);
                        if opt.is_some() {
                            stats.quote_success.fetch_add(1, Ordering::Relaxed);
                        }
                        opt
                    }
                    Err(_) => adjust_trade_size_async(base_amount_in, tolerance_bps, {
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
                            // Hoisted: the batched quote below needs BOTH samples.
                            let probe_in = probe_amount(amount);
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
                                        return Err(anyhow!("slipstream quote semaphore closed"));
                                    }
                                    Err(_) => {
                                        stats.queue_wait_timeouts.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::slipstream",
                                            chain = %chain_env_prefix,
                                            venue = "aerodrome_slipstream",
                                            token_in = %format!("0x{}", hex::encode(token_in)),
                                            token_out = %format!("0x{}", hex::encode(token_out)),
                                            fee = pool.fee,
                                            amount = %amount,
                                            timeout_secs = queue_wait_window.as_secs(),
                                            queue_wait_ms = wait_started_at.elapsed().as_millis() as u64,
                                            permits_available_before = permits_before,
                                            provider_class,
                                            "Slipstream quote stalled before network submission (semaphore wait timeout)"
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
                                // ONE round trip for both samples. slippage_from_samples
                                // needs a probe and the full size; quoting them
                                // separately cost 2 RPCs per fallback edge, and
                                // populate is ~960 calls/scan at ~600ms per round of 64.
                                let grid_amounts: Vec<U256> = if probe_in == amount {
                                    vec![amount]
                                } else {
                                    vec![probe_in, amount]
                                };
                                let result = timeout(
                                    rpc_timeout_window,
                                    quoter.quote_path_grid(
                                        path_inner.clone(),
                                        &grid_amounts,
                                        block_number,
                                    ),
                                )
                                .await;
                                let hold_elapsed_ms = hold_started_at.elapsed().as_millis() as usize;
                                stats
                                    .max_permit_hold_ms
                                    .fetch_max(hold_elapsed_ms, Ordering::Relaxed);
                                drop(permit);
                                stats.semaphore_releases.fetch_add(1, Ordering::Relaxed);
                                debug!(
                                    target: "venue::slipstream",
                                    chain = %chain_env_prefix,
                                    venue = "aerodrome_slipstream",
                                    token_in = %format!("0x{}", hex::encode(token_in)),
                                    token_out = %format!("0x{}", hex::encode(token_out)),
                                    fee = pool.fee,
                                    amount = %amount,
                                    queue_wait_ms,
                                    hold_ms = hold_elapsed_ms,
                                    inflight_quotes,
                                    permits_available_after_release = quote_semaphore.available_permits(),
                                    "Slipstream quote permit released"
                                );

                                let rpc_elapsed_ms = rpc_started_at.elapsed().as_millis() as u64;
                                let total_elapsed_ms = quote_started_at.elapsed().as_millis() as u64;

                                match result {
                                    Ok(Ok(outs)) => {
                                        let out =
                                            outs.last().copied().flatten().unwrap_or_default();
                                        let probe_out =
                                            outs.first().copied().flatten().unwrap_or(out);
                                        if out > U256::zero() {
                                            Ok(Some((out, probe_out)))
                                        } else {
                                            stats.quote_failures.fetch_add(1, Ordering::Relaxed);
                                            Ok(None)
                                        }
                                    }
                                    Ok(Err(err)) => {
                                        stats.quote_failures.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::slipstream",
                                            chain = %chain_env_prefix,
                                            venue = "aerodrome_slipstream",
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
                                            "Slipstream quote failed"
                                        );
                                        Ok(None)
                                    }
                                    Err(_) => {
                                        stats.quote_timeouts.fetch_add(1, Ordering::Relaxed);
                                        warn!(
                                            target: "venue::slipstream",
                                            chain = %chain_env_prefix,
                                            venue = "aerodrome_slipstream",
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
                                            "Slipstream quote timed out"
                                        );
                                        Ok(None)
                                    }
                                }
                            };
                            let (out, probe_out) = match timeout(total_deadline_window, quote_eval).await {
                                Ok(result) => match result? {
                                    Some(pair) => pair,
                                    None => return Ok(None),
                                },
                                Err(_) => {
                                    stats.total_deadline_timeouts.fetch_add(1, Ordering::Relaxed);
                                    warn!(
                                        target: "venue::slipstream",
                                        chain = %chain_env_prefix,
                                        venue = "aerodrome_slipstream",
                                        token_in = %format!("0x{}", hex::encode(token_in)),
                                        token_out = %format!("0x{}", hex::encode(token_out)),
                                        fee = pool.fee,
                                        amount = %amount,
                                        timeout_secs = total_deadline_window.as_secs(),
                                        elapsed_ms = quote_started_at.elapsed().as_millis() as u64,
                                        provider_class,
                                        "Slipstream quote exceeded total request deadline"
                                    );
                                    return Ok(None);
                                }
                            };
                            // probe_out comes from the batched grid above; the separate
                            // probe round trip is gone.
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
                .await?,
                };

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

                // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
                if protected_out.is_zero() {
                    hot_paths_quote
                        .as_ref()
                        .record_failure(token_in, token_out, pool.fee)
                        .await;
                    continue;
                }

                let weight = compute_edge_weight(protected_out, quote.amount_in);
                let edge = Edge {
                    from: token_in,
                    to: token_out,
                    rate_num: quote.amount_out,
                    rate_den: quote.amount_in,
                    venue: VenueEdge::Slipstream {
                        path: path.clone(),
                        pool: pool.pool,
                        tick_spacing: pool.fee,
                        router: slipstream_router,
                        state: cl_state.clone(),
                    },
                    estimated_gas: ESTIMATED_GAS_SLIPSTREAM,
                    weight,
                    // Depth from POOL STATE when we have it; the probe-derived
                    // value only as a fallback. `max_input` bounded by the quote
                    // probe is a fact about our sampling, not about the pool.
                    // Capacity from the pool's REAL balance of the input token
                    // when we have it. Falls back to the probe-derived value
                    // per-edge when the balance read failed — never to the
                    // virtual L/sqrt(P) reserves, which overstate holdings by
                    // 16-56x on deep pools and unboundedly on thin ones.
                    max_input: cl_state
                        .as_ref()
                        .and_then(|st| {
                            let zero_for_one = token_in == pool.token0;
                            let balance_in = if zero_for_one {
                                st.balance0
                            } else {
                                st.balance1
                            };
                            balance_in
                                .filter(|b| !b.is_zero())
                                .map(edge_capacity_from_pool_balance)
                        })
                        .unwrap_or_else(|| edge_capacity_from_quote(&quote, tolerance_bps)),
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: Some(block_number),
                    active: true,
                    tick_ladder: tick_ladders.get(&pool.pool).cloned(),
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
        target: "venue::slipstream",
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
        elapsed_ms = collector_started_at.elapsed().as_millis() as u64,
        "Slipstream hot pool summary",
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
                // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(protected_out, quote.amount_in);
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
                    max_input: edge_capacity_from_quote(&quote, tolerance_bps),
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: Some(ctx.block_number),
                    active: true,
                    tick_ladder: None,
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
                // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(protected_out, quote.amount_in);
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
                    max_input: edge_capacity_from_quote(&quote, tolerance_bps),
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: Some(block_number),
                    active: true,
                    tick_ladder: None,
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
    let concurrency = univ2_load_concurrency();
    let pool_monitor = pool_monitor.clone();

    // Same Multicall3 prefetch as the Solidly collector (spec §3.4): one batched
    // read for every pair, block-pinned, instead of 3 sequential RPCs per pool.
    let univ2_batched = {
        let mut pairs: Vec<Address> = hot_pools.iter().map(|p| p.pair).collect();
        pairs.sort_unstable();
        pairs.dedup();
        if pairs.is_empty() {
            std::collections::HashMap::new()
        } else {
            let started = Instant::now();
            let states =
                crate::quote_univ2::load_pair_states_batched(provider.clone(), &pairs, block_number)
                    .await;
            info!(
                target: "venue::univ2",
                pairs = pairs.len(),
                loaded = states.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "prefetched pair reserves via Multicall3"
            );
            states
        }
    };
    let univ2_batched = Arc::new(univ2_batched);

    let load_outcomes: Vec<Option<(ResolvedUniV2PoolCfg, UniV2PairState, Option<U64>)>> =
        stream::iter(hot_pools.iter().cloned().map(|pool| {
            let provider = provider.clone();
            let pool_monitor = pool_monitor.clone();
            let univ2_batched = Arc::clone(&univ2_batched);
            async move {
                let snapshot = if let Some(monitor) = &pool_monitor {
                    monitor.state_with_block(pool.pair).await
                } else {
                    None
                };
                // Prefer the monitor, then the batch, then the per-pair read.
                let snapshot = snapshot.or_else(|| {
                    univ2_batched
                        .get(&pool.pair)
                        .cloned()
                        .map(|state| (state, Some(block_number)))
                });

                match snapshot {
                    Some((state, last_block)) => Some((pool, state, last_block)),
                    None => match load_pair_state(provider, pool.pair).await {
                        Ok(Some(state)) => Some((pool, state, Some(block_number))),
                        Ok(None) => {
                            debug!(
                                pair = %format!("0x{}", hex::encode(pool.pair)),
                                "Skipping UniV2 pair with unsupported interface"
                            );
                            None
                        }
                        Err(err) => {
                            warn!(
                                error = %err,
                                pair = %format!("0x{}", hex::encode(pool.pair)),
                                "Failed to load UniV2 state"
                            );
                            None
                        }
                    },
                }
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;
    cached_states.extend(load_outcomes.into_iter().flatten());

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
            // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
            if protected_out.is_zero() {
                continue;
            }
            let weight = compute_edge_weight(protected_out, quote.amount_in);
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
                max_input: edge_capacity_from_reserve(reserve_in),
                tolerance_bps,
                observed_slippage_bps: quote.slippage_bps,
                quote_block,
                active: true,
                tick_ladder: None,
            };
            edges.push(edge);
        }
    }
    Ok(edges)
}

async fn collect_solidly_edges<C>(
    provider: Arc<Provider<C>>,
    pool_monitor: Option<Arc<crate::ingestion::PoolMonitor<C>>>,
    chain_env_prefix: &str,
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
    token_decimals: Arc<HashMap<Address, u8>>,
    block_number: U64,
) -> Result<Vec<Edge>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut edges = Vec::new();
    let solidly_started_at = Instant::now();
    let env_key = format!("{chain_env_prefix}_SOLIDLY_V2_POOLS");
    if let Some((raw, source)) = env_var_with_fallback(&env_key, "SOLIDLY_V2_POOLS") {
        let pools = resolve_solidly_pools(&raw, &source)?;
        let configured_pools = pools.len();

        // Load each UNIQUE pair's on-chain state ONCE, concurrently. The prior
        // sequential per-entry load (load_pair_state = 3 RPCs each, repeated for
        // both directions of every pool) was the dominant scan-latency cost
        // (~13s for ~26 entries). Dedup + bounded fan-out collapses it to ~1 RTT.
        let mut unique_pairs: Vec<Address> = pools.iter().map(|pool| pool.pair).collect();
        unique_pairs.sort_unstable();
        unique_pairs.dedup();
        let state_concurrency = solidly_state_concurrency();
        let pool_monitor_ref = pool_monitor.clone();
        let block_for_quotes = block_number;

        // Take whatever the pool monitor already has, then batch-load ONLY the
        // misses in one Multicall3 round-trip (spec §3.4). The previous fan-out
        // issued 3 sequential RPCs per pair — 102 calls for 34 Aerodrome pools —
        // and under rate limiting silently dropped 24-59% of pools per scan,
        // making edge coverage non-deterministic.
        let mut cached: Vec<(Address, UniV2PairState, Option<U64>)> = Vec::new();
        let mut misses: Vec<Address> = Vec::new();
        for pair in unique_pairs {
            let hit = match &pool_monitor_ref {
                Some(monitor) => monitor.state_with_block(pair).await,
                None => None,
            };
            match hit {
                Some((state, last_block)) => cached.push((pair, state, last_block)),
                None => misses.push(pair),
            }
        }

        let batched = if misses.is_empty() {
            std::collections::HashMap::new()
        } else {
            let started = Instant::now();
            let states = crate::quote_univ2::load_pair_states_batched(
                provider.clone(),
                &misses,
                block_for_quotes,
            )
            .await;
            info!(
                target: "venue::solidly",
                pairs = misses.len(),
                loaded = states.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "prefetched pair reserves via Multicall3"
            );
            states
        };

        // Anything the batch could not resolve falls back to the per-pair path,
        // so a partial batch degrades rather than dropping the pool outright.
        let still_missing: Vec<Address> = misses
            .iter()
            .copied()
            .filter(|pair| !batched.contains_key(pair))
            .collect();
        let fallback: Vec<(Address, Option<UniV2PairState>, Option<U64>)> =
            stream::iter(still_missing.into_iter().map(|pair| {
                let provider = provider.clone();
                async move {
                    (
                        pair,
                        load_pair_state(provider, pair).await.unwrap_or(None),
                        Some(block_for_quotes),
                    )
                }
            }))
            .buffer_unordered(state_concurrency)
            .collect()
            .await;

        let loaded_states: Vec<(Address, Option<UniV2PairState>, Option<U64>)> = cached
            .into_iter()
            .map(|(pair, state, blk)| (pair, Some(state), blk))
            .chain(
                batched
                    .into_iter()
                    .map(|(pair, state)| (pair, Some(state), Some(block_for_quotes))),
            )
            .chain(fallback)
            .collect();
        let pair_states: HashMap<Address, (UniV2PairState, Option<U64>)> = loaded_states
            .into_iter()
            .filter_map(|(pair, state, block)| state.map(|state| (pair, (state, block))))
            .collect();

        for pool in pools {
            let Some((state, quote_block)) = pair_states.get(&pool.pair).cloned() else {
                continue;
            };
            let quote_block = quote_block.or(Some(block_number));
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
                // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(protected_out, quote.amount_in);
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
                    max_input: edge_capacity_from_reserve(reserve_in),
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block,
                    active: true,
                    tick_ladder: None,
                };
                edges.push(edge);
            }
        }
        tracing::info!(
            target: "venue::solidly",
            chain = %chain_env_prefix,
            source = %source,
            configured_pools,
            edges_built = edges.len(),
            elapsed_ms = solidly_started_at.elapsed().as_millis() as u64,
            "Loaded Solidly/Aerodrome edges"
        );
    }
    Ok(edges)
}

async fn collect_univ4_edges(
    chain_env_prefix: &str,
    base_profiles: Arc<HashMap<Address, TradeSizing>>,
    default_profile: TradeSizing,
) -> Result<Vec<Edge>> {
    let mut edges = Vec::new();
    let env_key = format!("{chain_env_prefix}_UNIV4_POOLS");
    if let Some((raw, source)) = env_var_with_fallback(&env_key, "UNIV4_POOLS") {
        // The UniV4 quote here is a FIXED-PRICE (zero price-impact) approximation:
        // it assumes infinite liquidity at spot and produces phantom profits for any
        // non-trivial size. A correct V4 quote needs concentrated-liquidity tick
        // crossing (on-chain Quoter/StateView). Until that exists, these edges are
        // OFF by default and must be explicitly opted into.
        let allow_fixed_price = {
            static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *V.get_or_init(|| {
                std::env::var("ENABLE_UNIV4_FIXED_PRICE_QUOTES")
                    .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false)
            })
        };
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
                // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
                if protected_out.is_zero() {
                    continue;
                }
                let weight = compute_edge_weight(protected_out, quote.amount_in);
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
                    max_input: edge_capacity_from_quote(&quote, tolerance_bps),
                    tolerance_bps,
                    observed_slippage_bps: quote.slippage_bps,
                    quote_block: None,
                    active: true,
                    tick_ladder: None,
                };
                edges.push(edge);
            }
        }
    }
    Ok(edges)
}

pub fn edge_pool_address(edge: &Edge) -> Option<Address> {
    // One definition, on the venue itself. Two copies of this drifting apart
    // would mean requoting and route matching disagreed about which pool an
    // edge trades through.
    edge.venue.pool_address()
}

fn edge_touches_pools(edge: &Edge, touched: &HashSet<Address>) -> bool {
    edge_pool_address(edge)
        .map(|pool| touched.contains(&pool))
        .unwrap_or(false)
}

pub struct PopulateResult {
    pub edges: Vec<Edge>,
    // Diagnostics returned by populate_edges but not yet consumed by callers.
    #[allow(dead_code)]
    pub mode: PopulateMode,
    #[allow(dead_code)]
    pub digest: EdgeDigest,
}

/// Outcome of repricing one edge whose pool moved.
// No non-test caller until `process_base_flashblock` drives the requote; the
// allow goes with it, as it did for base_fast.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepriceOutcome {
    /// Pool did not move; the cached edge stands.
    Reused,
    /// Requoted successfully; the edge was replaced.
    Replaced,
    /// Requote failed. The edge is DEACTIVATED, not dropped and not left
    /// standing: a stale quote that still reads `active` is how a dead pool
    /// gets priced as if it were live.
    Deactivated,
}

/// Edge indices whose pool is in `touched`.
///
/// The hot path exists to avoid rebuilding a 683-pool universe because one pool
/// moved. An edge with no resolvable pool address — a bridge, a multi-hop path —
/// is never selected, so it keeps its cached quote rather than being silently
/// treated as fresh.
#[allow(dead_code)]
pub fn edges_touching(edges: &[Edge], touched: &HashSet<Address>) -> Vec<usize> {
    edges
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match edge_pool_address(e) {
            Some(pool) if touched.contains(&pool) => Some(i),
            _ => None,
        })
        .collect()
}

/// Apply one requote result to the cached edge set.
///
/// `Some(edge)` replaces; `None` means the quote failed and the cached edge is
/// deactivated rather than kept. Leaving a failed pool active is worse than
/// dropping the candidate: the searcher would price a cycle through a pool we
/// could not read, and the failure would surface as a reverted trade instead of
/// a skipped one.
#[allow(dead_code)]
pub fn apply_reprice(edges: &mut [Edge], index: usize, quoted: Option<Edge>) -> RepriceOutcome {
    match (edges.get_mut(index), quoted) {
        (Some(slot), Some(fresh)) => {
            *slot = fresh;
            RepriceOutcome::Replaced
        }
        (Some(slot), None) => {
            slot.active = false;
            RepriceOutcome::Deactivated
        }
        (None, _) => RepriceOutcome::Reused,
    }
}

/// Mark every edge NOT touched as reusable, and report the split.
///
/// Returns `(to_requote, reused)`. The caller requotes only the first list; the
/// second keeps its cached quote and its `quote_block`, so downstream staleness
/// checks still apply to it. Nothing here mutates a reused edge — an edge that
/// silently kept a fresh `quote_block` without being requoted would defeat
/// `max_quote_block_lag`.
#[allow(dead_code)]
pub fn reprice_touched(edges: &[Edge], touched: &HashSet<Address>) -> (Vec<usize>, usize) {
    let hot = edges_touching(edges, touched);
    let reused = edges.len().saturating_sub(hot.len());
    (hot, reused)
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
    token_decimals: Arc<HashMap<Address, u8>>,
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
    slipstream_quoter: Option<Arc<SlipstreamQuoter<C>>>,
    slipstream_router: Address,
    slipstream_tick_spacings: Option<Arc<HashSet<u32>>>,
    slipstream_validation: Option<UniV3ValidationConfig>,
    slipstream_validation_once: Arc<OnceCell<()>>,
    hot_slipstream_pools: &[PoolRecord],
    pancakeswap_quoter: Option<Arc<UniQuoter<C>>>,
    pancakeswap_fee_tiers: Option<Arc<HashSet<u32>>>,
    pancakeswap_validation: Option<UniV3ValidationConfig>,
    pancakeswap_validation_once: Arc<OnceCell<()>>,
    hot_pancakeswap_pools: &[PoolRecord],
    _hub_tokens: Arc<HashSet<Address>>,
    // Retained for caller signature stability; detection no longer prices.
    _wrapped_native: Address,
    populate_options: PopulateOptions,
    metrics: Option<Arc<Metrics>>,
    // Owned by `Runner`, constructed once per chain and reused across every
    // `scan_once()` call — NOT rebuilt here. That's what lets the epoch cache
    // inside `CachedTickSource` actually collapse repeat tick reads across
    // scans instead of paying full RPC cost every cycle. Never share one
    // instance across chains/providers: each `Runner` owns exactly one.
    cl_tick_cache: Arc<crate::cl_ticks::CachedTickSource<crate::cl_ticks::RpcTickSource<C>>>,
) -> Result<PopulateResult>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    ensure!(
        max_slippage_bps <= 10_000,
        "EDGE_SLIPPAGE_BPS must be less than or equal to 10_000 (100%). Got {max_slippage_bps}."
    );

    let default_profile = TradeSizing::new(default_base_amount, max_slippage_bps);
    let edge_ctx_template = EdgeBuildContext {
        base_profiles: base_profiles.clone(),
        default_profile,
        block_number,
    };

    if populate_options.touched_pools.is_empty() {
        if let Some(cached) = populate_options.cached_edges.clone() {
            if let Some(last_digest) = populate_options.last_digest {
                let cached_digest = edge_digest(&cached);
                if !edge_digest_changed_significantly(Some(last_digest), cached_digest) {
                    // Edge weights are rate-only, so a gas-price move cannot
                    // invalidate a cached edge set. Reuse it verbatim.
                    let reused = cached;
                    for edge in reused.iter() {
                        g.add_edge(edge.clone());
                    }
                    if let Some(metrics) = &metrics {
                        metrics
                            .populate_skipped_total
                            .with_label_values(&[chain_name.as_str()])
                            .inc();
                    }
                    return Ok(PopulateResult {
                        digest: edge_digest(&reused),
                        edges: reused,
                        mode: PopulateMode::Skipped,
                    });
                }
            }
        }
    }

    let incremental = !populate_options.touched_pools.is_empty();
    let pool_filter = if incremental {
        Some(populate_options.touched_pools.clone())
    } else {
        None
    };

    let mut edges = Vec::new();
    let min_edge_health_score_bps = {
        static V: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *V.get_or_init(|| {
            std::env::var("EDGE_MIN_HEALTH_SCORE_BPS")
                .ok()
                .and_then(|raw| raw.parse::<u32>().ok())
                .unwrap_or(0)
                .min(10_000)
        })
    };

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
    let edge_ctx = edge_ctx_template.clone();
    let univ3_ctx = Univ3EdgeContext {
        edge_ctx: edge_ctx.clone(),
        allowed_fee_tiers: univ3_fee_tiers,
        quoter: Arc::clone(&quoter),
        provider: Arc::clone(&provider),
        hot_paths: Arc::clone(&hot_paths),
        quote_semaphore: Arc::clone(&quote_semaphore),
        quote_concurrency_limit: quote_semaphore.available_permits(),
        token_whitelist: Arc::new(token_whitelist.clone()),
        chain_env_prefix: chain_env_prefix.to_string(),
        pool_filter: pool_filter.clone(),
        tick_cache: Arc::clone(&cl_tick_cache),
    };
    let hot_univ3_filtered = filter_hot_univ3_pools(hot_univ3_pools, token_whitelist);
    let hot_univ2_filtered = filter_hot_univ2_pools(hot_univ2_pools, token_whitelist);
    let univ3_skipped = hot_univ3_pools
        .len()
        .saturating_sub(hot_univ3_filtered.len());
    let univ2_skipped = hot_univ2_pools
        .len()
        .saturating_sub(hot_univ2_filtered.len());
    if univ3_skipped > 0 || univ2_skipped > 0 {
        debug!(
            chain = %chain_name,
            univ3_hot = hot_univ3_pools.len(),
            univ3_quoted = hot_univ3_filtered.len(),
            univ3_skipped,
            univ2_hot = hot_univ2_pools.len(),
            univ2_quoted = hot_univ2_filtered.len(),
            univ2_skipped,
            whitelist_tokens = token_whitelist.len(),
            "Pre-filtered hot pools outside token whitelist before quoting"
        );
    }

    let hot_slipstream_filtered = filter_hot_univ3_pools(hot_slipstream_pools, token_whitelist);
    let active_slipstream_quoter = slipstream_quoter
        .as_ref()
        .filter(|_| slipstream_router != Address::zero() && !hot_slipstream_filtered.is_empty());
    if let Some(active_quoter) = active_slipstream_quoter {
        if let Some(validation) = slipstream_validation.clone() {
            if slipstream_validation_once.get().is_none() {
                let quoter = Arc::clone(active_quoter);
                let chain = chain_name.clone();
                let env_prefix = chain_env_prefix.to_string();
                if let Err(err) = slipstream_validation_once
                    .get_or_try_init(|| {
                        let quoter = quoter.clone();
                        let chain = chain.clone();
                        let env_prefix = env_prefix.clone();
                        let validation = validation.clone();
                        async move {
                            match quoter.validate(&validation).await {
                                Ok(amount_out) => {
                                    info!(
                                        chain = %chain,
                                        env = %env_prefix,
                                        token_in = %format!("0x{}", hex::encode(validation.token_in)),
                                        token_out = %format!("0x{}", hex::encode(validation.token_out)),
                                        tick_spacing = validation.fee,
                                        amount_in = %validation.amount_in,
                                        amount_out = %amount_out,
                                        "Validated Slipstream quoter"
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
                                        tick_spacing = validation.fee,
                                        amount_in = %validation.amount_in,
                                        error = %err,
                                        "Slipstream quoter validation failed"
                                    );
                                    Err(anyhow::anyhow!(
                                        "Slipstream quoter validation failed for chain {chain} ({env_prefix}): {err_msg}"
                                    ))
                                }
                            }
                        }
                    })
                    .await
                {
                    return Err(err.context(format!(
                        "Slipstream quoter validation initialization failed for chain {chain_name} ({chain_env_prefix})"
                    )));
                }
            }
        }
    }

    let slipstream_ctx = active_slipstream_quoter.map(|active_quoter| SlipstreamEdgeContext {
        edge_ctx: edge_ctx.clone(),
        allowed_tick_spacings: slipstream_tick_spacings.clone(),
        quoter: Arc::clone(active_quoter),
        provider: Arc::clone(&provider),
        router: slipstream_router,
        hot_paths: Arc::clone(&hot_paths),
        quote_semaphore: Arc::clone(&quote_semaphore),
        quote_concurrency_limit: quote_semaphore.available_permits(),
        token_whitelist: Arc::new(token_whitelist.clone()),
        chain_env_prefix: chain_env_prefix.to_string(),
        pool_filter: pool_filter.clone(),
        tick_cache: Arc::clone(&cl_tick_cache),
    });
    let slipstream_collect = async {
        if let Some(ctx) = slipstream_ctx.as_ref() {
            collect_slipstream_edges(&hot_slipstream_filtered, ctx).await
        } else {
            Ok(Vec::new())
        }
    };

    let hot_pancakeswap_filtered = filter_hot_univ3_pools(hot_pancakeswap_pools, token_whitelist);
    let active_pancakeswap_quoter = pancakeswap_quoter
        .as_ref()
        .filter(|_| !hot_pancakeswap_filtered.is_empty());
    if let Some(active_quoter) = active_pancakeswap_quoter {
        if let Some(validation) = pancakeswap_validation.clone() {
            if pancakeswap_validation_once.get().is_none() {
                let quoter = Arc::clone(active_quoter);
                let chain = chain_name.clone();
                let env_prefix = chain_env_prefix.to_string();
                let validation_for_init = validation.clone();
                if let Err(err) = pancakeswap_validation_once
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
                                        venue = "pancakeswap_v3",
                                        token_in = %format!("0x{}", hex::encode(validation.token_in)),
                                        token_out = %format!("0x{}", hex::encode(validation.token_out)),
                                        fee_bps = validation.fee,
                                        amount_in = %validation.amount_in,
                                        amount_out = %amount_out,
                                        "Validated PancakeSwap V3 quoter"
                                    );
                                    Ok::<(), anyhow::Error>(())
                                }
                                Err(err) => {
                                    let err_msg = err.to_string();
                                    error!(
                                        chain = %chain,
                                        env = %env_prefix,
                                        venue = "pancakeswap_v3",
                                        error = %err,
                                        "PancakeSwap V3 quoter validation failed"
                                    );
                                    Err(anyhow::anyhow!(
                                        "PancakeSwap V3 quoter validation failed for chain {chain} ({env_prefix}): {err_msg}"
                                    ))
                                }
                            }
                        }
                    })
                    .await
                {
                    return Err(err.context(format!(
                        "PancakeSwap V3 quoter validation initialization failed for chain {chain_name} ({chain_env_prefix})"
                    )));
                }
            }
        }
    }
    let pancakeswap_ctx = active_pancakeswap_quoter.map(|active_quoter| Univ3EdgeContext {
        edge_ctx: edge_ctx.clone(),
        allowed_fee_tiers: pancakeswap_fee_tiers.clone(),
        quoter: Arc::clone(active_quoter),
        provider: Arc::clone(&provider),
        hot_paths: Arc::clone(&hot_paths),
        quote_semaphore: Arc::clone(&quote_semaphore),
        quote_concurrency_limit: quote_semaphore.available_permits(),
        token_whitelist: Arc::new(token_whitelist.clone()),
        chain_env_prefix: chain_env_prefix.to_string(),
        pool_filter: pool_filter.clone(),
        tick_cache: Arc::clone(&cl_tick_cache),
    });
    let pancakeswap_collect = async {
        if let Some(ctx) = pancakeswap_ctx.as_ref() {
            collect_univ3_edges(&hot_pancakeswap_filtered, ctx).await
        } else {
            Ok(Vec::new())
        }
    };

    type EdgeJoinResult = (
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
        Vec<Edge>,
    );
    let (
        mut univ3_edges,
        mut slipstream_edges,
        mut pancakeswap_edges,
        mut bal_edges,
        mut curve_edges,
        mut univ2_edges,
        mut solidly_edges,
        mut univ4_edges,
    ): EdgeJoinResult = tokio::try_join!(
        collect_univ3_edges(&hot_univ3_filtered, &univ3_ctx),
        slipstream_collect,
        pancakeswap_collect,
        collect_balancer_edges(provider.clone(), bal_vault, chain_env_prefix, &edge_ctx),
        collect_curve_edges(
            provider.clone(),
            chain_env_prefix,
            base_profiles.clone(),
            default_profile,
            block_number,
        ),
        collect_univ2_edges(
            provider.clone(),
            pool_monitor.clone(),
            &hot_univ2_filtered,
            base_profiles.clone(),
            default_profile,
            token_decimals.clone(),
            min_liquidity_tokens,
            block_number,
        ),
        collect_solidly_edges(
            provider.clone(),
            pool_monitor.clone(),
            chain_env_prefix,
            base_profiles.clone(),
            default_profile,
            token_decimals.clone(),
            block_number,
        ),
        collect_univ4_edges(
            chain_env_prefix,
            base_profiles.clone(),
            default_profile,
        ),
    )?;

    for edges in [
        &mut univ3_edges,
        &mut slipstream_edges,
        &mut pancakeswap_edges,
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
        &mut slipstream_edges,
        &mut pancakeswap_edges,
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

    if incremental {
        if let Some(cached) = populate_options.cached_edges {
            for edge in cached {
                if !edge_touches_pools(&edge, &populate_options.touched_pools) {
                    g.add_edge(edge.clone());
                    edges.push(edge);
                }
            }
        }
    }

    for edge in univ3_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }
    for edge in slipstream_edges.drain(..) {
        g.add_edge(edge.clone());
        edges.push(edge);
    }
    for edge in pancakeswap_edges.drain(..) {
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
                    // Detection-only haircut; `tolerance_bps` remains the execution min_out margin.
                let protected_out =
                    apply_slippage(quote.amount_out, crate::util::detection_haircut_bps());
                    if protected_out.is_zero() {
                        continue;
                    }
                    let weight = compute_edge_weight(protected_out, quote.amount_in);
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
                        max_input: edge_capacity_from_reserve(reserve_in),
                        tolerance_bps,
                        observed_slippage_bps: quote.slippage_bps,
                        quote_block: Some(block_number),
                        active: true,
                        tick_ladder: None,
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

    let mode = if incremental {
        if let Some(metrics) = &metrics {
            metrics
                .populate_incremental_total
                .with_label_values(&[chain_name.as_str()])
                .inc();
        }
        PopulateMode::Incremental
    } else {
        if let Some(metrics) = &metrics {
            metrics
                .populate_full_total
                .with_label_values(&[chain_name.as_str()])
                .inc();
        }
        PopulateMode::Full
    };

    Ok(PopulateResult {
        digest: edge_digest(&edges),
        edges,
        mode,
    })
}

#[cfg(test)]
mod tests {

/// Serialises the tests that mutate RPC endpoint env vars, and restores them
/// on drop.
///
/// `classify_provider_for_chain` reads these FRESH on every call, so two tests
/// mutating them concurrently flip each other's result. Observed live: a
/// different `classify_provider_*` test failed on each full-suite run while
/// every one passed in isolation.
///
/// The lock alone is not enough. A test that panics between `set_var` and its
/// trailing `remove_var` leaks the value into whichever test runs next, so the
/// restore has to happen on unwind too — hence the guard rather than a bare
/// lock. Same reasoning as `cl_sim::EnvGuard`.
#[cfg(test)]
struct RpcEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

#[cfg(test)]
impl RpcEnvGuard {
    const VARS: [&'static str; 4] = ["BASE_RPC_URL", "BASE_RPC_URLS", "RPC_URL", "RPC_URLS"];

    fn acquire() -> Self {
        static RPC_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // A poisoned lock means an earlier test panicked while holding it. The
        // env is restored by that test's own guard drop, so the lock itself is
        // still usable and recovering beats cascading failures.
        let lock = RPC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = Self::VARS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for k in Self::VARS {
            std::env::remove_var(k);
        }
        Self { _lock: lock, saved }
    }
}

#[cfg(test)]
impl Drop for RpcEnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}
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

    /// The virtual-reserve formula OVERSTATES real depth and must never be used
    /// to size a trade. This pins the trap so it is not re-wired.
    ///
    /// Measured on Base against `balanceOf`: WETH/USDC 0x6c561b446 has virtual
    /// reserve0 562,148 WETH against 10,046 actually held (56x), and virtual
    /// reserve1 1.40B USDC against 88.3M held (16x). Wiring it into `max_input`
    /// took candidate production from ~400/7min to ZERO.
    #[test]
    fn virtual_reserves_overstate_real_depth_and_are_not_a_capacity_source() {
        // sqrt(P)=1 => sqrt_price_x96 = 2^96, so reserve0 == reserve1 == L.
        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: U256::from(1u64) << 96,
            liquidity: 1_000_000_000_000_000_000_000, // 1e21
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 500,
            ..Default::default()
        };
        let (r0, r1) = cl_virtual_reserves(&state).expect("reserves derivable");
        assert_eq!(r0, U256::from(1_000_000_000_000_000_000_000u128));
        assert_eq!(r0, r1, "at sqrt(P)=1 both virtual reserves equal L");

        // The virtual curve claims depth a real pool need not hold. Model a pool
        // whose ACTUAL balance is a small fraction of the virtual reserve, which
        // is the ordinary case for concentrated liquidity.
        let real_balance = r0 / U256::from(56u64); // the measured WETH/USDC ratio
        assert!(
            edge_capacity_from_pool_balance(real_balance) < edge_capacity_from_reserve(r0),
            "capacity from the real balance must be BELOW the virtual-reserve figure; \
             if this ever inverts, the virtual formula is being trusted as depth"
        );

        // Balance-derived capacity uses the same fraction as UniV2/Solidly.
        assert_eq!(
            edge_capacity_from_pool_balance(real_balance),
            edge_capacity_from_reserve(real_balance)
        );

        // No active liquidity => no derivation, so callers fall back rather than
        // inventing a number.
        let empty = crate::cl_sim::ClPoolState { liquidity: 0, ..state };
        assert!(cl_virtual_reserves(&empty).is_none());
        assert!(edge_capacity_from_cl_state(&empty, true).is_none());
    }

    /// Balance-derived capacity must equal `balance_in * RESERVE_BPS / 10_000`
    /// — the same fraction UniV2/Solidly already use — and must NOT be
    /// `probe * PROBE_MULTIPLIER`.
    #[test]
    fn cl_capacity_uses_real_balance_not_probe() {
        // 10,046 WETH — the real holding of WETH/USDC 0x6c561b446 on Base.
        let balance_in = U256::from(10_046u64) * U256::exp10(18);
        let expected = balance_in * U256::from(EDGE_CAPACITY_RESERVE_BPS) / U256::from(10_000u32);
        assert_eq!(edge_capacity_from_pool_balance(balance_in), expected);
        assert_eq!(
            edge_capacity_from_pool_balance(balance_in),
            edge_capacity_from_reserve(balance_in),
            "CL and constant-product edges must size on the same basis"
        );

        // A tiny probe with no measured impact is capped at probe * 256 and
        // cannot see the pool's real depth.
        let tiny_probe = QuoteComputation {
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000u64),
            slippage_bps: 0,
        };
        assert!(
            edge_capacity_from_pool_balance(balance_in)
                > edge_capacity_from_quote(&tiny_probe, 50),
            "real balance must dominate the probe artifact"
        );
    }

    /// The virtual-reserve formula must never reach a capacity path.
    ///
    /// It overstates real holdings by 16-56x on deep Base pools (measured
    /// against balanceOf) and unboundedly on thin ones, because it describes the
    /// tangent constant-product curve running 0..infinity rather than the ticks
    /// that actually hold liquidity. This asserts the source itself, so the trap
    /// cannot be re-introduced by a future edit.
    #[test]
    fn virtual_reserves_are_not_referenced_by_capacity_paths() {
        let src = include_str!("venues.rs");
        // Needles are assembled at runtime so this test's own source does not
        // match itself via `include_str!`.
        let virtual_fn = concat!("cl_virtual", "_reserves");
        let capacity_field = concat!("max_", "input");
        for (n, line) in src.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains(virtual_fn) && code.contains(capacity_field) {
                panic!("line {}: virtual reserves reached a capacity path: {line}", n + 1);
            }
        }
        // And the balance path is what construction actually calls.
        assert!(
            src.contains(concat!("edge_capacity_from_pool", "_balance")),
            "CL construction must use the balance-derived helper"
        );
    }

    /// The shipped UniV4 inventory must parse as a PoolKey, not as a UniV3 pool.
    ///
    /// `data/base/uniswap_v4/pools.jsonl` previously held `{pool, token0, token1,
    /// fee, ...}` — the UniV3 shape, with entries that were literally UniV3 pool
    /// ADDRESSES. UniV4 has no per-pool address: a pool is identified by
    /// `poolId = keccak256(abi.encode(currency0, currency1, fee, tickSpacing,
    /// hooks))` under a single PoolManager, and quoting needs the KEY, not the id
    /// (the hash is one-way). A later revision held `{poolId, pair, liq_usd}`,
    /// which is the right identity but still unparseable here.
    ///
    /// This pins the file against the real resolver so neither shape can return.
    #[test]
    fn shipped_univ4_inventory_parses_as_poolkeys() {
        let raw = match std::fs::read_to_string("data/base/uniswap_v4/pools.json") {
            Ok(raw) => raw,
            // Not every checkout carries chain data; absence is not a failure.
            Err(_) => return,
        };
        if raw.trim().is_empty() {
            return;
        }
        let pools = resolve_univ4_pools(&raw, "data/base/uniswap_v4/pools.json")
            .expect("shipped UniV4 inventory must parse as PoolKeys");
        assert!(!pools.is_empty(), "inventory present but yielded no pools");
        for p in &pools {
            assert!(!p.pool_manager.is_zero(), "poolManager must be set");
            assert_ne!(p.token_in, p.token_out, "a pool cannot trade a token for itself");
            assert!(!p.sqrt_price_x96.is_zero(), "sqrtPriceX96 must be a live price");
            assert!(p.token0 < p.token1, "currencies must be sorted, as PoolKey requires");
        }
    }

    #[test]
    fn rpc_quote_timeout_parser_accepts_positive_values() {
        assert_eq!(parse_rpc_quote_timeout_secs(Some("15")), 15);
    }

    #[test]
    fn classify_provider_prefers_prefixed_single_url() {
        let _env = RpcEnvGuard::acquire();
        std::env::set_var("BASE_RPC_URL", "http://127.0.0.1:8545");

        assert_eq!(classify_provider_for_chain("BASE"), "local_fork");
    }

    #[test]
    fn classify_provider_uses_prefixed_url_list_when_single_missing() {
        // Guard clears all four vars on acquire and restores on drop.
        let _env = RpcEnvGuard::acquire();
        std::env::set_var(
            "BASE_RPC_URLS",
            " https://base.example , http://127.0.0.1:8545 ",
        );

        assert_eq!(classify_provider_for_chain("BASE"), "upstream");
    }

    #[test]
    fn classify_provider_uses_global_url_list_fallback() {
        let _env = RpcEnvGuard::acquire();
        std::env::set_var("RPC_URLS", " , http://localhost:8545, https://base.example");

        assert_eq!(classify_provider_for_chain("BASE"), "local_fork");
    }

    /// Replaces `optimal_trade_size_prefers_peak_profit`, which modelled the
    /// quote as `amount_out = amount_in + profit` — same units, with a genuine
    /// interior maximum. No production caller looks like that: every one
    /// (`quote_exact_input_from_state`, `quote_solidly_exact_input`,
    /// `bal.quote_single_given_in`, `curve.quote_get_dy`, ...) is a SINGLE SWAP
    /// converting token_in to token_out, so the two amounts are different
    /// tokens with different decimals and their difference is meaningless.
    ///
    /// For a single swap, output rises monotonically with input at a declining
    /// marginal rate. There is no interior optimum to find; the only meaningful
    /// bound is slippage tolerance, so the sizer must return the largest size
    /// still inside it.
    #[test]
    fn optimal_trade_size_takes_the_largest_size_within_tolerance() {
        let base = U256::from(200u64);
        let tolerance = 60u32;

        let quote = adjust_trade_size_sync(base, tolerance, |amount| {
            let x = amount.as_u64();
            if x == 0 {
                return Ok(None);
            }
            // Declining marginal rate, and slippage growing with size. Past the
            // tolerance the venue stops offering a usable quote.
            let slippage_bps = (x / 10) as u32;
            if slippage_bps > tolerance {
                return Ok(None);
            }
            Ok(Some(QuoteComputation {
                amount_in: amount,
                amount_out: U256::from(x * 2 - x * x / 4000),
                slippage_bps,
            }))
        })
        .expect("sizing should succeed")
        .expect("expected a quote");

        assert!(
            quote.slippage_bps <= tolerance,
            "must never exceed the tolerance"
        );
        assert!(
            quote.amount_in >= U256::from(400u64),
            "must reach a real notional, not stop at the base/1000 probe \
             (got {})",
            quote.amount_in
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

    fn v2_edge(pair: Address) -> Edge {
        Edge {
            from: Address::zero(),
            to: Address::zero(),
            rate_num: U256::one(),
            rate_den: U256::one(),
            venue: VenueEdge::UniV2 {
                pair,
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
            quote_block: Some(U64::from(100u64)),
            active: true,
            tick_ladder: None,
        }
    }

    /// The whole point of the hot path: one pool moving must not requote the
    /// other 682. The canonical loop rebuilt the universe every scan, which is
    /// 1.71s of a 4.20s decision against a 200ms flashblock cadence.
    #[test]
    fn only_edges_on_a_moved_pool_are_requoted() {
        let moved = Address::from_low_u64_be(1);
        let edges = vec![
            v2_edge(moved),
            v2_edge(Address::from_low_u64_be(2)),
            v2_edge(Address::from_low_u64_be(3)),
        ];
        let touched: HashSet<Address> = [moved].into_iter().collect();
        let (hot, reused) = reprice_touched(&edges, &touched);
        assert_eq!(hot, vec![0]);
        assert_eq!(reused, 2, "untouched edges keep their cached quote");
    }

    /// A failed requote must DEACTIVATE, never leave the stale quote standing.
    /// An edge we could not read but that still reads `active` gets priced as
    /// if it were live, and the failure surfaces as a reverted trade instead of
    /// a skipped candidate.
    #[test]
    fn a_failed_requote_deactivates_rather_than_keeping_a_stale_quote() {
        let mut edges = vec![v2_edge(Address::from_low_u64_be(1))];
        assert!(edges[0].active);
        assert_eq!(apply_reprice(&mut edges, 0, None), RepriceOutcome::Deactivated);
        assert!(
            !edges[0].active,
            "a pool we could not read must not stay priceable"
        );
    }

    #[test]
    fn a_successful_requote_replaces_the_cached_edge() {
        let mut edges = vec![v2_edge(Address::from_low_u64_be(1))];
        let mut fresh = v2_edge(Address::from_low_u64_be(1));
        fresh.quote_block = Some(U64::from(200u64));
        assert_eq!(
            apply_reprice(&mut edges, 0, Some(fresh)),
            RepriceOutcome::Replaced
        );
        assert_eq!(edges[0].quote_block, Some(U64::from(200u64)));
    }

    /// Reused edges must keep their ORIGINAL quote_block. Refreshing it without
    /// requoting would make a stale edge look fresh and defeat
    /// max_quote_block_lag entirely.
    #[test]
    fn reuse_does_not_forge_freshness() {
        let edges = vec![v2_edge(Address::from_low_u64_be(2))];
        let before = edges[0].quote_block;
        let touched: HashSet<Address> = [Address::from_low_u64_be(1)].into_iter().collect();
        let (hot, reused) = reprice_touched(&edges, &touched);
        assert!(hot.is_empty());
        assert_eq!(reused, 1);
        assert_eq!(edges[0].quote_block, before, "reuse must not touch the edge");
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
                tick_ladder: None,
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
            tick_ladder: None,
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

    #[test]
    fn hot_pool_prefilter_requires_both_tokens_in_whitelist() {
        let token_a = Address::from_low_u64_be(1);
        let token_b = Address::from_low_u64_be(2);
        let token_c = Address::from_low_u64_be(3);
        let mut whitelist = HashSet::new();
        whitelist.insert(token_a);
        whitelist.insert(token_b);

        let pools = vec![
            PoolRecord {
                pool: Address::from_low_u64_be(100),
                token0: token_a,
                token1: token_b,
                fee: 500,
                created_block: 0,
                hub_usd_liquidity: None,
            },
            PoolRecord {
                pool: Address::from_low_u64_be(101),
                token0: token_a,
                token1: token_c,
                fee: 500,
                created_block: 0,
                hub_usd_liquidity: None,
            },
        ];

        let filtered = filter_hot_univ3_pools(&pools, &whitelist);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pool, pools[0].pool);
    }

    /// Grid points for a real WETH->USDC edge: 18-decimal input, 6-decimal
    /// output. `amount_out - amount_in` is ~= -amount_in here, which is what
    /// made "most profitable" mean "smallest" and rated every such edge at the
    /// base/1000 probe.
    fn weth_usdc_grid() -> Vec<QuoteComputation> {
        [
            (10_000_000_000_000_000u128, 20_690_000u128),      // 0.01 WETH probe
            (2_500_000_000_000_000_000, 5_172_500_000),        // 2.5 WETH
            (10_000_000_000_000_000_000, 20_690_000_000),      // 10 WETH
            (100_000_000_000_000_000_000, 206_900_000_000),    // 100 WETH
        ]
        .into_iter()
        .map(|(i, o)| QuoteComputation {
            amount_in: U256::from(i),
            amount_out: U256::from(o),
            slippage_bps: 5,
        })
        .collect()
    }

    #[test]
    fn best_quote_picks_the_largest_size_within_tolerance() {
        let grid = weth_usdc_grid();
        let best = best_quote(&grid).expect("a winner");
        assert_eq!(
            best.amount_in,
            U256::from(100_000_000_000_000_000_000u128),
            "must represent the edge at the largest executable size, not the probe"
        );
    }

    #[test]
    fn best_quote_is_not_fooled_by_cross_decimal_pairs() {
        // The regression itself: with the old `amount_out - amount_in` ranking,
        // the 0.01 WETH probe won because -1e16 > -1e20.
        let grid = weth_usdc_grid();
        let best = best_quote(&grid).expect("a winner");
        assert_ne!(
            best.amount_in,
            U256::from(10_000_000_000_000_000u128),
            "the probe size must never be selected as representative"
        );
    }

    #[test]
    fn effective_rate_is_comparable_across_sizes() {
        // Rate falls with size on a real curve; the comparator must see that.
        let small = QuoteComputation {
            amount_in: U256::from(1_000_000_000_000_000_000u128),
            amount_out: U256::from(2_069_000_000u128),
            slippage_bps: 0,
        };
        let large = QuoteComputation {
            amount_in: U256::from(100_000_000_000_000_000_000u128),
            amount_out: U256::from(206_000_000_000u128),
            slippage_bps: 40,
        };
        assert!(
            effective_rate(&small) > effective_rate(&large),
            "slippage must show up as a worse effective rate"
        );
    }

    #[test]
    fn effective_rate_handles_zero_input() {
        let z = QuoteComputation {
            amount_in: U256::zero(),
            amount_out: U256::from(1u64),
            slippage_bps: 0,
        };
        assert_eq!(effective_rate(&z), Decimal::ZERO, "must not divide by zero");
    }
}
