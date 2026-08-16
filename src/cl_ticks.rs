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

use ethers::providers::{JsonRpcClient, Provider};
use std::sync::Arc;
use tracing::debug;

/// Selector for `tickBitmap(int16)`.
fn tick_bitmap_selector() -> [u8; 4] {
    let h = ethers::utils::keccak256(b"tickBitmap(int16)");
    [h[0], h[1], h[2], h[3]]
}

/// Selector for `ticks(int24)`.
fn ticks_selector() -> [u8; 4] {
    let h = ethers::utils::keccak256(b"ticks(int24)");
    [h[0], h[1], h[2], h[3]]
}

/// ABI-encode a signed 32-bit value into a sign-extended 32-byte word.
fn encode_signed_word(value: i32) -> [u8; 32] {
    let mut word = if value < 0 { [0xffu8; 32] } else { [0u8; 32] };
    word[28..32].copy_from_slice(&value.to_be_bytes());
    word
}

pub(crate) fn tick_bitmap_calldata(word_pos: i16) -> Vec<u8> {
    let mut call = tick_bitmap_selector().to_vec();
    call.extend_from_slice(&encode_signed_word(i32::from(word_pos)));
    call
}

pub(crate) fn ticks_calldata(tick: i32) -> Vec<u8> {
    let mut call = ticks_selector().to_vec();
    call.extend_from_slice(&encode_signed_word(tick));
    call
}

/// Decode `liquidityNet` — the second field of the 8-field `ticks()` struct —
/// as a signed 128-bit value.
///
/// Requires only `len >= 64` rather than the full 256-byte struct, so it stays
/// correct on UniV3 forks that extend the tail of `Tick.Info`. Reading the
/// FIRST field instead would silently return `liquidityGross`, which is always
/// non-negative and would make every tick crossing add liquidity.
pub fn decode_liquidity_net(data: &[u8]) -> Option<i128> {
    if data.len() < 64 {
        return None;
    }
    let word = &data[32..64];
    // int128 occupies the low 16 bytes, sign-extended across the word.
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&word[16..32]);
    Some(i128::from_be_bytes(buf))
}

/// Tick data read from chain via Multicall3.
pub struct RpcTickSource<C> {
    provider: Arc<Provider<C>>,
}

impl<C> RpcTickSource<C> {
    pub fn new(provider: Arc<Provider<C>>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<C> TickDataSource for RpcTickSource<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    async fn tick_words(
        &self,
        pool: Address,
        word_positions: &[i16],
        block: U64,
    ) -> Result<Vec<Option<U256>>> {
        if word_positions.is_empty() {
            return Ok(Vec::new());
        }
        let calls: Vec<(Address, Vec<u8>)> = word_positions
            .iter()
            .map(|w| (pool, tick_bitmap_calldata(*w)))
            .collect();
        let results =
            crate::quote_cl::multicall3_aggregate3(&self.provider, &calls, block).await?;
        Ok(results
            .into_iter()
            .map(|r| match r {
                Some(bytes) if bytes.len() >= 32 => Some(U256::from_big_endian(&bytes[..32])),
                _ => None,
            })
            .collect())
    }

    async fn liquidity_net(
        &self,
        pool: Address,
        ticks: &[i32],
        block: U64,
    ) -> Result<Vec<Option<i128>>> {
        if ticks.is_empty() {
            return Ok(Vec::new());
        }
        // Chunked so one aggregate3 stays inside node eth_call gas limits,
        // matching the 32-pool convention in `cl_sim::load_cl_pool_states_batched`.
        const TICKS_PER_BATCH: usize = 128;
        let mut out = Vec::with_capacity(ticks.len());
        for chunk in ticks.chunks(TICKS_PER_BATCH) {
            let calls: Vec<(Address, Vec<u8>)> =
                chunk.iter().map(|t| (pool, ticks_calldata(*t))).collect();
            match crate::quote_cl::multicall3_aggregate3(&self.provider, &calls, block).await {
                Ok(results) => {
                    out.extend(results.into_iter().map(|r| {
                        r.as_deref().and_then(decode_liquidity_net)
                    }));
                }
                Err(err) => {
                    debug!(
                        target: "cl_ticks",
                        error = %err,
                        ticks = chunk.len(),
                        "batched ticks() read failed; ladder will be short"
                    );
                    out.extend(std::iter::repeat(None).take(chunk.len()));
                }
            }
        }
        Ok(out)
    }
}

use dashmap::DashMap;

/// Caching decorator over any `TickDataSource`.
///
/// Keyed by `(pool, word|tick, epoch)` where `epoch = block / ttl_blocks`.
/// Tick liquidity changes only on mint/burn in that range, so an epoch of a
/// few dozen blocks trades a bounded staleness window for the removal of
/// nearly all tick RPC from the hot path. `invalidate_pool` is the escape
/// hatch when a mint/burn event is observed.
///
/// Only SUCCESSFUL reads are cached. The maps hold bare values, not
/// `Option`, because a `None` from the inner source means the read FAILED —
/// reverted, malformed, or a whole-chunk RPC error that `RpcTickSource`
/// swallows into `Ok(vec![None; 128])`. It is never a legitimate stable
/// answer: a genuinely empty bitmap word is `Some(0)` and a genuinely zero
/// net is `Some(0)`.
///
/// Caching a failure would be actively harmful. `contains_key` would report
/// the entry present, so the tick would never re-enter the `missing` set and
/// one transient hiccup would mark up to 128 ticks unreadable for the rest of
/// the epoch — silently starving that pool's ladder with no retry path, since
/// `invalidate_pool` only fires on an observed mint/burn, not on RPC health.
/// Leaving failures absent costs one re-fetch and restores the retry.
pub struct CachedTickSource<S> {
    inner: S,
    ttl_blocks: u64,
    words: DashMap<(Address, i16, u64), U256>,
    nets: DashMap<(Address, i32, u64), i128>,
}

impl<S> CachedTickSource<S> {
    pub fn new(inner: S, ttl_blocks: u64) -> Self {
        Self {
            inner,
            ttl_blocks: ttl_blocks.max(1),
            words: DashMap::new(),
            nets: DashMap::new(),
        }
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }

