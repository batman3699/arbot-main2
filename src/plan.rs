use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;

use crate::graph::{Edge, Graph, VenueEdge};
use crate::math::mul_div;
use crate::quote_solidly::{quote_exact_input_from_state as quote_solidly_out, SolidlyPairState};
use crate::quote_univ2::{quote_exact_input_from_state as quote_univ2_out, UniV2PairState};
use crate::quote_univ3::UniQuoter;
use crate::util::{apply_slippage, encode_univ3_path};
use tracing::{info, warn};

/// Real expected output for a single hop at the actual (optimized) trade size.
///
/// For constant-product (UniV2) and Solidly pools the full curve state lives on
/// the edge, so we compute the TRUE on-chain output. The previous implementation
/// linearly extrapolated the probe-size rate (`rate_num/rate_den`), which
/// overstates output at larger sizes (a secant lies above the convex AMM curve)
/// and makes exact-output UniV2/Solidly swaps revert on-chain. Other venues
/// (UniV3/Curve/Balancer) keep the linear estimate, where min_out is only a floor
/// and is validated by pre-broadcast simulation.
/// Haircut applied to single-tick CL quotes, in bps, covering liquidity the
/// simulator cannot see because it does not cross ticks. Set 0 once the
/// simulator is multi-tick.
fn cl_tick_buffer_bps() -> u32 {
    static V: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        crate::util::env_parse_opt::<u32>("ARBOT_CL_TICK_BUFFER_BPS")
            .unwrap_or(50)
            .min(2_000)
    })
}

/// Slack applied to a MULTI-TICK hop's `min_out`.
///
/// Distinct from [`cl_tick_buffer_bps`], which compensates for the single-tick
/// model holding liquidity constant. Multi-tick models the crossing, so
/// re-applying THAT buffer would double-count it. But modelling the curve is not
/// the same as matching the router, and the residual gap was measured, not
/// guessed.
///
/// Slipstream pool 0xdbc6998296caa1652a810dc8d3baf4a8294330f1, 4340.955932 USDC
/// -> WETH, quoted through the deployed router with the planner's own path bytes
/// (tickSpacing=1):
///
///   planner expected_out  1.891636 WETH
///   planner min_out       1.890690 WETH
///   router pays           1.885833 WETH
///
/// The model overstates the router by ~31 bps, while `edge.tolerance_bps` on
/// these edges is 5. `min_out` therefore lands 25.8 bps ABOVE what the router
/// will pay and `amountOutMinimum` fails — observed on 100% of candidates across
/// 57k+ records, invariant to trade size, tick buffer and pricing model.
///
/// Default 75 bps: covers the measured 31 with margin for the pool moving
/// between quote and execution.
fn cl_exec_buffer_bps() -> u32 {
    static V: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        crate::util::env_parse_opt::<u32>("ARBOT_CL_EXEC_BUFFER_BPS")
            .unwrap_or(75)
            .min(2_000)
    })
}

/// Low-cardinality venue label for diagnostics.
fn venue_kind_label(venue: &VenueEdge) -> &'static str {
    match venue {
        VenueEdge::UniV3 { .. } => "univ3",
        VenueEdge::Slipstream { .. } => "slipstream",
        VenueEdge::UniV2 { .. } => "univ2",
        VenueEdge::SolidlyV2 { .. } => "solidly_v2",
        VenueEdge::Balancer { .. } => "balancer",
        VenueEdge::Curve { .. } => "curve",
        VenueEdge::Univ4 { .. } => "univ4",
        VenueEdge::Bridge { .. } => "bridge",
        VenueEdge::Liquidation { .. } => "liquidation",
    }
}

/// How far `floor` sits below `expected`, in bps. The quantity that decides
/// whether a hop reverts: if the pool pays less than this haircut allows for,
/// the router's `min_out` check fails.
fn bps_below(expected: U256, floor: U256) -> u64 {
    if expected.is_zero() || floor >= expected {
        return 0;
    }
    let diff = expected.saturating_sub(floor);
    mul_div(diff, U256::from(10_000u64), expected).as_u64()
}

