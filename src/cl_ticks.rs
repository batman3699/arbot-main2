//! Tick data acquisition behind a swappable source.
//!
//! The swap loop in `cl_swap` is pure; everything that needs the network lives
//! here. `TickDataSource` is the seam: `RpcTickSource` today, a local-node
//! source later, `StaticTickSource` in tests — none of which the math layer
//! can distinguish.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use ethers::types::{Address, U256, U64};
use std::collections::HashMap;

/// Bitmap words fetched per side of the current price. Each word covers 256
/// spacings — at spacing 60 that is 15,360 ticks, roughly a 4.6x price move,
/// so 8 words is far more range than any arbitrage-sized swap needs.
pub const MAX_TICK_WORDS: usize = 8;

/// Where a tick lives in the pool's `tickBitmap`.
///
/// `compressed = floor(tick / spacing)` — floor, not truncation. Rust's `/`
/// truncates toward zero, which puts every negative tick one word too high.
pub fn word_position(tick: i32, tick_spacing: i32) -> (i16, u8) {
    let spacing = if tick_spacing == 0 { 1 } else { tick_spacing };
    let compressed = tick.div_euclid(spacing);
    let word = compressed.div_euclid(256);
    let bit = compressed.rem_euclid(256);
    (word as i16, bit as u8)
}

/// Expand one bitmap word into the initialized ticks it marks.
pub fn ticks_in_word(word_pos: i16, bitmap: U256, tick_spacing: i32) -> Vec<i32> {
    let spacing = if tick_spacing == 0 { 1 } else { tick_spacing };
    let mut out = Vec::new();
    if bitmap.is_zero() {
        return out;
    }
    for bit in 0u32..256 {
        if !(bitmap & (U256::one() << bit)).is_zero() {
            let compressed = i32::from(word_pos) * 256 + bit as i32;
            out.push(compressed * spacing);
        }
    }
    out
}

/// Source of a pool's tick bitmap words and per-tick net liquidity.
///
/// Both methods are batched by design: a per-tick round trip at the measured
/// 240ms RTT is the cost this whole layer exists to avoid.
#[async_trait]
pub trait TickDataSource: Send + Sync {
    /// Fetch `tickBitmap(word)` for each requested word position. `None` marks
    /// a word that could not be read (reverted, malformed) — distinct from a
    /// zero word, which legitimately means "no initialized ticks here".
    async fn tick_words(
        &self,
        pool: Address,
        word_positions: &[i16],
        block: U64,
    ) -> Result<Vec<Option<U256>>>;

    /// Fetch `ticks(tick).liquidityNet` for each requested tick, in order.
    async fn liquidity_net(
        &self,
        pool: Address,
        ticks: &[i32],
        block: U64,
    ) -> Result<Vec<Option<i128>>>;
}

/// In-memory source seeded with a fixed tick table.
///
/// Exists so the ladder builder and swap loop can be tested end-to-end with
/// zero network and fully deterministic data.
pub struct StaticTickSource {
    pool: Address,
    ticks: HashMap<i32, i128>,
    tick_spacing: i32,
}

impl StaticTickSource {
    pub fn new(pool: Address, ticks: Vec<(i32, i128)>, tick_spacing: i32) -> Self {
        Self {
            pool,
            ticks: ticks.into_iter().collect(),
            tick_spacing: if tick_spacing == 0 { 1 } else { tick_spacing },
        }
    }
}

#[async_trait]
impl TickDataSource for StaticTickSource {
    async fn tick_words(
        &self,
        pool: Address,
        word_positions: &[i16],
        _block: U64,
    ) -> Result<Vec<Option<U256>>> {
        if pool != self.pool {
            return Ok(vec![None; word_positions.len()]);
        }
        let mut words: HashMap<i16, U256> = HashMap::new();
        for tick in self.ticks.keys() {
            let (word, bit) = word_position(*tick, self.tick_spacing);
            *words.entry(word).or_insert_with(U256::zero) |= U256::one() << bit;
        }
        Ok(word_positions
            .iter()
            .map(|w| Some(words.get(w).copied().unwrap_or_else(U256::zero)))
            .collect())
    }

