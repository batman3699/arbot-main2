//! Local concentrated-liquidity exact-input simulator (slot0 / liquidity / ticks).
//!
//! Gated by `ARBOT_LOCAL_CL_QUOTES=1` (default on). When disabled or state is
//! incomplete, callers fall back to on-chain quoter RPC.

use anyhow::{Context, Result};
use ethers::{
    prelude::*,
    providers::JsonRpcClient,
    types::{Address, BlockId, BlockNumber, U256, U64},
};
use std::sync::Arc;
use tracing::debug;

// The pure half of this module moved to `apex_math::cl_state` in Phase 2:
// `ClPoolState` is what `cl_swap` and `cl_ticks` OPERATE on, and declaring it
// here — in the module that merely fetches it — was the whole `cl_swap ->
// cl_sim` edge that made the originally-specified crate split a cycle
// (PLAN.md §33 Phase 2, scope correction). Re-exported so every
// `crate::cl_sim::ClPoolState` in this crate keeps resolving.
// `validate_pool` is not re-exported: nothing in this crate calls it, and a
// re-export nobody uses is one more name to grep through during retirement.
// It lives at `apex_math::cl_state::validate_pool`.
pub use apex_math::cl_state::{quote_exact_input_single_tick, ClPoolState};

abigen!(
    IClPoolState,
    r#"[
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked)
        function liquidity() external view returns (uint128)
        function tickSpacing() external view returns (int24)
        function fee() external view returns (uint24)
    ]"#,
);

/// Serialises tests that mutate `ARBOT_LOCAL_CL_QUOTES`. The env is process
/// global, so a test flipping it races any concurrent test that reads it.
/// Lives here (not in main.rs's test module) because `cl_sim` compiles into both
/// the lib and bin targets, and the lib cannot see bin-only items.
#[cfg(test)]
pub(crate) static CL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that restores `ARBOT_CL_MULTI_TICK` to its pre-guard value when
/// dropped — including when the drop happens during panic unwinding.
///
/// `CL_ENV_LOCK` only serialises access to the var across tests; it does
/// nothing about a single test that panics on an assertion between
/// `set_var`/`remove_var` and its intended trailing cleanup. Without this
/// guard that leaves the flag set (or cleared) for whichever test the process
/// happens to run next, which is a spurious, cascading, hard-to-reproduce
/// failure — not a real bug in the code under test. `Drop::drop` runs on
/// unwind, so constructing this guard right after taking `CL_ENV_LOCK` makes
/// cleanup unconditional.
///
/// Lives here (not in main.rs's test module), following `CL_ENV_LOCK`, so
/// both `cl_sim`'s own tests and `plan`'s tests can share one implementation.
/// Callers must still take `CL_ENV_LOCK` themselves first — this guard
/// governs value restoration, not cross-test serialisation.
#[cfg(test)]
pub(crate) struct MultiTickEnvGuard {
    previous: Option<String>,
}

#[cfg(test)]
impl MultiTickEnvGuard {
    /// Snapshot the current value, then set `ARBOT_CL_MULTI_TICK = value`.
    pub(crate) fn set(value: &str) -> Self {
        let previous = std::env::var("ARBOT_CL_MULTI_TICK").ok();
        std::env::set_var("ARBOT_CL_MULTI_TICK", value);
        Self { previous }
    }

