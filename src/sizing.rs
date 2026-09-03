use ethers::providers::JsonRpcClient;
use ethers::types::{Address, U256, U64};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::flash_loan::{
    best_single_provider, flash_fee_for_provider, FlashLoanQuote, FlashLoanSelection,
};
use crate::graph::{Edge, VenueEdge};
use crate::math::mul_div;
use crate::quote_balancer::BalQuote;
use crate::quote_curve::CurveQuote;
use crate::quote_solidly::{
    quote_exact_input_from_state as quote_solidly_exact_input, SolidlyPairState,
};
use crate::quote_univ2::{quote_exact_input_from_state, UniV2PairState};
use crate::quote_slipstream::SlipstreamQuoter;
use crate::quote_univ3::UniQuoter;
use crate::quote_univ4::quote_fixed_price_exact_input;
use crate::util::{apply_slippage, u256_to_f64, NativePrice};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizingResult {
    pub amount_in: U256,
    pub allocations: Vec<FlashLoanSelection>,
    pub gross: U256,
    pub flash_fee: U256,
    pub max_slippage_bps: u32,
    pub net_after_fee_and_gas: U256,
    pub quote_count: usize,
}

#[derive(Clone, Debug)]
struct QuoteValue {
    amount_out: U256,
    slippage_bps: u32,
    #[allow(dead_code)]
    block: U64,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct QuoteKey {
    block: U64,
    pool: [u8; 32],
    token_in: [u8; 20],
    token_out: [u8; 20],
    amount_in: U256,
}

fn log_spaced_amounts(min_amount: U256, max_amount: U256, samples: usize) -> Vec<U256> {
    if min_amount.is_zero() || max_amount < min_amount || samples == 0 {
        return Vec::new();
    }
    if min_amount == max_amount || samples == 1 {
        return vec![min_amount];
    }

    let min_f = u256_to_f64(min_amount).max(1.0);
    let max_f = u256_to_f64(max_amount).max(min_f);
    let log_min = min_f.ln();
    let log_max = max_f.ln();
    let mut out = Vec::with_capacity(samples);
    for i in 0..samples {
        let t = i as f64 / (samples - 1) as f64;
        let v = (log_min + t * (log_max - log_min)).exp();
        let mut value = U256::from(v.round() as u128);
        if value < min_amount {
            value = min_amount;
        }
        if value > max_amount {
            value = max_amount;
        }
        out.push(value);
    }
    out.sort();
    out.dedup();
    if *out.first().unwrap_or(&min_amount) != min_amount {
        out.insert(0, min_amount);
    }
    if *out.last().unwrap_or(&max_amount) != max_amount {
        out.push(max_amount);
    }
    out
}

fn edge_sample_amounts(amount_in: U256, max_input: U256) -> Vec<U256> {
    const EDGE_CURVE_SAMPLES: usize = 5;
    const EDGE_CURVE_MIN_DIVISOR: u64 = 4;
    const EDGE_CURVE_MAX_MULTIPLIER: u64 = 4;

    if amount_in.is_zero() {
        return Vec::new();
    }

    let mut min_amount = amount_in / U256::from(EDGE_CURVE_MIN_DIVISOR);
    if min_amount.is_zero() {
        min_amount = U256::from(1u64);
    }

    let mut max_amount = amount_in.saturating_mul(U256::from(EDGE_CURVE_MAX_MULTIPLIER));
    if !max_input.is_zero() {
        max_amount = max_amount.min(max_input);
    }

    if max_amount < min_amount {
        return vec![amount_in];
    }

    let mut samples = log_spaced_amounts(min_amount, max_amount, EDGE_CURVE_SAMPLES);
    samples.push(amount_in);
    samples.sort();
    samples.dedup();
    samples
}

fn cpmm_newton_optimal_input(edge: &Edge, min_amount: U256, max_amount: U256) -> Option<U256> {
    let (reserve_in, reserve_out, fee_bps) = match &edge.venue {
        crate::graph::VenueEdge::UniV2 {
            reserve_in,
            reserve_out,
            fee_bps,
            ..
        } => (*reserve_in, *reserve_out, *fee_bps),
        crate::graph::VenueEdge::SolidlyV2 {
            reserve_in,
            reserve_out,
            fee_bps,
            stable,
            ..
        } if !stable => (*reserve_in, *reserve_out, *fee_bps),
        _ => return None,
    };

    if reserve_in.is_zero() || reserve_out.is_zero() || max_amount < min_amount {
        return None;
    }

    let rin = u256_to_f64(reserve_in);
    let rout = u256_to_f64(reserve_out);
    if rin <= 0.0 || rout <= 0.0 {
        return None;
    }
    let gamma = (10_000f64 - fee_bps as f64).max(1.0) / 10_000f64;

    let mut x = (rin / 20.0).max(u256_to_f64(min_amount));
    let lower = u256_to_f64(min_amount).max(1.0);
    let upper = u256_to_f64(max_amount).max(lower);

    for _ in 0..10 {
        let denom = rin + gamma * x;
        if denom <= 0.0 {
            break;
        }
        let out = (gamma * x * rout) / denom;
        let f = out - x;
        let fp = (gamma * rout * rin) / (denom * denom) - 1.0;
        if !f.is_finite() || !fp.is_finite() || fp.abs() < 1e-9 {
            break;
        }
        let next = (x - (f / fp)).clamp(lower, upper);
        if (next - x).abs() / x.max(1.0) < 1e-4 {
            x = next;
            break;
        }
        x = next;
    }

    if !x.is_finite() {
        return None;
    }

    let value = U256::from(x.round() as u128);
    Some(value.clamp(min_amount, max_amount))
}

fn pool_key_from_edge(edge: &Edge) -> [u8; 32] {
    match &edge.venue {
        crate::graph::VenueEdge::UniV3 { pool, .. } => {
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(pool.as_bytes());
            out
        }
        crate::graph::VenueEdge::Slipstream { pool, .. } => {
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(pool.as_bytes());
            out
        }
        crate::graph::VenueEdge::Balancer { pool_id, .. } => *pool_id,
        crate::graph::VenueEdge::Curve { pool, .. } => {
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(pool.as_bytes());
            out
        }
        crate::graph::VenueEdge::UniV2 { pair, .. } => {
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(pair.as_bytes());
            out
        }
        crate::graph::VenueEdge::SolidlyV2 { pair, .. } => {
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(pair.as_bytes());
            out
        }
        crate::graph::VenueEdge::Univ4 { pool_manager, .. } => {
            let mut out = [0u8; 32];
            out[12..].copy_from_slice(pool_manager.as_bytes());
            out
        }
        _ => [0u8; 32],
    }
}

fn address_key(addr: ethers::types::Address) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(addr.as_bytes());
    out
}

