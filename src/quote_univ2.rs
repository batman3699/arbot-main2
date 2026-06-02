use anyhow::{anyhow, Result};
use ethers::{prelude::*, providers::JsonRpcClient};
use std::sync::Arc;

use crate::math::mul_div;

abigen!(
    IUniswapV2Pair,
    r#"[
        function token0() external view returns (address)
        function token1() external view returns (address)
        function getReserves() external view returns (uint112,uint112,uint32)
    ]"#,
);

pub struct UniV2Quote {
    pub amount_out: U256,
    pub price_impact_bps: u32,
}

#[derive(Clone, Debug)]
pub struct UniV2PairState {
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
}

impl UniV2PairState {
    pub fn reserves_for(&self, token_in: Address) -> Option<(U256, U256)> {
        if token_in == self.token0 {
            Some((self.reserve0, self.reserve1))
        } else if token_in == self.token1 {
            Some((self.reserve1, self.reserve0))
        } else {
            None
        }
    }
}

pub async fn load_pair_state<C>(
    provider: Arc<Provider<C>>,
    pair: Address,
) -> Result<Option<UniV2PairState>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = IUniswapV2Pair::new(pair, provider.clone());
    let (reserve0, reserve1, _) = match contract.get_reserves().call().await {
        Ok(result) => result,
        Err(err) if should_skip_pair(&err) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let token0 = match contract.token_0().call().await {
        Ok(token) => token,
        Err(err) if should_skip_pair(&err) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let token1 = match contract.token_1().call().await {
        Ok(token) => token,
        Err(err) if should_skip_pair(&err) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(Some(UniV2PairState {
        token0,
        token1,
        reserve0: U256::from(reserve0),
        reserve1: U256::from(reserve1),
    }))
}

fn should_skip_pair<M: Middleware>(err: &ContractError<M>) -> bool {
    let msg = err.to_string();
    msg.contains("execution reverted")
        || msg.contains("Invalid name")
        || msg.contains("empty bytes")
}

pub fn quote_exact_input_from_state(
    state: &UniV2PairState,
    token_in: Address,
    amount_in: U256,
    fee_bps: u32,
) -> Result<Option<UniV2Quote>> {
    if amount_in.is_zero() {
        return Ok(None);
    }

    let (reserve_in, reserve_out) = match state.reserves_for(token_in) {
        Some(reserves) => reserves,
        None => return Ok(None),
    };

    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Ok(None);
    }

    let fee_den = U256::from(10_000u64);
    let fee_num = U256::from(10_000u64.saturating_sub(fee_bps as u64));
    if fee_num.is_zero() {
        return Err(anyhow!("fee basis points must be less than 10_000"));
    }

    let amount_in_with_fee = amount_in * fee_num / fee_den;
    if amount_in_with_fee.is_zero() {
        return Ok(None);
    }

    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in + amount_in_with_fee;
    if denominator.is_zero() {
        return Ok(None);
    }

    let amount_out = numerator / denominator;
    if amount_out.is_zero() {
        return Ok(None);
    }

    let price_impact_denom = reserve_in.saturating_add(amount_in);
    let price_impact_bps_u256 = if price_impact_denom.is_zero() {
        U256::zero()
    } else {
        mul_div(amount_in, U256::from(10_000u64), price_impact_denom)
    };
    let price_impact_bps = u32::try_from(price_impact_bps_u256.as_u64()).unwrap_or(u32::MAX);

    Ok(Some(UniV2Quote {
        amount_out,
        price_impact_bps,
    }))
}