    /// Snapshot the current value, then remove `ARBOT_CL_MULTI_TICK`.
    pub(crate) fn cleared() -> Self {
        let previous = std::env::var("ARBOT_CL_MULTI_TICK").ok();
        std::env::remove_var("ARBOT_CL_MULTI_TICK");
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for MultiTickEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("ARBOT_CL_MULTI_TICK", value),
            None => std::env::remove_var("ARBOT_CL_MULTI_TICK"),
        }
    }
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
/// This issues `6 * pools` sub-calls inside a single `aggregate3`, chunked so
/// one batch stays within node `eth_call` gas limits. Pools whose sub-calls
/// revert or return zero liquidity are simply absent from the result, and the
/// caller falls back to the per-pool path for those.
/// `pools` entries are `(pool, fee_hint, token0, token1)`. The token addresses
/// are needed for the `balanceOf` sub-calls that give each pool's REAL depth —
/// see [`ClPoolState::balance0`]. Pass `Address::zero()` for a token to skip its
/// balance read; the state then carries `None` and the caller must fail closed.
pub async fn load_cl_pool_states_batched<C>(
    provider: Arc<Provider<C>>,
    pools: &[(Address, Option<u32>, Address, Address)],
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

    // balanceOf(address) — the pool's real holding of each token, which is the
    // only sound capacity bound (see `ClPoolState::balance0`).
    const BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
    let balance_of = |pool: &Address| {
        let mut data = Vec::with_capacity(36);
        data.extend_from_slice(&BALANCE_OF);
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(pool.as_bytes());
        data
    };

    // 6 sub-calls per pool; 24 pools => 144 sub-calls per batch, comparable to
    // the previous 32x4=128 so node gas limits are respected.
    const POOLS_PER_BATCH: usize = 24;
    const CALLS_PER_POOL: usize = 6;
    for chunk in pools.chunks(POOLS_PER_BATCH) {
        let mut calls: Vec<(Address, Vec<u8>)> =
            Vec::with_capacity(chunk.len() * CALLS_PER_POOL);
        for (pool, _, token0, token1) in chunk {
            calls.push((*pool, slot0_sel.to_vec()));
            calls.push((*pool, liq_sel.to_vec()));
            calls.push((*pool, spacing_sel.to_vec()));
            calls.push((*pool, fee_sel.to_vec()));
            // Target is the TOKEN, argument is the pool.
            calls.push((*token0, balance_of(pool)));
            calls.push((*token1, balance_of(pool)));
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

        for (i, (pool, fee_hint, token0, token1)) in chunk.iter().enumerate() {
            let base = i * CALLS_PER_POOL;
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

            // Omitting the pool is the documented contract of this function
            // (see `ingestion.rs`: "silently omits any pool it cannot read"),
            // and every other field here already fails closed the same way.
            // Omitting the pool is the documented contract of this function
            // (see `ingestion.rs`: "silently omits any pool it cannot read"),
            // and every other field here already fails closed the same way.
            let Some(tick_spacing) = decode_tick_spacing(results.get(base + 2)) else {
                continue;
            };
            let fee_on_chain = match results.get(base + 3) {
                Some(Some(b)) if b.len() >= 32 => {
                    Some(U256::from_big_endian(&b[..32]).low_u32())
                }
                _ => None,
            };

            // Fail closed on a failed/short balance read: `None` makes the
            // caller keep its probe-derived capacity rather than invent depth.
            let decode_balance = |slot: usize, token: &Address| -> Option<U256> {
                if token.is_zero() {
                    return None;
                }
                match results.get(slot) {
                    Some(Some(b)) if b.len() >= 32 => Some(U256::from_big_endian(&b[..32])),
                    _ => None,
                }
            };
            let balance0 = decode_balance(base + 4, token0);
            let balance1 = decode_balance(base + 5, token1);

            out.insert(
                *pool,
                ClPoolState {
                    sqrt_price_x96,
                    liquidity,
                    tick,
                    tick_spacing,
                    fee_ppm: resolve_fee_ppm(fee_on_chain, *fee_hint),
                    balance0,
                    balance1,
                },
            );
        }
    }
    out
}

/// The swap fee in ppm. The CHAIN wins; the caller's hint is a fallback.
///
/// This used to be `fee_hint.or(fee_on_chain)`, which prefers the caller. That
/// is backwards, and on Slipstream it was badly wrong: `PoolRecord.fee` is the
/// venue's POOL KEY, not its fee -- a fee tier on univ3, where the two happen
/// to be the same number, but a TICK SPACING on Slipstream, where they are not
/// (see the note in `venues.rs` about resolving slipstream paths by spacing).
/// Callers pass that key straight in as `fee_hint`.
///
/// Measured on Base 2026-09-06: Slipstream pools whose inventory `fee` reads
/// 100 charge 2500 ppm on chain, 1 charges 400, another 100 charges 212. So
/// those pools were priced at 1-100 ppm against real fees of 212-2500 --
/// understating the fee by up to 24 bps PER HOP, in the direction that invents
/// profit. UniV3 and Pancake hid it: their key IS their fee, and both agreed
/// with the chain in every pool sampled.
///
/// A zero or out-of-range chain fee is treated as no answer: a decode that
/// yields 0 would otherwise price the pool as free.
fn resolve_fee_ppm(fee_on_chain: Option<u32>, fee_hint: Option<u32>) -> u32 {
    /// Assume the worst common tier when nothing reports a fee.
    ///
    /// The old default was 3_000, the MEDIAN tier -- which understates a 1%
    /// pool and so manufactures profit that is not there. If a fee must be
    /// guessed, guess the one that makes a trade least likely: overstating the
    /// fee costs an opportunity, understating it costs money.
    const PESSIMISTIC_FEE_PPM: u32 = 10_000;

    fee_on_chain
        .filter(|ppm| *ppm > 0 && *ppm < 1_000_000)
        .or(fee_hint)
        .unwrap_or(PESSIMISTIC_FEE_PPM)
}

/// The pool's tick spacing, or `None` if it did not report a usable one.
///
/// A failed read is an ABSENT pool, not a UniV3-shaped one. This used to
/// default to 60, which turns a broken `tickSpacing()` call into a
/// healthy-looking pool whose ticks are then walked on the wrong grid -- and 60
/// is positive and plausible, so the `tick_spacing <= 0` guard in
/// `cl_ticks::build_tick_ladder` never fires on it.
///
/// Measured on Base 2026-09-06: Aerodrome Slipstream pools report spacings of
/// 1, 100, 200 and 2000, and not one sampled pool used 60. Even UniV3 is not
/// uniform -- its 1bps tier is spacing 1. This loader is venue-agnostic, so 60
/// was wrong for most of what it sees.
fn decode_tick_spacing(word: Option<&Option<Vec<u8>>>) -> Option<i32> {
    let raw = word?.as_ref()?;
    if raw.len() < 32 {
        return None;
    }
    // `decode_int24` yields 0 for a short word, so this also covers a response
    // that is present but truncated.
    let spacing = decode_int24(&raw[..32]);
    (spacing > 0).then_some(spacing)
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
    if sqrt_price_x96.is_zero() {
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

    // Same rule as the batched loader above, and this path matters more: it is
    // the per-pool FALLBACK, reached precisely when the batched read already
    // failed, so an RPC fault here is the likely case rather than the rare one.
    let tick_spacing = contract
        .tick_spacing()
        .block(block_id)
        .call()
        .await
        .context("CL pool tickSpacing()")?;
    if tick_spacing <= 0 {
        return Ok(None);
    }
    let fee_on_chain = contract.fee().block(block_id).call().await.ok();
    let fee_ppm = resolve_fee_ppm(fee_on_chain, fee_hint);

    Ok(Some(ClPoolState {
        sqrt_price_x96,
        liquidity,
        tick,
        tick_spacing,
        fee_ppm,
        // Per-pool fallback reads no balances. Fail closed: the caller keeps the
        // probe-derived capacity rather than inventing depth.
        balance0: None,
        balance1: None,
    }))
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

/// Multi-tick simulation. Default OFF: it changes quoted prices on a funded
/// bot, so it stays behind a flag until `cl_parity` shows agreement with the
/// on-chain quoter.
pub fn multi_tick_enabled() -> bool {
    crate::util::env_flag("ARBOT_CL_MULTI_TICK", false)
}

/// Bitmap words fetched per side when building a ladder.
pub fn cl_ladder_words() -> usize {
    crate::util::env_parse_opt::<usize>("ARBOT_CL_LADDER_WORDS")
        .unwrap_or(2)
        .clamp(1, crate::cl_ticks::MAX_TICK_WORDS)
}

/// Ceiling on tick crossings per quote. A swap needing more is reported
/// exhausted rather than quoted, bounding worst-case loop cost.
pub fn cl_max_ticks_crossed() -> u32 {
    crate::util::env_parse_opt::<u32>("ARBOT_CL_MAX_TICKS")
        .unwrap_or(128)
        .clamp(1, 1_024)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(value: i32) -> Option<Vec<u8>> {
        let mut w = vec![0u8; 32];
        let raw = (value as u32) & 0x00ff_ffff;
        // Sign-extend across the word the way an int24 return does.
        if value < 0 {
            for b in w.iter_mut().take(29) {
                *b = 0xff;
            }
        }
        w[29] = (raw >> 16) as u8;
        w[30] = (raw >> 8) as u8;
        w[31] = raw as u8;
        Some(w)
    }

    /// The chain's fee wins over the caller's hint, because on Slipstream the
    /// hint is not a fee at all.
    ///
    /// `PoolRecord.fee` is the venue's POOL KEY. On univ3 that key IS the fee,
    /// which is why `fee_hint.or(fee_on_chain)` looked correct for years. On
    /// Slipstream the key is the TICK SPACING, so preferring it priced those
    /// pools at 1-100 ppm against real fees of 212-2500 ppm -- understating the
    /// cost by up to 24 bps per hop, in the direction that invents profit.
    #[test]
    fn the_chain_fee_beats_a_hint_that_is_really_a_pool_key() {
        // Measured on Base 2026-09-06 (inventory `fee`, on-chain `fee()`).
        for (key, chain) in [(100u32, 2500u32), (100, 212), (1, 400)] {
            assert_eq!(
                resolve_fee_ppm(Some(chain), Some(key)),
                chain,
                "a slipstream tick spacing of {key} must not be charged as a fee"
            );
        }
        // univ3 and pancake: key and fee agree, so nothing changes for them.
        for tier in [100u32, 500, 3_000, 10_000] {
            assert_eq!(resolve_fee_ppm(Some(tier), Some(tier)), tier);
        }
        // The hint is still the fallback when the chain does not answer.
        assert_eq!(resolve_fee_ppm(None, Some(500)), 500);
    }

    /// An unreported fee must not become a cheap one.
    #[test]
    fn an_unknown_fee_is_assumed_expensive_not_median() {
        // Nothing reported: the old default was 3_000, the MEDIAN tier, which
        // understates every pool above it and so manufactures profit.
        assert_eq!(
            resolve_fee_ppm(None, None),
            10_000,
            "an unknown fee must be assumed expensive, never median"
        );
        // A zero or absurd chain answer is not an answer. Charging 0 would
        // price the pool as free, which is the most profitable lie available.
        assert_eq!(resolve_fee_ppm(Some(0), Some(500)), 500);
        assert_eq!(resolve_fee_ppm(Some(0), None), 10_000);
        assert_eq!(resolve_fee_ppm(Some(1_000_000), None), 10_000);
    }

    /// A missing tick spacing means the pool is unavailable, never that it is a
    /// 60-spacing UniV3 pool.
    ///
    /// The old `_ => 60` produced a pool that looks healthy and is then walked
    /// on the wrong tick grid. Nothing downstream catches it: 60 is positive, so
    /// `cl_ticks::build_tick_ladder`'s `tick_spacing <= 0` guard stays quiet.
    #[test]
    fn a_pool_that_reports_no_tick_spacing_is_omitted_not_defaulted() {
        // The failure modes, all of which used to become 60.
        assert_eq!(decode_tick_spacing(None), None, "call absent from results");
        assert_eq!(decode_tick_spacing(Some(&None)), None, "sub-call reverted");
        assert_eq!(
            decode_tick_spacing(Some(&Some(vec![0u8; 8]))),
            None,
            "truncated response"
        );
        assert_eq!(
            decode_tick_spacing(Some(&Some(vec![0u8; 32]))),
            None,
            "a zero spacing is not a grid"
        );
        assert_eq!(
            decode_tick_spacing(Some(&word(-60))),
            None,
            "a negative spacing is not a grid"
        );
    }

    /// Every spacing this loader actually meets on Base must survive, which is
    /// the reason a single default was wrong: measured 2026-09-06, Slipstream
    /// reports 1, 100, 200 and 2000, and UniV3's 1bps tier reports 1.
    #[test]
    fn real_venue_tick_spacings_are_all_accepted() {
        for spacing in [1, 10, 50, 60, 100, 200, 2000] {
            assert_eq!(
                decode_tick_spacing(Some(&word(spacing))),
                Some(spacing),
                "spacing {spacing} is a real venue value and must load"
            );
        }
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

    #[test]
    fn multi_tick_defaults_off() {
        let _lock = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = MultiTickEnvGuard::cleared();
        assert!(
            !multi_tick_enabled(),
            "multi-tick must stay off until the parity harness is green"
        );
    }

    #[test]
    fn multi_tick_honours_the_flag() {
        let _lock = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = MultiTickEnvGuard::set("1");
        assert!(multi_tick_enabled());
    }

    #[test]
    fn ladder_words_is_clamped_to_the_word_ceiling() {
        let _guard = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_CL_LADDER_WORDS", "999");
        assert!(cl_ladder_words() <= crate::cl_ticks::MAX_TICK_WORDS);
        std::env::remove_var("ARBOT_CL_LADDER_WORDS");
    }
}