fn slippage_bps(expected_out: U256, actual_out: U256) -> u32 {
    if expected_out.is_zero() || actual_out >= expected_out {
        return 0;
    }
    let diff = expected_out.saturating_sub(actual_out);
    let bps = mul_div(diff, U256::from(10_000u64), expected_out);
    u32::try_from(bps.as_u64()).unwrap_or(u32::MAX)
}

struct QuoteContext<'a, C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    quoter: &'a UniQuoter<C>,
    slipstream_quoter: Option<&'a SlipstreamQuoter<C>>,
    pancakeswap_quoter: Option<&'a UniQuoter<C>>,
    pancakeswap_pools: Option<&'a HashSet<Address>>,
    bal_quote: &'a BalQuote<C>,
    curve_quote: &'a CurveQuote<C>,
    cache: &'a Mutex<HashMap<QuoteKey, QuoteValue>>,
    quote_count: &'a AtomicUsize,
}

async fn quote_edge_amount<'a, C>(
    edge: &Edge,
    amount_in: U256,
    block: U64,
    ctx: &QuoteContext<'a, C>,
) -> Option<QuoteValue>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    if amount_in.is_zero() {
        return None;
    }
    let key = QuoteKey {
        block,
        pool: pool_key_from_edge(edge),
        token_in: address_key(edge.from),
        token_out: address_key(edge.to),
        amount_in,
    };
    {
        let cache_guard = ctx.cache.lock().await;
        if let Some(cached) = cache_guard.get(&key) {
            return Some(cached.clone());
        }
    }

    let value = match &edge.venue {
        crate::graph::VenueEdge::UniV2 {
            token0,
            token1,
            reserve_in,
            reserve_out,
            fee_bps,
            ..
        } => {
            let (reserve0, reserve1) = if edge.from == *token0 {
                (*reserve_in, *reserve_out)
            } else {
                (*reserve_out, *reserve_in)
            };
            let state = UniV2PairState {
                token0: *token0,
                token1: *token1,
                reserve0,
                reserve1,
            };
            let quote =
                quote_exact_input_from_state(&state, edge.from, amount_in, *fee_bps).ok()??;
            QuoteValue {
                amount_out: quote.amount_out,
                slippage_bps: quote.price_impact_bps,
                block,
            }
        }
        // Local first: the size search evaluates up to 25 candidate amounts and
        // used to pay one `eth_call` per hop per amount, i.e. ~50 round trips to
        // size a single 2-hop cycle. With `cycle_candidate_cap_per_block` at 500
        // that is far more RPC than a 2s Base block can carry, so the time
        // budgets silently truncated the candidate set. Quoting from the pool
        // state captured at edge-build time removes the network entirely from
        // the search; the exact pre-broadcast revm simulation still gates the
        // one size we actually choose.
        crate::graph::VenueEdge::UniV3 {
            path,
            pool,
            state: Some(cl_state),
            ..
        }
        | crate::graph::VenueEdge::Slipstream {
            path,
            pool,
            state: Some(cl_state),
            ..
        } if crate::cl_sim::local_cl_quotes_enabled() && path.len() <= 2 => {
            // `state` describes ONE pool, so this path is only valid for a
            // single-pool hop; multi-hop encoded paths fall through to the
            // router quoter below. UniV3-family pools order tokens by address,
            // so `from` is token0 exactly when it sorts below `to`.
            let zero_for_one = edge.from < edge.to;
            let (out, used_multi) = crate::plan::cl_hop_out(
                cl_state,
                edge.tick_ladder.as_deref(),
                amount_in,
                zero_for_one,
            )?;
            if out.is_zero() {
                return None;
            }
            // Deliberately does NOT touch `ctx.quote_count` — that counter
            // tracks RPC quotes, and the point of this arm is that it issues
            // none. The split between the two is the P1 win made observable.
            debug!(
                pool = %format!("0x{}", hex::encode(pool)),
                ?amount_in,
                used_multi,
                "sized hop locally from cached CL state (no RPC)"
            );
            let expected = mul_div(amount_in, edge.rate_num, edge.rate_den);
            QuoteValue {
                amount_out: out,
                slippage_bps: slippage_bps(expected, out),
                block,
            }
        }
        crate::graph::VenueEdge::UniV3 { path, pool, .. } => {
            ctx.quote_count.fetch_add(1, Ordering::Relaxed);
            let quoter = if let (Some(pq), Some(set)) =
                (ctx.pancakeswap_quoter, ctx.pancakeswap_pools)
            {
                if set.contains(pool) {
                    pq
                } else {
                    ctx.quoter
                }
            } else {
                ctx.quoter
            };
            // The error was swallowed by `.ok()?`, which made every failure
            // here indistinguishable and surfaced only as `cycle unquotable`
            // -- 2,401 of them in one five-minute run on 2026-09-03, against
            // 512 real rejections, with no way to tell why. The path encoding
            // and fee tier were ruled out by calling this quoter directly with
            // the same bytes (0.1 WETH -> 240 USDC at every tier), so whatever
            // remains is in this call's arguments or its block pin.
            let out = match quoter.quote_path(path.clone(), amount_in, block).await {
                Ok(out) => out,
                Err(err) => {
                    warn!(
                        target: "arb_exec::latency",
                        error = %err,
                        pool = %format!("{pool:#x}"),
                        amount_in = %amount_in,
                        block = %block,
                        path_hops = path.len(),
                        "univ3 quote failed"
                    );
                    return None;
                }
            };
            let expected = mul_div(amount_in, edge.rate_num, edge.rate_den);
            QuoteValue {
                amount_out: out,
                slippage_bps: slippage_bps(expected, out),
                block,
            }
        }
        crate::graph::VenueEdge::Slipstream { path, .. } => {
            let slipstream = ctx.slipstream_quoter?;
            ctx.quote_count.fetch_add(1, Ordering::Relaxed);
            let out = slipstream
                .quote_path(path.clone(), amount_in, block)
                .await
                .ok()?;
            let expected = mul_div(amount_in, edge.rate_num, edge.rate_den);
            QuoteValue {
                amount_out: out,
                slippage_bps: slippage_bps(expected, out),
                block,
            }
        }
        crate::graph::VenueEdge::Balancer {
            pool_id,
            token_in,
            token_out,
        } => {
            ctx.quote_count.fetch_add(1, Ordering::Relaxed);
            let out = ctx
                .bal_quote
                .quote_single_given_in((*pool_id).into(), *token_in, *token_out, amount_in, block)
                .await
                .ok()?;
            let expected = mul_div(amount_in, edge.rate_num, edge.rate_den);
            QuoteValue {
                amount_out: out,
                slippage_bps: slippage_bps(expected, out),
                block,
            }
        }
        crate::graph::VenueEdge::Curve { pool, i, j, .. } => {
            ctx.quote_count.fetch_add(1, Ordering::Relaxed);
            let out = ctx
                .curve_quote
                .quote_get_dy(*pool, *i, *j, amount_in, block)
                .await
                .ok()?;
            let expected = mul_div(amount_in, edge.rate_num, edge.rate_den);
            QuoteValue {
                amount_out: out,
                slippage_bps: slippage_bps(expected, out),
                block,
            }
        }
        crate::graph::VenueEdge::SolidlyV2 {
            token0,
            token1,
            reserve_in,
            reserve_out,
            fee_bps,
            stable,
            decimals0,
            decimals1,
            ..
        } => {
            let (reserve0, reserve1) = if edge.from == *token0 {
                (*reserve_in, *reserve_out)
            } else {
                (*reserve_out, *reserve_in)
            };
            let state = SolidlyPairState {
                token0: *token0,
                token1: *token1,
                reserve0,
                reserve1,
                stable: *stable,
                decimals0: *decimals0,
                decimals1: *decimals1,
            };
            let quote =
                quote_solidly_exact_input(&state, edge.from, amount_in, *fee_bps).ok()??;
            QuoteValue {
                amount_out: quote.amount_out,
                slippage_bps: quote.price_impact_bps,
                block,
            }
        }
        crate::graph::VenueEdge::Univ4 {
            sqrt_price_x96,
            token0,
            fee,
            ..
        } => {
            let zero_for_one = edge.from == *token0;
            let quote =
                quote_fixed_price_exact_input(*sqrt_price_x96, amount_in, *fee, zero_for_one)
                    .ok()??;
            let expected = mul_div(amount_in, edge.rate_num, edge.rate_den);
            QuoteValue {
                amount_out: quote.amount_out,
                slippage_bps: slippage_bps(expected, quote.amount_out),
                block,
            }
        }
        _ => return None,
    };

    let mut cache_guard = ctx.cache.lock().await;
    cache_guard.insert(key, value.clone());
    Some(value)
}

