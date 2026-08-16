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
use tracing::debug;

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

/// Serialises tests that mutate `ARBOT_LOCAL_CL_QUOTES`. The env is process
/// global, so a test flipping it races any concurrent test that reads it.
/// Lives here (not in main.rs's test module) because `cl_sim` compiles into both
/// the lib and bin targets, and the lib cannot see bin-only items.
#[cfg(test)]
pub(crate) static CL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Selectors for the four CL pool state reads, derived rather than hardcoded so
/// a typo cannot silently produce a batch of reverting sub-calls.
fn cl_state_selectors() -> [[u8; 4]; 4] {
    let sel = |sig: &str| {
        let h = ethers::utils::keccak256(sig.as_bytes());
        [h[0], h[1], h[2], h[3]]
    };
    [
        sel("slot0()"),
        sel("liquidity()"),
        sel("tickSpacing()"),
        sel("fee()"),
    ]
}

/// Decode a 32-byte big-endian word holding a signed int24 (two's complement).
fn decode_int24(word: &[u8]) -> i32 {
    if word.len() < 32 {
        return 0;
    }
    // int24 occupies the low 3 bytes, sign-extended across the full word.
    let raw = ((word[29] as u32) << 16) | ((word[30] as u32) << 8) | (word[31] as u32);
    if raw & 0x80_0000 != 0 {
        (raw | 0xff00_0000) as i32
    } else {
        raw as i32
    }
}

/// Load CL pool state for MANY pools in one Multicall3 round-trip.
///
/// The per-pool [`load_cl_pool_state`] issues four SEQUENTIAL `eth_call`s
/// (slot0, liquidity, tickSpacing, fee). Across a hot-pool set that is the
/// dominant scan cost and the exact pattern spec §3.4 forbids: a 64-pool venue
/// cost 256 round-trips, which at a 15 req/s provider limit is ~17s of pure
/// network wait — measured populate times were 43-48s against a 200ms budget.
///
/// This issues `4 * pools` sub-calls inside a single `aggregate3`, chunked so
/// one batch stays within node `eth_call` gas limits. Pools whose sub-calls
/// revert or return zero liquidity are simply absent from the result, and the
/// caller falls back to the per-pool path for those.
pub async fn load_cl_pool_states_batched<C>(
    provider: Arc<Provider<C>>,
    pools: &[(Address, Option<u32>)],
    block: U64,
) -> std::collections::HashMap<Address, ClPoolState>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    use std::collections::HashMap;
    let mut out: HashMap<Address, ClPoolState> = HashMap::new();
    if pools.is_empty() {
        return out;
    }
    let [slot0_sel, liq_sel, spacing_sel, fee_sel] = cl_state_selectors();

    // 4 sub-calls per pool; 32 pools => 128 sub-calls per batch.
    const POOLS_PER_BATCH: usize = 32;
    for chunk in pools.chunks(POOLS_PER_BATCH) {
        let mut calls: Vec<(Address, Vec<u8>)> = Vec::with_capacity(chunk.len() * 4);
        for (pool, _) in chunk {
            calls.push((*pool, slot0_sel.to_vec()));
            calls.push((*pool, liq_sel.to_vec()));
            calls.push((*pool, spacing_sel.to_vec()));
            calls.push((*pool, fee_sel.to_vec()));
        }

        let results =
            match crate::quote_cl::multicall3_aggregate3(&provider, &calls, block).await {
                Ok(r) => r,
                Err(err) => {
                    debug!(
                        target: "cl_sim",
                        error = %err,
                        pools = chunk.len(),
                        "batched CL state read failed; callers fall back per-pool"
                    );
                    continue;
                }
            };

        for (i, (pool, fee_hint)) in chunk.iter().enumerate() {
            let base = i * 4;
            let Some(Some(slot0)) = results.get(base) else {
                continue;
            };
            if slot0.len() < 64 {
                continue;
            }
            let sqrt_price_x96 = U256::from_big_endian(&slot0[..32]);
            if sqrt_price_x96.is_zero() {
                continue;
            }
            let tick = decode_int24(&slot0[32..64]);

            let Some(Some(liq_raw)) = results.get(base + 1) else {
                continue;
            };
            if liq_raw.len() < 32 {
                continue;
            }
            let liquidity = U256::from_big_endian(&liq_raw[..32]).low_u128();
            if liquidity == 0 {
                continue;
            }

            let tick_spacing = match results.get(base + 2) {
                Some(Some(b)) if b.len() >= 32 => decode_int24(&b[..32]),
                _ => 60,
            };
            let fee_on_chain = match results.get(base + 3) {
                Some(Some(b)) if b.len() >= 32 => {
                    Some(U256::from_big_endian(&b[..32]).low_u32())
                }
                _ => None,
            };

            out.insert(
                *pool,
                ClPoolState {
                    sqrt_price_x96,
                    liquidity,
                    tick,
                    tick_spacing,
                    fee_ppm: fee_hint.or(fee_on_chain).unwrap_or(3_000),
                },
            );
        }
    }
    out
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
        // Mutating a process-global env var races any other test that reads it
        // (plan::tests exercises the CL curve path gated on this flag), so both
        // sides must take the same lock.
        let _guard = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("ARBOT_LOCAL_CL_QUOTES");
        assert!(local_cl_quotes_enabled());
        std::env::set_var("ARBOT_LOCAL_CL_QUOTES", "0");
        assert!(!local_cl_quotes_enabled());
        std::env::remove_var("ARBOT_LOCAL_CL_QUOTES");
    }

    #[test]
    fn cl_state_selectors_match_signatures() {
        // Derived, not hardcoded — a wrong selector would make every batched
        // sub-call revert and silently degrade to the per-pool fallback.
        let [slot0, liquidity, spacing, fee] = cl_state_selectors();
        assert_eq!(slot0, &ethers::utils::id("slot0()")[..4]);
        assert_eq!(liquidity, &ethers::utils::id("liquidity()")[..4]);
        assert_eq!(spacing, &ethers::utils::id("tickSpacing()")[..4]);
        assert_eq!(fee, &ethers::utils::id("fee()")[..4]);
        // Canonical UniV3 values, as a second independent check.
        assert_eq!(slot0, [0x38, 0x50, 0xc7, 0xbd]);
        assert_eq!(liquidity, [0x1a, 0x68, 0x65, 0x02]);
    }

    #[test]
    fn decode_int24_handles_negative_ticks() {
        // Ticks are int24 two's complement inside a 32-byte word. Treating a
        // negative tick as unsigned would place the pool at an absurd price.
        let mut word = [0u8; 32];

        word[29..32].copy_from_slice(&[0x00, 0x00, 0x0a]);
        assert_eq!(decode_int24(&word), 10);

        // -1 => 0xFFFFFF in the low three bytes, sign-extended above.
        for b in word.iter_mut() {
            *b = 0xff;
        }
        assert_eq!(decode_int24(&word), -1);

        // -887272 (UniV3 MIN_TICK): 2^24 - 887272 = 15889944 = 0xF27618
        let mut w2 = [0xffu8; 32];
        w2[29..32].copy_from_slice(&[0xf2, 0x76, 0x18]);
        assert_eq!(decode_int24(&w2), -887_272);

        // 887272 (UniV3 MAX_TICK) = 0x0D89E8
        let mut w3 = [0u8; 32];
        w3[29..32].copy_from_slice(&[0x0d, 0x89, 0xe8]);
        assert_eq!(decode_int24(&w3), 887_272);

        // Short/empty returndata must not panic.
        assert_eq!(decode_int24(&[0u8; 8]), 0);
        assert_eq!(decode_int24(&[]), 0);
    }
}