    async fn liquidity_net(
        &self,
        pool: Address,
        ticks: &[i32],
        _block: U64,
    ) -> Result<Vec<Option<i128>>> {
        if pool != self.pool {
            return Ok(vec![None; ticks.len()]);
        }
        Ok(ticks.iter().map(|t| self.ticks.get(t).copied()).collect())
    }
}

use crate::cl_math::{MAX_TICK, MIN_TICK};
use crate::cl_sim::ClPoolState;
use crate::cl_swap::TickLadder;

/// Materialise a `TickLadder` around the pool's current price.
///
/// Fetches `words_per_side` bitmap words on each side of the current word,
/// expands the set bits into ticks, then batch-reads `liquidityNet` for those
/// ticks. Two round trips regardless of how many ticks turn up.
///
/// The ladder's bounds are derived from the words actually fetched, NOT from
/// the ticks found. A pool with sparse liquidity yields a short tick list over
/// a wide proven range, and that range is what makes `Exhausted` meaningful.
pub async fn build_ladder<S: TickDataSource + ?Sized>(
    source: &S,
    pool: Address,
    state: &ClPoolState,
    block: U64,
    words_per_side: usize,
) -> Result<TickLadder> {
    // A conforming UniV3-family pool always reports a positive tick spacing
    // (1/10/60/200). Zero or negative means this address is not a pool we can
    // model: a stale inventory entry, a proxy returning zeros, a
    // non-conforming fork. Refuse it here rather than substituting a
    // plausible-looking 1.
    //
    // This is reachable from chain data. `cl_sim::load_cl_pool_states_batched`
    // only falls back to 60 when the sub-CALL fails or returns short data — a
    // pool that successfully returns an all-zero word decodes to 0 and arrives
    // here intact. Substituting 1 would compute bitmap words for entirely the
    // wrong ticks and yield a silently meaningless ladder.
    //
    // The guard lives here, not in `word_position`/`ticks_in_word`, because
    // those are total pure functions with no way to report an error, and
    // making them panic on a zero spacing would violate the plan's "no panic
    // on chain data" constraint.
    if state.tick_spacing <= 0 {
        return Err(anyhow!(
            "pool 0x{} reports non-positive tick_spacing {}; refusing to build a ladder",
            hex::encode(pool),
            state.tick_spacing
        ));
    }
    let spacing = state.tick_spacing;
    let span = words_per_side.min(MAX_TICK_WORDS) as i32;
    let (centre_word, _) = word_position(state.tick, spacing);

    let word_positions: Vec<i16> = (-span..=span)
        .filter_map(|offset| i32::from(centre_word).checked_add(offset))
        .filter(|w| *w >= i32::from(i16::MIN) && *w <= i32::from(i16::MAX))
        .map(|w| w as i16)
        .collect();
    if word_positions.is_empty() {
        return Ok(TickLadder::new(Vec::new(), state.tick, state.tick));
    }

    let words = source.tick_words(pool, &word_positions, block).await?;

    let mut candidate_ticks: Vec<i32> = Vec::new();
    for (word_pos, word) in word_positions.iter().zip(words.iter()) {
        if let Some(bitmap) = word {
            candidate_ticks.extend(ticks_in_word(*word_pos, *bitmap, spacing));
        }
    }
    candidate_ticks.sort_unstable();
    candidate_ticks.dedup();

    // Coverage spans every tick the fetched words describe, whether or not a
    // bit was set there.
    //
    // Computed in i64 and clamped to the protocol tick range. `spacing` is
    // chain data decoded as int24 (up to 8_388_607) and `highest_word * 256 +
    // 255` reaches ~889_599, so the product overflows i32 for any spacing
    // above ~933_000 — reachable from a non-conforming pool, the same threat
    // model the tick_spacing guard above exists for. Unchecked i32 math there
    // PANICS in debug/test (overflow-checks on) and silently WRAPS in release
    // (this crate's [profile.release] does not set overflow-checks), handing
    // `TickLadder` a corrupted value as a *proven* bound. Both outcomes are
    // forbidden by this plan's global constraints. i64 has ample headroom
    // (worst case ~7.5e12 against i64::MAX ~9.2e18), and clamping is exact
    // rather than lossy: a ladder cannot cover ticks the AMM cannot represent.
    let lowest_word = i64::from(*word_positions.first().unwrap_or(&0));
    let highest_word = i64::from(*word_positions.last().unwrap_or(&0));
    let spacing_i64 = i64::from(spacing);
    let lower_bound = (lowest_word * 256 * spacing_i64)
        .clamp(i64::from(MIN_TICK), i64::from(MAX_TICK)) as i32;
    let upper_bound = ((highest_word * 256 + 255) * spacing_i64)
        .clamp(i64::from(MIN_TICK), i64::from(MAX_TICK)) as i32;

    if candidate_ticks.is_empty() {
        return Ok(TickLadder::new(Vec::new(), lower_bound, upper_bound));
    }

    let nets = source.liquidity_net(pool, &candidate_ticks, block).await?;
    let ticks: Vec<(i32, i128)> = candidate_ticks
        .into_iter()
        .zip(nets.into_iter())
        .filter_map(|(tick, net)| net.map(|n| (tick, n)))
        .filter(|(_, net)| *net != 0)
        .collect();

    Ok(TickLadder::new(ticks, lower_bound, upper_bound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::{Address, U256, U64};

    #[test]
    fn word_position_splits_compressed_tick() {
        // compressed = tick / spacing; word = compressed >> 8; bit = compressed % 256.
        assert_eq!(word_position(0, 60), (0, 0));
        assert_eq!(word_position(60, 60), (0, 1));
        assert_eq!(word_position(60 * 255, 60), (0, 255));
        assert_eq!(word_position(60 * 256, 60), (1, 0));
    }

    /// Negative ticks must floor-divide, not truncate toward zero — Solidity's
    /// `int24` compression rounds down, and truncation puts -1 in the wrong
    /// word.
    #[test]
    fn word_position_floors_for_negative_ticks() {
        assert_eq!(word_position(-60, 60), (-1, 255));
        assert_eq!(word_position(-60 * 256, 60), (-1, 0));
        assert_eq!(word_position(-60 * 257, 60), (-2, 255));
    }

    #[test]
    fn ticks_in_word_decodes_set_bits() {
        // Bits 0, 3 and 255 set.
        let bitmap = U256::one() | (U256::one() << 3) | (U256::one() << 255);
        assert_eq!(ticks_in_word(0, bitmap, 60), vec![0, 180, 60 * 255]);
    }

    #[test]
    fn ticks_in_word_is_empty_for_zero_bitmap() {
        assert!(ticks_in_word(0, U256::zero(), 60).is_empty());
    }

    #[tokio::test]
    async fn static_source_returns_seeded_ticks() {
        let pool = Address::zero();
        let src = StaticTickSource::new(pool, vec![(-60, 100), (60, -100)], 60);

        let nets = src
            .liquidity_net(pool, &[-60, 0, 60], U64::zero())
            .await
            .expect("static source never fails");
        assert_eq!(nets, vec![Some(100), None, Some(-100)]);
    }

    #[tokio::test]
    async fn static_source_sets_bits_for_seeded_ticks() {
        let pool = Address::zero();
        let src = StaticTickSource::new(pool, vec![(60, -100)], 60);

        let words = src.tick_words(pool, &[0], U64::zero()).await.expect("static source");
        let word = words[0].expect("word 0 present");
        assert!(!(word & (U256::one() << 1)).is_zero(), "tick 60 -> bit 1 must be set");
    }

    use crate::cl_sim::ClPoolState;

    fn state_at_tick_zero() -> ClPoolState {
        ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
        }
    }

    #[tokio::test]
    async fn build_ladder_collects_seeded_ticks() {
        let pool = Address::zero();
        let src = StaticTickSource::new(
            pool,
            vec![(-120, 500), (-60, 300), (60, -300), (120, -500)],
            60,
        );

        let ladder = build_ladder(&src, pool, &state_at_tick_zero(), U64::zero(), 1)
            .await
            .expect("ladder builds");

        assert_eq!(ladder.len(), 4);
        assert_eq!(
            ladder.next_initialized(0, true),
            crate::cl_swap::LadderStep::Initialized { tick: -60, liquidity_net: 300 }
        );
        assert_eq!(
            ladder.next_initialized(0, false),
            crate::cl_swap::LadderStep::Initialized { tick: 60, liquidity_net: -300 }
        );
    }

    /// The bounds must reflect the words actually fetched. Claiming wider
    /// coverage than was read reintroduces the extrapolation bug one layer up.
    #[tokio::test]
    async fn build_ladder_bounds_match_words_fetched() {
        let pool = Address::zero();
        let src = StaticTickSource::new(pool, vec![(60, -300)], 60);

        let ladder = build_ladder(&src, pool, &state_at_tick_zero(), U64::zero(), 1)
            .await
            .expect("ladder builds");

        // One word each side of word 0 => words -1..=1 => compressed -256..=511
        // => ticks -15360..=30660 at spacing 60.
        assert_eq!(ladder.lower_bound(), -256 * 60);
        assert_eq!(ladder.upper_bound(), (2 * 256 - 1) * 60);
    }

    /// A malformed pool must be refused, not silently modelled with a
    /// substituted spacing. Reachable: the batched CL state loader only falls
    /// back to 60 when the call FAILS, so a pool returning an all-zero word
    /// decodes to 0 and arrives here.
    #[tokio::test]
    async fn build_ladder_refuses_a_pool_with_non_positive_tick_spacing() {
        let pool = Address::zero();
        let src = StaticTickSource::new(pool, vec![(60, -300)], 60);
        let mut state = state_at_tick_zero();
        state.tick_spacing = 0;

        let err = build_ladder(&src, pool, &state, U64::zero(), 1)
            .await
            .expect_err("a zero tick_spacing must be refused, not silently defaulted");
        assert!(
            err.to_string().contains("tick_spacing"),
            "error must name the offending field, got: {err}"
        );

        state.tick_spacing = -60;
        assert!(
            build_ladder(&src, pool, &state, U64::zero(), 1).await.is_err(),
            "a negative tick_spacing must be refused too"
        );
    }

    /// A pool reporting an absurd but int24-representable spacing must not
    /// overflow the bounds arithmetic. At spacing 8_388_607 with 8 words per
    /// side the raw product is ~1.9e10, far past `i32::MAX`: unchecked i32
    /// math panics in debug/test and silently wraps in release.
    #[tokio::test]
    async fn build_ladder_bounds_survive_an_absurd_tick_spacing() {
        let pool = Address::zero();
        let src = StaticTickSource::new(pool, Vec::new(), 60);
        let mut state = state_at_tick_zero();
        state.tick_spacing = 8_388_607;

        let ladder = build_ladder(&src, pool, &state, U64::zero(), 8)
            .await
            .expect("an absurd spacing must clamp, not panic or error");

        assert!(
            ladder.lower_bound() >= crate::cl_math::MIN_TICK,
            "lower bound {} escaped the protocol range",
            ladder.lower_bound()
        );
        assert!(
            ladder.upper_bound() <= crate::cl_math::MAX_TICK,
            "upper bound {} escaped the protocol range",
            ladder.upper_bound()
        );
        assert!(
            ladder.lower_bound() < ladder.upper_bound(),
            "clamping must not invert the bounds"
        );
    }

    #[tokio::test]
    async fn build_ladder_survives_a_pool_with_no_initialized_ticks() {
        let pool = Address::zero();
        let src = StaticTickSource::new(pool, Vec::new(), 60);

        let ladder = build_ladder(&src, pool, &state_at_tick_zero(), U64::zero(), 1)
            .await
            .expect("ladder builds");

        assert!(ladder.is_empty());
        assert_eq!(ladder.next_initialized(0, true), crate::cl_swap::LadderStep::Exhausted);
    }
}
