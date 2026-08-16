//! Tick data acquisition behind a swappable source.
//!
//! The swap loop in `cl_swap` is pure; everything that needs the network lives
//! here. `TickDataSource` is the seam: `RpcTickSource` today, a local-node
//! source later, `StaticTickSource` in tests — none of which the math layer
//! can distinguish.

use anyhow::Result;
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
}
