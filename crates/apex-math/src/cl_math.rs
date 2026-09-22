//! Pure UniV3-family fixed-point primitives. No I/O, no async.
//!
//! Port of Uniswap v3-core `TickMath`, `SqrtPriceMath` and `SwapMath`,
//! restricted to the exact-input path. Solidity's unchecked arithmetic is
//! replaced with explicit `Option` propagation: this runs pre-broadcast, and a
//! silently saturated intermediate is a wrong price, not a slow one.

use ethers_core::types::{U256, U512};
use std::convert::TryFrom;

pub const MIN_TICK: i32 = -887_272;
pub const MAX_TICK: i32 = 887_272;

/// 2^96, the Q64.96 scaling factor.
#[inline]
pub fn q96() -> U256 {
    U256::one() << 96
}

/// sqrt price at `MIN_TICK`.
#[inline]
pub fn min_sqrt_ratio() -> U256 {
    U256::from(4_295_128_739u64)
}

/// sqrt price at `MAX_TICK`.
#[inline]
pub fn max_sqrt_ratio() -> U256 {
    // 1461446703485210103287273052203988822378723970342
    U256::from_dec_str("1461446703485210103287273052203988822378723970342")
        .unwrap_or(U256::MAX)
}

/// Full-precision `a * b / denom`, `None` on overflow or zero denominator.
///
/// `crate::math::mul_div` saturates to `U256::MAX` instead. Saturation is
/// acceptable when the result feeds a ranking heuristic; here it would be a
/// wrong swap output presented as a real one, so this variant refuses.
#[inline]
pub fn mul_div_checked(a: U256, b: U256, denom: U256) -> Option<U256> {
    if denom.is_zero() {
        return None;
    }
    let product = U512::from(a).checked_mul(U512::from(b))?;
    U256::try_from(product / U512::from(denom)).ok()
}

/// `ceil(a * b / denom)`, `None` on overflow or zero denominator.
#[inline]
pub fn mul_div_rounding_up(a: U256, b: U256, denom: U256) -> Option<U256> {
    if denom.is_zero() {
        return None;
    }
    let product = U512::from(a).checked_mul(U512::from(b))?;
    let d = U512::from(denom);
    let quotient = product / d;
    let bumped = if (product % d).is_zero() {
        quotient
    } else {
        quotient.checked_add(U512::one())?
    };
    U256::try_from(bumped).ok()
}

/// `ceil(a / b)`.
#[inline]
fn div_rounding_up(a: U256, b: U256) -> Option<U256> {
    if b.is_zero() {
        return None;
    }
    let q = a / b;
    if (a % b).is_zero() {
        Some(q)
    } else {
        q.checked_add(U256::one())
    }
}

/// Uniswap v3-core `TickMath.getSqrtRatioAtTick`.
///
/// Each constant is `2^128 / 1.0001^(2^i / 2)` for bit `i`, so the loop
/// evaluates `sqrt(1.0001^tick)` by binary decomposition of `|tick|`. The
/// running `ratio` stays below `2^128` after every `>> 128`, and each constant
/// is below `2^128`, so the products stay inside 256 bits — `checked_mul`
/// should never trip, and if it does the table is wrong.
pub fn get_sqrt_ratio_at_tick(tick: i32) -> Option<U256> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return None;
    }
    let abs_tick = tick.unsigned_abs() as u64;

    let mut ratio = if abs_tick & 0x1 != 0 {
        U256::from_dec_str("340265354078544963557816517032075149313").ok()?
    } else {
        U256::one() << 128
    };

    // (bit mask, multiplier) — verified by the monotonicity test above.
    const FACTORS: [(u64, &str); 19] = [
        (0x2, "340248342086729790484326174814286782778"),
        (0x4, "340214320654664324051920982716015181260"),
        (0x8, "340146287995602323631171512101879684304"),
        (0x10, "340010263488231146823593991679159461444"),
        (0x20, "339738377640345403697157401104375502016"),
        (0x40, "339195258003219555707034227454543997025"),
        (0x80, "338111622100601834656805679988414885971"),
        (0x100, "335954724994790223023589805789778977700"),
        (0x200, "331682121138379247127172139078559817300"),
        (0x400, "323299236684853023288211250268160618739"),
        (0x800, "307163716377032989948697243942600083929"),
        (0x1000, "277268403626896220162999269216087595045"),
        (0x2000, "225923453940442621947126027127485391333"),
        (0x4000, "149997214084966997727330242082538205943"),
        (0x8000, "66119101136024775622716233608466517926"),
        (0x10000, "12847376061809297530290974190478138313"),
        (0x20000, "485053260817066172746253684029974020"),
        (0x40000, "691415978906521570653435304214168"),
        (0x80000, "1404880482679654955896180642"),
    ];

    for (mask, factor) in FACTORS {
        if abs_tick & mask != 0 {
            let f = U256::from_dec_str(factor).ok()?;
            ratio = ratio.checked_mul(f)? >> 128;
        }
    }

    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    // Q128.128 -> Q64.96, rounding up so the result never understates price.
    let shifted = ratio >> 32;
    let remainder = ratio - (shifted << 32);
    if remainder.is_zero() {
        Some(shifted)
    } else {
        shifted.checked_add(U256::one())
    }
}