    pub fn cached_words(&self) -> usize {
        self.words.len()
    }

    /// Drop every cached entry for one pool. Call on an observed mint/burn.
    pub fn invalidate_pool(&self, pool: Address) {
        self.words.retain(|(p, _, _), _| *p != pool);
        self.nets.retain(|(p, _, _), _| *p != pool);
    }

    fn epoch(&self, block: U64) -> u64 {
        block.as_u64() / self.ttl_blocks
    }
}

#[async_trait]
impl<S> TickDataSource for CachedTickSource<S>
where
    S: TickDataSource,
{
    async fn tick_words(
        &self,
        pool: Address,
        word_positions: &[i16],
        block: U64,
    ) -> Result<Vec<Option<U256>>> {
        let epoch = self.epoch(block);
        let missing: Vec<i16> = word_positions
            .iter()
            .filter(|w| !self.words.contains_key(&(pool, **w, epoch)))
            .copied()
            .collect();

        if !missing.is_empty() {
            let fetched = self.inner.tick_words(pool, &missing, block).await?;
            for (w, value) in missing.iter().zip(fetched.into_iter()) {
                // Successful reads only — see the struct docstring.
                if let Some(word) = value {
                    self.words.insert((pool, *w, epoch), word);
                }
            }
        }

        Ok(word_positions
            .iter()
            .map(|w| self.words.get(&(pool, *w, epoch)).map(|v| *v))
            .collect())
    }

    async fn liquidity_net(
        &self,
        pool: Address,
        ticks: &[i32],
        block: U64,
    ) -> Result<Vec<Option<i128>>> {
        let epoch = self.epoch(block);
        let missing: Vec<i32> = ticks
            .iter()
            .filter(|t| !self.nets.contains_key(&(pool, **t, epoch)))
            .copied()
            .collect();

        if !missing.is_empty() {
            let fetched = self.inner.liquidity_net(pool, &missing, block).await?;
            for (t, value) in missing.iter().zip(fetched.into_iter()) {
                // Successful reads only — see the struct docstring. This is
                // the path that matters most: `RpcTickSource::liquidity_net`
                // turns a whole-chunk RPC error into `Ok(vec![None; 128])`, so
                // caching `None` here would poison 128 ticks per hiccup.
                if let Some(net) = value {
                    self.nets.insert((pool, *t, epoch), net);
                }
            }
        }

        Ok(ticks
            .iter()
            .map(|t| self.nets.get(&(pool, *t, epoch)).map(|v| *v))
            .collect())
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

    /// Pin the selector bytes as LITERALS, independently confirmed with
    /// `cast sig`. Asserting against `keccak256` of the same signature string
    /// the implementation uses would be circular and catch nothing: a typo in
    /// that string would change both sides together, compile fine, pass the
    /// suite, and then revert 100% of on-chain calls — degrading silently
    /// through the chunk-failure path into "the ladder is short" rather than
    /// surfacing as a failure.
    #[test]
    fn calldata_selectors_match_the_deployed_signatures() {
        assert_eq!(
            &tick_bitmap_calldata(0)[0..4],
            &[0x53, 0x39, 0xc2, 0x96],
            "tickBitmap(int16) selector must be 0x5339c296"
        );
        assert_eq!(
            &ticks_calldata(0)[0..4],
            &[0xf3, 0x0d, 0xba, 0x93],
            "ticks(int24) selector must be 0xf30dba93"
        );
    }

    #[test]
    fn tick_bitmap_calldata_encodes_signed_word_position() {
        let call = tick_bitmap_calldata(-1);
        assert_eq!(call.len(), 36, "4-byte selector + one 32-byte word");
        // int16 -1 sign-extends to all-ones.
        assert!(call[4..36].iter().all(|b| *b == 0xff), "-1 must sign-extend");

        let call = tick_bitmap_calldata(1);
        assert_eq!(call[35], 1);
        assert!(call[4..35].iter().all(|b| *b == 0), "positive word must zero-extend");
    }

    #[test]
    fn ticks_calldata_encodes_signed_tick() {
        let call = ticks_calldata(-60);
        assert_eq!(call.len(), 36);
        assert_eq!(&call[33..36], &[0xff, 0xff, 0xc4], "-60 in two's complement");
        assert!(call[4..33].iter().all(|b| *b == 0xff), "-60 must sign-extend");
    }

    /// `liquidityNet` is the SECOND field of the `ticks()` tuple. Reading the
    /// first (`liquidityGross`, always positive) instead would silently make
    /// every crossing add liquidity.
    #[test]
    fn decode_liquidity_net_reads_the_second_field_signed() {
        let mut data = vec![0u8; 256];
        // Field 0: liquidityGross = 5 (must be ignored).
        data[31] = 5;
        // Field 1: liquidityNet = -1.
        for b in data[32..64].iter_mut() {
            *b = 0xff;
        }
        assert_eq!(decode_liquidity_net(&data), Some(-1));

        let mut data = vec![0u8; 256];
        data[63] = 7;
        assert_eq!(decode_liquidity_net(&data), Some(7));
    }

    #[test]
    fn decode_liquidity_net_rejects_short_return_data() {
        assert_eq!(decode_liquidity_net(&[0u8; 32]), None);
        assert_eq!(decode_liquidity_net(&[]), None);
    }

    /// Pin the 64-byte boundary exactly. The decoder deliberately requires
    /// only the first two fields so it stays portable across UniV3 forks that
    /// extend the tail of `Tick.Info`; tightening it to the full 256 bytes
    /// would silently stop decoding those forks. 63 must fail, 64 must work.
    #[test]
    fn decode_liquidity_net_accepts_exactly_two_words() {
        assert_eq!(decode_liquidity_net(&[0u8; 63]), None, "63 bytes is short");

        let mut data = vec![0u8; 64];
        data[63] = 9;
        assert_eq!(
            decode_liquidity_net(&data),
            Some(9),
            "exactly two words must decode — do not tighten this to 256"
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSource {
        inner: StaticTickSource,
        word_calls: AtomicUsize,
        net_calls: AtomicUsize,
        /// What was actually asked of the inner source, per call. Counting
        /// invocations alone cannot distinguish "fetched only the missing
        /// tick" from "refetched everything" — both are one call.
        net_args: std::sync::Mutex<Vec<Vec<i32>>>,
    }

    #[async_trait]
    impl TickDataSource for CountingSource {
        async fn tick_words(
            &self,
            pool: Address,
            word_positions: &[i16],
            block: U64,
        ) -> Result<Vec<Option<U256>>> {
            self.word_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.tick_words(pool, word_positions, block).await
        }
        async fn liquidity_net(
            &self,
            pool: Address,
            ticks: &[i32],
            block: U64,
        ) -> Result<Vec<Option<i128>>> {
            self.net_calls.fetch_add(1, Ordering::SeqCst);
            self.net_args
                .lock()
                .expect("net_args mutex")
                .push(ticks.to_vec());
            self.inner.liquidity_net(pool, ticks, block).await
        }
    }

    fn counting_source() -> CountingSource {
        CountingSource {
            inner: StaticTickSource::new(Address::zero(), vec![(-60, 300), (60, -300)], 60),
            word_calls: AtomicUsize::new(0),
            net_calls: AtomicUsize::new(0),
            net_args: std::sync::Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn cached_source_serves_repeat_reads_without_hitting_inner() {
        let pool = Address::zero();
        let cached = CachedTickSource::new(counting_source(), 32);

        let first = cached.tick_words(pool, &[0], U64::from(1_000u64)).await.expect("first");
        let second = cached.tick_words(pool, &[0], U64::from(1_001u64)).await.expect("second");

        assert_eq!(first, second);
        assert_eq!(
            cached.inner().word_calls.load(Ordering::SeqCst),
            1,
            "a second read inside the epoch must not reach the inner source"
        );
    }

    #[tokio::test]
    async fn cached_source_refetches_after_the_epoch_rolls() {
        let pool = Address::zero();
        let cached = CachedTickSource::new(counting_source(), 32);

        cached.tick_words(pool, &[0], U64::from(1_000u64)).await.expect("first");
        cached.tick_words(pool, &[0], U64::from(1_064u64)).await.expect("two epochs later");

        assert_eq!(
            cached.inner().word_calls.load(Ordering::SeqCst),
            2,
            "crossing the epoch boundary must refetch"
        );
    }

    #[tokio::test]
    async fn invalidate_pool_forces_a_refetch() {
        let pool = Address::zero();
        let cached = CachedTickSource::new(counting_source(), 32);

        cached.tick_words(pool, &[0], U64::from(1_000u64)).await.expect("first");
        cached.invalidate_pool(pool);
        cached.tick_words(pool, &[0], U64::from(1_000u64)).await.expect("after invalidate");

        assert_eq!(cached.inner().word_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cached_source_only_fetches_the_missing_ticks() {
        let pool = Address::zero();
        let cached = CachedTickSource::new(counting_source(), 32);

        cached.liquidity_net(pool, &[-60], U64::from(1_000u64)).await.expect("first");
        let both = cached
            .liquidity_net(pool, &[-60, 60], U64::from(1_000u64))
            .await
            .expect("second");

        assert_eq!(both, vec![Some(300), Some(-300)], "cached and fresh values must merge");
        assert_eq!(cached.inner().net_calls.load(Ordering::SeqCst), 2);

        // The call COUNT alone cannot prove the cache did its job — a
        // regression that refetched the whole slice on any partial miss would
        // also be 2 calls with identical merged values, and would defeat this
        // component's entire purpose. Assert what was actually requested.
        let args = cached.inner().net_args.lock().expect("net_args mutex").clone();
        assert_eq!(
            args,
            vec![vec![-60], vec![60]],
            "second call must request ONLY the uncached tick, not the full slice"
        );
    }

    /// A failed read must not be cached: `None` means the read failed, never
    /// that the value is legitimately absent. `RpcTickSource::liquidity_net`
    /// turns a whole-chunk RPC error into `Ok(vec![None; 128])`, so caching it
    /// would mark up to 128 ticks unreadable for the rest of the epoch with no
    /// retry path.
    #[tokio::test]
    async fn cached_source_retries_a_failed_read_instead_of_caching_it() {
        let pool = Address::zero();
        // Tick 999 is not seeded, so StaticTickSource yields None for it.
        let cached = CachedTickSource::new(counting_source(), 32);

        let first = cached.liquidity_net(pool, &[999], U64::from(1_000u64)).await.expect("first");
        assert_eq!(first, vec![None], "unseeded tick reads as a failure");

        let second = cached.liquidity_net(pool, &[999], U64::from(1_000u64)).await.expect("second");
        assert_eq!(second, vec![None]);

        assert_eq!(
            cached.inner().net_calls.load(Ordering::SeqCst),
            2,
            "a failed read must be retried within the epoch, not served from cache"
        );
    }
}
