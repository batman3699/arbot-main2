//! The pure half of the concentrated-liquidity simulator: the pool state the
//! math operates on, and the single-tick exact-input quote.
//!
//! Split out of `arb-exec`'s `cl_sim` in Phase 2 (PLAN.md §33 Phase 2, scope
//! correction). `cl_sim` declared `ClPoolState` because it is what *fetches*
//! it, but `cl_swap`'s pure swap loop and `cl_ticks`'s ladder builder are what
//! *use* it, and that single misplaced type was the whole `cl_swap -> cl_sim`
//! edge — the edge that made the originally-specified crate split a cycle.
//! The loader (`load_cl_pool_state`, the `abigen!` bindings, the env gates)
//! stays behind and moves to `apex-venues`.
//!
//! Nothing here touches a provider. That is load-bearing: `apex-math` depends
//! on `ethers-core`, not `ethers`, so a provider type cannot be named in this
//! crate even by accident.

use anyhow::{anyhow, Result};
use ethers_core::types::{Address, U256};

#[derive(Clone, Debug, Default)]
pub struct ClPoolState {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    /// Current tick from `slot0`. Drives ladder navigation in `cl_swap`.
    pub tick: i32,
    /// Pool tick spacing. Drives bitmap word/bit decomposition in `cl_ticks`.
    pub tick_spacing: i32,
    /// Swap fee in hundredths of a bip (UniV3 fee tier or on-chain fee()).
    pub fee_ppm: u32,
    /// The pool's ACTUAL holding of token0/token1, from `balanceOf`, read at the
    /// same block as the rest of this state. `None` when the read failed.
    ///
    /// This is the only sound capacity source for a CL edge. `liquidity` with
    /// `sqrt_price_x96` yields the VIRTUAL constant-product reserves (`L/sqrt(P)`,
    /// `L*sqrt(P)`) of the curve the pool is tangent to at the current price —
    /// that curve runs 0..infinity, far outside the ticks actually holding
    /// liquidity, and measured on Base it overstates real holdings by 16-56x on
    /// deep WETH/USDC and by orders of magnitude on thin pools. A pool can never
    /// pay out more than it holds, so the balance is the honest bound.
    ///
    /// `None` must FAIL CLOSED at the call site: keep the probe-derived capacity
    /// for that edge rather than inventing depth.
    pub balance0: Option<U256>,
    pub balance1: Option<U256>,
}

/// Minimal single-tick exact-input quote.
///
/// Holds liquidity CONSTANT and does NOT cross ticks — `state.tick` and
/// `state.tick_spacing` are ignored. For any swap large enough to cross a tick
/// boundary the result is systematically OPTIMISTIC. Callers compensate with
/// `ARBOT_CL_TICK_BUFFER_BPS` (see `plan.rs::hop_expected_out`). Prefer
/// `cl_swap::quote_exact_input_multi_tick` where a `TickLadder` is available.
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
            ..Default::default()
        };
        let out = quote_exact_input_single_tick(&state, U256::from(1_000_000u64), true, 3_000)
            .unwrap()
            .expect("quote");
        assert!(out > U256::zero());
    }

}