/// Price one CL hop, preferring the multi-tick model.
///
/// Returns `(amount_out, used_multi_tick)`. `used_multi_tick` is true only
/// when a ladder produced a NON-exhausted quote — i.e. when tick crossing was
/// genuinely modelled. Callers use it to decide whether the
/// `ARBOT_CL_TICK_BUFFER_BPS` haircut still applies.
pub fn cl_hop_out(
    state: &crate::cl_sim::ClPoolState,
    ladder: Option<&crate::cl_swap::TickLadder>,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<(U256, bool)> {
    if crate::cl_sim::multi_tick_enabled() {
        if ladder.is_none() {
            tracing::debug!(
                target: "minout",
                "multi-tick ON but this edge carries NO ladder; forced to single-tick"
            );
        }
        if let Some(ladder) = ladder {
            if let Some(quote) = crate::cl_swap::quote_exact_input_multi_tick(
                state,
                ladder,
                amount_in,
                zero_for_one,
                crate::cl_sim::cl_max_ticks_crossed(),
            ) {
                if !quote.exhausted
                    && !quote.amount_out.is_zero()
                    && !quote.is_unfillable(amount_in)
                {
                    return Some((quote.amount_out, true));
                }
                // `exhausted` is an ANSWER, not a failure: the pool cannot fill
                // this size. Falling through to single-tick here was the bug
                // that made every candidate a phantom — that model holds
                // liquidity constant, i.e. assumes INFINITE depth, so it is
                // guaranteed to be *more* optimistic than the model that just
                // said "impossible". Exhaustion is precisely the signal that
                // the constant-liquidity assumption is invalid, so it must
                // never be the trigger for adopting it.
                //
                // Measured on WETH/bsdETH 0xdea629c5587037d0925ff85f1961d95db62bedd6
                // (fee 500) at block 50222943. The pool holds 248.9 WETH but
                // only 0.154 bsdETH:
                //   single-tick model   1.904190 bsdETH   <- what we planned on
                //   on-chain quoter     0.0000169 bsdETH  <- what it actually pays
                // a ~112,000x overstatement, and the quoter drives the pool to
                // MIN_SQRT_RATIO+1, i.e. it drains that side outright. Every
                // such edge reverts `Too little received`, and draining
                // thousands of empty ticks is what burned 3-4M gas per attempt.
                //
                // Exhaustion is ambiguous: `cl_swap` reports it BOTH when the
                // pool runs out of liquidity and when our ladder simply did not
                // reach far enough. Rejecting on it alone threw away hops the
                // pool can genuinely fill — measured: 641,933 PLAY against a
                // pool holding 6,115,660 (0.1x) and 48,615 AERO against 244,323
                // (0.2x) were both rejected as "cannot fill", and this was 100%
                // of `plan_build_failed`. Widening the ladder made it WORSE
                // (34% -> 48% of rejections) because more hops then reach the
                // multi-tick path at all.
                //
                // Disambiguate against the pool's REAL input-side balance, the
                // same ground truth that sets `max_input`. If the size is within
                // the fraction of depth we already deem tradeable, our ladder
                // was the limit, not the pool: fall through to the single-tick
                // estimate, which carries `cl_tick_buffer_bps`. If the balance
                // is unknown, FAIL CLOSED and reject — never invent depth.
                let balance_in = if zero_for_one {
                    state.balance0
                } else {
                    state.balance1
                };
                let within_real_depth = balance_in
                    .filter(|b| !b.is_zero())
                    .is_some_and(|b| amount_in <= b / 3);
                tracing::debug!(
                    target: "minout",
                    ticks_crossed = quote.ticks_crossed,
                    ladder_len = ladder.len(),
                    amount_in = %amount_in,
                    partial_out = %quote.amount_out,
                    balance_in = ?balance_in,
                    within_real_depth,
                    "multi-tick exhausted"
                );
                if !within_real_depth {
                    return None;
                }
                // else: fall through to single-tick below.
            }
        }
    }
    let out = crate::cl_sim::quote_exact_input_single_tick(
        state,
        amount_in,
        zero_for_one,
        state.fee_ppm,
    )
    .ok()??;
    if out.is_zero() {
        return None;
    }
    Some((out, false))
}

/// Expected output for one hop, or `None` when the hop is genuinely unpriceable.
///
/// `None` is not "we failed to compute" — it is "this pool cannot fill this
/// size". The distinction matters because every fallback estimate available
/// here is MORE optimistic than the model that refused: the secant lies above
/// the convex curve, and the single-tick model assumes constant (infinite)
/// liquidity. Substituting either one for a refusal manufactures an edge that
/// cannot execute, which is precisely how a pool holding 0.154 bsdETH came to
/// be quoted at 1.904.
fn hop_expected_out(edge: &Edge, from: Address, current_amount: U256) -> Option<U256> {
    let linear = mul_div(current_amount, edge.rate_num, edge.rate_den);
    match &edge.venue {
        // Concentrated-liquidity hops carry their pool state on the edge, so the
        // true curve output is computable locally — the same `cl_sim` call the
        // size search already uses to price these hops.
        //
        // These kept the LINEAR estimate while UniV2/Solidly moved to real curve
        // math, and that asymmetry is what made every CL cycle revert. A secant
        // lies above a convex curve, so the linear rate OVERSTATES output; the
        // resulting `min_out` floor sits above what the pool can actually pay and
        // the swap reverts with `Too little received`. Measured on Base: 50/50
        // candidates, all `venue_path: [univ3, slipstream]`, reverted on exactly
        // this. Sizing said the cycle was profitable because sizing quoted the
        // real curve; only the plan disagreed.
        //
        // Mirrors sizing's guard: `state` describes ONE pool, so this is valid
        // only for a single-pool hop. Multi-hop encoded paths keep the linear
        // estimate and remain covered by pre-broadcast simulation.
        VenueEdge::UniV3 {
            path,
            state: Some(cl_state),
            ..
        }
        | VenueEdge::Slipstream {
            path,
            state: Some(cl_state),
            ..
        } if crate::cl_sim::local_cl_quotes_enabled() && path.len() <= 2 => {
            // UniV3-family pools order tokens by address, so `from` is token0
            // exactly when it sorts below `to`.
            let zero_for_one = edge.from < edge.to;
            match cl_hop_out(cl_state, edge.tick_ladder.as_deref(), current_amount, zero_for_one) {
                // Multi-tick modelled the crossing, so the tick buffer would
                // double-count it and give up real edge. Use the quote as-is.
                Some((out, true)) => {
                    // Crossing is modelled, so `cl_tick_buffer_bps` would
                    // double-count. The residual gap to the ROUTER is not
                    // modelled — measured at ~31 bps — so apply the smaller
                    // execution buffer. Without it `min_out` lands above what
                    // the router pays and every hop reverts.
                    let discounted = crate::util::apply_slippage(out, cl_exec_buffer_bps());
                    tracing::debug!(
                        target: "minout",
                        curve_out = %out,
                        linear_out = %linear,
                        discounted = %discounted,
                        exec_buffer_bps = cl_exec_buffer_bps(),
                        "CL hop priced multi-tick"
                    );
                    Some(discounted)
                }
                // Single-tick fallback: liquidity was held constant, so the
                // estimate is systematically optimistic and the buffer still
                // covers the unmodelled crossing.
                Some((out, false)) => {
                    let discounted = crate::util::apply_slippage(out, cl_tick_buffer_bps());
                    tracing::debug!(
                        target: "minout",
                        curve_out = %out,
                        linear_out = %linear,
                        discounted = %discounted,
                        buffer_bps = cl_tick_buffer_bps(),
                        "CL hop priced single-tick with crossing buffer"
                    );
                    Some(discounted)
                }
                // `cl_hop_out` refused. Do NOT substitute `linear`: the secant
                // lies above the convex curve, so it is the most optimistic
                // estimate in the file and would resurrect exactly the phantom
                // edge that was just rejected. Propagate the refusal and let
                // the caller drop the cycle.
                None => None,
            }
        }
        VenueEdge::UniV2 {
            token0,
            token1,
            reserve_in,
            reserve_out,
            fee_bps,
            ..
        } => {
            let (reserve0, reserve1) = if from == *token0 {
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
            match quote_univ2_out(&state, from, current_amount, *fee_bps) {
                Ok(Some(quote)) => Some(quote.amount_out),
                _ => Some(linear),
            }
        }
        VenueEdge::SolidlyV2 {
            token0,
            token1,
            stable,
            reserve_in,
            reserve_out,
            fee_bps,
            decimals0,
            decimals1,
            ..
        } => {
            let (reserve0, reserve1) = if from == *token0 {
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
            match quote_solidly_out(&state, from, current_amount, *fee_bps) {
                Ok(Some(quote)) => Some(quote.amount_out),
                _ => Some(linear),
            }
        }
        _ => Some(linear),
    }
}

#[derive(Clone)]
pub enum GenericPreAction {
    Approve { token: Address, amount: U256 },
    Transfer { token: Address, amount: U256 },
}

#[derive(Clone)]
pub enum StepData {
    Uniswap {
        path: Bytes,
        amount_in: U256,
        min_out: U256,
    },
    JitLiquidityAdd {
        pool: Address,
        token0: Address,
        token1: Address,
        amount0: U256,
        amount1: U256,
        tick_range: u16,
    },
    JitLiquidityRemove {
        pool: Address,
        target_token: Address,
        fee: u32,
        min_out: U256,
    },
    Balancer {
        pool_id: [u8; 32],
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        min_out: U256,
    },
    Bridge {
        adapter: Address,
        token_in: Address,
        amount_in: U256,
        dst_chain_id: u64,
        max_bridge_time_secs: u64,
        call: Bytes,
    },
    Generic {
        target: Address,
        call: Bytes,
        pre_action: Option<GenericPreAction>,
    },
}

pub struct Plan {
    pub steps: Vec<StepData>,
    pub cycle_slippage_bps: u32,
}

impl Plan {
    /// Copy of this plan with every per-hop `min_out` floor removed.
    ///
    /// Diagnostic only, and NEVER for dispatch: a plan with no floor has no
    /// slippage protection and would execute at any price. Its purpose is to
    /// answer the one question a `Too little received` revert refuses to —
    /// what the pools ACTUALLY pay. Simulating this variant succeeds where the
    /// real plan reverts, so the achieved output can be compared against the
    /// demanded floor and the gap measured instead of guessed.
    ///
    /// Floors are set to 1 rather than 0 because several venue adapters treat a
    /// zero `min_out` as "unset" and reject the step.
    #[allow(dead_code)]
    pub fn with_relaxed_min_outs(&self) -> Plan {
        let one = U256::one();
        let steps = self
            .steps
            .iter()
            .cloned()
            .map(|step| match step {
                StepData::Uniswap {
                    path, amount_in, ..
                } => StepData::Uniswap {
                    path,
                    amount_in,
                    min_out: one,
                },
                StepData::JitLiquidityRemove {
                    pool,
                    target_token,
                    fee,
                    ..
                } => StepData::JitLiquidityRemove {
                    pool,
                    target_token,
                    fee,
                    min_out: one,
                },
                StepData::Balancer {
                    pool_id,
                    token_in,
                    token_out,
                    amount_in,
                    ..
                } => StepData::Balancer {
                    pool_id,
                    token_in,
                    token_out,
                    amount_in,
                    min_out: one,
                },
                other => other,
            })
            .collect();
        Plan {
            steps,
            cycle_slippage_bps: self.cycle_slippage_bps,
        }
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct JitConfig {
    pub enabled: bool,
    pub min_amount_in: U256,
    pub seed_bps: u32,
    pub tick_range: u16,
    pub disable_on_quote_failure: bool,
}

const UNIV2_SWAP_SELECTOR: [u8; 4] = [0x02, 0x2c, 0x0d, 0x9f];
const UNIV4_SWAP_SIGNATURE: &str =
    "swap((address,address,uint24,int24,address),(bool,int256,uint160),bytes)";

#[async_trait]
#[allow(dead_code)]
pub trait JitQuoter: Send + Sync {
    async fn quote_path(
        &self,
        path: Vec<(Address, Option<u32>)>,
        amount_in: U256,
        block: U64,
    ) -> Result<U256>;
}

#[async_trait]
impl<C> JitQuoter for UniQuoter<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    async fn quote_path(
        &self,
        path: Vec<(Address, Option<u32>)>,
        amount_in: U256,
        block: U64,
    ) -> Result<U256> {
        self.quote_path(path, amount_in, block).await
    }
}

/// `edge_indices` names the EXACT edge for each hop and is not advisory.
///
/// This used to re-resolve every hop with `graph.edge_between(from, to)`, which
/// returns whichever edge the graph currently prefers for that token pair. With
/// parallel venues on one pair -- the case this bot exists to exploit -- the
/// detector can price WETH->USDC on Aerodrome while the planner builds calldata
/// for Uniswap. Pool, fee, router and liquidity then describe a different trade
/// than the one that was sized, and the identity the search had already
/// computed was discarded one stage before it mattered.
///
/// A mismatch is a hard error. Emitting a plan for a route nobody priced is
/// worse than emitting nothing: it reverts after paying gas, or it fills at a
/// price that was never checked.
///
/// The argument count is over clippy's limit and stays that way deliberately:
/// bundling these into a struct is the right end state, but it belongs with the
/// `RouteSnapshot` work that carries one immutable route object through quote,
/// size, plan, simulate and submit -- not as a drive-by rename here.
#[allow(clippy::too_many_arguments)]
pub async fn build_plan_for_cycle(
    graph: &Graph,
    cycle: &[usize],
    edge_indices: &[usize],
    amount_in: U256,
    executor_address: Address,
    jit: Option<&JitConfig>,
    jit_quoter: Option<&dyn JitQuoter>,
    block_number: U64,
) -> Result<Plan> {
    let _ = (jit_quoter, block_number);
    if edge_indices.len() != cycle.len().saturating_sub(1) {
        return Err(anyhow!(
            "edge count {} does not match cycle of {} hops",
            edge_indices.len(),
            cycle.len().saturating_sub(1)
        ));
    }
    // PlanV2 execution currently supports a single flash loan. The planner must enforce the
    // single-loan invariant before building executor loan data.
    let mut steps = Vec::new();
    let mut current_amount = amount_in;
    let mut max_slippage_bps = 0u32;
    for (hop, window) in cycle.windows(2).enumerate() {
        let from = graph.nodes[window[0]];
        let to = graph.nodes[window[1]];
        let Some(edge) = graph
            .edge_by_index(edge_indices[hop])
            .filter(|edge| edge.active && edge.from == from && edge.to == to)
        else {
            warn!(
                hop,
                edge_index = edge_indices[hop],
                from = %from,
                to = %to,
                "plan build refused: edge index does not name this hop"
            );
            return Err(anyhow!(
                "cycle edge mismatch at hop {hop}: {from:?}->{to:?} (edge index {})",
                edge_indices[hop]
            ));
        };
        if matches!(
            &edge.venue,
            VenueEdge::UniV3 { state: None, .. } | VenueEdge::Slipstream { state: None, .. }
        ) {
            tracing::debug!(target: "minout", "CL hop has NO pool state; using linear estimate");
        }
        // A refusal here means the pool cannot fill this size. That is a real
        // answer and the cycle must die on it — the alternative is emitting a
        // plan whose `min_out` the pool provably cannot pay, which reverts
        // `Too little received` after burning gas draining empty ticks.
        let Some(expected_out) = hop_expected_out(edge, from, current_amount) else {
            return Err(anyhow!(
                "hop {from:?}->{to:?} is unpriceable: pool cannot fill {current_amount}"
            ));
        };
        let min_out = apply_slippage(expected_out, edge.tolerance_bps);
        if min_out.is_zero() {
            return Err(anyhow!("min_out is zero for hop {from:?}->{to:?}"));
        }
        max_slippage_bps = max_slippage_bps.max(edge.observed_slippage_bps);
        match &edge.venue {
            VenueEdge::UniV3 { path, pool, fee, .. } => {
                if let Some(cfg) = jit {
                    if cfg.enabled && current_amount >= cfg.min_amount_in && path.len() == 2 {
                        let token_in = path[0].0;
                        let token_out = path[1].0;
                        let seed_total = mul_div(
                            current_amount,
                            U256::from(cfg.seed_bps),
                            U256::from(10_000u64),
                        );
                        if seed_total > U256::zero() && seed_total < current_amount {
                            let pre_swap_in = seed_total / U256::from(2u64);
                            let liquidity_in = seed_total.saturating_sub(pre_swap_in);
                            let pre_swap_expected_out =
                                mul_div(pre_swap_in, edge.rate_num, edge.rate_den);
                            let pre_swap_min_out =
                                apply_slippage(pre_swap_expected_out, edge.tolerance_bps);
                            if pre_swap_min_out.is_zero() {
                                return Err(anyhow!(
                                    "jit pre-swap min_out is zero for hop {from:?}->{to:?}"
                                ));
                            }
                            max_slippage_bps = max_slippage_bps.max(edge.observed_slippage_bps);
                            let bytes = encode_univ3_path(path)
                                .context("encode jit univ3 pre-swap path")?;
                            steps.push(StepData::Uniswap {
                                path: Bytes::from(bytes.clone()),
                                amount_in: pre_swap_in,
                                min_out: pre_swap_min_out,
                            });

                            let (token0, token1) = if token_in < token_out {
                                (token_in, token_out)
                            } else {
                                (token_out, token_in)
                            };
                            let (amount0, amount1) = if token0 == token_in {
                                (liquidity_in, pre_swap_min_out)
                            } else {
                                (pre_swap_min_out, liquidity_in)
                            };

                            steps.push(StepData::JitLiquidityAdd {
                                pool: *pool,
                                token0,
                                token1,
                                amount0,
                                amount1,
                                tick_range: cfg.tick_range,
                            });

                            let remove_expected_out =
                                mul_div(liquidity_in, edge.rate_num, edge.rate_den);
                            let remove_min_out =
                                apply_slippage(remove_expected_out, edge.tolerance_bps);
                            if remove_min_out.is_zero() {
                                return Err(anyhow!(
                                    "jit remove min_out is zero for hop {from:?}->{to:?}"
                                ));
                            }

                            let main_in = current_amount.saturating_sub(seed_total);
                            let expected_main_out = mul_div(main_in, edge.rate_num, edge.rate_den);
                            let main_min_out =
                                apply_slippage(expected_main_out, edge.tolerance_bps);
                            if main_min_out.is_zero() {
                                return Err(anyhow!(
                                    "jit main min_out is zero for hop {from:?}->{to:?}"
                                ));
                            }
                            steps.push(StepData::Uniswap {
                                path: Bytes::from(bytes),
                                amount_in: main_in,
                                min_out: main_min_out,
                            });

                            steps.push(StepData::JitLiquidityRemove {
                                pool: *pool,
                                target_token: token_out,
                                fee: *fee,
                                min_out: remove_min_out,
                            });

                            let extra_out =
                                pre_swap_expected_out.saturating_add(remove_expected_out);
                            current_amount = expected_main_out.saturating_add(extra_out);
                            continue;
                        }
                    }
                }

                let bytes = encode_univ3_path(path).context("encode univ3 path")?;
                steps.push(StepData::Uniswap {
                    path: Bytes::from(bytes),
                    amount_in: current_amount,
                    min_out,
                });
            }
            VenueEdge::Slipstream {
                path,
                router,
                ..
            } => {
                let bytes = encode_univ3_path(path).context("encode slipstream path")?;
                // Aerodrome Slipstream's router is a UniV3 SwapRouter (v1) fork:
                //
                //   exactInput((bytes,address,uint256,uint256,uint256))  0xc04b8d59
                //   struct ExactInputParams { path, recipient, deadline, amountIn, amountOutMinimum }
                //
                // The previous encoding was wrong twice over. It emitted
                // 0xc04b8d70 — not a real selector, a transposition of
                // 0xc04b8d59 — and it packed only FOUR fields (SwapRouter02's
                // deadline-less shape, which is 0xb858183f). Verified against the
                // deployed router's bytecode: 0xc04b8d59 is present; neither
                // 0xc04b8d70 nor 0xb858183f is.
                //
                // An unknown selector matches no function, so the router reverted
                // with EMPTY return data, which the executor surfaces as the
                // catch-all `InvalidGenericAction()`. Every Slipstream hop failed
                // this way, which is why no cycle touching Slipstream could ever
                // execute regardless of price.
                const EXACT_INPUT: [u8; 4] = [0xc0, 0x4b, 0x8d, 0x59];
                // Deadline is the router's own staleness guard. `U256::MAX`
                // disables it deliberately: the executor already enforces a
                // plan-level deadline in `_executePlan`, and a second, tighter
                // one derived here — with no access to block.timestamp — could
                // only reject otherwise-valid plans.
                let deadline = U256::MAX;
                let mut data = Vec::with_capacity(EXACT_INPUT.len() + 32 * 6);
                data.extend_from_slice(&EXACT_INPUT);
                data.extend(ethers::abi::encode(&[ethers::abi::Token::Tuple(vec![
                    ethers::abi::Token::Bytes(bytes),
                    ethers::abi::Token::Address(executor_address),
                    ethers::abi::Token::Uint(deadline),
                    ethers::abi::Token::Uint(current_amount),
                    ethers::abi::Token::Uint(min_out),
                ])]));
                steps.push(StepData::Generic {
                    target: *router,
                    call: Bytes::from(data),
                    pre_action: Some(GenericPreAction::Approve {
                        token: from,
                        amount: current_amount,
                    }),
                });
            }
            VenueEdge::Balancer {
                pool_id,
                token_in,
                token_out,
            } => {
                steps.push(StepData::Balancer {
                    pool_id: *pool_id,
                    token_in: *token_in,
                    token_out: *token_out,
                    amount_in: current_amount,
                    min_out,
                });
            }
            VenueEdge::Curve {
                pool,
                selector,
                i,
                j,
            } => {
                let mut data = Vec::with_capacity(selector.len() + 32 * 4);
                data.extend_from_slice(selector);
                data.extend(ethers::abi::encode(&[
                    ethers::abi::Token::Int((*i).into()),
                    ethers::abi::Token::Int((*j).into()),
                    ethers::abi::Token::Uint(current_amount),
                    ethers::abi::Token::Uint(min_out),
                ]));
                steps.push(StepData::Generic {
                    target: *pool,
                    call: Bytes::from(data),
                    pre_action: Some(GenericPreAction::Approve {
                        token: from,
                        amount: current_amount,
                    }),
                });
            }
            VenueEdge::UniV2 { pair, token0, .. } => {
                let (amount0_out, amount1_out) = if from == *token0 {
                    (U256::zero(), expected_out)
                } else {
                    (expected_out, U256::zero())
                };
                let mut data = Vec::with_capacity(UNIV2_SWAP_SELECTOR.len() + 32 * 4);
                data.extend_from_slice(&UNIV2_SWAP_SELECTOR);
                data.extend(ethers::abi::encode(&[
                    ethers::abi::Token::Uint(amount0_out),
                    ethers::abi::Token::Uint(amount1_out),
                    ethers::abi::Token::Address(executor_address),
                    ethers::abi::Token::Bytes(Vec::new()),
                ]));
                steps.push(StepData::Generic {
                    target: *pair,
                    call: Bytes::from(data),
                    pre_action: Some(GenericPreAction::Transfer {
                        token: from,
                        amount: current_amount,
                    }),
                });
            }
            VenueEdge::SolidlyV2 { pair, token0, .. } => {
                let (amount0_out, amount1_out) = if from == *token0 {
                    (U256::zero(), expected_out)
                } else {
                    (expected_out, U256::zero())
                };
                let mut data = Vec::with_capacity(UNIV2_SWAP_SELECTOR.len() + 32 * 4);
                data.extend_from_slice(&UNIV2_SWAP_SELECTOR);
                data.extend(ethers::abi::encode(&[
                    ethers::abi::Token::Uint(amount0_out),
                    ethers::abi::Token::Uint(amount1_out),
                    ethers::abi::Token::Address(executor_address),
                    ethers::abi::Token::Bytes(Vec::new()),
                ]));
                steps.push(StepData::Generic {
                    target: *pair,
                    call: Bytes::from(data),
                    pre_action: Some(GenericPreAction::Transfer {
                        token: from,
                        amount: current_amount,
                    }),
                });
            }
            VenueEdge::Univ4 {
                pool_manager,
                token0,
                token1,
                fee,
                tick_spacing,
                hooks,
                ..
            } => {
                let selector = ethers::utils::id(UNIV4_SWAP_SIGNATURE);
                let zero_for_one = from == *token0;
                let pool_key = ethers::abi::Token::Tuple(vec![
                    ethers::abi::Token::Address(*token0),
                    ethers::abi::Token::Address(*token1),
                    ethers::abi::Token::Uint(U256::from(*fee)),
                    ethers::abi::Token::Int((*tick_spacing).into()),
                    ethers::abi::Token::Address(*hooks),
                ]);
                let swap_params = ethers::abi::Token::Tuple(vec![
                    ethers::abi::Token::Bool(zero_for_one),
                    ethers::abi::Token::Int(current_amount),
                    ethers::abi::Token::Uint(U256::zero()),
                ]);
                let mut data = Vec::with_capacity(4 + 32 * 7);
                data.extend_from_slice(&selector[0..4]);
                data.extend(ethers::abi::encode(&[
                    pool_key,
                    swap_params,
                    ethers::abi::Token::Bytes(Vec::new()),
                ]));
                steps.push(StepData::Generic {
                    target: *pool_manager,
                    call: Bytes::from(data),
                    pre_action: Some(GenericPreAction::Approve {
                        token: from,
                        amount: current_amount,
                    }),
                });
            }
            VenueEdge::Bridge {
                router,
                token_in,
                token_out,
                dst_chain_id,
                selector,
                bridge_name,
                max_bridge_time_secs,
                estimated_time_secs,
                fee_bps,
                liquidity_limit,
            } => {
                info!(
                    bridge = %bridge_name,
                    dst_chain = dst_chain_id,
                    fee_bps = fee_bps,
                    eta_secs = estimated_time_secs,
                    liquidity_cap_wei = %liquidity_limit,
                    "Encoding bridge step"
                );
                let mut data = Vec::with_capacity(selector.len() + 32 * 7);
                data.extend_from_slice(selector);
                data.extend(ethers::abi::encode(&[
                    ethers::abi::Token::Address(*token_in),
                    ethers::abi::Token::Address(*token_out),
                    ethers::abi::Token::Uint(U256::from(*dst_chain_id)),
                    ethers::abi::Token::Uint(current_amount),
                    ethers::abi::Token::Uint(min_out),
                    ethers::abi::Token::Uint(U256::from(*max_bridge_time_secs)),
                    ethers::abi::Token::Address(executor_address),
                ]));
                steps.push(StepData::Bridge {
                    adapter: *router,
                    token_in: *token_in,
                    amount_in: current_amount,
                    dst_chain_id: *dst_chain_id,
                    max_bridge_time_secs: *max_bridge_time_secs,
                    call: Bytes::from(data),
                });
            }
            VenueEdge::Liquidation {
                adapter,
                selector,
                flash_loan_pool,
                debt_token,
                collateral_token,
                user,
                receive_atoken,
                protocol,
            } => {
                info!(
                    protocol = %protocol,
                    debt = %format!("{:#x}", debt_token),
                    collateral = %format!("{:#x}", collateral_token),
                    "Encoding liquidation step"
                );
                let mut data = Vec::with_capacity(selector.len() + 32 * 7);
                data.extend_from_slice(selector);
                data.extend(ethers::abi::encode(&[
                    ethers::abi::Token::Address(*flash_loan_pool),
                    ethers::abi::Token::Address(*debt_token),
                    ethers::abi::Token::Address(*collateral_token),
                    ethers::abi::Token::Address(*user),
                    ethers::abi::Token::Uint(current_amount),
                    ethers::abi::Token::Uint(min_out),
                    ethers::abi::Token::Bool(*receive_atoken),
                ]));
                steps.push(StepData::Generic {
                    target: *adapter,
                    call: Bytes::from(data),
                    pre_action: Some(GenericPreAction::Approve {
                        token: *debt_token,
                        amount: current_amount,
                    }),
                });
            }
        }
        // Per-hop record of exactly what floor this plan demands, and from what.
        // A `Too little received` revert names no amounts, so without this the
        // only way to reason about a failure is to guess which hop was too
        // tight. Pair it with `Plan::with_relaxed_min_outs` to get the achieved
        // output and turn the guess into a measurement.
        tracing::debug!(
            target: "minout",
            hop = steps.len().saturating_sub(1),
            venue = venue_kind_label(&edge.venue),
            pool = ?crate::venues::edge_pool_address(edge),
            token_in = %format!("{from:#x}"),
            token_out = %format!("{to:#x}"),
            amount_in = %current_amount,
            expected_out = %expected_out,
            min_out = %min_out,
            tolerance_bps = edge.tolerance_bps,
            haircut_bps = %bps_below(expected_out, min_out),
            "plan hop min_out"
        );
        current_amount = min_out;
    }

    Ok(Plan {
        steps,
        cycle_slippage_bps: max_slippage_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Graph, VenueEdge};
    use crate::util::compute_edge_weight;
    use ethers::abi::{decode, ParamType, Token};
    use ethers::types::Address;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    /// The `Too little received` bug: a linear rate is a SECANT, and a secant
    /// lies above a convex AMM curve. Deriving `min_out` from it sets a floor the
    /// pool cannot pay, so the swap reverts on-chain even though sizing — which
    /// quotes the real curve — called the cycle profitable.
    #[test]
    fn cl_hop_expected_out_uses_the_curve_not_the_secant() {
        let _guard = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_LOCAL_CL_QUOTES", "1");
        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: U256::from(1u64) << 96, // price = 1
            liquidity: 1_000_000_000_000_000u128,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3000,
            ..Default::default()
        };
        let state_for_raw = state.clone();
        // rate_num/rate_den = 1:1 => the linear estimate ignores both fee and
        // price impact, so it is strictly optimistic.
        let edge = Edge {
            from: addr(1),
            to: addr(2),
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(addr(1), Some(3000))],
                pool: addr(99),
                fee: 3000,
                state: Some(state),
            },
            estimated_gas: 0,
            weight: compute_edge_weight(U256::from(1u64), U256::from(1u64)),
            max_input: U256::zero(),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        };

        let amount_in = U256::from(1_000_000_000u64);
        let linear = mul_div(amount_in, edge.rate_num, edge.rate_den);
        let actual = hop_expected_out(&edge, addr(1), amount_in).expect("hop must be priceable");

        assert!(!actual.is_zero(), "curve quote must resolve");
        assert!(
            actual < linear,
            "curve output {actual} must be BELOW the linear secant {linear};              a floor above the curve is what reverts on-chain"
        );

        // And below the raw single-tick quote too: that quote holds liquidity
        // constant, so it over-reads any swap that crosses a tick.
        let raw = crate::cl_sim::quote_exact_input_single_tick(
            &state_for_raw,
            amount_in,
            true,
            state_for_raw.fee_ppm,
        )
        .unwrap()
        .unwrap();
        assert!(
            actual < raw,
            "min_out basis {actual} must sit under the un-buffered single-tick quote {raw}"
        );
    }

    /// The relaxed plan exists to measure what pools actually pay when the real
    /// plan reverts. It is diagnostic ONLY — it carries no slippage protection,
    /// so the invariant that matters is that it differs from the dispatchable
    /// plan in exactly one way: the floors are gone.
    /// A wrong selector reverts with EMPTY data, which the executor reports as
    /// the catch-all `InvalidGenericAction()` — indistinguishable from a dozen
    /// other faults. Pin the exact bytes and the exact arity so this can never
    /// silently regress into that black hole again.
    #[tokio::test]
    async fn slipstream_step_targets_the_routers_real_exact_input() {
        let mut graph = Graph::default();
        let a = addr(1);
        let b = addr(2);
        let router = addr(77);
        let ai = graph.add_node(a);
        let bi = graph.add_node(b);
        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(101u64),
            rate_den: U256::from(100u64),
            venue: VenueEdge::Slipstream {
                path: vec![(a, Some(100))],
                pool: addr(88),
                tick_spacing: 100,
                router,
                state: None,
            },
            estimated_gas: 0,
            weight: fp_weight(101, 100),
            max_input: U256::zero(),
            tolerance_bps: 50,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });
        graph.add_edge(Edge {
            from: b,
            to: a,
            rate_num: U256::from(101u64),
            rate_den: U256::from(100u64),
            venue: VenueEdge::UniV2 {
                pair: addr(99),
                token_out: a,
                token0: a,
                token1: b,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: fp_weight(101, 100),
            max_input: U256::zero(),
            tolerance_bps: 50,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        let plan = build_plan_for_cycle(
            &graph,
            &[ai, bi, ai],
            &graph.best_edge_indices_for_node_path(&[ai, bi, ai]).expect("edges for test cycle"),
            U256::from(1_000u64),
            addr(5),
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan builds");

        let generic = plan
            .steps
            .iter()
            .find_map(|s| match s {
                StepData::Generic { target, call, .. } if *target == router => Some(call.clone()),
                _ => None,
            })
            .expect("slipstream emits a generic router call");

        // exactInput((bytes,address,uint256,uint256,uint256)) on the deployed
        // Slipstream router. NOT 0xc04b8d70 (nonexistent) and NOT 0xb858183f
        // (SwapRouter02's deadline-less 4-field form).
        assert_eq!(&generic[..4], &[0xc0, 0x4b, 0x8d, 0x59], "wrong selector");

        // Five fields: head offset + recipient + deadline + amountIn + minOut,
        // then the dynamic `path` tail. A 4-field body would be 32 bytes shorter.
        let body = &generic[4..];
        let decoded = ethers::abi::decode(
            &[ethers::abi::ParamType::Tuple(vec![
                ethers::abi::ParamType::Bytes,
                ethers::abi::ParamType::Address,
                ethers::abi::ParamType::Uint(256),
                ethers::abi::ParamType::Uint(256),
                ethers::abi::ParamType::Uint(256),
            ])],
            body,
        )
        .expect("params decode as the router's 5-field tuple");
        let ethers::abi::Token::Tuple(fields) = &decoded[0] else {
            panic!("expected tuple");
        };
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[1], ethers::abi::Token::Address(addr(5)), "recipient");
        assert_eq!(
            fields[2],
            ethers::abi::Token::Uint(U256::MAX),
            "deadline field must be present and third"
        );
        assert_eq!(
            fields[3],
            ethers::abi::Token::Uint(U256::from(1_000u64)),
            "amountIn"
        );
    }

    #[test]
    fn relaxed_plan_strips_every_min_out_floor() {
        let plan = Plan {
            steps: vec![
                StepData::Uniswap {
                    path: Bytes::from(vec![1u8, 2, 3]),
                    amount_in: U256::from(1_000u64),
                    min_out: U256::from(990u64),
                },
                StepData::Balancer {
                    pool_id: [7u8; 32],
                    token_in: addr(1),
                    token_out: addr(2),
                    amount_in: U256::from(500u64),
                    min_out: U256::from(495u64),
                },
            ],
            cycle_slippage_bps: 50,
        };
        let relaxed = plan.with_relaxed_min_outs();

        assert_eq!(relaxed.steps.len(), plan.steps.len(), "no step may be lost");
        assert_eq!(
            relaxed.cycle_slippage_bps, plan.cycle_slippage_bps,
            "only per-hop floors change"
        );
        for step in &relaxed.steps {
            match step {
                // 1, not 0: several adapters reject a zero min_out as "unset".
                StepData::Uniswap { min_out, amount_in, .. } => {
                    assert_eq!(*min_out, U256::one());
                    assert_eq!(*amount_in, U256::from(1_000u64), "amounts preserved");
                }
                StepData::Balancer { min_out, amount_in, .. } => {
                    assert_eq!(*min_out, U256::one());
                    assert_eq!(*amount_in, U256::from(500u64), "amounts preserved");
                }
                _ => panic!("unexpected step kind"),
            }
        }
    }

    #[test]
    fn bps_below_measures_the_haircut() {
        assert_eq!(bps_below(U256::from(10_000u64), U256::from(9_900u64)), 100);
        assert_eq!(bps_below(U256::from(10_000u64), U256::from(10_000u64)), 0);
        // A floor ABOVE expected is the reverting case; report 0 rather than
        // underflowing into a nonsense value.
        assert_eq!(bps_below(U256::from(10_000u64), U256::from(10_500u64)), 0);
        assert_eq!(bps_below(U256::zero(), U256::from(1u64)), 0);
    }

    #[test]
    fn cl_hop_falls_back_to_linear_without_pool_state() {
        // No `state` => the curve is not computable locally, so the linear
        // estimate stands and pre-broadcast simulation remains the guard.
        let edge = Edge {
            from: addr(1),
            to: addr(2),
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(addr(1), Some(3000))],
                pool: addr(99),
                fee: 3000,
                state: None,
            },
            estimated_gas: 0,
            weight: compute_edge_weight(U256::from(2u64), U256::from(1u64)),
            max_input: U256::zero(),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        };
        let amount_in = U256::from(1_000u64);
        assert_eq!(
            hop_expected_out(&edge, addr(1), amount_in),
            Some(U256::from(2_000u64))
        );
    }

    fn fp_weight(num: u64, den: u64) -> i64 {
        compute_edge_weight(U256::from(num), U256::from(den))
    }

    #[tokio::test]
    async fn builds_plan_with_progressive_amounts() {
        let mut graph = Graph::default();

        let a = addr(1);
        let b = addr(2);
        let c = addr(3);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [0u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
            tick_ladder: None,
        });
        graph.add_edge(Edge {
            from: b,
            to: c,
            rate_num: U256::from(3u64),
            rate_den: U256::from(2u64),
            venue: VenueEdge::Balancer {
                pool_id: [1u8; 32],
                token_in: b,
                token_out: c,
            },
            estimated_gas: 0,
            weight: fp_weight(3, 2),
            max_input: U256::from(2u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
            tick_ladder: None,
        });
        graph.add_edge(Edge {
            from: c,
            to: a,
            rate_num: U256::from(4u64),
            rate_den: U256::from(3u64),
            venue: VenueEdge::Balancer {
                pool_id: [2u8; 32],
                token_in: c,
                token_out: a,
            },
            estimated_gas: 0,
            weight: fp_weight(4, 3),
            max_input: U256::from(3u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&a).unwrap(),
            *graph.ix.get(&b).unwrap(),
            *graph.ix.get(&c).unwrap(),
            *graph.ix.get(&a).unwrap(),
        ];

        let base_amount = U256::from(100u64);
        let executor = addr(99);
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"),
            base_amount,
            executor,
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan should build for balancer cycle");

        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.cycle_slippage_bps, 0);

        let first_expected = U256::from(200u64);
        let second_expected = U256::from(300u64);
        let third_expected = U256::from(400u64);

        match &plan.steps[0] {
            StepData::Balancer {
                amount_in,
                min_out,
                token_in,
                token_out,
                ..
            } => {
                assert_eq!(*amount_in, base_amount);
                assert_eq!(*min_out, first_expected);
                assert_eq!(*token_in, a);
                assert_eq!(*token_out, b);
            }
            _ => panic!("expected balancer step"),
        }

        match &plan.steps[1] {
            StepData::Balancer {
                amount_in, min_out, ..
            } => {
                assert_eq!(*amount_in, first_expected);
                assert_eq!(*min_out, second_expected);
            }
            _ => panic!("expected balancer step"),
        }

        match &plan.steps[2] {
            StepData::Balancer {
                amount_in, min_out, ..
            } => {
                assert_eq!(*amount_in, second_expected);
                assert_eq!(*min_out, third_expected);
            }
            _ => panic!("expected balancer step"),
        }
    }

    #[tokio::test]
    async fn univ2_step_uses_expected_out() {
        let mut graph = Graph::default();
        let token_in = addr(10);
        let token_out = addr(11);
        let pair = addr(12);

        graph.add_edge(Edge {
            from: token_in,
            to: token_out,
            rate_num: U256::from(1_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::UniV2 {
                pair,
                token_out,
                token0: token_in,
                token1: token_out,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(1_000u64),
            tolerance_bps: 100,
            observed_slippage_bps: 100,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });
        graph.add_edge(Edge {
            from: token_out,
            to: token_in,
            rate_num: U256::from(1_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::Balancer {
                pool_id: [1u8; 32],
                token_in: token_out,
                token_out: token_in,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&token_in).unwrap(),
            *graph.ix.get(&token_out).unwrap(),
            *graph.ix.get(&token_in).unwrap(),
        ];
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"),
            U256::from(1_000u64),
            addr(99),
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan should build");

        let StepData::Generic { call, .. } = &plan.steps[0] else {
            panic!("expected generic step");
        };
        let decoded = decode(
            &[
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Address,
                ParamType::Bytes,
            ],
            &call.0[4..],
        )
        .expect("decode univ2 swap");
        let amount0_out = decoded[0].clone().into_uint().unwrap();
        let amount1_out = decoded[1].clone().into_uint().unwrap();
        assert_eq!(amount0_out, U256::zero());
        // Real constant-product output for 1_000 in @ 30bps on 1e6/1e6 reserves.
        // (Was 1_000 under the old linear extrapolation of rate_num/rate_den.)
        assert_eq!(amount1_out, U256::from(996u64));
    }

    /// The plan must use the edge the SEARCH chose, not whichever edge the
    /// graph now prefers for that token pair.
    ///
    /// Two pools serve WETH->USDC here, which is the ordinary case on any chain
    /// worth trading and the case this bot exists to exploit. Re-resolving by
    /// (from, to) returns the graph's favourite, so the detector could price one
    /// pool while the planner emitted calldata for the other -- different pool,
    /// fee, reserves and router, and an economic calculation belonging to a
    /// trade nobody is about to make.
    #[tokio::test]
    async fn the_plan_uses_the_edge_the_search_chose_not_the_graphs_favourite() {
        let (token_in, token_out) = (addr(10), addr(11));
        let (cheap_pair, rich_pair) = (addr(20), addr(21));

        let leg = |pair, r_in: u64, r_out: u64, w| Edge {
            from: token_in,
            to: token_out,
            rate_num: U256::from(r_out),
            rate_den: U256::from(r_in),
            venue: VenueEdge::UniV2 {
                pair,
                token_out,
                token0: token_in,
                token1: token_out,
                reserve_in: U256::from(r_in),
                reserve_out: U256::from(r_out),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: w,
            max_input: U256::MAX,
            tolerance_bps: 100,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        };

        let mut graph = Graph::default();
        // Index 0: the pool the graph prefers on weight.
        graph.add_edge(leg(cheap_pair, 1_000_000, 1_010_000, fp_weight(1, 2)));
        // Index 1: a DIFFERENT pool on the same pair, the one we will name.
        graph.add_edge(leg(rich_pair, 5_000_000, 5_010_000, fp_weight(1, 1)));
        // Return leg, index 2.
        graph.add_edge(Edge {
            from: token_out,
            to: token_in,
            rate_num: U256::from(1_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::UniV2 {
                pair: addr(22),
                token_out: token_in,
                token0: token_out,
                token1: token_in,
                reserve_in: U256::from(9_000_000u64),
                reserve_out: U256::from(9_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::MAX,
            tolerance_bps: 100,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        let a = *graph.ix.get(&token_in).unwrap();
        let b = *graph.ix.get(&token_out).unwrap();
        let cycle = vec![a, b, a];

        // Name the SECOND pool. The graph's own preference is the first.
        let plan = build_plan_for_cycle(
            &graph, &cycle, &[1usize, 2usize],
            U256::from(1_000u64), addr(99), None, None, U64::zero(),
        )
        .await
        .expect("plan builds on the named edges");

        let pools: Vec<Address> = plan
            .steps
            .iter()
            .filter_map(|step| match step {
                // A UniV2 hop targets the PAIR directly, so the step names the
                // pool and the substitution would be visible right here.
                StepData::Generic { target, .. } => Some(*target),
                _ => None,
            })
            .collect();
        assert!(
            pools.contains(&rich_pair),
            "plan must use the named pool {rich_pair:?}, got {pools:?}"
        );
        assert!(
            !pools.contains(&cheap_pair),
            "plan used the graph's preferred pool instead of the one priced"
        );

        // And an index that does not serve this hop is a hard error, never a
        // silent substitution.
        let wrong = build_plan_for_cycle(
            &graph, &cycle, &[2usize, 1usize],
            U256::from(1_000u64), addr(99), None, None, U64::zero(),
        )
        .await;
        assert!(wrong.is_err(), "mismatched edge index must refuse to plan");

        // So is the wrong number of indices.
        let short = build_plan_for_cycle(
            &graph, &cycle, &[1usize],
            U256::from(1_000u64), addr(99), None, None, U64::zero(),
        )
        .await;
        assert!(short.is_err(), "edge count must match hop count");
    }

    #[tokio::test]
    async fn univ2_step_uses_real_curve_not_linear_extrapolation() {
        // Regression: for a trade size large vs. reserves, linear extrapolation of
        // the probe-size rate overstates output and makes the exact-output UniV2
        // swap revert on-chain. The plan must request the REAL constant-product
        // output instead.
        let mut graph = Graph::default();
        let token_in = addr(10);
        let token_out = addr(11);
        let pair = addr(12);

        // Probe-size rate stored on the edge is ~1:1 (rate_num==rate_den), so the
        // old linear path would request 100_000 out for 100_000 in.
        graph.add_edge(Edge {
            from: token_in,
            to: token_out,
            rate_num: U256::from(1_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::UniV2 {
                pair,
                token_out,
                token0: token_in,
                token1: token_out,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::MAX,
            tolerance_bps: 100,
            observed_slippage_bps: 100,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });
        graph.add_edge(Edge {
            from: token_out,
            to: token_in,
            rate_num: U256::from(1_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::Balancer {
                pool_id: [1u8; 32],
                token_in: token_out,
                token_out: token_in,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::MAX,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&token_in).unwrap(),
            *graph.ix.get(&token_out).unwrap(),
            *graph.ix.get(&token_in).unwrap(),
        ];
        let trade = U256::from(100_000u64);
        let plan = build_plan_for_cycle(&graph, &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"), trade, addr(99), None, None, U64::zero())
            .await
            .expect("plan should build");

        let StepData::Generic { call, .. } = &plan.steps[0] else {
            panic!("expected generic step");
        };
        let decoded = decode(
            &[
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Address,
                ParamType::Bytes,
            ],
            &call.0[4..],
        )
        .expect("decode univ2 swap");
        let amount1_out = decoded[1].clone().into_uint().unwrap();

        // Real constant-product output for 100_000 in @ 30bps on 1e6/1e6 reserves.
        let amount_in_with_fee = U256::from(100_000u64) * U256::from(9_970u64) / U256::from(10_000u64);
        let expected_real =
            amount_in_with_fee * U256::from(1_000_000u64) / (U256::from(1_000_000u64) + amount_in_with_fee);
        assert_eq!(amount1_out, expected_real, "must request real CPMM output");
        assert!(
            amount1_out < trade,
            "real output {amount1_out} must be below the linear extrapolation {trade}"
        );
    }

    #[tokio::test]
    async fn applies_slippage_to_minimum_outputs() {
        let mut graph = Graph::default();

        let a = addr(10);
        let b = addr(11);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [3u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 500,
            observed_slippage_bps: 100,
            quote_block: None,

            active: true,
            tick_ladder: None,
        });

        graph.add_edge(Edge {
            from: b,
            to: a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [4u8; 32],
                token_in: b,
                token_out: a,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&a).unwrap(),
            *graph.ix.get(&b).unwrap(),
            *graph.ix.get(&a).unwrap(),
        ];

        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"),
            U256::from(100u64),
            addr(42),
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan should build with slippage");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.cycle_slippage_bps, 100);

        match &plan.steps[0] {
            StepData::Balancer { min_out, .. } => {
                assert_eq!(*min_out, U256::from(190u64));
            }
            _ => panic!("expected balancer step"),
        }

        match &plan.steps[1] {
            StepData::Balancer {
                amount_in, min_out, ..
            } => {
                assert_eq!(*amount_in, U256::from(190u64));
                assert_eq!(*min_out, U256::from(190u64));
            }
            _ => panic!("expected balancer step"),
        }
    }

    #[tokio::test]
    async fn solidly_step_uses_expected_out() {
        let mut graph = Graph::default();
        let token_in = addr(20);
        let token_out = addr(21);
        let pair = addr(22);

        graph.add_edge(Edge {
            from: token_in,
            to: token_out,
            rate_num: U256::from(5_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::SolidlyV2 {
                pair,
                token_out,
                token0: token_in,
                token1: token_out,
                stable: false,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
                decimals0: 18,
                decimals1: 18,
            },
            estimated_gas: 0,
            weight: fp_weight(5, 1),
            max_input: U256::from(1_000u64),
            tolerance_bps: 100,
            observed_slippage_bps: 100,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });
        graph.add_edge(Edge {
            from: token_out,
            to: token_in,
            rate_num: U256::from(1_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::Balancer {
                pool_id: [2u8; 32],
                token_in: token_out,
                token_out: token_in,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&token_in).unwrap(),
            *graph.ix.get(&token_out).unwrap(),
            *graph.ix.get(&token_in).unwrap(),
        ];
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"),
            U256::from(1_000u64),
            addr(99),
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan should build for solidly cycle");

        let StepData::Generic { call, .. } = &plan.steps[0] else {
            panic!("expected solidly generic step");
        };
        let data = call.as_ref();
        assert_eq!(&data[..4], &UNIV2_SWAP_SELECTOR);
        let decoded = decode(
            &[
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Address,
                ParamType::Bytes,
            ],
            &data[4..],
        )
        .expect("decode");
        let amount0_out = decoded[0].clone().into_uint().expect("amount0");
        let amount1_out = decoded[1].clone().into_uint().expect("amount1");
        assert_eq!(amount0_out, U256::zero());
        // Real volatile (x*y=k) output for 1_000 in @ 30bps on 1e6/1e6 reserves.
        // (Was 5_000 under the old linear extrapolation of rate_num/rate_den.)
        assert_eq!(amount1_out, U256::from(996u64));
    }

    #[tokio::test]
    async fn univ4_step_encodes_swap() {
        let mut graph = Graph::default();
        let token0 = addr(30);
        let token1 = addr(31);
        let pool_manager = addr(32);
        let hooks = addr(33);

        graph.add_edge(Edge {
            from: token0,
            to: token1,
            rate_num: U256::from(2_000u64),
            rate_den: U256::from(1_000u64),
            venue: VenueEdge::Univ4 {
                pool_manager,
                token0,
                token1,
                fee: 300,
                tick_spacing: 60,
                hooks,
                sqrt_price_x96: U256::from(1u128) << 96,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });
        graph.add_edge(Edge {
            from: token1,
            to: token0,
            rate_num: U256::from(1_000u64),
            rate_den: U256::from(2_000u64),
            venue: VenueEdge::Balancer {
                pool_id: [0u8; 32],
                token_in: token1,
                token_out: token0,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 2),
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&token0).unwrap(),
            *graph.ix.get(&token1).unwrap(),
            *graph.ix.get(&token0).unwrap(),
        ];
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"),
            U256::from(500u64),
            addr(99),
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan should build for univ4 cycle");

        let StepData::Generic { call, .. } = &plan.steps[0] else {
            panic!("expected univ4 generic step");
        };
        let data = call.as_ref();
        let selector = ethers::utils::id(UNIV4_SWAP_SIGNATURE);
        assert_eq!(&data[..4], &selector[..4]);
        let decoded = decode(
            &[
                ParamType::Tuple(vec![
                    ParamType::Address,
                    ParamType::Address,
                    ParamType::Uint(24),
                    ParamType::Int(24),
                    ParamType::Address,
                ]),
                ParamType::Tuple(vec![
                    ParamType::Bool,
                    ParamType::Int(256),
                    ParamType::Uint(160),
                ]),
                ParamType::Bytes,
            ],
            &data[4..],
        )
        .expect("decode");
        let key = decoded[0].clone().into_tuple().expect("key tuple");
        assert_eq!(key[0].clone().into_address().unwrap(), token0);
        assert_eq!(key[1].clone().into_address().unwrap(), token1);
        assert_eq!(key[2].clone().into_uint().unwrap(), U256::from(300u64));
        assert_eq!(key[3].clone().into_int().unwrap(), U256::from(60u64));
        assert_eq!(key[4].clone().into_address().unwrap(), hooks);

        let params = decoded[1].clone().into_tuple().expect("params tuple");
        assert!(params[0].clone().into_bool().unwrap());
        assert_eq!(params[1].clone().into_int().unwrap(), U256::from(500u64));
        assert_eq!(params[2].clone().into_uint().unwrap(), U256::zero());
    }

    #[tokio::test]
    async fn adds_jit_steps_around_univ3_swap_when_enabled() {
        let mut graph = Graph::default();

        let a = addr(1);
        let b = addr(2);
        let pool = addr(555);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(a, None), (b, Some(3000))],
                pool,
                fee: 3000,
                state: None,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 100,
            observed_slippage_bps: 50,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        graph.add_edge(Edge {
            from: b,
            to: a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [0u8; 32],
                token_in: b,
                token_out: a,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&a).unwrap(),
            *graph.ix.get(&b).unwrap(),
            *graph.ix.get(&a).unwrap(),
        ];

        let jit_cfg = JitConfig {
            enabled: true,
            min_amount_in: U256::from(50u64),
            seed_bps: 1000,
            tick_range: 2,
            disable_on_quote_failure: true,
        };

        struct StaticQuoter;
        #[async_trait]
        impl JitQuoter for StaticQuoter {
            async fn quote_path(
                &self,
                _path: Vec<(Address, Option<u32>)>,
                _amount_in: U256,
                _block: U64,
            ) -> Result<U256> {
                Ok(U256::from(7u64))
            }
        }
        let quoter = StaticQuoter;

        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"),
            U256::from(100u64),
            addr(11),
            Some(&jit_cfg),
            Some(&quoter),
            U64::zero(),
        )
        .await
        .expect("jit plan should build");

        let pre_swap = plan.steps.iter().find(|step| {
            matches!(step, StepData::Uniswap { amount_in, .. } if *amount_in == U256::from(5u64))
        });
        assert!(pre_swap.is_some(), "expected preswap uniswap step");

        let add_idx = plan
            .steps
            .iter()
            .position(|step| matches!(step, StepData::JitLiquidityAdd { .. }))
            .expect("expected jit add step");
        if let StepData::JitLiquidityAdd {
            pool: p,
            amount0,
            amount1,
            ..
        } = &plan.steps[add_idx]
        {
            assert_eq!(*p, pool);
            assert_eq!(*amount0 + *amount1, U256::from(14u64));
        }

        let remove_idx = plan
            .steps
            .iter()
            .position(|step| matches!(step, StepData::JitLiquidityRemove { .. }))
            .expect("expected jit remove step");
        assert!(add_idx < remove_idx);
        if let StepData::JitLiquidityRemove { min_out, .. } = &plan.steps[remove_idx] {
            let expected_remove_out = mul_div(U256::from(5u64), U256::from(2u64), U256::from(1u64));
            let expected_min_out = apply_slippage(expected_remove_out, 100);
            assert_eq!(*min_out, expected_min_out);
        }

        match plan.steps.last() {
            Some(StepData::Balancer {
                token_in,
                token_out,
                ..
            }) => {
                assert_eq!(*token_in, b);
                assert_eq!(*token_out, a);
            }
            _ => panic!("expected trailing balancer step"),
        }
    }

    #[tokio::test]
    async fn univ2_step_uses_expected_output_in_call_data() {
        let mut graph = Graph::default();

        let token_a = addr(21);
        let token_b = addr(22);
        let pair = addr(23);

        graph.add_edge(Edge {
            from: token_a,
            to: token_b,
            rate_num: U256::from(3u64),
            rate_den: U256::from(2u64),
            venue: VenueEdge::UniV2 {
                pair,
                token_out: token_b,
                token0: token_a,
                token1: token_b,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 100,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
            tick_ladder: None,
        });

        graph.add_edge(Edge {
            from: token_b,
            to: token_a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [5u8; 32],
                token_in: token_b,
                token_out: token_a,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
            tick_ladder: None,
        });

        let cycle = vec![
            *graph.ix.get(&token_a).unwrap(),
            *graph.ix.get(&token_b).unwrap(),
            *graph.ix.get(&token_a).unwrap(),
        ];

        let base_amount = U256::from(100u64);
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"),
            base_amount,
            addr(99),
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan should build for transfer cycle");

        assert_eq!(plan.steps.len(), 2);

        // Plan now requests the REAL constant-product output (reserves 1e6/1e6,
        // 30bps), not the linear rate_num/rate_den extrapolation.
        let aiwf = base_amount * U256::from(9_970u64) / U256::from(10_000u64);
        let expected_out =
            aiwf * U256::from(1_000_000u64) / (U256::from(1_000_000u64) + aiwf);

        match &plan.steps[0] {
            StepData::Generic {
                target,
                call,
                pre_action,
            } => {
                assert_eq!(*target, pair);
                match pre_action {
                    Some(GenericPreAction::Transfer { token, amount }) => {
                        assert_eq!(*token, token_a);
                        assert_eq!(*amount, base_amount);
                    }
                    _ => panic!("expected transfer pre-action"),
                }

                let decoded = decode(
                    &[
                        ParamType::Uint(256),
                        ParamType::Uint(256),
                        ParamType::Address,
                        ParamType::Bytes,
                    ],
                    &call[UNIV2_SWAP_SELECTOR.len()..],
                )
                .expect("decode univ2 call data");

                let amount0 = match &decoded[0] {
                    Token::Uint(value) => *value,
                    _ => panic!("unexpected token type for amount0"),
                };
                let amount1 = match &decoded[1] {
                    Token::Uint(value) => *value,
                    _ => panic!("unexpected token type for amount1"),
                };

                assert_eq!(amount0, U256::zero());
                assert_eq!(amount1, expected_out);
            }
            _ => panic!("expected generic step for univ2"),
        }
    }

    #[test]
    fn cl_hop_out_uses_the_ladder_when_multi_tick_is_enabled() {
        let _lock = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::cl_sim::MultiTickEnvGuard::set("1");

        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            ..Default::default()
        };
        // Positive net below the price: crossing -60 downward removes 500e9,
        // taking liquidity 1000e9 -> 500e9. See the SIGN CONVENTION note in
        // Task 2.
        let ladder = crate::cl_swap::TickLadder::new(
            vec![(-180, 50_000_000_000), (-60, 500_000_000_000)],
            -180,
            180,
        );
        // Deliberately NOT 10^18: at this pool's tiny synthetic liquidity
        // (1e12), that size fully drains both ladder ticks (-60 and -180)
        // after consuming only ~6.04e9 and reports `exhausted`, which is
        // correct behaviour but not what this test wants to exercise. 5e9
        // crosses -60 (so the liquidity-drop discount is observed) while
        // staying below the ~6.04e9 point where the ladder runs out.
        let amount_in = U256::from(5_000_000_000u64);

        let (out, used_multi) =
            cl_hop_out(&state, Some(&ladder), amount_in, true).expect("hop prices");
        assert!(used_multi, "an available ladder must be used");

        let single = crate::cl_sim::quote_exact_input_single_tick(&state, amount_in, true, 3_000)
            .expect("single call")
            .expect("single quote");
        assert!(out < single, "multi-tick must be below the optimistic estimate");
    }

    /// An exhausted ladder must REJECT the hop, never fall back.
    ///
    /// This test previously asserted the opposite — that exhaustion falls back
    /// to single-tick — and that assertion was the bug, pinned. Exhaustion means
    /// the pool cannot fill the size; single-tick holds liquidity constant, so
    /// it assumes infinite depth and is strictly more optimistic than the model
    /// that just refused. Falling back therefore converts "impossible" into an
    /// attractive quote.
    ///
    /// Real instance: WETH/bsdETH 0xdea629c5587037d0925ff85f1961d95db62bedd6
    /// (fee 500) holds 248.9 WETH but only 0.154 bsdETH. The fallback quoted
    /// 2 WETH -> 1.904 bsdETH; the on-chain quoter pays 0.0000169 and drives the
    /// pool to MIN_SQRT_RATIO+1. Every candidate built on such an edge reverted.
    #[test]
    fn cl_hop_out_rejects_an_exhausted_ladder_instead_of_falling_back() {
        let _lock = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::cl_sim::MultiTickEnvGuard::set("1");

        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            ..Default::default()
        };
        // Coverage far too narrow for the size below.
        let ladder = crate::cl_swap::TickLadder::new(vec![(-60, 900_000_000_000)], -60, 60);
        let amount_in = U256::from(10u64).pow(U256::from(24u64));

        assert!(
            cl_hop_out(&state, Some(&ladder), amount_in, true).is_none(),
            "an exhausted ladder must reject the hop, not fall back to a more optimistic model"
        );

        // And the refusal must survive to the caller: the single-tick model,
        // asked the same question, happily answers — which is exactly why it
        // must not be consulted here.
        assert!(
            crate::cl_sim::quote_exact_input_single_tick(&state, amount_in, true, 3_000)
                .ok()
                .flatten()
                .is_some_and(|v| !v.is_zero()),
            "guard: single-tick answers this size, so the fallback would have masked the refusal"
        );
    }

    /// Exhaustion with the pool DEMONSTRABLY holding the depth must not reject.
    ///
    /// `cl_swap` reports `exhausted` both when liquidity runs out and when our
    /// ladder was too short. Rejecting on that alone discarded hops at 0.1-0.2x
    /// of a pool's real holdings and accounted for 100% of `plan_build_failed`.
    /// The real balance disambiguates.
    #[test]
    fn cl_hop_out_accepts_an_exhausted_ladder_when_real_depth_covers_the_size() {
        let _lock = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::cl_sim::MultiTickEnvGuard::set("1");

        let base = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            ..Default::default()
        };
        // Ladder far too narrow for the size => the quote will report exhausted.
        let ladder = crate::cl_swap::TickLadder::new(vec![(-60, 900_000_000_000)], -60, 60);
        let amount_in = U256::from(10u64).pow(U256::from(24u64));

        // No balance known => FAIL CLOSED, still rejected.
        assert!(
            cl_hop_out(&base, Some(&ladder), amount_in, true).is_none(),
            "unknown balance must fail closed"
        );

        // Balance comfortably covers the size (amount is 1/10th of holdings) =>
        // our ladder was the limit, not the pool. Price it single-tick.
        let funded = crate::cl_sim::ClPoolState {
            balance0: Some(amount_in * U256::from(10u64)),
            ..base.clone()
        };
        let priced = cl_hop_out(&funded, Some(&ladder), amount_in, true);
        assert!(
            priced.is_some_and(|(out, used_multi)| !out.is_zero() && !used_multi),
            "real depth covers the size, so fall through to single-tick rather \
             than discarding a fillable hop"
        );

        // Size exceeds the tradeable fraction of real depth => genuinely reject.
        let thin = crate::cl_sim::ClPoolState {
            balance0: Some(amount_in),
            ..base
        };
        assert!(
            cl_hop_out(&thin, Some(&ladder), amount_in, true).is_none(),
            "a size at 100% of holdings is not fillable"
        );
    }

    #[test]
    fn cl_hop_out_ignores_the_ladder_when_the_flag_is_off() {
        let _lock = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::cl_sim::MultiTickEnvGuard::cleared();

        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            ..Default::default()
        };
        let ladder = crate::cl_swap::TickLadder::new(vec![(-60, 500_000_000_000)], -180, 180);

        let (_, used_multi) = cl_hop_out(&state, Some(&ladder), U256::from(1_000u64), true)
            .expect("hop prices");
        assert!(!used_multi, "flag off must keep the single-tick path");
    }

    /// A successful multi-tick quote takes the EXECUTION buffer, not the tick
    /// buffer. Pins both halves: the tick buffer must not double-count a
    /// modelled crossing, and the quote must not pass through raw either.
    ///
    /// This previously asserted the quote passed through UNDISCOUNTED. That was
    /// measured wrong: on Slipstream pool
    /// 0xdbc6998296caa1652a810dc8d3baf4a8294330f1, quoting the planner's own
    /// path bytes through the deployed router showed the model overstates by
    /// ~31 bps while `tolerance_bps` is 5, so `min_out` landed 25.8 bps above
    /// what the router pays and `amountOutMinimum` failed on 100% of
    /// candidates. Modelling the curve correctly is not the same as matching
    /// the router.
    #[test]
    fn hop_expected_out_applies_the_execution_buffer_to_a_multi_tick_quote() {
        let _lock = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::cl_sim::MultiTickEnvGuard::set("1");

        // Same fixture shape as `cl_hop_expected_out_uses_the_curve_not_the_secant`,
        // with a ladder attached. 5e9 crosses tick -60 exactly once and stops
        // mid-range, so the quote is NOT exhausted (see the sizing note in the
        // `cl_hop_out` tests above for where those bounds come from).
        let ladder = std::sync::Arc::new(crate::cl_swap::TickLadder::new(
            vec![(-180, 50_000_000_000), (-60, 500_000_000_000)],
            -180,
            180,
        ));
        let amount_in = U256::from(5_000_000_000u64);

        let cl_state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
            ..Default::default()
        };
        // rate_num/rate_den = 1:1, as in the sibling fixture — the linear
        // estimate is irrelevant to this test; only the haircut matters.
        let edge = Edge {
            from: addr(1),
            to: addr(2),
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(addr(1), Some(3000))],
                pool: addr(99),
                fee: 3000,
                state: Some(cl_state.clone()),
            },
            estimated_gas: 0,
            weight: compute_edge_weight(U256::from(1u64), U256::from(1u64)),
            max_input: U256::zero(),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: Some(ladder.clone()),
        };

        let (multi_out, used_multi) =
            cl_hop_out(&cl_state, Some(ladder.as_ref()), amount_in, true).expect("multi quote");
        assert!(used_multi, "fixture must produce a non-exhausted multi-tick quote");

        let actual = hop_expected_out(&edge, edge.from, amount_in).expect("hop must be priceable");

        assert_eq!(
            actual,
            crate::util::apply_slippage(multi_out, cl_exec_buffer_bps()),
            "a successful multi-tick quote must take the EXECUTION buffer"
        );
        assert!(
            actual < multi_out,
            "it must NOT pass through raw — min_out then sits above what the \
             router pays and every hop reverts with Too little received"
        );
        // Arms-not-inverted check. Identity, not magnitude: the execution
        // buffer is deliberately larger than the tick buffer (75 vs 50), so
        // "greater than the tick-buffer discount" no longer discriminates.
        assert_ne!(
            cl_exec_buffer_bps(),
            cl_tick_buffer_bps(),
            "precondition: the two buffers must differ for this test to \
             discriminate between the arms"
        );
        assert_ne!(
            actual,
            crate::util::apply_slippage(multi_out, cl_tick_buffer_bps()),
            "the TICK buffer must not be what got applied to a modelled \
             crossing — if this fails, the two match arms in hop_expected_out \
             are inverted"
        );
    }
}