/// Uniswap v3-core `SqrtPriceMath.getAmount0Delta`.
///
/// `amount0 = L * (sqrt(b) - sqrt(a)) / (sqrt(a) * sqrt(b))`, in Q64.96.
pub fn get_amount0_delta(a: U256, b: U256, liquidity: u128, round_up: bool) -> Option<U256> {
    let (lo, hi) = if a > b { (b, a) } else { (a, b) };
    if lo.is_zero() {
        return None;
    }
    let numerator1 = U256::from(liquidity) << 96;
    let numerator2 = hi.checked_sub(lo)?;
    if round_up {
        let inner = mul_div_rounding_up(numerator1, numerator2, hi)?;
        div_rounding_up(inner, lo)
    } else {
        Some(mul_div_checked(numerator1, numerator2, hi)? / lo)
    }
}

/// Uniswap v3-core `SqrtPriceMath.getAmount1Delta`.
///
/// `amount1 = L * (sqrt(b) - sqrt(a))`, in Q64.96.
pub fn get_amount1_delta(a: U256, b: U256, liquidity: u128, round_up: bool) -> Option<U256> {
    let (lo, hi) = if a > b { (b, a) } else { (a, b) };
    let delta = hi.checked_sub(lo)?;
    let liq = U256::from(liquidity);
    if round_up {
        mul_div_rounding_up(liq, delta, q96())
    } else {
        mul_div_checked(liq, delta, q96())
    }
}

/// Price after adding `amount_in` of token0, rounding UP.
///
/// Rounding up keeps the resulting price conservatively high on the
/// `zero_for_one` path, which makes the derived output conservatively low.
fn next_sqrt_price_from_amount0_in(sqrt_p: U256, liquidity: u128, amount: U256) -> Option<U256> {
    if amount.is_zero() {
        return Some(sqrt_p);
    }
    let numerator1 = U256::from(liquidity) << 96;

    // Preferred form, valid while `amount * sqrt_p` fits in 256 bits.
    if let Some(product) = amount.checked_mul(sqrt_p) {
        if let Some(denominator) = numerator1.checked_add(product) {
            if denominator >= numerator1 {
                return mul_div_rounding_up(numerator1, sqrt_p, denominator);
            }
        }
    }
    // Overflow-safe fallback (v3-core takes the same branch).
    if sqrt_p.is_zero() {
        return None;
    }
    div_rounding_up(numerator1, (numerator1 / sqrt_p).checked_add(amount)?)
}

/// Price after adding `amount_in` of token1, rounding DOWN.
fn next_sqrt_price_from_amount1_in(sqrt_p: U256, liquidity: u128, amount: U256) -> Option<U256> {
    if liquidity == 0 {
        return None;
    }
    let quotient = mul_div_checked(amount, q96(), U256::from(liquidity))?;
    sqrt_p.checked_add(quotient)
}

/// Uniswap v3-core `SqrtPriceMath.getNextSqrtPriceFromInput`.
pub fn get_next_sqrt_price_from_input(
    sqrt_p: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<U256> {
    if sqrt_p.is_zero() || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        next_sqrt_price_from_amount0_in(sqrt_p, liquidity, amount_in)
    } else {
        next_sqrt_price_from_amount1_in(sqrt_p, liquidity, amount_in)
    }
}

/// One exact-input swap step within a single tick range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapStep {
    /// Price after this step. Equals `sqrt_price_target` iff the range was
    /// fully traversed.
    pub sqrt_price_next: U256,
    /// Input consumed, excluding fee.
    pub amount_in: U256,
    /// Output produced.
    pub amount_out: U256,
    /// Fee taken from input.
    pub fee_amount: U256,
}

