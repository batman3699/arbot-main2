use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;

use crate::graph::{Graph, VenueEdge};
use crate::math::mul_div;
use crate::quote_univ3::UniQuoter;
use crate::util::{apply_slippage, encode_univ3_path};
use tracing::{info, warn};

pub enum GenericPreAction {
    Approve { token: Address, amount: U256 },
    Transfer { token: Address, amount: U256 },
}

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

pub async fn build_plan_for_cycle(
    graph: &Graph,
    cycle: &[usize],
    amount_in: U256,
    executor_address: Address,
    jit: Option<&JitConfig>,
    jit_quoter: Option<&dyn JitQuoter>,
    block_number: U64,
) -> Result<Plan> {
    let _ = (jit_quoter, block_number);
    // PlanV2 execution currently supports a single flash loan. The planner must enforce the
    // single-loan invariant before building executor loan data.
    let mut steps = Vec::new();
    let mut current_amount = amount_in;
    let mut max_slippage_bps = 0u32;
    for window in cycle.windows(2) {
        let from = graph.nodes[window[0]];
        let to = graph.nodes[window[1]];
        let Some(edge) = graph.edge_between(from, to) else {
            warn!(from = %from, to = %to, "Skipping plan build for missing edge");
            return Err(anyhow!("missing edge between {from:?} and {to:?}"));
        };
        let expected_out = mul_div(current_amount, edge.rate_num, edge.rate_den);
        let min_out = apply_slippage(expected_out, edge.tolerance_bps);
        if min_out.is_zero() {
            return Err(anyhow!("min_out is zero for hop {from:?}->{to:?}"));
        }
        max_slippage_bps = max_slippage_bps.max(edge.observed_slippage_bps);
        match &edge.venue {
            VenueEdge::UniV3 { path, pool, fee } => {
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
    use crate::util::{compute_edge_weight, NativePrice};
    use ethers::abi::{decode, ParamType, Token};
    use ethers::types::Address;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn fp_weight(num: u64, den: u64) -> i64 {
        compute_edge_weight(
            U256::from(num),
            U256::from(den),
            0,
            U256::zero(),
            U256::from(1u64),
            NativePrice::unit(),
        )
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
        });

        let cycle = vec![
            *graph.ix.get(&token_in).unwrap(),
            *graph.ix.get(&token_out).unwrap(),
            *graph.ix.get(&token_in).unwrap(),
        ];
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
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
        assert_eq!(amount1_out, U256::from(1_000u64));
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
        });

        let cycle = vec![
            *graph.ix.get(&a).unwrap(),
            *graph.ix.get(&b).unwrap(),
            *graph.ix.get(&a).unwrap(),
        ];

        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
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
        });

        let cycle = vec![
            *graph.ix.get(&token_in).unwrap(),
            *graph.ix.get(&token_out).unwrap(),
            *graph.ix.get(&token_in).unwrap(),
        ];
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
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
        assert_eq!(amount1_out, U256::from(5_000u64));
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
        });

        let cycle = vec![
            *graph.ix.get(&token0).unwrap(),
            *graph.ix.get(&token1).unwrap(),
            *graph.ix.get(&token0).unwrap(),
        ];
        let plan = build_plan_for_cycle(
            &graph,
            &cycle,
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
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 100,
            observed_slippage_bps: 50,
            quote_block: None,
            active: true,
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
            base_amount,
            addr(99),
            None,
            None,
            U64::zero(),
        )
        .await
        .expect("plan should build for transfer cycle");

        assert_eq!(plan.steps.len(), 2);

        let expected_out = mul_div(base_amount, U256::from(3u64), U256::from(2u64));

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
}
