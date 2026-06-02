use anyhow::{anyhow, Result};
use ethers::types::{Address, U256};

use crate::math::mul_div;

pub struct SolidlyQuote {
    pub amount_out: U256,
    pub price_impact_bps: u32,
}

#[derive(Clone, Debug)]
pub struct SolidlyPairState {
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    #[allow(dead_code)]
    pub stable: bool,
}

impl SolidlyPairState {
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

pub fn quote_exact_input_from_state(
    state: &SolidlyPairState,
    token_in: Address,
    amount_in: U256,
    fee_bps: u32,
) -> Result<Option<SolidlyQuote>> {
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

    Ok(Some(SolidlyQuote {
        amount_out,
        price_impact_bps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(id: u64) -> Address {
        Address::from_low_u64_be(id)
    }

    #[test]
    fn quotes_volatile_pool_with_fee() {
        let state = SolidlyPairState {
            token0: addr(1),
            token1: addr(2),
            reserve0: U256::from(1_000_000u64),
            reserve1: U256::from(2_000_000u64),
            stable: false,
        };
        let amount_in = U256::from(10_000u64);
        let quote = quote_exact_input_from_state(&state, addr(1), amount_in, 30)
            .expect("quote result")
            .expect("quote");

        let fee_num = U256::from(9_970u64);
        let fee_den = U256::from(10_000u64);
        let amount_in_with_fee = amount_in * fee_num / fee_den;
        let expected = amount_in_with_fee * state.reserve1 / (state.reserve0 + amount_in_with_fee);

        assert_eq!(quote.amount_out, expected);
        assert!(quote.price_impact_bps > 0);
    }
}
