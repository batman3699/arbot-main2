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
    /// pool -> the quoter that indexes it, for UniV3 forks sharing the
    /// Slipstream branch. Absent means "use the one configured quoter", which
    /// is correct only when a single fork venue is enabled.
    slipstream_quoters: Option<&'a HashMap<Address, Address>>,
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
        crate::graph::VenueEdge::Slipstream { path, pool, .. } => {
            let slipstream = ctx.slipstream_quoter?;
            ctx.quote_count.fetch_add(1, Ordering::Relaxed);
            // Each fork's quoter indexes only its OWN pools, so asking the
            // wrong one is not an inaccurate quote -- it is a question about a
            // pool that contract has never seen, and it reverts.
            let out = match ctx.slipstream_quoters.and_then(|m| m.get(pool).copied()) {
                Some(addr) => slipstream
                    .quote_path_at(addr, path.clone(), amount_in, block)
                    .await,
                None => slipstream.quote_path(path.clone(), amount_in, block).await,
            };
            let out = match out {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        target: "arb_exec::latency",
                        error = %err,
                        pool = %format!("{pool:#x}"),
                        amount_in = %amount_in,
                        "slipstream quote failed"
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

    let mut attempted = 0usize;
    let mut refused = 0usize;
    for sample in samples {
        attempted += 1;
        // A sample that will not quote is SKIPPED, not fatal. The grid probes
        // up to 4x the base amount deliberately, so the largest sizes are
        // expected to exceed what some pools can fill -- that is what the
        // search is for. Aborting the hop on the first refusal threw away every
        // size that DID quote, and `current_quote`/`fallback` below exist
        // precisely to tolerate a partial grid; the `?` here meant they were
        // never reached. Measured 2026-09-03: a Slipstream pool refused a 124
        // WETH probe and the hop was declared unquotable, which is then
        // recorded as no_profitable_size.
        let Some(quote) = quote_edge_amount(edge, sample, block, ctx).await else {
            refused += 1;
            continue;
        };
        if fallback.is_none() {
            fallback = Some(quote.clone());
        }
        if sample == amount_in {
            current_quote = Some(quote.clone());
        }
        min_slippage = min_slippage.min(quote.slippage_bps);
        max_slippage = max_slippage.max(quote.slippage_bps);
    }

    crate::util::GRID_SAMPLES_ATTEMPTED.fetch_add(attempted, Ordering::Relaxed);
    crate::util::GRID_SAMPLES_REFUSED.fetch_add(refused, Ordering::Relaxed);
    let survivors = attempted.saturating_sub(refused);
    if survivors == 1 {
        crate::util::GRID_HOPS_SINGLE_SAMPLE.fetch_add(1, Ordering::Relaxed);
    } else if refused == 0 {
        crate::util::GRID_HOPS_INTACT.fetch_add(1, Ordering::Relaxed);
    }

    // Substituting another size's quote is NOT free: `amount_out` is then the
    // output for a DIFFERENT input, and the caller uses it as the output for
    // this one. `fallback` is the first sample that quoted and the grid is
    // sorted ascending, so the substitute is the smallest surviving size --
    // as little as amount_in/4. Counted before it is used, because it reads
    // downstream as an unprofitable cycle rather than an unfillable size.
    if current_quote.is_some() {
        crate::util::HOP_QUOTE_EXACT.fetch_add(1, Ordering::Relaxed);
    } else if fallback.is_some() {
        crate::util::HOP_QUOTE_SIZE_SUBSTITUTED.fetch_add(1, Ordering::Relaxed);
        debug!(
            requested = ?amount_in,
            venue = venue_kind(edge),
            attempted,
            refused,
            "exact size refused; substituting another size's quote for this hop"
        );
    }
    // Only a grid where NOTHING quoted is unquotable.
    let Some(quote) = current_quote.or(fallback) else {
        if refused > 0 {
            debug!(
                target: "arb_exec::latency",
                attempted,
                refused,
                venue = venue_kind(edge),
                "every size in the grid was refused"
            );
        }
        return None;
    };
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
    pub slipstream_quoters: Option<&'a HashMap<Address, Address>>,
    pub bal_quote: &'a BalQuote<C>,
    pub curve_quote: &'a CurveQuote<C>,
    pub block_number: U64,
    /// Chain base fee, kept SEPARATE from `gas_price`.
    ///
    /// `gas_price` is base + our configured priority bid, and that bid is a
    /// choice rather than a market fact: measured 2026-09-04 it was 1 gwei
    /// against a 0.006 gwei base, so it set the effective price 166x above the
    /// floor. A census that folds the two together cannot tell a route that
    /// loses from a bid that is too high.
    pub base_fee_per_gas: Option<U256>,
    /// What the fast path's LOCAL pricing claimed for this cycle, in bps.
    ///
    /// Diagnostic only -- never read by the search. It exists so a refusal can
    /// be paired with the claim that produced it: local pricing and the quoter
    /// disagreeing is the open question, and a refusal that does not carry the
    /// claim it contradicts cannot answer it. `NaN` from the scan path, which
    /// has no such claim.
    pub local_gross_bps: f64,
}

/// `out/in - 1` in basis points, as an f64, for diagnostics only.
///
/// U256 has no fractional part, so the ratio is taken after converting to f64
/// rather than before: `out * 10_000 / amount` in integer arithmetic would
/// truncate every sub-basis-point difference to zero.
fn u256_ratio_bps(out: U256, amount: U256) -> f64 {
    let (o, a) = (
        out.to_string().parse::<f64>().unwrap_or(f64::NAN),
        amount.to_string().parse::<f64>().unwrap_or(f64::NAN),
    );
    if !(a.is_finite() && a > 0.0) {
        return f64::NAN;
    }
    (o / a - 1.0) * 10_000.0
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
                    slipstream_quoters: params.slipstream_quoters,
                    bal_quote: params.bal_quote,
                    curve_quote: params.curve_quote,
                    cache: cache.as_ref(),
                    quote_count: quote_count.as_ref(),
                },
            )
            .await?;
            if out <= amount {
                let quoted_bps = u256_ratio_bps(out, amount);
                debug!(
                    ?amount, ?out, quoted_bps,
                    local_gross_bps = params.local_gross_bps,
                    disagreement_bps = params.local_gross_bps - quoted_bps,
                    hops = params.edges.len(),
                    "rejecting candidate: cycle output <= input"
                );
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
                    slipstream_quoters: params.slipstream_quoters,
                    bal_quote: params.bal_quote,
                    curve_quote: params.curve_quote,
                    cache: cache.as_ref(),
                    quote_count: quote_count.as_ref(),
                },
            )
            .await?;
            if out <= candidate {
                let quoted_bps = u256_ratio_bps(out, candidate);
                debug!(
                    ?candidate, ?out, quoted_bps,
                    local_gross_bps = params.local_gross_bps,
                    disagreement_bps = params.local_gross_bps - quoted_bps,
                    hops = params.edges.len(),
                    "rejecting newton candidate: cycle output <= input"
                );
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

    // PROFITABILITY CENSUS. Off unless `ARBOT_CENSUS=1`.
    //
    // Sweeps a fixed logarithmic ladder of sizes over THIS route, quoting each
    // for real, and writes one row per (route, size). It answers the only
    // question that decides whether this system can earn: across every viable
    // cycle, what is the maximum realised net profit over trade size?
    //
    // Deliberately NOT the search. The optimiser returns the best size and
    // discards every other point, and a route that loses at 0.1 WETH and wins
    // at 3 tells you nothing through a single verdict. Ranking on
    // `max realised net` needs the whole curve.
    //
    // Returns None so nothing executes: this is a measurement pass.
    if census_enabled() {
        census_sweep(&params, &quote_cache, &quote_count_total, gas_cost).await;
        return None;
    }

    // FORCED ATTEMPT. Off unless `ARBOT_FORCE_ATTEMPT=1`.
    //
    // Every sizing refusal measured so far is `cycle output <= input`, so the
    // search has never once returned a size and the whole path beyond it --
    // plan construction, router encoding, the flash-loan callback, gas
    // estimation, dispatch -- has never executed. An unprofitable trade that
    // is BUILT and rejected on chain tells us more about that path than
    // another profitable-trade hunt that never reaches it.
    //
    // Sized at `min_amount`, the smallest legal notional, from the real quote
    // at that size. Nothing here fabricates a profit: `net_after_fee_and_gas`
    // is zero, so the caller's own `net_profit < min_profit` gate still sees an
    // unprofitable trade and every downstream check runs on true numbers.
    if result.is_none() && force_attempt() {
        let allocations = match best_single_provider(params.min_amount, params.quotes) {
            Some(selection) => vec![selection],
            None => {
                warn!("forced attempt: no flash loan provider at min_amount");
                return None;
            }
        };
        let quoted = simulate_cycle_with_quotes(
            params.min_amount,
            params.edges,
            params.block_number,
            &QuoteContext {
                quoter: params.quoter,
                slipstream_quoter: params.slipstream_quoter,
                pancakeswap_quoter: params.pancakeswap_quoter,
                pancakeswap_pools: params.pancakeswap_pools,
                slipstream_quoters: params.slipstream_quoters,
                bal_quote: params.bal_quote,
                curve_quote: params.curve_quote,
                cache: quote_cache.as_ref(),
                quote_count: quote_count_total.as_ref(),
            },
        )
        .await;
        let (out, max_slippage_bps) = match quoted {
            Some(v) => v,
            None => {
                warn!("forced attempt: the cycle would not quote at min_amount");
                return None;
            }
        };
        let fee = allocations.iter().fold(U256::zero(), |acc, alloc| {
            acc.saturating_add(flash_fee_for_provider(
                alloc.provider,
                alloc.amount,
                alloc.fee_bps,
            ))
        });
        warn!(
            amount_in = ?params.min_amount,
            ?out,
            ?gas_cost,
            ?fee,
            "FORCED ATTEMPT: sizing an unprofitable cycle to exercise the \
             execution path. This is expected to revert."
        );
        return Some(SizingResult {
            amount_in: params.min_amount,
            allocations,
            gross: out.saturating_sub(params.min_amount),
            flash_fee: fee,
            max_slippage_bps,
            net_after_fee_and_gas: U256::zero(),
            quote_count: quote_count_total.load(Ordering::Relaxed),
        });
    }

    result.map(|result| SizingResult {
        quote_count: quote_count_total.load(Ordering::Relaxed),
        ..result
    })
}

