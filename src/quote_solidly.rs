use anyhow::{anyhow, Result};
use ethers::types::{Address, U256};

use crate::math::mul_div;

/// 1e18 fixed-point scale used by Solidly/Velodrome/Aerodrome stable-pool math.
const WAD: u128 = 1_000_000_000_000_000_000;

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
    pub stable: bool,
    /// Token decimals are required for the stable invariant, which normalizes
    /// reserves to 1e18 before applying x3y+xy3=k. Volatile pools ignore these
    /// (decimals cancel in the constant-product ratio).
    pub decimals0: u8,
    pub decimals1: u8,
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

fn wad() -> U256 {
    U256::from(WAD)
}

fn pow10(exp: u8) -> U256 {
    U256::from(10u64).pow(U256::from(exp))
}

/// Normalize a raw token amount to 1e18 fixed point given its decimals.
fn to_wad(amount: U256, decimals: u8) -> U256 {
    mul_div(amount, wad(), pow10(decimals))
}

/// De-normalize a 1e18 fixed-point amount back to raw token units.
fn from_wad(amount_wad: U256, decimals: u8) -> U256 {
    mul_div(amount_wad, pow10(decimals), wad())
}

/// Solidly stable invariant k = x^3*y + x*y^3, evaluated in 1e18 fixed point.
/// Matches Velodrome/Aerodrome `Pool._k` integer arithmetic exactly so that the
/// off-chain min_out we derive agrees with on-chain execution.
fn stable_k(x: U256, y: U256) -> Option<U256> {
    let w = wad();
    let a = x.checked_mul(y)?.checked_div(w)?; // (x*y)/1e18
    let x2 = x.checked_mul(x)?.checked_div(w)?; // (x*x)/1e18
    let y2 = y.checked_mul(y)?.checked_div(w)?; // (y*y)/1e18
    let b = x2.checked_add(y2)?;
    a.checked_mul(b)?.checked_div(w) // (a*b)/1e18
}

/// f(x0, y) = x0*y*(x0^2 + y^2) in 1e18 fixed point (Velodrome `_f`).
fn stable_f(x0: U256, y: U256) -> Option<U256> {
    let w = wad();
    let a = x0.checked_mul(y)?.checked_div(w)?;
    let x0sq = x0.checked_mul(x0)?.checked_div(w)?;
    let ysq = y.checked_mul(y)?.checked_div(w)?;
    let b = x0sq.checked_add(ysq)?;
    a.checked_mul(b)?.checked_div(w)
}

/// d/dy f(x0, y) = 3*x0*y^2 + x0^3 in 1e18 fixed point (Velodrome `_d`).
fn stable_d(x0: U256, y: U256) -> Option<U256> {
    let w = wad();
    let ysq = y.checked_mul(y)?.checked_div(w)?;
    let term1 = U256::from(3u64)
        .checked_mul(x0)?
        .checked_mul(ysq)?
        .checked_div(w)?;
    let x0sq = x0.checked_mul(x0)?.checked_div(w)?;
    let term2 = x0sq.checked_mul(x0)?.checked_div(w)?;
    term1.checked_add(term2)
}