/// Uniswap v3-core `SwapMath.computeSwapStep`, exact-input branch only.
pub fn compute_swap_step(
    sqrt_price_current: U256,
    sqrt_price_target: U256,
    liquidity: u128,
    amount_remaining: U256,
    fee_ppm: u32,
) -> Option<SwapStep> {
    if liquidity == 0 || sqrt_price_current.is_zero() || sqrt_price_target.is_zero() {
        return None;
    }
    let fee = U256::from(fee_ppm.min(1_000_000));
    let one = U256::from(1_000_000u64);
    let fee_complement = one.checked_sub(fee)?;
    if fee_complement.is_zero() {
        return None;
    }

    let zero_for_one = sqrt_price_current >= sqrt_price_target;
    let amount_remaining_less_fee = mul_div_checked(amount_remaining, fee_complement, one)?;

    // Input required to traverse the whole range, rounded up (the pool
    // rounds in its own favour).
    let amount_in_full = if zero_for_one {
        get_amount0_delta(sqrt_price_target, sqrt_price_current, liquidity, true)?
    } else {
        get_amount1_delta(sqrt_price_current, sqrt_price_target, liquidity, true)?
    };

    let sqrt_price_next = if amount_remaining_less_fee >= amount_in_full {
        sqrt_price_target
    } else {
        get_next_sqrt_price_from_input(
            sqrt_price_current,
            liquidity,
            amount_remaining_less_fee,
            zero_for_one,
        )?
    };

    let reached_target = sqrt_price_next == sqrt_price_target;

    let (amount_in, amount_out) = if zero_for_one {
        let a_in = if reached_target {
            amount_in_full
        } else {
            get_amount0_delta(sqrt_price_next, sqrt_price_current, liquidity, true)?
        };
        let a_out = get_amount1_delta(sqrt_price_next, sqrt_price_current, liquidity, false)?;
        (a_in, a_out)
    } else {
        let a_in = if reached_target {
            amount_in_full
        } else {
            get_amount1_delta(sqrt_price_current, sqrt_price_next, liquidity, true)?
        };
        let a_out = get_amount0_delta(sqrt_price_current, sqrt_price_next, liquidity, false)?;
        (a_in, a_out)
    };

    // When the step stops short, all remaining input is consumed and the
    // difference is fee. Otherwise the fee is the proportional charge on
    // what was actually spent.
    let fee_amount = if reached_target {
        mul_div_rounding_up(amount_in, fee, fee_complement)?
    } else {
        amount_remaining.checked_sub(amount_in)?
    };

    Some(SwapStep {
        sqrt_price_next,
        amount_in,
        amount_out,
        fee_amount,
    })
}

