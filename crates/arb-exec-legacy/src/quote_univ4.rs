use anyhow::Result;
use ethers::types::{U256, U512};

pub struct Univ4Quote {
    pub amount_out: U256,
}

pub fn quote_fixed_price_exact_input(
    sqrt_price_x96: U256,
    amount_in: U256,
    fee: u32,
    zero_for_one: bool,
) -> Result<Option<Univ4Quote>> {
    if amount_in.is_zero() || sqrt_price_x96.is_zero() {
        return Ok(None);
    }

    let fee_den = U256::from(1_000_000u64);
    let fee_num = U256::from(1_000_000u64.saturating_sub(fee as u64));
    if fee_num.is_zero() {
        return Ok(None);
    }
    let amount_in_with_fee = amount_in * fee_num / fee_den;
    if amount_in_with_fee.is_zero() {
        return Ok(None);
    }

    let sqrt_u512 = U512::from(sqrt_price_x96);
    let price_x192 = sqrt_u512 * sqrt_u512;
    let q192 = U512::from(1u128) << 192;
    let amount_in_u512 = U512::from(amount_in_with_fee);

    let amount_out = if zero_for_one {
        amount_in_u512
            .checked_mul(price_x192)
            .and_then(|v| v.checked_div(q192))
    } else {
        amount_in_u512
            .checked_mul(q192)
            .and_then(|v| v.checked_div(price_x192))
    }
    .unwrap_or_default();

    let amount_out_u256 = U256::try_from(amount_out).unwrap_or_default();
    Ok(Some(Univ4Quote {
        amount_out: amount_out_u256,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_at_unit_price_when_sqrt_price_is_one() {
        let sqrt_price_x96 = U256::from(1u128) << 96;
        let amount_in = U256::from(1_000_000u64);
        let quote =
            quote_fixed_price_exact_input(sqrt_price_x96, amount_in, 0, true).expect("quote");
        let quote = quote.expect("quote value");
        assert_eq!(quote.amount_out, amount_in);
    }

    #[test]
    fn applies_fee_to_quote() {
        let sqrt_price_x96 = U256::from(1u128) << 96;
        let amount_in = U256::from(1_000_000u64);
        let quote =
            quote_fixed_price_exact_input(sqrt_price_x96, amount_in, 3_000, true).expect("quote");
        let quote = quote.expect("quote value");
        assert_eq!(quote.amount_out, U256::from(997_000u64));
    }
}