/// Whether to run the profitability census instead of the sizing search.
fn census_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("ARBOT_CENSUS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if on {
            warn!(
                path = %census_path(),
                "ARBOT_CENSUS is set: sweeping trade sizes and recording net \
                 profit. Nothing will execute."
            );
        }
        on
    })
}

fn census_path() -> String {
    std::env::var("ARBOT_CENSUS_PATH").unwrap_or_else(|_| "/tmp/arbot_census.jsonl".to_string())
}

/// The size ladder, in NATIVE wei. Logarithmic, because the profit curve is.
///
/// A cycle's net is `out(x) - x - fixed`, which rises then falls: too small and
/// the fixed costs dominate, too large and price impact does. A linear sweep
/// spends its points where nothing changes; a log sweep finds the peak.
const CENSUS_LADDER_NATIVE: [f64; 10] = [
    1e15, 3e15, 1e16, 3e16, 1e17, 3e17, 1e18, 3e18, 1e19, 3e19,
];

/// Quote one route across the whole ladder and record what it really earns.
async fn census_sweep<C>(
    params: &OptimizeTradeParams<'_, C>,
    cache: &Arc<Mutex<HashMap<QuoteKey, QuoteValue>>>,
    quote_count: &Arc<AtomicUsize>,
    gas_cost: U256,
) where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    use std::io::Write;

    let route: Vec<String> = params
        .edges
        .iter()
        .map(|e| {
            format!(
                "{{\"venue\":\"{}\",\"pool\":\"{:#x}\",\"from\":\"{:#x}\",\"to\":\"{:#x}\"}}",
                venue_kind(e),
                e.venue.pool_address().unwrap_or_default(),
                e.from,
                e.to
            )
        })
        .collect();
    let route_json = format!("[{}]", route.join(","));
    let start = params.edges.first().map(|e| e.from).unwrap_or_default();

    // Gas, decomposed. `gas_cost` is what the runner would charge at its own
    // bid; `gas_cost_at_base` is the same trade at the chain's base fee with no
    // priority. Both are recorded so a route that loses can be told apart from
    // a bid that is too high.
    let gas_units = params.estimated_gas;
    let eff_price = params.gas_price;
    let base_fee = params.base_fee_per_gas.unwrap_or(eff_price);
    let l1 = params.l1_data_fee;
    let l2 = eff_price.saturating_mul(U256::from(gas_units));
    let gas_at_base = base_fee
        .saturating_mul(U256::from(gas_units))
        .saturating_add(l1);
    let gas_at_base_tokens = params
        .native_price
        .tokens_for_native_strict(gas_at_base)
        .unwrap_or(gas_cost);

    let mut rows = String::new();
    for native in CENSUS_LADDER_NATIVE {
        // The ladder is in native; the trade is denominated in the start token.
        let Some(amount) = params
            .native_price
            .tokens_for_native_strict(U256::from(native as u128))
        else {
            continue;
        };
        if amount.is_zero() {
            continue;
        }
        // Capacity is a fact about the pools and the loan, not a preference.
        // A size nothing can fund is recorded as refused rather than skipped,
        // because "no provider at 10 WETH" is itself part of the answer.
        let provider = best_single_provider(amount, params.quotes);
        let (flash_fee, provider_ok) = match &provider {
            Some(sel) => (
                flash_fee_for_provider(sel.provider, sel.amount, sel.fee_bps),
                sel.amount >= amount,
            ),
            None => (U256::zero(), false),
        };
        let quoted = if provider_ok {
            simulate_cycle_with_quotes(
                amount,
                params.edges,
                params.block_number,
                &QuoteContext {
                    quoter: params.quoter,
                    slipstream_quoter: params.slipstream_quoter,
                    pancakeswap_quoter: params.pancakeswap_quoter,
                    pancakeswap_pools: params.pancakeswap_pools,
                    slipstream_quoters: params.slipstream_quoters,
                    bal_quote: params.bal_quote,
                    curve_quote: params.curve_quote,
                    cache: cache.as_ref(),
                    quote_count: quote_count.as_ref(),
                },
            )
            .await
        } else {
            None
        };

        let (out, slip) = match quoted {
            Some((o, s)) => (o, s),
            None => {
                rows.push_str(&format!(
                    "{{\"route\":{route_json},\"hops\":{},\"start\":\"{start:#x}\",                     \"amount_in\":\"{amount}\",\"refused\":true,                     \"reason\":\"{}\"}}\n",
                    params.edges.len(),
                    if provider_ok { "unquotable" } else { "no_flash_capacity" },
                ));
                continue;
            }
        };
        // Signed, because most of these lose and the loss is the finding.
        let gross = i128::try_from(out.min(U256::from(u128::MAX)).as_u128()).unwrap_or(i128::MAX)
            - i128::try_from(amount.min(U256::from(u128::MAX)).as_u128()).unwrap_or(i128::MAX);
        let costs = i128::try_from(
            flash_fee
                .saturating_add(gas_cost)
                .min(U256::from(u128::MAX))
                .as_u128(),
        )
        .unwrap_or(i128::MAX);
        let net = gross - costs;
        let costs_at_base = i128::try_from(
            flash_fee
                .saturating_add(gas_at_base_tokens)
                .min(U256::from(u128::MAX))
                .as_u128(),
        )
        .unwrap_or(i128::MAX);
        let net_at_base = gross - costs_at_base;
        let amt_f = u256_to_f64(amount);
        let net_bps = if amt_f > 0.0 { net as f64 / amt_f * 10_000.0 } else { f64::NAN };
        rows.push_str(&format!(
            "{{\"route\":{route_json},\"hops\":{},\"start\":\"{start:#x}\",             \"amount_in\":\"{amount}\",\"amount_out\":\"{out}\",             \"gross\":{gross},\"flash_fee\":\"{flash_fee}\",             \"gas_units\":{gas_units},\"effective_gas_price\":\"{eff_price}\",             \"base_fee_per_gas\":\"{base_fee}\",\"l1_fee\":\"{l1}\",\"l2_fee\":\"{l2}\",             \"gas_cost\":\"{gas_cost}\",\"gas_cost_at_base\":\"{gas_at_base}\",             \"net\":{net},\"net_at_base_fee\":{net_at_base},\"net_bps\":{net_bps:.4},             \"max_slippage_bps\":{slip},\"refused\":false}}\n",
            params.edges.len(),
        ));
    }

    if rows.is_empty() {
        return;
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(census_path())
    {
        Ok(mut f) => {
            if let Err(err) = f.write_all(rows.as_bytes()) {
                warn!(error = %err, "census row write failed");
            }
        }
        Err(err) => warn!(error = %err, path = %census_path(), "census file open failed"),
    }
}

/// Whether to size a cycle the search rejected, so the execution path runs.
///
/// Read once. This is an operator switch for a deliberate, loss-making probe
/// of the pipeline, never a trading mode: it sizes at the smallest legal
/// notional and reports zero net profit, so nothing downstream mistakes the
/// result for an opportunity.
fn force_attempt() -> bool {
    static FORCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FORCE.get_or_init(|| {
        let on = std::env::var("ARBOT_FORCE_ATTEMPT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if on {
            warn!(
                "ARBOT_FORCE_ATTEMPT is set: unprofitable cycles will be sized at \
                 the minimum notional to exercise the execution path"
            );
        }
        on
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
            slipstream_quoters: None,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            block_number: U64::zero(),
            local_gross_bps: f64::NAN,
            base_fee_per_gas: None,
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
            slipstream_quoters: None,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            block_number: U64::zero(),
            local_gross_bps: f64::NAN,
            base_fee_per_gas: None,
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
            slipstream_quoters: None,
            bal_quote: &bal_quote,
            curve_quote: &curve_quote,
            block_number: U64::zero(),
            local_gross_bps: f64::NAN,
            base_fee_per_gas: None,
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
            slipstream_quoters: None,
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
