use ethers::types::{U256, U512};
use std::convert::TryFrom;

#[inline]
pub fn mul_div(a: U256, b: U256, denom: U256) -> U256 {
    if denom.is_zero() {
        return U256::zero();
    }
    let product = U512::from(a).saturating_mul(U512::from(b));
    let result = product / U512::from(denom);
    match U256::try_from(result) {
        Ok(value) => value,
        Err(_) => U256::MAX,
    }
}

#[allow(dead_code)]
pub fn scale_from_wei(amount: U256, decimals: u8) -> U256 {
    if decimals == 18 {
        return amount;
    }
    let factor = U256::exp10((18u32 - decimals as u32) as usize);
    amount / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_saturates_on_overflow() {
        let max = U256::MAX;
        let result = mul_div(max, max, U256::from(1u64));
        assert_eq!(result, U256::MAX);
    }

    #[test]
    fn mul_div_returns_zero_on_zero_denominator() {
        let result = mul_div(U256::from(10u64), U256::from(5u64), U256::zero());
        assert_eq!(result, U256::zero());
    }

    #[test]
    fn scale_from_wei_respects_target_decimals() {
        let raw = U256::exp10(18);
        assert_eq!(scale_from_wei(raw, 18), raw);

        let scaled = scale_from_wei(raw, 6);
        assert_eq!(scaled, U256::exp10(6));
    }
}
