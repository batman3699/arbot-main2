//! Differential harness: multi-tick model vs the deployed on-chain quoter.
//!
//! The unit tests prove the simulator is self-consistent. Only this proves it
//! matches the pool. Run it before enabling `ARBOT_CL_MULTI_TICK` anywhere
//! near funds.
//!
//! Usage:
//!   ARBOT_RPC_URL=... cargo run --bin cl_parity -- <pool> <fee_ppm> <amount_in>...

use anyhow::{anyhow, Context, Result};
use arb_exec::{cl_sim, cl_swap, cl_ticks};
use ethers::{
    contract::abigen,
    providers::{Http, Middleware, Provider},
    types::{Address, U256, U64},
};
use std::{str::FromStr, sync::Arc};

abigen!(
    IClPoolTokens,
    r#"[
        function token0() external view returns (address)
        function token1() external view returns (address)
    ]"#,
);

/// The crate has no shared `pool_tokens` helper, so read the pair off the pool.
async fn pool_tokens(provider: Arc<Provider<Http>>, pool: Address) -> Result<(Address, Address)> {
    let c = IClPoolTokens::new(pool, provider);
    let token0 = c.token_0().call().await.context("pool token0()")?;
    let token1 = c.token_1().call().await.context("pool token1()")?;
    Ok((token0, token1))
}

/// Resolve an HTTP endpoint: explicit `ARBOT_RPC_URL` wins, otherwise take the
/// first usable entry from the chain's configured `BASE_RPC_URLS` list so this
/// runs against the same provider the bot already uses.
fn resolve_rpc() -> Result<String> {
    if let Ok(url) = std::env::var("ARBOT_RPC_URL") {
        if !url.trim().is_empty() {
            return Ok(url);
        }
    }
    let raw = std::env::var("BASE_RPC_URLS")
        .context("set ARBOT_RPC_URL, or BASE_RPC_URLS in .env")?;
    arb_exec::util::parse_endpoint_list(&raw)
        .into_iter()
        .find_map(|e| arb_exec::util::coerce_http_url(&e))
        .ok_or_else(|| anyhow!("no usable HTTP endpoint in BASE_RPC_URLS"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args().skip(1);
    let pool = Address::from_str(&args.next().ok_or_else(|| anyhow!("usage: cl_parity <pool> <fee_ppm> <amount_in>..."))?)
        .context("pool address")?;
    let fee_ppm: u32 = args
        .next()
        .ok_or_else(|| anyhow!("missing fee_ppm"))?
        .parse()
        .context("fee_ppm")?;
    let amounts: Vec<U256> = args
        .map(|a| U256::from_dec_str(&a).context("amount_in"))
        .collect::<Result<_>>()?;
    if amounts.is_empty() {
        return Err(anyhow!("supply at least one amount_in"));
    }

    let _ = dotenvy::dotenv();
    let rpc = resolve_rpc()?;
    let provider = Arc::new(Provider::<Http>::try_from(rpc).context("provider")?);
    let block: U64 = provider.get_block_number().await.context("block number")?;

    let state = cl_sim::load_cl_pool_state(provider.clone(), pool, block, Some(fee_ppm))
        .await?
        .ok_or_else(|| anyhow!("pool has no usable CL state"))?;

    let source = cl_ticks::CachedTickSource::new(cl_ticks::RpcTickSource::new(provider.clone()), 32);
    let ladder = cl_ticks::build_ladder(&source, pool, &state, block, 4).await?;

    println!(
        "pool=0x{} block={} tick={} spacing={} liquidity={} ladder_ticks={} coverage=[{},{}]",
        hex::encode(pool),
        block,
        state.tick,
        state.tick_spacing,
        state.liquidity,
        ladder.len(),
        ladder.lower_bound(),
        ladder.upper_bound(),
    );
    println!("amount_in,single_tick,multi_tick,ticks_crossed,exhausted,single_err_bps,multi_err_bps");

    // `UniQuoter::new` takes (provider, quoter_address, factory_address).
    // Both are already configured per chain; read them rather than hardcoding.
    let quoter_addr = Address::from_str(
        &std::env::var("BASE_UNIV3_QUOTER").context("BASE_UNIV3_QUOTER must be set")?,
    )
    .context("BASE_UNIV3_QUOTER")?;
    let factory_addr = Address::from_str(
        &std::env::var("BASE_UNIV3_FACTORY").context("BASE_UNIV3_FACTORY must be set")?,
    )
    .context("BASE_UNIV3_FACTORY")?;
    let quoter =
        arb_exec::quote_univ3::UniQuoter::new(provider.clone(), quoter_addr, factory_addr);
    let (token0, token1) = pool_tokens(provider.clone(), pool).await?;

    let mut worst_multi_bps: i64 = 0;
    for amount in amounts {
        let on_chain = quoter
            .quote_path(vec![(token0, None), (token1, Some(fee_ppm))], amount, block)
            .await
            .context("on-chain quote")?;

        let single = cl_sim::quote_exact_input_single_tick(&state, amount, true, fee_ppm)?
            .unwrap_or_default();
        let multi = cl_swap::quote_exact_input_multi_tick(&state, &ladder, amount, true, 128);

        let err_bps = |model: U256| -> i64 {
            if on_chain.is_zero() {
                return 0;
            }
            let (diff, sign) = if model >= on_chain {
                (model - on_chain, 1i64)
            } else {
                (on_chain - model, -1i64)
            };
            sign * (diff * U256::from(10_000u64) / on_chain).as_u64() as i64
        };

        let (multi_out, crossed, exhausted) = match multi {
            Some(q) => (q.amount_out, q.ticks_crossed, q.exhausted),
            None => (U256::zero(), 0, true),
        };
        let multi_bps = err_bps(multi_out);
        if !exhausted && multi_bps.abs() > worst_multi_bps.abs() {
            worst_multi_bps = multi_bps;
        }

        println!(
            "{amount},{single},{multi_out},{crossed},{exhausted},{},{multi_bps}",
            err_bps(single)
        );
    }

    println!("\nworst non-exhausted multi-tick error: {worst_multi_bps} bps");
    if worst_multi_bps.abs() > 5 {
        return Err(anyhow!(
            "multi-tick deviates from the on-chain quoter by {worst_multi_bps} bps (limit 5) — do NOT enable ARBOT_CL_MULTI_TICK"
        ));
    }
    println!("PASS: within 5 bps of the on-chain quoter");
    Ok(())
}