async fn quote_edge_with_curve<'a, C>(
    edge: &Edge,
    amount_in: U256,
    block: U64,
    ctx: &QuoteContext<'a, C>,
) -> Option<(QuoteValue, u32)>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let samples = edge_sample_amounts(amount_in, edge.max_input);
    if samples.is_empty() {
        return None;
    }

    let mut min_slippage = u32::MAX;
    let mut max_slippage = 0u32;
    let mut current_quote: Option<QuoteValue> = None;
    let mut fallback: Option<QuoteValue> = None;

    for sample in samples {
        let quote = quote_edge_amount(edge, sample, block, ctx).await?;
        if fallback.is_none() {
            fallback = Some(quote.clone());
        }
        if sample == amount_in {
            current_quote = Some(quote.clone());
        }
        min_slippage = min_slippage.min(quote.slippage_bps);
        max_slippage = max_slippage.max(quote.slippage_bps);
    }

    let quote = current_quote.or(fallback)?;
    let curvature_bps = max_slippage.saturating_sub(min_slippage);
    Some((quote, curvature_bps))
}

/// Stable, low-cardinality venue label for diagnostics. Deliberately the venue
/// KIND, not the pool address: the useful question when quoting fails is "which
/// integration is failing", and pool addresses would explode the cardinality of
/// any aggregation built on this.
fn venue_kind(edge: &Edge) -> &'static str {
    match &edge.venue {
        VenueEdge::UniV3 { .. } => "univ3",
        VenueEdge::Slipstream { .. } => "slipstream",
        VenueEdge::Balancer { .. } => "balancer",
        VenueEdge::Curve { .. } => "curve",
        VenueEdge::UniV2 { .. } => "univ2",
        VenueEdge::SolidlyV2 { .. } => "solidly_v2",
        VenueEdge::Univ4 { .. } => "univ4",
        VenueEdge::Bridge { .. } => "bridge",
        VenueEdge::Liquidation { .. } => "liquidation",
    }
}

