//! Aerodrome v2's volatile pools: x·y=k state and its quote (Task 8.5 R24).
//!
//! # The whole state is two reserves and a fee
//!
//! A volatile pool's `Sync(reserve0, reserve1)` follows every swap, mint and
//! burn, so a pool is its last `Sync` and the fee its factory sets. The fee is
//! `getFee(pool, false)` in basis points, held as `state.fee_ppm` (× 100), and
//! re-read each head: the factory's fee manager can set one per pool.
//!
//! # Quoted as the pool quotes
//!
//! `Pool.getAmountOut`: `in -= in · fee / 10_000`, then `in · rOut / (rIn + in)`,
//! both floored. Checked to the unit against eight `getAmountOut` answers of the
//! WETH/USDC pool `0xcDAC0d6c…` at block 52,364,894 (`tests/live_cp.rs`).
//! `apex_math::quote_common::apply_swap_fee` floors `in · (1 − fee)` instead,
//! one unit less where `in · fee` is not whole, and one of those eight sees it.

use crate::live::abi::{self, selector};
use crate::live::book::{PoolSnapshot, Unloadable};
use crate::live::inventory::PoolSpec;
use alloy_primitives::{Address, B256};
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use ethers_core::types::U256;

/// A volatile pool's reserves, as its last `Sync` or `getReserves()` stated them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reserves {
    pub reserve0: U256,
    pub reserve1: U256,
}

/// What the pool pays for `amount_in`, exactly as `getAmountOut` computes it.
/// `None` for nothing in, an empty side, nothing out, or arithmetic that would
/// overflow — never a guess.
pub fn quote_out(r: &Reserves, fee_ppm: u32, amount_in: U256, zero_for_one: bool) -> Option<U256> {
    let (r_in, r_out) = if zero_for_one { (r.reserve0, r.reserve1) } else { (r.reserve1, r.reserve0) };
    if amount_in.is_zero() || r_in.is_zero() || r_out.is_zero() {
        return None;
    }
    let fee = amount_in.checked_mul(U256::from(fee_ppm))? / U256::from(1_000_000u64);
    let after_fee = amount_in.checked_sub(fee)?;
    let out = after_fee.checked_mul(r_out)? / r_in.checked_add(after_fee)?;
    (!out.is_zero()).then_some(out)
}

/// The reads one pool's state takes, in the order [`decode_state`] decodes them.
pub const READS: usize = 9;

pub fn state_calls(s: &PoolSpec) -> Vec<(Address, Vec<u8>)> {
    let factory = s.venue.factory();
    vec![
        (s.pool, abi::call0(selector::TOKEN0)),
        (s.pool, abi::call0(selector::TOKEN1)),
        (s.pool, abi::call0(selector::FACTORY)),
        (s.pool, abi::call0(selector::STABLE)),
        (s.pool, abi::call0(selector::GET_RESERVES)),
        (s.token0, abi::call0(selector::DECIMALS)),
        (s.token1, abi::call0(selector::DECIMALS)),
        (factory, abi::call_address_bool(selector::GET_FEE, s.pool, false)),
        (factory, abi::call_address(selector::IS_POOL, s.pool)),
    ]
}

/// One pool, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub reserves: Reserves,
    pub fee_ppm: u32,
    pub decimals: (u8, u8),
    pub factory: Address,
}

/// A `getFee` answer, basis points, as ppm.
pub fn fee_ppm_from(answer: &[u8]) -> Option<u32> {
    abi::word_uint(answer, 0, 32).and_then(|bps| u32::try_from(bps.checked_mul(100)?).ok())
}

/// One pool's answers, or which read refused it. The inventory is not trusted:
/// the tokens must be the chain's, the pool its factory's — by the pool's own
/// `factory()` **and** the factory's `isPool` — and the curve volatile.
pub fn decode_state(s: &PoolSpec, a: &[Option<Vec<u8>>]) -> Result<Loaded, Unloadable> {
    let get = |i: usize, read: &'static str| a[i].as_deref().ok_or(Unloadable::Unreadable { read });
    let token0 = abi::word_address(get(0, "token0")?, 0).ok_or(Unloadable::Unreadable { read: "token0" })?;
    let token1 = abi::word_address(get(1, "token1")?, 0).ok_or(Unloadable::Unreadable { read: "token1" })?;
    if (token0, token1) != (s.token0, s.token1) {
        return Err(Unloadable::WrongTokens { chain: (token0, token1) });
    }
    let factory = abi::word_address(get(2, "factory")?, 0).ok_or(Unloadable::Unreadable { read: "factory" })?;
    let known = abi::word_uint(get(8, "isPool")?, 0, 1).ok_or(Unloadable::Unreadable { read: "isPool" })?;
    if factory != s.venue.factory() || known != 1 {
        return Err(Unloadable::WrongFactory { factory });
    }
    if abi::word_uint(get(3, "stable")?, 0, 1).ok_or(Unloadable::Unreadable { read: "stable" })? != 0 {
        return Err(Unloadable::NotVolatile);
    }
    let reserves = get(4, "getReserves")?;
    let word = |i| abi::word_u256(reserves, i).map(apex_types::compat::u256_to_ethers);
    let reserves = Reserves {
        reserve0: word(0).ok_or(Unloadable::Unreadable { read: "getReserves" })?,
        reserve1: word(1).ok_or(Unloadable::Unreadable { read: "getReserves" })?,
    };
    if reserves.reserve0.is_zero() || reserves.reserve1.is_zero() {
        return Err(Unloadable::NoLiquidity);
    }
    let d0 = abi::word_uint(get(5, "decimals")?, 0, 8).ok_or(Unloadable::Unreadable { read: "decimals" })?;
    let d1 = abi::word_uint(get(6, "decimals")?, 0, 8).ok_or(Unloadable::Unreadable { read: "decimals" })?;
    let fee_ppm = fee_ppm_from(get(7, "getFee")?).ok_or(Unloadable::Unreadable { read: "getFee" })?;
    Ok(Loaded { reserves, fee_ppm, decimals: (d0 as u8, d1 as u8), factory })
}

/// A constant-product pool's book entry: reserves and fee, and an **empty**
/// tick state — zero price and liquidity, no balances, no ladder — so a
/// concentrated-liquidity path that missed the dispatch fails closed.
pub fn snapshot(spec: &PoolSpec, l: Loaded, code_hash: B256, block: u64) -> PoolSnapshot {
    PoolSnapshot {
        spec: spec.clone(),
        state: ClPoolState {
            sqrt_price_x96: U256::zero(),
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            fee_ppm: l.fee_ppm,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::new(Vec::new(), 0, 0),
        decimals: l.decimals,
        factory: l.factory,
        code_hash,
        block,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: Some(l.reserves),
    }
}