/// Newton's method solve for the new output reserve `y` given the post-trade
/// input reserve `x0` and invariant `xy`. Mirrors Velodrome `Pool._get_y`.
fn stable_get_y(x0: U256, xy: U256, y_start: U256) -> Option<U256> {
    let w = wad();
    let mut y = y_start;
    for _ in 0..255 {
        let k = stable_f(x0, y)?;
        let y_prev = y;
        if k < xy {
            let d = stable_d(x0, y)?;
            if d.is_zero() {
                return None;
            }
            let dy = (xy - k).checked_mul(w)?.checked_div(d)?;
            y = y.checked_add(dy)?;
        } else {
            let d = stable_d(x0, y)?;
            if d.is_zero() {
                return None;
            }
            let dy = (k - xy).checked_mul(w)?.checked_div(d)?;
            y = y.saturating_sub(dy);
        }
        if y > y_prev {
            if y - y_prev <= U256::one() {
                return Some(y);
            }
        } else if y_prev - y <= U256::one() {
            return Some(y);
        }
    }
    Some(y)
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

    if state.stable {
        return quote_stable(state, token_in, amount_in_with_fee);
    }

    // Volatile pools use the constant-product (x*y=k) curve. Decimals cancel in
    // the ratio, so raw reserves are used directly.
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

/// Correct Solidly/Velodrome stable-swap quote (x^3*y + x*y^3 = k) with reserves
/// normalized to 1e18. `amount_in_with_fee` is the raw (token-decimal) input AFTER
/// the swap fee has been applied.
fn quote_stable(
    state: &SolidlyPairState,
    token_in: Address,
    amount_in_with_fee: U256,
) -> Result<Option<SolidlyQuote>> {
    let (dec_in, dec_out) = if token_in == state.token0 {
        (state.decimals0, state.decimals1)
    } else {
        (state.decimals1, state.decimals0)
    };

    // Invariant from current reserves, each normalized to 1e18.
    let r0n = to_wad(state.reserve0, state.decimals0);
    let r1n = to_wad(state.reserve1, state.decimals1);
    let xy = match stable_k(r0n, r1n) {
        Some(k) if !k.is_zero() => k,
        _ => return Ok(None),
    };

    let (reserve_in_n, reserve_out_n) = if token_in == state.token0 {
        (r0n, r1n)
    } else {
        (r1n, r0n)
    };
    let amount_in_n = to_wad(amount_in_with_fee, dec_in);
    if amount_in_n.is_zero() {
        return Ok(None);
    }

    let new_reserve_in = match reserve_in_n.checked_add(amount_in_n) {
        Some(v) => v,
        None => return Ok(None),
    };
    let y_after = match stable_get_y(new_reserve_in, xy, reserve_out_n) {
        Some(y) => y,
        None => return Ok(None),
    };
    let out_n = reserve_out_n.saturating_sub(y_after);
    if out_n.is_zero() {
        return Ok(None);
    }

    let amount_out = from_wad(out_n, dec_out);
    if amount_out.is_zero() {
        return Ok(None);
    }

    // Conservative price-impact proxy: pure-curve slippage of the (post-fee) input
    // vs output in normalized units. ~0 for a balanced pool near peg, rising as the
    // trade pushes the pool off balance. Errs toward overstating impact (safe).
    let price_impact_bps = if out_n >= amount_in_n {
        0
    } else {
        let impact = mul_div(
            amount_in_n.saturating_sub(out_n),
            U256::from(10_000u64),
            amount_in_n,
        );
        u32::try_from(impact.as_u64()).unwrap_or(u32::MAX)
    };

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
            decimals0: 18,
            decimals1: 18,
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

    #[test]
    fn stable_pool_near_peg_has_low_impact_and_near_unit_rate() {
        // Balanced 18-decimal stable pool (e.g. DAI/USDC-style), 1M:1M.
        let one = U256::exp10(18);
        let reserve = U256::from(1_000_000u64) * one;
        let state = SolidlyPairState {
            token0: addr(1),
            token1: addr(2),
            reserve0: reserve,
            reserve1: reserve,
            stable: true,
            decimals0: 18,
            decimals1: 18,
        };
        // Trade 1,000 units (small vs 1M reserves) with a 1bps fee.
        let amount_in = U256::from(1_000u64) * one;
        let quote = quote_exact_input_from_state(&state, addr(1), amount_in, 1)
            .expect("quote result")
            .expect("quote");

        // Stable curve near peg returns ~1:1 minus fee; volatile CPMM would lose
        // far more on the same notional. Output must be very close to input.
        let lower = amount_in * U256::from(9_980u64) / U256::from(10_000u64);
        assert!(
            quote.amount_out > lower,
            "stable output {} unexpectedly low vs input {}",
            quote.amount_out,
            amount_in
        );
        assert!(quote.amount_out < amount_in, "must be below input after fee");
        assert!(
            quote.price_impact_bps < 50,
            "near-peg stable impact should be small, got {}",
            quote.price_impact_bps
        );
    }

    #[test]
    fn stable_beats_volatile_for_same_reserves() {
        // For a balanced stable pool, the stable curve must return more output than
        // the volatile constant-product curve for the same notional (lower slippage).
        let one = U256::exp10(18);
        let reserve = U256::from(1_000_000u64) * one;
        let amount_in = U256::from(50_000u64) * one;

        let stable = SolidlyPairState {
            token0: addr(1),
            token1: addr(2),
            reserve0: reserve,
            reserve1: reserve,
            stable: true,
            decimals0: 18,
            decimals1: 18,
        };
        let volatile = SolidlyPairState {
            stable: false,
            ..stable.clone()
        };

        let stable_out = quote_exact_input_from_state(&stable, addr(1), amount_in, 5)
            .unwrap()
            .unwrap()
            .amount_out;
        let volatile_out = quote_exact_input_from_state(&volatile, addr(1), amount_in, 5)
            .unwrap()
            .unwrap()
            .amount_out;

        assert!(
            stable_out > volatile_out,
            "stable curve ({stable_out}) should beat volatile ({volatile_out}) for balanced reserves"
        );
    }

    #[test]
    fn stable_handles_mixed_decimals() {
        // token0 18-dec, token1 6-dec (e.g. DAI/USDC). Balanced in real terms:
        // 1,000,000 DAI (1e24) and 1,000,000 USDC (1e12).
        let state = SolidlyPairState {
            token0: addr(1),
            token1: addr(2),
            reserve0: U256::from(1_000_000u64) * U256::exp10(18),
            reserve1: U256::from(1_000_000u64) * U256::exp10(6),
            stable: true,
            decimals0: 18,
            decimals1: 6,
        };
        // Swap 1,000 DAI -> expect ~1,000 USDC (6 decimals) minus small fee/impact.
        let amount_in = U256::from(1_000u64) * U256::exp10(18);
        let quote = quote_exact_input_from_state(&state, addr(1), amount_in, 1)
            .expect("quote result")
            .expect("quote");

        let expected_lo = U256::from(998u64) * U256::exp10(6);
        let expected_hi = U256::from(1_001u64) * U256::exp10(6);
        assert!(
            quote.amount_out > expected_lo && quote.amount_out < expected_hi,
            "cross-decimal stable out {} not within expected band",
            quote.amount_out
        );
    }
}