async fn simulate_cycle_with_quotes<'a, C>(
    amount: U256,
    edges: &[Edge],
    block: U64,
    ctx: &QuoteContext<'a, C>,
) -> Option<(U256, u32)>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut current = amount;
    let mut max_slippage = 0u32;

    let hops = edges.len();
    for (hop, edge) in edges.iter().enumerate() {
        if !edge.max_input.is_zero() && current > edge.max_input {
            debug!(
                ?amount,
                ?current,
                ?edge.max_input,
                hop,
                hops,
                venue = venue_kind(edge),
                from = ?edge.from,
                to = ?edge.to,
                "rejecting candidate: edge max_input exceeded"
            );
            return None;
        }

        // The `?` here used to discard WHICH hop failed and on WHAT venue, so
        // every quote failure in the cycle collapsed into one undifferentiated
        // `no_quote_available`. A cycle is only as quotable as its worst hop —
        // naming that hop and venue is the difference between "quoting is
        // broken" and "this one venue cannot quote".
        let Some((quote, curvature_bps)) = quote_edge_with_curve(edge, current, block, ctx).await
        else {
            warn!(
                target: "sizing",
                ?amount,
                ?current,
                hop,
                hops,
                venue = venue_kind(edge),
                from = ?edge.from,
                to = ?edge.to,
                "no quote for hop; cycle unquotable"
            );
            return None;
        };
        let curvature_buffer = curvature_bps / 2;
        let base_buffer_bps = edge
            .tolerance_bps
            .max(edge.observed_slippage_bps);
        // quote.amount_out already includes curve/price impact. Re-applying quote slippage
        // as min-out haircuts double-counts deterministic AMM curvature and can turn a
        // genuinely profitable route into false negatives.
        let effective_slippage_bps = base_buffer_bps.saturating_add(curvature_buffer);

        debug!(
            ?amount,
            from = ?edge.from,
            to = ?edge.to,
            quoted_out = ?quote.amount_out,
            quote_slippage_bps = quote.slippage_bps,
            base_buffer_bps,
            curvature_bps,
            effective_slippage_bps,
            "sizing hop quote"
        );

        current = apply_slippage(quote.amount_out, effective_slippage_bps);
        max_slippage = max_slippage.max(effective_slippage_bps);
    }

    Some((current, max_slippage))
}

