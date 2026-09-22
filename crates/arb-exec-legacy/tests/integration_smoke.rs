use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use ethers::abi::Token;
use ethers::contract::abigen;
use ethers::types::{Address, Bytes, U256, U64};
use tokio::time::timeout;

use arb_exec::abi::{ExecutorLoan, ExecutorPlan, ExecutorStep, MultiVenueArbExecutor};
use arb_exec::chain::load_chain_from_sources;
use arb_exec::graph::{Edge, Graph, VenueEdge};
use arb_exec::ops_inputs::load_ops_inputs;
use arb_exec::plan::{build_plan_for_cycle, StepData};
use arb_exec::quote_univ3::IUniswapV3Factory;
use arb_exec::registry::Registry;
use arb_exec::util::connect_http_provider_with_fallbacks;

const DEFAULT_FEES: [u32; 3] = [500, 3000, 10000];
const UNIV3_SWAP_ROUTER_LEGACY_01: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";
const UNIV3_SWAP_ROUTER_02: &str = "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45";
const ETH_USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const ETH_WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const ETH_UNIV3_FACTORY: &str = "0x1F98431c8aD98523631AE4a59f267346ea31F984";
const ETH_UNIV3_QUOTER_V2: &str = "0x61fFE014bA17989E743c5F6cB21bF9697530B21e";
const BASE_WETH: &str = "0x4200000000000000000000000000000000000006";
const BASE_USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913";
const FEE_3000: u32 = 3000;

abigen!(
    IQuoterV2SingleQuote,
    r#"[
        {
          "inputs": [
            {
              "components": [
                {"internalType": "address", "name": "tokenIn", "type": "address"},
                {"internalType": "address", "name": "tokenOut", "type": "address"},
                {"internalType": "uint256", "name": "amountIn", "type": "uint256"},
                {"internalType": "uint24", "name": "fee", "type": "uint24"},
                {"internalType": "uint160", "name": "sqrtPriceLimitX96", "type": "uint160"}
              ],
              "internalType": "struct IQuoterV2.QuoteExactInputSingleParams",
              "name": "params",
              "type": "tuple"
            }
          ],
          "name": "quoteExactInputSingle",
          "outputs": [
            {"internalType": "uint256", "name": "amountOut", "type": "uint256"},
            {"internalType": "uint160", "name": "sqrtPriceX96After", "type": "uint160"},
            {"internalType": "uint32", "name": "initializedTicksCrossed", "type": "uint32"},
            {"internalType": "uint256", "name": "gasEstimate", "type": "uint256"}
          ],
          "stateMutability": "nonpayable",
          "type": "function"
        }
    ]"#,
);

