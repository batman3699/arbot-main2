//! Local concentrated-liquidity exact-input simulator (slot0 / liquidity / ticks).
//!
//! Gated by `ARBOT_LOCAL_CL_QUOTES=1` (default on). When disabled or state is
//! incomplete, callers fall back to on-chain quoter RPC.

use anyhow::{anyhow, Context, Result};
use ethers::{
    prelude::*,
    providers::JsonRpcClient,
    types::{Address, BlockId, BlockNumber, U256, U64},
};
use std::sync::Arc;

abigen!(
    IClPoolState,
    r#"[
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked)
        function liquidity() external view returns (uint128)
        function tickSpacing() external view returns (int24)
        function fee() external view returns (uint24)
    ]"#,
);

#[derive(Clone, Debug)]
pub struct ClPoolState {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    // Populated from chain but not yet consumed (single-tick sim doesn't cross
    // ticks); retained for the planned multi-tick simulation.
    #[allow(dead_code)]
    pub tick: i32,
    #[allow(dead_code)]
    pub tick_spacing: i32,
    /// Swap fee in hundredths of a bip (UniV3 fee tier or on-chain fee()).
    pub fee_ppm: u32,
}

pub fn local_cl_quotes_enabled() -> bool {
    std::env::var("ARBOT_LOCAL_CL_QUOTES")
        .map(|raw| !matches!(raw.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true)
}

#[allow(dead_code)]
pub fn cl_quote_parity_enabled() -> bool {
    std::env::var("ARBOT_CL_QUOTE_PARITY")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Load slot0 + liquidity once per pool per block (tick spacing + fee from chain).
pub async fn load_cl_pool_state<C>(
    provider: Arc<Provider<C>>,
    pool: Address,
    block: U64,
    fee_hint: Option<u32>,
) -> Result<Option<ClPoolState>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = IClPoolState::new(pool, provider);
    let block_id = if block.is_zero() {
        BlockId::Number(BlockNumber::Latest)
    } else {
        BlockId::Number(BlockNumber::Number(block))
    };

    let (sqrt_price_x96, tick, _, _, _, _, _) = contract
        .slot_0()
        .block(block_id)
        .call()
        .await
        .context("CL pool slot0()")?;
    if U256::from(sqrt_price_x96).is_zero() {
        return Ok(None);
    }

    let liquidity = contract
        .liquidity()
        .block(block_id)
        .call()
        .await
        .context("CL pool liquidity()")?;
    if liquidity == 0 {
        return Ok(None);
    }

    let tick_spacing = contract
        .tick_spacing()
        .block(block_id)
        .call()
        .await
        .unwrap_or(60);
    let fee_on_chain = contract.fee().block(block_id).call().await.ok();
    let fee_ppm = fee_hint
        .or(fee_on_chain.map(|f| f as u32))
        .unwrap_or(3_000);

    Ok(Some(ClPoolState {
        sqrt_price_x96: U256::from(sqrt_price_x96),
        liquidity,
        tick,
        tick_spacing,
        fee_ppm,
    }))
}

/// Minimal single-tick exact-input quote. Returns `None` when price would cross ticks.
pub fn quote_exact_input_single_tick(
    state: &ClPoolState,
    amount_in: U256,
    zero_for_one: bool,
    fee_ppm: u32,
) -> Result<Option<U256>> {
    if amount_in.is_zero() || state.liquidity == 0 || state.sqrt_price_x96.is_zero() {
        return Ok(None);
    }
    let fee_n = U256::from(1_000_000u64 - u64::from(fee_ppm.min(1_000_000)));
    let Some(amount_in_less_fee) = amount_in.checked_mul(fee_n) else {
        return Ok(None);
    };
    let amount_in_less_fee = amount_in_less_fee / U256::from(1_000_000u64);
    if amount_in_less_fee.is_zero() {
        return Ok(None);
    }

    let sqrt_p = state.sqrt_price_x96;
    let liq = U256::from(state.liquidity);
    let q96 = U256::from(1u128) << 96;

    let amount_out = if zero_for_one {
        let Some(num) = liq.checked_mul(sqrt_p) else {
            return Ok(None);
        };
        let Some(in_scaled) = amount_in_less_fee.checked_mul(sqrt_p) else {
            return Ok(None);
        };
        let denom = liq.saturating_add(in_scaled / q96);
        if denom.is_zero() {
            return Ok(None);
        }
        let sqrt_p_next = num / denom;
        if sqrt_p_next >= sqrt_p {
            return Ok(None);
        }
        let Some(delta) = sqrt_p.checked_sub(sqrt_p_next) else {
            return Ok(None);
        };
        liq.checked_mul(delta).map(|v| v / q96).unwrap_or(U256::zero())
    } else {
        let Some(in_scaled) = amount_in_less_fee.checked_mul(q96) else {
            return Ok(None);
        };
        let sqrt_p_next = sqrt_p.saturating_add(in_scaled / liq);
        if sqrt_p_next <= sqrt_p {
            return Ok(None);
        }
        let Some(delta) = sqrt_p_next.checked_sub(sqrt_p) else {
            return Ok(None);
        };
        let Some(numer) = liq.checked_mul(q96).and_then(|v| v.checked_mul(delta)) else {
            return Ok(None);
        };
        let Some(denom) = sqrt_p.checked_mul(sqrt_p_next) else {
            return Ok(None);
        };
        if denom.is_zero() {
            return Ok(None);
        }
        numer / denom
    };

    if amount_out.is_zero() {
        return Ok(None);
    }
    Ok(Some(amount_out))
}

#[allow(dead_code)]
pub fn validate_pool(_pool: Address, state: &ClPoolState) -> Result<()> {
    if state.sqrt_price_x96.is_zero() {
        return Err(anyhow!("CL pool sqrt_price_x96 is zero"));
    }
    Ok(())
}

/// Compare local single-tick quotes against quoter for parity logging (live pools).
#[allow(dead_code)]
pub async fn log_cl_quote_parity<C>(
    provider: Arc<Provider<C>>,
    quoter: &crate::quote_univ3::UniQuoter<C>,
    pool: Address,
    token0: Address,
    token1: Address,
    fee: u32,
    block: U64,
) -> Result<()>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    if !cl_quote_parity_enabled() {
        return Ok(());
    }
    let Some(state) = load_cl_pool_state(provider, pool, block, Some(fee)).await? else {
        return Ok(());
    };
    let amount_in = U256::from(1_000_000u64);
    let path = vec![(token0, None), (token1, Some(fee))];
    let local = quote_exact_input_single_tick(&state, amount_in, true, state.fee_ppm)?;
    let on_chain = quoter.quote_path(path, amount_in, block).await.ok();
    tracing::debug!(
        target: "cl_sim",
        pool = %format!("0x{}", hex::encode(pool)),
        local = ?local,
        on_chain = ?on_chain,
        "CL quote parity check"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_tick_quote_non_zero() {
        let state = ClPoolState {
            sqrt_price_x96: U256::from(1u128) << 96,
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
        };
        let out = quote_exact_input_single_tick(&state, U256::from(1_000_000u64), true, 3_000)
            .unwrap()
            .expect("quote");
        assert!(out > U256::zero());
    }

    #[test]
    fn local_cl_quotes_default_enabled() {
        std::env::remove_var("ARBOT_LOCAL_CL_QUOTES");
        assert!(local_cl_quotes_enabled());
        std::env::set_var("ARBOT_LOCAL_CL_QUOTES", "0");
        assert!(!local_cl_quotes_enabled());
        std::env::remove_var("ARBOT_LOCAL_CL_QUOTES");
    }
}