async fn optimize_with_evaluator<'a>(
    min_amount: U256,
    max_amount: U256,
    mut evaluate: impl FnMut(U256) -> Pin<Box<dyn Future<Output = Option<SizingResult>> + Send + 'a>>
        + Send,
) -> Option<SizingResult> {
    const SAMPLE_POINTS: usize = 9;
    const REFINE_ITERS: usize = 8;
    const GOLDEN_NUM: u64 = 618_034;
    const GOLDEN_DEN: u64 = 1_000_000;

    let samples = log_spaced_amounts(min_amount, max_amount, SAMPLE_POINTS);
    if samples.is_empty() {
        return None;
    }

    let mut scored: Vec<(U256, SizingResult)> = Vec::new();
    for amount in samples {
        if let Some(result) = evaluate(amount).await {
            scored.push((amount, result));
        }
    }
    scored.sort_by_key(|s| s.0);
    if scored.is_empty() {
        return None;
    }
    let (best_idx, _) = scored
        .iter()
        .enumerate()
        .max_by_key(|(_, (_, res))| res.net_after_fee_and_gas)?;
        
    if scored.len() <= 1 {
        return scored.into_iter().map(|(_, res)| res).next();
    }

    if best_idx == 0 || best_idx + 1 >= scored.len() {
        return Some(scored[best_idx].1.clone());
    }

    let mut lo = scored[best_idx - 1].0;
    let mut hi = scored[best_idx + 1].0;
    let mut best = scored[best_idx].1.clone();

    for _ in 0..REFINE_ITERS {
        if hi <= lo {
            break;
        }
        let span = hi.saturating_sub(lo);
        if span.is_zero() {
            break;
        }
        let offset = mul_div(span, U256::from(GOLDEN_NUM), U256::from(GOLDEN_DEN));
        let x1 = hi.saturating_sub(offset);
        let x2 = lo.saturating_add(offset);

        let r1 = evaluate(x1).await;
        let r2 = evaluate(x2).await;

        match (r1, r2) {
            (Some(res1), Some(res2)) => {
                if res1.net_after_fee_and_gas >= res2.net_after_fee_and_gas {
                    hi = x2;
                    if res1.net_after_fee_and_gas > best.net_after_fee_and_gas {
                        best = res1;
                    }
                } else {
                    lo = x1;
                    if res2.net_after_fee_and_gas > best.net_after_fee_and_gas {
                        best = res2;
                    }
                }
            }
            (Some(res1), None) => {
                hi = x2;
                if res1.net_after_fee_and_gas > best.net_after_fee_and_gas {
                    best = res1;
                }
            }
            (None, Some(res2)) => {
                lo = x1;
                if res2.net_after_fee_and_gas > best.net_after_fee_and_gas {
                    best = res2;
                }
            }
            (None, None) => break,
        }
    }

    Some(best)
}
        
pub struct OptimizeTradeParams<'a, C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub edges: &'a [Edge],
    pub quotes: &'a [FlashLoanQuote],
    pub min_amount: U256,
    pub max_amount: U256,
    pub gas_price: U256,
    pub estimated_gas: u64,
    pub l1_data_fee: U256,
    pub native_price: NativePrice,
    pub quoter: &'a UniQuoter<C>,
    pub slipstream_quoter: Option<&'a SlipstreamQuoter<C>>,
    pub pancakeswap_quoter: Option<&'a UniQuoter<C>>,
    pub pancakeswap_pools: Option<&'a HashSet<Address>>,
    pub bal_quote: &'a BalQuote<C>,
    pub curve_quote: &'a CurveQuote<C>,
    pub block_number: U64,
}

