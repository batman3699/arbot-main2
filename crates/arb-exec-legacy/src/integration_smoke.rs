use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ethers::abi::Token;
use ethers::providers::Middleware;
use ethers::types::{Address, Bytes, U256, U64};
use tokio::time::timeout;

use arb_exec::abi::{ExecutorLoan, ExecutorPlan, ExecutorStep, MultiVenueArbExecutor};
use arb_exec::chain::load_chain_from_sources;
use arb_exec::graph::{Edge, Graph, VenueEdge};
use arb_exec::ops_inputs::load_ops_inputs;
use arb_exec::plan::{build_plan_for_cycle, StepData};
use arb_exec::quote_univ3::{IUniswapV3Factory, UniQuoter};
use arb_exec::registry::Registry;
use arb_exec::util::connect_http_provider_with_fallbacks;

const DEFAULT_FEES: [u32; 3] = [500, 3000, 10000];

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
    let (token_in, token_out, fee, amount_in, amount_out, pool) =
        find_univ3_quote(&cfg, provider.clone(), &fallback_tokens).await?;

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
    let factory = IUniswapV3Factory::new(cfg.univ3_factory, provider.clone());
    let quoter = UniQuoter::new(provider.clone(), cfg.univ3_quoter, cfg.univ3_factory);
    let mut tokens = cfg.tokens.clone();
    if tokens.len() < 2 {
        tokens = fallback_tokens.to_vec();
    }
    if tokens.len() < 2 {
        return Err(anyhow!("token list too small for {}", cfg.name));
    }

    let block = provider
        .get_block_number()
        .await
        .context("fetch block number")?;
    let amount_in = U256::from(1_000_000u64);

    for token_in in tokens.iter().copied() {
        for token_out in tokens.iter().copied() {
            if token_in == token_out {
                continue;
            }
            for fee in DEFAULT_FEES {
                let pool = factory
                    .get_pool(token_in, token_out, fee)
                    .call()
                    .await
                    .unwrap_or_default();
                if pool.is_zero() {
                    continue;
                }
                let path = vec![(token_in, Some(fee)), (token_out, None)];
                let out = quoter
                    .quote_path(path, amount_in, block)
                    .await
                    .unwrap_or_default();
                if !out.is_zero() {
                    return Ok((token_in, token_out, fee, amount_in, out, pool));
                }
            }
        }
    }

    Err(anyhow!("no univ3 quote found for {}", cfg.name))
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
            path: vec![(token_in, Some(fee)), (token_out, None)],
            pool: Address::zero(),
            fee,
        },
        estimated_gas: 160_000,
        weight: 0,
        max_input: amount_in,
        tolerance_bps: 50,
        observed_slippage_bps: 50,
        quote_block: None,
        active: true,
    };
    graph.add_edge(edge_forward);
    let edge_reverse = Edge {
        from: token_out,
        to: token_in,
        rate_num: amount_in,
        rate_den: amount_out,
        venue: VenueEdge::UniV3 {
            path: vec![(token_out, Some(fee)), (token_in, None)],
            pool: Address::zero(),
            fee,
        },
        estimated_gas: 160_000,
        weight: 0,
        max_input: amount_out,
        tolerance_bps: 50,
        observed_slippage_bps: 50,
        quote_block: None,
        active: true,
    };
    graph.add_edge(edge_reverse);

    let cycle = vec![
        *graph.ix.get(&token_in).unwrap(),
        *graph.ix.get(&token_out).unwrap(),
        *graph.ix.get(&token_in).unwrap(),
    ];

    build_plan_for_cycle(&graph, &cycle,
            &graph.best_edge_indices_for_node_path(&cycle).expect("edges for test cycle"), amount_in, executor, None, None, U64::zero()).await
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