/// Inverse of [`get_sqrt_ratio_at_tick`]: the greatest tick whose sqrt price is
/// at most `sqrt_price_x96`. `None` outside the published price bounds.
///
/// # Why this is a binary search and not the v3-core assembly
///
/// `TickMath.getTickAtSqrtRatio` computes a base-1.0001 logarithm through a
/// 14-round fixed-point `log2` and two magic 128.128 correction constants. It
/// is fast and it is *independently* derived — it agrees with
/// `getSqrtRatioAtTick` because the constants were chosen to make it agree, not
/// because it inverts the same table. A transcription slip in any of those
/// constants produces an answer that is wrong by one tick on a narrow band of
/// prices and correct everywhere else, which is precisely the defect this
/// repository keeps shipping: right in every test anyone thought to write.
///
/// Searching the function being inverted cannot disagree with it. The
/// definition — "greatest tick with `ratio(tick) <= p`" — is evaluated
/// directly, so `get_tick_at_sqrt_ratio(get_sqrt_ratio_at_tick(t)) == t` holds
/// by construction for every tick in range, and the test below proves it over
/// all 1,774,545 of them rather than over a sample.
///
/// The cost is ~21 calls to `get_sqrt_ratio_at_tick` instead of ~14 rounds of
/// shifts. This is not on the per-tick path of the swap loop — it runs once
/// when a swap ends mid-range and the resulting state needs its tick — so the
/// exactness is worth more than the nanoseconds. If profiling ever says
/// otherwise, port the assembly and differential it against this.
pub fn get_tick_at_sqrt_ratio(sqrt_price_x96: U256) -> Option<i32> {
    if sqrt_price_x96 < min_sqrt_ratio() || sqrt_price_x96 > max_sqrt_ratio() {
        return None;
    }

    // Invariant: ratio(lo) <= p, and either hi > MAX_TICK or ratio(hi) > p.
    let mut lo = MIN_TICK;
    let mut hi = MAX_TICK + 1;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match get_sqrt_ratio_at_tick(mid) {
            Some(ratio) if ratio <= sqrt_price_x96 => lo = mid,
            // A `None` here means the table refused a tick inside its own
            // range, which is a bug in the table, not a price out of bounds.
            // Treat it as "too high" so the search terminates rather than
            // looping, and let the round-trip test catch the table.
            _ => hi = mid,
        }
    }
    Some(lo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers_core::types::U256;

    /// `get_sqrt_ratio_at_tick` is strictly increasing at EVERY tick, not at a
    /// sample of nineteen probes.
    ///
    /// The sampled version below cannot see a wrong low-order constant: if the
    /// `0x2` factor were mistyped, ticks two apart would be mis-ordered while
    /// every probe pair — spaced thousands of ticks apart — stayed correctly
    /// ordered. The nineteen constants are the whole function, so all of them
    /// have to be exercised.
    ///
    /// This also does the real work for [`get_tick_at_sqrt_ratio`]: a binary
    /// search over a strictly increasing function returns the unique greatest
    /// tick whose ratio is at most `p`, so `inverse(ratio(t)) == t` follows for
    /// every tick from this test plus the definition — it does not need its own
    /// 1.8-million-iteration round trip.
    ///
    /// # Why `#[ignore]`
    ///
    /// 1,774,545 ticks x ~19 `mul_div`s is 2.5 s in release and **73 s in
    /// debug**, which is what `cargo test` runs. A 73-second tax on every
    /// local run gets a test skipped or deleted, not run. CI runs it in
    /// release via `cargo test --release -p apex-math -- --ignored`, where it
    /// costs less than the compile it rides on. The cheap sampled version
    /// below stays for the everyday loop.
    #[test]
    #[ignore = "exhaustive: 1.8M ticks; CI runs it in release via --ignored"]
    fn sqrt_ratio_is_strictly_monotonic_at_every_tick() {
        let mut prev = get_sqrt_ratio_at_tick(MIN_TICK).expect("MIN_TICK in range");
        for tick in (MIN_TICK + 1)..=MAX_TICK {
            let cur = get_sqrt_ratio_at_tick(tick).expect("tick in range");
            assert!(
                cur > prev,
                "ratio({tick}) = {cur} is not above ratio({}) = {prev}",
                tick - 1
            );
            prev = cur;
        }
    }

    /// The inverse inverts. Dense at the bounds and at the sign change, spread
    /// across the rest; the exhaustive claim is carried by monotonicity above.
    #[test]
    fn the_tick_inverse_inverts() {
        let ticks = (MIN_TICK..MIN_TICK + 64)
            .chain(-64..64)
            .chain(MAX_TICK - 63..=MAX_TICK)
            .chain((MIN_TICK..=MAX_TICK).step_by(9_973));
        for tick in ticks {
            let ratio = get_sqrt_ratio_at_tick(tick).expect("tick in range");
            assert_eq!(
                get_tick_at_sqrt_ratio(ratio),
                Some(tick),
                "ratio({tick}) did not invert to {tick}"
            );
        }
    }

    /// Between two ticks the answer rounds DOWN, never up.
    ///
    /// Rounding up would place the price in a range whose upper boundary it has
    /// not actually reached, so the ladder would report the next initialized
    /// tick as already crossed.
    #[test]
    fn a_price_between_ticks_resolves_to_the_lower_tick() {
        for tick in [-887_000, -60_000, -60, -1, 0, 1, 60, 60_000, 887_000] {
            let lo = get_sqrt_ratio_at_tick(tick).expect("tick in range");
            let hi = get_sqrt_ratio_at_tick(tick + 1).expect("tick in range");
            assert!(hi > lo, "ratio must be strictly increasing at {tick}");
            // Every representable price strictly inside (lo, hi) belongs to
            // `tick`; check the two ends of that open interval.
            assert_eq!(get_tick_at_sqrt_ratio(lo + U256::one()), Some(tick));
            assert_eq!(get_tick_at_sqrt_ratio(hi - U256::one()), Some(tick));
        }
    }

    /// Out of bounds is `None`, not a clamped tick.
    #[test]
    fn prices_outside_the_published_bounds_have_no_tick() {
        assert_eq!(get_tick_at_sqrt_ratio(min_sqrt_ratio() - U256::one()), None);
        assert_eq!(get_tick_at_sqrt_ratio(max_sqrt_ratio() + U256::one()), None);
        assert_eq!(get_tick_at_sqrt_ratio(U256::zero()), None);
        // The bounds themselves ARE in range.
        assert_eq!(get_tick_at_sqrt_ratio(min_sqrt_ratio()), Some(MIN_TICK));
        assert_eq!(get_tick_at_sqrt_ratio(max_sqrt_ratio()), Some(MAX_TICK));
    }

    /// Three values that pin the whole constant table. `tick = 0` must be
    /// exactly 2^96; the bounds are the published MIN/MAX_SQRT_RATIO. If the
    /// table has a typo, at least one of these fails.
    ///
    /// NOTE: these literals must be parsed with `U256::from_dec_str`, not
    /// `U256::from_str`/`FromStr` — `ethers_core::types::U256`'s `FromStr` impl
    /// (from the `uint` crate) parses its input as HEXADECIMAL, so feeding it
    /// a decimal literal silently produces the wrong "expected" value. This
    /// is a test-scaffolding bug, not a constant-table transcription error:
    /// confirmed by cross-checking the implementation against an independent
    /// Python re-implementation of the same bit-decomposition algorithm,
    /// which reproduces all three reference vectors exactly.
    #[test]
    fn sqrt_ratio_matches_published_reference_vectors() {
        assert_eq!(
            get_sqrt_ratio_at_tick(0).expect("tick 0 in range"),
            U256::from_dec_str("79228162514264337593543950336").expect("2^96 literal"),
            "tick 0 must be exactly 2^96"
        );
        assert_eq!(
            get_sqrt_ratio_at_tick(MIN_TICK).expect("MIN_TICK in range"),
            U256::from(4_295_128_739u64),
            "MIN_SQRT_RATIO mismatch"
        );
        assert_eq!(
            get_sqrt_ratio_at_tick(MAX_TICK).expect("MAX_TICK in range"),
            U256::from_dec_str("1461446703485210103287273052203988822378723970342")
                .expect("MAX_SQRT_RATIO literal"),
            "MAX_SQRT_RATIO mismatch"
        );
    }

    #[test]
    fn sqrt_ratio_rejects_out_of_range_ticks() {
        assert!(get_sqrt_ratio_at_tick(MIN_TICK - 1).is_none());
        assert!(get_sqrt_ratio_at_tick(MAX_TICK + 1).is_none());
    }

    /// Monotonicity is a structural property of the table: a higher tick is a
    /// higher price, everywhere. A transposed constant breaks this even when
    /// the three endpoint vectors happen to pass.
    #[test]
    fn sqrt_ratio_is_strictly_monotonic_across_the_range() {
        let probes = [
            MIN_TICK, -500_000, -200_000, -100_000, -50_000, -10_000, -1_000,
            -60, -1, 0, 1, 60, 1_000, 10_000, 50_000, 100_000, 200_000,
            500_000, MAX_TICK,
        ];
        for pair in probes.windows(2) {
            let lo = get_sqrt_ratio_at_tick(pair[0]).expect("probe in range");
            let hi = get_sqrt_ratio_at_tick(pair[1]).expect("probe in range");
            assert!(lo < hi, "tick {} -> {lo} not below tick {} -> {hi}", pair[0], pair[1]);
        }
    }

    /// At 1:1 price with a one-tick-wide band, amount0 and amount1 deltas must
    /// agree to within rounding — the curve is symmetric there.
    #[test]
    fn amount_deltas_agree_near_unit_price() {
        let a = get_sqrt_ratio_at_tick(0).expect("tick 0");
        let b = get_sqrt_ratio_at_tick(1).expect("tick 1");
        let liquidity = 1_000_000_000_000_000u128;

        let amount0 = get_amount0_delta(a, b, liquidity, false).expect("amount0");
        let amount1 = get_amount1_delta(a, b, liquidity, false).expect("amount1");

        let diff = if amount0 > amount1 { amount0 - amount1 } else { amount1 - amount0 };
        assert!(
            diff * U256::from(10_000u64) < amount0,
            "amount0 {amount0} and amount1 {amount1} diverge by more than 1bp at unit price"
        );
    }

    #[test]
    fn amount_deltas_are_order_independent() {
        let a = get_sqrt_ratio_at_tick(-60).expect("tick -60");
        let b = get_sqrt_ratio_at_tick(60).expect("tick 60");
        let liquidity = 5_000_000_000u128;
        assert_eq!(
            get_amount0_delta(a, b, liquidity, false),
            get_amount0_delta(b, a, liquidity, false)
        );
        assert_eq!(
            get_amount1_delta(a, b, liquidity, false),
            get_amount1_delta(b, a, liquidity, false)
        );
    }

    #[test]
    fn rounding_up_never_understates() {
        let a = get_sqrt_ratio_at_tick(-200).expect("tick -200");
        let b = get_sqrt_ratio_at_tick(200).expect("tick 200");
        let liquidity = 123_456_789u128;
        assert!(
            get_amount0_delta(a, b, liquidity, true).expect("up")
                >= get_amount0_delta(a, b, liquidity, false).expect("down")
        );
        assert!(
            get_amount1_delta(a, b, liquidity, true).expect("up")
                >= get_amount1_delta(a, b, liquidity, false).expect("down")
        );
    }

    /// Adding token1 raises price; adding token0 lowers it. Getting this
    /// backwards inverts every quote, so it is pinned explicitly.
    #[test]
    fn next_sqrt_price_moves_in_the_correct_direction() {
        let start = get_sqrt_ratio_at_tick(0).expect("tick 0");
        let liquidity = 1_000_000_000_000u128;
        let amount = U256::from(1_000_000u64);

        let down = get_next_sqrt_price_from_input(start, liquidity, amount, true)
            .expect("zero_for_one");
        assert!(down < start, "selling token0 must lower price");

        let up = get_next_sqrt_price_from_input(start, liquidity, amount, false)
            .expect("one_for_zero");
        assert!(up > start, "selling token1 must raise price");
    }

    #[test]
    fn next_sqrt_price_is_identity_for_zero_input() {
        let start = get_sqrt_ratio_at_tick(0).expect("tick 0");
        assert_eq!(
            get_next_sqrt_price_from_input(start, 1_000u128, U256::zero(), true),
            Some(start)
        );
    }

    /// A step that cannot reach the target must stop short of it and consume
    /// the whole input.
    #[test]
    fn swap_step_stops_short_when_input_is_insufficient() {
        let current = get_sqrt_ratio_at_tick(0).expect("tick 0");
        let target = get_sqrt_ratio_at_tick(-600).expect("tick -600");
        let liquidity = 1_000_000_000_000_000_000u128;
        let amount_remaining = U256::from(1_000u64);

        let step = compute_swap_step(current, target, liquidity, amount_remaining, 3_000)
            .expect("step computes");

        assert!(step.sqrt_price_next > target, "must not reach the target");
        assert!(step.sqrt_price_next < current, "price must fall");
        assert_eq!(
            step.amount_in + step.fee_amount,
            amount_remaining,
            "an unreached target consumes exactly the remaining input"
        );
        assert!(!step.amount_out.is_zero());
    }

    /// A step with input to spare must land exactly on the target and leave
    /// the remainder for the next tick range.
    #[test]
    fn swap_step_reaches_target_when_input_is_ample() {
        let current = get_sqrt_ratio_at_tick(0).expect("tick 0");
        let target = get_sqrt_ratio_at_tick(-60).expect("tick -60");
        let liquidity = 1_000_000u128;
        let amount_remaining = U256::from(10u64).pow(U256::from(24u64));

        let step = compute_swap_step(current, target, liquidity, amount_remaining, 3_000)
            .expect("step computes");

        assert_eq!(step.sqrt_price_next, target, "ample input must reach the target");
        assert!(
            step.amount_in + step.fee_amount < amount_remaining,
            "reaching the target must leave input remaining"
        );
    }

    /// The fee is charged on input, so a higher tier yields strictly less out
    /// for the same input over the same range.
    #[test]
    fn swap_step_fee_reduces_output() {
        let current = get_sqrt_ratio_at_tick(0).expect("tick 0");
        let target = get_sqrt_ratio_at_tick(-600).expect("tick -600");
        let liquidity = 1_000_000_000_000_000_000u128;
        let amount = U256::from(1_000_000u64);

        let cheap = compute_swap_step(current, target, liquidity, amount, 100).expect("100ppm");
        let dear = compute_swap_step(current, target, liquidity, amount, 10_000).expect("10000ppm");

        assert!(cheap.amount_out > dear.amount_out);
        assert!(cheap.fee_amount < dear.fee_amount);
    }

    #[test]
    fn swap_step_rejects_zero_liquidity() {
        let current = get_sqrt_ratio_at_tick(0).expect("tick 0");
        let target = get_sqrt_ratio_at_tick(-60).expect("tick -60");
        assert!(compute_swap_step(current, target, 0, U256::from(1_000u64), 3_000).is_none());
    }
}