pub async fn optimize_trade_size<C>(params: OptimizeTradeParams<'_, C>) -> Option<SizingResult>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let gas_cost_native = params
        .gas_price
        .saturating_mul(U256::from(params.estimated_gas))
        .saturating_add(params.l1_data_fee);
    // Fail closed on an unreliable price rather than fabricating a gas cost.
    //
    // The non-strict `tokens_for_native` silently returns `native_cost`
    // UNCHANGED when `native_amount == 0`, i.e. it reinterprets wei as raw
    // token units. For a 6-decimal token that misreads ~1e14 wei of gas as
    // ~1e8 USDC, and every candidate reads unprofitable forever. Gas is a
    // fixed cost the optimal size must clear (spec §3.3), so sizing on a price
    // we do not trust is never correct — decline to size instead.
    let Some(gas_cost) = params
        .native_price
        .tokens_for_native_strict(gas_cost_native)
    else {
        debug!(

            reason = "unpriceable_start_token",
            ?gas_cost_native,
            "declined to size: no reliable native price for the start token"
        );
        return None;
    };
    let upper_cap = params
        .quotes
        .iter()
        .fold(U256::zero(), |acc, q| acc.saturating_add(q.max_amount))
        .min(params.max_amount);
    // These two early exits were SILENT. They fire before any profitability
    // math, so a candidate rejected here never reaches the `net_after_fee <=
    // gas_cost` logs further down — the caller then reports the catch-all
    // "had no profitable sizing", which wrongly reads as an economics verdict
    // when it is really "the tradable range is empty".
    if upper_cap < params.min_amount {
        debug!(

            reason = "range_empty",
            ?upper_cap,
            min_amount = ?params.min_amount,
            max_amount = ?params.max_amount,
            quote_legs = params.quotes.len(),
            ?gas_cost,
            "declined to size: tradable range is empty (upper_cap < min_amount)"
        );
        return None;
    }
    debug!(

        ?upper_cap,
        min_amount = ?params.min_amount,
        ?gas_cost,
        estimated_gas = params.estimated_gas,
        "sizing search entered"
    );

    let quote_cache: Arc<Mutex<HashMap<QuoteKey, QuoteValue>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let quote_count_total = Arc::new(AtomicUsize::new(0));

    let mut newton_candidates: Vec<U256> = params
        .edges
        .iter()
        .filter_map(|edge| cpmm_newton_optimal_input(edge, params.min_amount, upper_cap))
        .collect();
    newton_candidates.sort_unstable();
    newton_candidates.dedup();

    let mut result = optimize_with_evaluator(params.min_amount, upper_cap, |amount| {
        let cache = Arc::clone(&quote_cache);
        let quote_count = Arc::clone(&quote_count_total);
        Box::pin(async move {
            let allocations = match best_single_provider(amount, params.quotes) {
                Some(selection) => vec![selection],
                None => {
                    debug!(?amount, "rejecting candidate: no flash loan provider");
                    return None;
                }
            };
            let total_allocated = allocations
                .iter()
                .fold(U256::zero(), |acc, p| acc.saturating_add(p.amount));
            if total_allocated < amount {
                debug!(?amount, ?total_allocated, "rejecting candidate: insufficient flash allocation");
                return None;
            }
            let (out, max_slippage_bps) = simulate_cycle_with_quotes(
                amount,
                params.edges,
                params.block_number,
                &QuoteContext {
                    quoter: params.quoter,
                    slipstream_quoter: params.slipstream_quoter,
                    pancakeswap_quoter: params.pancakeswap_quoter,
                    pancakeswap_pools: params.pancakeswap_pools,
                    bal_quote: params.bal_quote,
                    curve_quote: params.curve_quote,
                    cache: cache.as_ref(),
                    quote_count: quote_count.as_ref(),
                },
            )
            .await?;
            if out <= amount {
                debug!(?amount, ?out, "rejecting candidate: cycle output <= input");
                return None;
            }
            let gross = out.saturating_sub(amount);
            let fee = allocations.iter().fold(U256::zero(), |acc, alloc| {
                acc.saturating_add(flash_fee_for_provider(
                    alloc.provider,
                    alloc.amount,
                    alloc.fee_bps,
                ))
            });
            let net_after_fee = gross.saturating_sub(fee);
            if net_after_fee <= gas_cost {
                debug!(
                    ?amount,
                    ?gross,
                    ?fee,
                    ?gas_cost,
                    ?net_after_fee,
                    "rejecting candidate: net_after_fee <= gas_cost"
                );
                return None;
            }
            let net_after_fee_and_gas = net_after_fee.saturating_sub(gas_cost);
            Some(SizingResult {
                amount_in: amount,
                allocations,
                gross,
                flash_fee: fee,
                max_slippage_bps,
                net_after_fee_and_gas,
                quote_count: 0,
            })
        })
    })
    .await;

    for candidate in newton_candidates {
        let cache = Arc::clone(&quote_cache);
        let quote_count = Arc::clone(&quote_count_total);
        let Some(candidate_result) = (async move {
            let allocations = match best_single_provider(candidate, params.quotes) {
                Some(selection) => vec![selection],
                None => {
                    debug!(?candidate, "rejecting newton candidate: no flash loan provider");
                    return None;
                }
            };
            let total_allocated = allocations
                .iter()
                .fold(U256::zero(), |acc, p| acc.saturating_add(p.amount));
            if total_allocated < candidate {
                debug!(?candidate, ?total_allocated, "rejecting newton candidate: insufficient flash allocation");
                return None;
            }
            let (out, max_slippage_bps) = simulate_cycle_with_quotes(
                candidate,
                params.edges,
                params.block_number,
                &QuoteContext {
                    quoter: params.quoter,
                    slipstream_quoter: params.slipstream_quoter,
                    pancakeswap_quoter: params.pancakeswap_quoter,
                    pancakeswap_pools: params.pancakeswap_pools,
                    bal_quote: params.bal_quote,
                    curve_quote: params.curve_quote,
                    cache: cache.as_ref(),
                    quote_count: quote_count.as_ref(),
                },
            )
            .await?;
            if out <= candidate {
                debug!(?candidate, ?out, "rejecting newton candidate: cycle output <= input");
                return None;
            }
            let gross = out.saturating_sub(candidate);
            let fee = allocations.iter().fold(U256::zero(), |acc, alloc| {
                acc.saturating_add(flash_fee_for_provider(
                    alloc.provider,
                    alloc.amount,
                    alloc.fee_bps,
                ))
            });
            let net_after_fee = gross.saturating_sub(fee);
            if net_after_fee <= gas_cost {
                debug!(
                    ?candidate,
                    ?gross,
                    ?fee,
                    ?gas_cost,
                    ?net_after_fee,
                    "rejecting newton candidate: net_after_fee <= gas_cost"
                );
                return None;
            }
            let net_after_fee_and_gas = net_after_fee.saturating_sub(gas_cost);
            Some(SizingResult {
                amount_in: candidate,
                allocations,
                gross,
                flash_fee: fee,
                max_slippage_bps,
                net_after_fee_and_gas,
                quote_count: 0,
            })
        })
        .await
        else {
            continue;
        };

        let should_replace = result
            .as_ref()
            .map(|existing| candidate_result.net_after_fee_and_gas > existing.net_after_fee_and_gas)
            .unwrap_or(true);

        if should_replace {
            result = Some(candidate_result);
        }
    }

    result.map(|result| SizingResult {
        quote_count: quote_count_total.load(Ordering::Relaxed),
        ..result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::VenueEdge;
    use ethers::providers::Provider;
    use ethers::types::Address;
    use std::sync::Arc;

    fn edge(rate_num: u64, rate_den: u64, max_input: u64) -> Edge {
        let reserve_in = U256::from(1_000_000_000u64);
        let reserve_out =
            mul_div(reserve_in, U256::from(rate_num), U256::from(rate_den)).max(U256::from(1u64));
        Edge {
            from: Default::default(),
            to: Default::default(),
            rate_num: U256::from(rate_num),
            rate_den: U256::from(rate_den),
            venue: VenueEdge::UniV2 {
                pair: Default::default(),
                token_out: Default::default(),
                token0: Default::default(),
                token1: Default::default(),
                reserve_in,
                reserve_out,
                fee_bps: 30,
            },
            estimated_gas: 50_000,
            weight: 0,
            max_input: U256::from(max_input),
            tolerance_bps: 50,
            observed_slippage_bps: 75,
            quote_block: None,
            active: true,
            tick_ladder: None,
        }
    }

    #[tokio::test]
    async fn picks_amount_with_highest_net_profit() {
        let edges = vec![edge(2, 1, 1_000_000)];
        let quotes = vec![FlashLoanQuote {
            provider: crate::flash_loan::FlashLoanProvider::Balancer,
            max_amount: U256::from(1_000_000u64),
            fee_bps: 0,
            provider_addr: None,
        }];

        let (provider, _) = Provider::mocked();
        let provider = Arc::new(provider);
        let quoter = UniQuoter::new(provider.clone(), Address::zero(), Address::zero());
        let bal_quote = BalQuote::new(provider.clone(), Address::zero());
        let curve_quote = CurveQuote::new(provider.clone());

        let result = optimize_trade_size(OptimizeTradeParams {
            edges: &edges,
            quotes: &quotes,
            min_amount: U256::from(10_000u64),
            max_amount: U256::from(500_000u64),
            gas_price: U256::zero(),
            estimated_gas: 0,
            l1_data_fee: U256::zero(),
            native_price: NativePrice::new(U256::exp10(18), U256::exp10(18), true),
            quoter: &quoter,
            slipstream_quoter: None,
            pancakeswap_quoter: None,
            pancakeswap_pools: None,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            block_number: U64::zero(),
        })
        .await
        .unwrap();

        assert_eq!(
            result.allocations[0].provider,
            crate::flash_loan::FlashLoanProvider::Balancer
        );
        assert!(result.amount_in >= U256::from(10_000u64));
        assert!(result.net_after_fee_and_gas > U256::zero());
        assert!(result.max_slippage_bps >= 75);
    }

    #[tokio::test]
    async fn respects_provider_cap_and_fee() {
        let edges = vec![edge(110, 100, 2_000_000)];
        let quotes = vec![
            FlashLoanQuote {
                provider: crate::flash_loan::FlashLoanProvider::Balancer,
                max_amount: U256::from(50_000u64),
                fee_bps: 0,
                provider_addr: None,
            },
            FlashLoanQuote {
                provider: crate::flash_loan::FlashLoanProvider::AaveV3,
                max_amount: U256::from(500_000u64),
                fee_bps: 9,
                provider_addr: None,
            },
        ];

        let (provider, _) = Provider::mocked();
        let provider = Arc::new(provider);
        let quoter = UniQuoter::new(provider.clone(), Address::zero(), Address::zero());
        let bal_quote = BalQuote::new(provider.clone(), Address::zero());
        let curve_quote = CurveQuote::new(provider.clone());

        let result = optimize_trade_size(OptimizeTradeParams {
            edges: &edges,
            quotes: &quotes,
            min_amount: U256::from(100_000u64),
            max_amount: U256::from(1_000_000u64),
            gas_price: U256::zero(),
            estimated_gas: 0,
            l1_data_fee: U256::zero(),
            native_price: NativePrice::new(U256::exp10(18), U256::exp10(18), true),
            quoter: &quoter,
            slipstream_quoter: None,
            pancakeswap_quoter: None,
            pancakeswap_pools: None,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            block_number: U64::zero(),
        })
        .await
        .unwrap();

        assert!(result
            .allocations
            .iter()
            .any(|alloc| alloc.provider == crate::flash_loan::FlashLoanProvider::AaveV3));
        assert!(result.amount_in >= U256::from(100_000u64));
        assert!(result.flash_fee > U256::zero());
    }

    #[tokio::test]
    async fn returns_none_when_all_edges_too_thin() {
        let edges = vec![edge(120, 100, 5_000)];
        let quotes = vec![FlashLoanQuote {
            provider: crate::flash_loan::FlashLoanProvider::Balancer,
            max_amount: U256::from(100_000u64),
            fee_bps: 0,
            provider_addr: None,
        }];

        let (provider, _) = Provider::mocked();
        let provider = Arc::new(provider);
        let quoter = UniQuoter::new(provider.clone(), Address::zero(), Address::zero());
        let bal_quote = BalQuote::new(provider.clone(), Address::zero());
        let curve_quote = CurveQuote::new(provider.clone());

        let result = optimize_trade_size(OptimizeTradeParams {
            edges: &edges,
            quotes: &quotes,
            min_amount: U256::from(10_000u64),
            max_amount: U256::from(50_000u64),
            gas_price: U256::from(1u64),
            estimated_gas: 50_000,
            l1_data_fee: U256::zero(),
            native_price: NativePrice::new(U256::exp10(18), U256::exp10(18), true),
            quoter: &quoter,
            slipstream_quoter: None,
            pancakeswap_quoter: None,
            pancakeswap_pools: None,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            block_number: U64::zero(),
        })
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn univ2_quote_matches_reserves() {
        let token_in = Address::from_low_u64_be(1);
        let token_out = Address::from_low_u64_be(2);
        let edge = Edge {
            from: token_in,
            to: token_out,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV2 {
                pair: Address::from_low_u64_be(3),
                token_out,
                token0: token_in,
                token1: token_out,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(500_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        };

        let (provider, _) = Provider::mocked();
        let provider = Arc::new(provider);
        let quoter = UniQuoter::new(provider.clone(), Address::zero(), Address::zero());
        let bal_quote = BalQuote::new(provider.clone(), Address::zero());
        let curve_quote = CurveQuote::new(provider);
        let cache = Mutex::new(HashMap::new());
        let quote_count = AtomicUsize::new(0);

        let quote_ctx = QuoteContext {
            quoter: &quoter,
            slipstream_quoter: None,
            pancakeswap_quoter: None,
            pancakeswap_pools: None,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            cache: &cache,
            quote_count: &quote_count,
        };
        let quote = quote_edge_amount(&edge, U256::from(10_000u64), U64::zero(), &quote_ctx)
            .await
            .expect("quote");
        assert!(quote.amount_out > U256::zero());
        assert_eq!(quote_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn refinement_finds_near_peak() {
        let min_amount = U256::from(100_000u64);
        let max_amount = U256::from(2_000_000u64);
        let target = 1_000_000u64;

        let result = optimize_with_evaluator(min_amount, max_amount, |amount| {
            Box::pin(async move {
                let amt = amount.as_u64();
                let distance = amt.abs_diff(target);
                let score = 2_000_000u64.saturating_sub(distance);
                Some(SizingResult {
                    amount_in: amount,
                    allocations: Vec::new(),
                    gross: U256::from(score),
                    flash_fee: U256::zero(),
                    max_slippage_bps: 0,
                    net_after_fee_and_gas: U256::from(score),
                    quote_count: 0,
                })
            })
        })
        .await
        .expect("result");

        let amt = result.amount_in.as_u64();
        assert!(
            (target.saturating_sub(150_000)..=target.saturating_add(150_000)).contains(&amt),
            "expected amount near target, got {amt}"
        );
    }

    #[test]
    fn edge_samples_include_amount_and_respect_cap() {
        let amount_in = U256::from(1_000u64);
        let cap = U256::from(2_500u64);
        let samples = edge_sample_amounts(amount_in, cap);
        assert!(samples.contains(&amount_in));
        assert!(samples.iter().all(|value| *value <= cap));
        assert!(samples.len() >= 2);
    }

    #[test]
    fn cpmm_newton_seed_respects_bounds() {
        let candidate = cpmm_newton_optimal_input(
            &edge(105, 100, 1_000_000),
            U256::from(10_000u64),
            U256::from(500_000u64),
        )
        .expect("cpmm edge should produce a candidate");

        assert!(candidate >= U256::from(10_000u64));
        assert!(candidate <= U256::from(500_000u64));
    }
}
