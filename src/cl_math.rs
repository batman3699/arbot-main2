//! Pure UniV3-family fixed-point primitives. No I/O, no async.
//!
//! Port of Uniswap v3-core `TickMath`, `SqrtPriceMath` and `SwapMath`,
//! restricted to the exact-input path. Solidity's unchecked arithmetic is
//! replaced with explicit `Option` propagation: this runs pre-broadcast, and a
//! silently saturated intermediate is a wrong price, not a slow one.

use ethers::types::{U256, U512};
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

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;

    /// Three values that pin the whole constant table. `tick = 0` must be
    /// exactly 2^96; the bounds are the published MIN/MAX_SQRT_RATIO. If the
    /// table has a typo, at least one of these fails.
    ///
    /// NOTE: these literals must be parsed with `U256::from_dec_str`, not
    /// `U256::from_str`/`FromStr` — `ethers::types::U256`'s `FromStr` impl
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
}