#[tokio::test]
async fn integration_smoke_per_chain() -> Result<()> {
    if std::env::var("ARBOT_INTEGRATION_SMOKE").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let ops_path = Path::new("ops/inputs.yaml");
    if !ops_path.exists() {
        return Ok(());
    }

    let ops_inputs = load_ops_inputs(ops_path).context("load ops/inputs.yaml")?;
    let target_chain = parse_target_chain();
    let fork_rpc_override = std::env::var("ARBOT_FORK_RPC_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let mut chains_tested = 0usize;

    let registry_path = std::env::var("REGISTRY_PATH")
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .unwrap_or_else(|| "config/registry.json".to_string());
    let registry = if Path::new(&registry_path).exists() {
        let raw = std::fs::read_to_string(&registry_path)
            .with_context(|| format!("read {registry_path}"))?;
        Some(serde_json::from_str::<Registry>(&raw).context("parse registry json")?)
    } else {
        None
    };

    let chain = ops_inputs
        .chains
        .iter()
        .find(|chain| chain.chain_name.eq_ignore_ascii_case(&target_chain))
        .ok_or_else(|| anyhow!("chain '{target_chain}' missing in ops/inputs.yaml"))?;
    let chain_name = chain.chain_name.clone();
    let has_univ3_venue = chain
        .venues
        .iter()
        .any(|venue| matches!(venue.kind, Some(arb_exec::ops_inputs::VenueKind::Univ3Like)));

    let ops_chain = ops_inputs
        .chain_overrides(&chain_name)
        .ok_or_else(|| anyhow!("missing chain overrides for {chain_name}"))?;
    let registry_chain = registry
        .as_ref()
        .and_then(|reg| reg.chains.get(&chain_name))
        .cloned();
    let cfg = load_chain_from_sources(
        &chain_name,
        registry.as_ref().map(|reg| reg.version.clone()),
        registry_chain.as_ref(),
        Some(&ops_chain),
    )
    .with_context(|| format!("load chain config for {chain_name}"))?;

    if has_univ3_venue {
        assert_ne!(
            cfg.univ3_router,
            parse_addr(UNIV3_SWAP_ROUTER_LEGACY_01),
            "{chain_name} is still configured with legacy Uniswap V3 SwapRouter01; expected SwapRouter02"
        );
        if chain_name.eq_ignore_ascii_case("ethereum") {
            assert_eq!(
                cfg.univ3_router,
                parse_addr(UNIV3_SWAP_ROUTER_02),
                "ethereum must be configured with Uniswap SwapRouter02"
            );
        }
    }

    let mut endpoints = cfg.rpc_endpoints();
    if let Some(override_url) = &fork_rpc_override {
        endpoints = vec![override_url.clone()];
    }

    endpoints.retain(|url| !has_unresolved_placeholder(url));
    if endpoints.is_empty() {
        return Err(anyhow!(
            "{chain_name} has no usable RPC endpoint after filtering unresolved placeholders"
        ));
    }
    let provider = Arc::new(
        connect_http_provider_with_fallbacks(&chain_name, &endpoints, Duration::from_secs(2))
            .await
            .with_context(|| format!("connect rpc for {chain_name}"))?,
    );
    chains_tested += 1;

    let fallback_tokens = ops_inputs
        .universe
        .hub_tokens
        .iter()
        .filter_map(|raw| Address::from_str(raw).ok())
        .collect::<Vec<_>>();
    let (token_in, token_out, fee, amount_in, amount_out, pool) = match find_univ3_quote(
        &cfg,
        provider.clone(),
        &fallback_tokens,
    )
    .await
    {
        Ok(quote) => quote,
        Err(err) if !has_univ3_venue => {
            eprintln!(
                    "skipping univ3 quote assertions for {chain_name}; no univ3 venue configured: {err}"
                );
            return Ok(());
        }
        Err(err) if fork_rpc_override.is_some() && !chain_name.eq_ignore_ascii_case("ethereum") => {
            eprintln!(
                "skipping univ3 quote assertions for {chain_name}; fork RPC quote failed: {err}"
            );
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    assert!(!pool.is_zero(), "no univ3 pool found for {chain_name}");
    assert!(
        !amount_out.is_zero(),
        "quote returned zero for {chain_name}"
    );

    let dry_plan = build_dry_run_plan(
        token_in,
        token_out,
        fee,
        amount_in,
        amount_out,
        parse_addr_str(&chain.executor_address).unwrap_or_default(),
    )
    .await?;
    assert!(!dry_plan.steps.is_empty(), "empty plan for {chain_name}");
    let encoded_steps = encode_steps(dry_plan.steps)?;
    assert!(
        !encoded_steps.is_empty(),
        "encoded empty step list for {chain_name}"
    );

    if std::env::var("ARBOT_SMOKE_EXECUTOR").ok().as_deref() == Some("1") {
        if let (Some(owner), Some(exec_addr)) = (
            parse_addr_opt(&chain.executor_owner),
            parse_addr_str(&chain.executor_address),
        ) {
            let ctx = SimulatePlanContext {
                provider: provider.clone(),
                executor: exec_addr,
                owner,
                token_in,
                token_out,
                fee,
                amount_in,
                amount_out,
                bal_vault: parse_addr_opt(&ops_chain.balancer_vault),
                aave_pool: parse_addr_opt(&ops_chain.aave_pool),
            };
            simulate_executor_plan(ctx)
                .await
                .with_context(|| format!("simulate executor plan for {chain_name}"))?;
        }
    }

    if std::env::var("ARBOT_REQUIRE_CHAIN_COVERAGE")
        .ok()
        .as_deref()
        == Some("1")
    {
        assert!(chains_tested > 0, "no integration chains were tested");
    }

    Ok(())
}

fn parse_target_chain() -> String {
    std::env::var("ARBOT_INTEGRATION_CHAIN")
        .ok()
        .map(|raw| raw.trim().to_ascii_lowercase())
        .filter(|raw| !raw.is_empty())
        .unwrap_or_else(|| "ethereum".to_string())
}

fn has_unresolved_placeholder(raw: &str) -> bool {
    raw.contains("${")
}

async fn find_univ3_quote(
    cfg: &arb_exec::chain::ChainCfg,
    provider: Arc<ethers::providers::Provider<ethers::providers::Http>>,
    fallback_tokens: &[Address],
) -> Result<(Address, Address, u32, U256, U256, Address)> {
    if cfg.name.eq_ignore_ascii_case("ethereum") {
        let usdc = parse_addr(ETH_USDC);
        let weth = parse_addr(ETH_WETH);
        let factory_addr = parse_addr(ETH_UNIV3_FACTORY);
        let quoter_addr = parse_addr(ETH_UNIV3_QUOTER_V2);

        ensure!(
            cfg.univ3_factory == factory_addr,
            "ethereum univ3_factory mismatch: expected {factory_addr:?}, got {:?}",
            cfg.univ3_factory
        );
        ensure!(
            cfg.univ3_quoter == quoter_addr,
            "ethereum univ3_quoter mismatch: expected {quoter_addr:?}, got {:?}",
            cfg.univ3_quoter
        );

        let factory = IUniswapV3Factory::new(factory_addr, provider.clone());
        let pool = factory.get_pool(usdc, weth, FEE_3000).call().await?;
        ensure!(
            pool != Address::zero(),
            "missing Ethereum Uniswap v3 USDC/WETH 0.3% pool"
        );

        let amount_in = U256::from(1_000_000_000u64);

        let quoter = IQuoterV2SingleQuote::new(quoter_addr, provider);
        let (amount_out, _, _, _) =
            quote_exact_input_single_v2(&quoter, usdc, weth, FEE_3000, amount_in, U256::zero())
                .await?;
        ensure!(
            amount_out > U256::zero(),
            "zero quote for Ethereum Uniswap v3 USDC/WETH 0.3%"
        );

        return Ok((usdc, weth, FEE_3000, amount_in, amount_out, pool));
    }

    let factory = IUniswapV3Factory::new(cfg.univ3_factory, provider.clone());
    let quoter = IQuoterV2SingleQuote::new(cfg.univ3_quoter, provider.clone());

    if cfg.name.eq_ignore_ascii_case("base") {
        let weth = parse_addr(BASE_WETH);
        let usdc = parse_addr(BASE_USDC);
        let amount_in = U256::from(1_000_000u64);
        for fee in DEFAULT_FEES {
            let pool = factory
                .get_pool(weth, usdc, fee)
                .call()
                .await
                .with_context(|| format!("base probe get_pool WETH/USDC fee={fee}"))?;
            if pool.is_zero() {
                eprintln!("base univ3 probe: no pool for WETH/USDC fee={fee}");
                continue;
            }
            match quote_exact_input_single_v2(&quoter, weth, usdc, fee, amount_in, U256::zero())
                .await
            {
                Ok((amount_out, _, _, _)) if !amount_out.is_zero() => {
                    eprintln!(
                        "base univ3 probe success: token_in={:#x} token_out={:#x} fee={} amount_in={} amount_out={} pool={:#x}",
                        weth, usdc, fee, amount_in, amount_out, pool
                    );
                    return Ok((weth, usdc, fee, amount_in, amount_out, pool));
                }
                Ok(_) => {
                    eprintln!("base univ3 probe: zero quote for WETH/USDC fee={fee}");
                }
                Err(err) => {
                    eprintln!("base univ3 probe: quote error for WETH/USDC fee={fee}: {err}");
                }
            }
        }
    }

    let mut tokens = cfg.tokens.clone();
    if tokens.len() < 2 {
        tokens = fallback_tokens.to_vec();
    }
    match cfg.name.as_str() {
        "ethereum" => {
            tokens.push(Address::from_str(
                "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
            )?);
            tokens.push(Address::from_str(
                "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            )?);
        }
        "base" => {
            tokens.push(Address::from_str(
                "0x4200000000000000000000000000000000000006",
            )?);
            tokens.push(Address::from_str(
                "0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913",
            )?);
        }
        _ => {}
    }
    tokens.sort_unstable();
    tokens.dedup();
    if tokens.len() < 2 {
        return Err(anyhow!("token list too small for {}", cfg.name));
    }

    let amount_in = U256::from(1_000_000u64);

    let mut attempted = 0usize;
    let mut no_pool = 0usize;
    let mut quote_zero = 0usize;
    let mut quote_err = 0usize;

    for token_in in tokens.iter().copied() {
        for token_out in tokens.iter().copied() {
            if token_in == token_out {
                continue;
            }
            for fee in DEFAULT_FEES {
                attempted = attempted.saturating_add(1);
                let pool = factory
                    .get_pool(token_in, token_out, fee)
                    .call()
                    .await
                    .unwrap_or_default();
                if pool.is_zero() {
                    no_pool = no_pool.saturating_add(1);
                    continue;
                }
                let out = match quote_exact_input_single_v2(
                    &quoter,
                    token_in,
                    token_out,
                    fee,
                    amount_in,
                    U256::zero(),
                )
                .await
                {
                    Ok((amount_out, _, _, _)) => amount_out,
                    Err(err) => {
                        quote_err = quote_err.saturating_add(1);
                        eprintln!(
                            "univ3 quote rejected: token_in={:#x} token_out={:#x} fee={} reason=quote_error err={}",
                            token_in, token_out, fee, err
                        );
                        continue;
                    }
                };
                if !out.is_zero() {
                    eprintln!(
                        "univ3 quote success: chain={} token_in={:#x} token_out={:#x} fee={} amount_in={} amount_out={} pool={:#x} attempts={}",
                        cfg.name, token_in, token_out, fee, amount_in, out, pool, attempted
                    );
                    return Ok((token_in, token_out, fee, amount_in, out, pool));
                }
                quote_zero = quote_zero.saturating_add(1);
            }
        }
    }

    eprintln!(
        "univ3 quote search exhausted: chain={} tokens={} attempts={} no_pool={} quote_zero={} quote_err={}",
        cfg.name,
        tokens.len(),
        attempted,
        no_pool,
        quote_zero,
        quote_err
    );

    Err(anyhow!("no univ3 quote found for {}", cfg.name))
}

async fn quote_exact_input_single_v2(
    quoter: &IQuoterV2SingleQuote<ethers::providers::Provider<ethers::providers::Http>>,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    sqrt_price_limit_x96: U256,
) -> Result<(U256, U256, u32, U256)> {
    let params = i_quoter_v2_single_quote::QuoteExactInputSingleParams {
        token_in,
        token_out,
        amount_in,
        fee,
        sqrt_price_limit_x96,
    };
    let quote = quoter.quote_exact_input_single(params).call().await?;
    Ok(quote)
}

struct SimulatePlanContext {
    provider: Arc<ethers::providers::Provider<ethers::providers::Http>>,
    executor: Address,
    owner: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    amount_out: U256,
    bal_vault: Option<Address>,
    aave_pool: Option<Address>,
}

async fn simulate_executor_plan(ctx: SimulatePlanContext) -> Result<()> {
    let plan = build_dry_run_plan(
        ctx.token_in,
        ctx.token_out,
        ctx.fee,
        ctx.amount_in,
        ctx.amount_out,
        ctx.executor,
    )
    .await?;
    let steps = encode_steps(plan.steps)?;

    let (provider_id, provider_addr) = if let Some(pool) = ctx.aave_pool {
        (1u8, pool)
    } else if let Some(vault) = ctx.bal_vault {
        (0u8, vault)
    } else {
        return Err(anyhow!("no flash loan provider available"));
    };

    let loans = vec![ExecutorLoan {
        token: ctx.token_in,
        amount: ctx.amount_in,
        provider: provider_id,
        provider_addr,
    }];

    let plan_args = ExecutorPlan {
        loans,
        cycle_slippage_bps: 50,
        steps,
        min_profit: U256::zero(),
    };

    let executor = MultiVenueArbExecutor::new(ctx.executor, ctx.provider);
    let mut call = executor.start_v2(plan_args);
    call.tx.set_from(ctx.owner);
    let call_future = call.call();
    let _ = timeout(Duration::from_secs(15), call_future).await??;
    Ok(())
}

async fn build_dry_run_plan(
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    amount_out: U256,
    executor: Address,
) -> Result<arb_exec::plan::Plan> {
    let mut graph = Graph::default();
    let edge_forward = Edge {
        from: token_in,
        to: token_out,
        rate_num: amount_out,
        rate_den: amount_in,
        venue: VenueEdge::UniV3 {
            path: vec![(token_in, None), (token_out, Some(fee))],
            pool: Address::zero(),
            fee,
            state: None,
        },
        estimated_gas: 160_000,
        weight: 0,
        max_input: amount_in,
        tolerance_bps: 50,
        observed_slippage_bps: 50,
        quote_block: None,
        active: true,
        tick_ladder: None,
    };
    graph.add_edge(edge_forward);
    let edge_reverse = Edge {
        from: token_out,
        to: token_in,
        rate_num: amount_in,
        rate_den: amount_out,
        venue: VenueEdge::UniV3 {
            path: vec![(token_out, None), (token_in, Some(fee))],
            pool: Address::zero(),
            fee,
            state: None,
        },
        estimated_gas: 160_000,
        weight: 0,
        max_input: amount_out,
        tolerance_bps: 50,
        observed_slippage_bps: 50,
        quote_block: None,
        active: true,
        tick_ladder: None,
    };
    graph.add_edge(edge_reverse);

    let cycle = vec![
        *graph.ix.get(&token_in).unwrap(),
        *graph.ix.get(&token_out).unwrap(),
        *graph.ix.get(&token_in).unwrap(),
    ];

    build_plan_for_cycle(
        &graph,
        &cycle,
        // The two edges this test added, in order. Named explicitly rather
        // than re-resolved by token pair -- that re-resolution is exactly
        // what the planner signature now prevents.
        &[0usize, 1usize],
        amount_in,
        executor,
        None,
        None,
        U64::zero(),
    )
    .await
}

fn encode_steps(steps: Vec<StepData>) -> Result<Vec<ExecutorStep>> {
    let mut encoded = Vec::with_capacity(steps.len());
    for step in steps {
        match step {
            StepData::Uniswap {
                path,
                amount_in,
                min_out,
            } => {
                let data = ethers::abi::encode(&[
                    Token::Bytes(path.to_vec()),
                    Token::Uint(amount_in),
                    Token::Uint(min_out),
                ]);
                encoded.push(ExecutorStep {
                    op: 0u8,
                    data: Bytes::from(data),
                });
            }
            _ => {
                return Err(anyhow!("unexpected step in smoke plan"));
            }
        }
    }
    Ok(encoded)
}

fn parse_addr(raw: &str) -> Address {
    Address::from_str(raw).expect("invalid static address")
}

fn parse_addr_str(raw: &str) -> Option<Address> {
    if raw.trim().is_empty() {
        None
    } else {
        Address::from_str(raw).ok()
    }
}

fn parse_addr_opt(raw: &Option<String>) -> Option<Address> {
    raw.as_ref().and_then(|value| parse_addr_str(value))
}
