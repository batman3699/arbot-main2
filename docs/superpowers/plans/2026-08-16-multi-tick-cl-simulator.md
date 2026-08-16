# Multi-Tick Concentrated-Liquidity Simulator — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the constant-liquidity `quote_exact_input_single_tick` with a
tick-crossing exact-input simulator whose tick data comes from a swappable
source, so `min_out` stops being systematically optimistic and the tick-buffer
haircut can be retired.

**Architecture:** Split the problem in two. The *swap loop* is pure synchronous
math over a materialised `TickLadder` — no async, no RPC, no provider generic,
fully unit-testable offline. *Tick acquisition* sits behind an `async_trait`
`TickDataSource` with three implementations (static test double, batched RPC,
caching decorator), so moving to a local node is a constructor change and
nothing in the math layer knows the difference. Every quote reports whether it
stayed inside the ladder's proven coverage; a swap that would run past it
returns `exhausted` and callers fall back rather than extrapolating — that
extrapolation is the exact defect this plan removes.

**Tech Stack:** Rust 2021, `ethers` 2.0.14 (`U256`/`U512`/`Address`),
`async-trait` 0.1, `dashmap` 6, `anyhow`, `tracing`. All already in
`Cargo.toml`; this plan adds no dependencies.

## Global Constraints

- **New modules must be declared in BOTH `src/lib.rs` and `src/main.rs`.** They
  are separate compilation units and `cl_sim` already compiles into both. A
  module added to only one produces an "unresolved module" build failure in the
  other target.
- **No `unwrap()`/`panic!` on any value derived from chain data.** Return
  `Option`/`Result`. This code runs pre-broadcast on a funded bot.
- **Exact-input only.** Exact-output (`getNextSqrtPriceFromOutput`, the
  `add = false` branches) is out of scope; no caller needs it. Do not implement
  it "for completeness".
- **Never saturate silently in swap math.** `crate::math::mul_div` returns
  `U256::MAX` on overflow. Swap math must use the checked variant added in
  Task 1 and propagate `None`.
- **Feature flag:** `ARBOT_CL_MULTI_TICK` (default **off** until Task 9's parity
  harness is green). Read via `crate::util::env_flag("ARBOT_CL_MULTI_TICK", false)`.
- **`quote_exact_input_single_tick` is not deleted by this plan.** It remains the
  fallback when the ladder is unavailable or exhausted.
- Test command throughout: `cargo test --lib <name>` (lib target; the bin target
  duplicates these modules and does not need a separate run).

---

## Pre-Flight: correct one misleading docstring

`src/cl_sim.rs:248` currently reads:

```rust
/// Minimal single-tick exact-input quote. Returns `None` when price would cross ticks.
```

The function does **not** detect tick crossing — its only guards are direction
sanity and overflow. It silently extrapolates constant liquidity past tick
boundaries. `src/plan.rs:100-112` describes the real behaviour correctly, so the
system compensates, but this docstring will actively mislead whoever implements
this plan. Fix it in the first commit of Task 1:

```rust
/// Minimal single-tick exact-input quote.
///
/// Holds liquidity CONSTANT and does NOT cross ticks — `state.tick` and
/// `state.tick_spacing` are ignored. For any swap large enough to cross a tick
/// boundary the result is systematically OPTIMISTIC. Callers compensate with
/// `ARBOT_CL_TICK_BUFFER_BPS` (see `plan.rs::hop_expected_out`). Prefer
/// `cl_swap::quote_exact_input_multi_tick` where a `TickLadder` is available.
```

---

## File Structure

| File | Responsibility | Approx. size |
|---|---|---|
| **Create** `src/cl_math.rs` | Pure fixed-point primitives: `get_sqrt_ratio_at_tick`, `get_amount0_delta`, `get_amount1_delta`, `get_next_sqrt_price_from_input`, `compute_swap_step`, `mul_div_checked`, `mul_div_rounding_up`. No I/O, no async, no generics. | ~420 |
| **Create** `src/cl_swap.rs` | `TickLadder` (materialised tick table + coverage bounds + binary-search cursor) and `quote_exact_input_multi_tick` (the pure swap loop). Depends only on `cl_math`. | ~330 |
| **Create** `src/cl_ticks.rs` | `TickDataSource` trait, `StaticTickSource` (test double), `RpcTickSource` (batched multicall3), `CachedTickSource` (decorator), and `build_ladder`. The only async/RPC file. | ~400 |
| **Modify** `src/cl_sim.rs` | Fix the docstring; add `ARBOT_CL_MULTI_TICK` flag helper. `ClPoolState` gains no fields — `tick`/`tick_spacing` simply lose their `#[allow(dead_code)]`. | +40 |
| **Modify** `src/plan.rs:61-126` | `hop_expected_out` prefers the multi-tick quote; applies the tick buffer **only** on the single-tick fallback path. | ~+45 |
| **Modify** `src/sizing.rs:315-350` | Same preference in the local-sizing arm. | ~+30 |
| **Modify** `src/lib.rs`, `src/main.rs` | Three `mod` declarations each. | +3 each |
| **Create** `src/bin/cl_parity.rs` | Offline differential harness: multi-tick vs on-chain quoter over real pools. | ~180 |

**Why three files, not one:** the pure math is the part that must be
exhaustively unit-tested and never changes again; the tick sourcing is the part
that changes when the local node lands. Keeping them in separate modules means
the node swap cannot regress the math, and each file stays small enough to hold
in context during review.

---

### Task 1: Pure fixed-point primitives (`cl_math.rs`)

**Files:**
- Create: `src/cl_math.rs`
- Modify: `src/lib.rs` (add `pub mod cl_math;`), `src/main.rs` (add `mod cl_math;`)
- Modify: `src/cl_sim.rs:248` (docstring correction from Pre-Flight)
- Test: inline `#[cfg(test)] mod tests` in `src/cl_math.rs`

**Interfaces:**
- Consumes: `crate::math` (reference only — this module defines its own checked variant), `ethers::types::{U256, U512}`
- Produces:
  - `pub const MIN_TICK: i32 = -887272;`
  - `pub const MAX_TICK: i32 = 887272;`
  - `pub fn min_sqrt_ratio() -> U256`
  - `pub fn max_sqrt_ratio() -> U256`
  - `pub fn q96() -> U256`
  - `pub fn mul_div_checked(a: U256, b: U256, denom: U256) -> Option<U256>`
  - `pub fn mul_div_rounding_up(a: U256, b: U256, denom: U256) -> Option<U256>`
  - `pub fn get_sqrt_ratio_at_tick(tick: i32) -> Option<U256>`
  - `pub fn get_amount0_delta(a: U256, b: U256, liquidity: u128, round_up: bool) -> Option<U256>`
  - `pub fn get_amount1_delta(a: U256, b: U256, liquidity: u128, round_up: bool) -> Option<U256>`
  - `pub fn get_next_sqrt_price_from_input(sqrt_p: U256, liquidity: u128, amount_in: U256, zero_for_one: bool) -> Option<U256>`
  - `pub struct SwapStep { pub sqrt_price_next: U256, pub amount_in: U256, pub amount_out: U256, pub fee_amount: U256 }`
  - `pub fn compute_swap_step(sqrt_price_current: U256, sqrt_price_target: U256, liquidity: u128, amount_remaining: U256, fee_ppm: u32) -> Option<SwapStep>`

> **CORRECTNESS HAZARD — read before starting.** The 20 magic constants in
> `get_sqrt_ratio_at_tick` decide every price this system quotes. A single wrong
> digit produces prices that are plausible but wrong, on a bot heading for a
> funded run.
>
> **These constants were machine-verified while this plan was written.** Each
> was checked against the independent closed form `2^128 · 1.0001^(−2^(k−1))`
> (bit 0 being `2^128 · 1.0001^(−1/2)`) at 80-digit precision — all 20 agree to
> within last-place rounding. The full algorithm was then executed against the
> three published reference values and reproduced each exactly:
> `tick 0 → 2^96`, `MIN_TICK → 4295128739`,
> `MAX_TICK → 1461446703485210103287273052203988822378723970342`, and is
> strictly monotonic across 19 probes spanning the full tick range.
>
> That verification is why the constants appear here at all — but it does not
> replace the tests. Step 1 pins the same three vectors in Rust; Task 9 adds a
> live differential check against the deployed quoter, which is the only thing
> that validates the constants *and* the swap loop *and* the tick data together.
> If Step 2 fails, the transcription into Rust drifted — diff the `FACTORS`
> table against the values above before touching anything else.

- [ ] **Step 1: Write the failing reference-vector test**

Create `src/cl_math.rs` containing only the test module for now:

```rust
//! Pure UniV3-family fixed-point primitives. No I/O, no async.
//!
//! Port of Uniswap v3-core `TickMath`, `SqrtPriceMath` and `SwapMath`,
//! restricted to the exact-input path. Solidity's unchecked arithmetic is
//! replaced with explicit `Option` propagation: this runs pre-broadcast, and a
//! silently saturated intermediate is a wrong price, not a slow one.

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;

    // NOTE: parse decimal literals with `U256::from_dec_str`, NEVER with
    // `U256::from_str`/`FromStr`. `ethers::types::U256` inherits its `FromStr`
    // from the `uint` crate, which parses HEXADECIMAL. An all-digit decimal
    // string parses "successfully" as hex and yields a different number:
    // `from_str("79228162514264337593543950336")` returns
    // 39310485791873132399816509589095222, not 2^96. Verified empirically.

    /// Three values that pin the whole constant table. `tick = 0` must be
    /// exactly 2^96; the bounds are the published MIN/MAX_SQRT_RATIO. If the
    /// table has a typo, at least one of these fails.
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
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test --lib cl_math
```

Expected: FAIL to compile — `cannot find function get_sqrt_ratio_at_tick`,
`cannot find value MIN_TICK`.

- [ ] **Step 3: Implement the tick-math constants and `get_sqrt_ratio_at_tick`**

Insert above the test module in `src/cl_math.rs`:

```rust
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
```

> The constants are given in decimal rather than hex deliberately: the hex forms
> differ from each other by one or two digits in the high nibbles and transcribe
> badly, while a decimal typo shifts magnitude enough for the monotonicity test
> to catch it.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --lib cl_math
```

Expected: PASS — 3 tests. **If `sqrt_ratio_matches_published_reference_vectors`
fails, stop and diff the `FACTORS` table against Uniswap v3-core `TickMath.sol`.
Do not adjust the test to match the implementation.**

- [ ] **Step 5: Commit**

```bash
git add src/cl_math.rs src/lib.rs src/main.rs src/cl_sim.rs
git commit -m "feat(cl): add TickMath sqrt-ratio primitives with reference-vector tests"
```

- [ ] **Step 6: Write failing tests for the amount deltas**

Append to the `tests` module in `src/cl_math.rs`:

```rust
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
```

- [ ] **Step 7: Run to verify they fail**

```bash
cargo test --lib cl_math
```

Expected: FAIL to compile — `cannot find function get_amount0_delta`.

- [ ] **Step 8: Implement the amount deltas and next-price**

Append to `src/cl_math.rs`, above the tests:

```rust
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
```

- [ ] **Step 9: Run to verify they pass**

```bash
cargo test --lib cl_math
```

Expected: PASS — 8 tests.

- [ ] **Step 10: Commit**

```bash
git add src/cl_math.rs
git commit -m "feat(cl): add SqrtPriceMath amount deltas and exact-input price step"
```

- [ ] **Step 11: Write the failing `compute_swap_step` tests**

Append to the `tests` module:

```rust
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
```

- [ ] **Step 12: Run to verify they fail**

```bash
cargo test --lib cl_math
```

Expected: FAIL to compile — `cannot find function compute_swap_step`.

- [ ] **Step 13: Implement `compute_swap_step`**

Append to `src/cl_math.rs`, above the tests:

```rust
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
```

- [ ] **Step 14: Run to verify they pass**

```bash
cargo test --lib cl_math
```

Expected: PASS — 12 tests.

- [ ] **Step 15: Commit**

```bash
git add src/cl_math.rs
git commit -m "feat(cl): add exact-input computeSwapStep"
```

---

### Task 2: `TickLadder` and the pure swap loop (`cl_swap.rs`)

**Files:**
- Create: `src/cl_swap.rs`
- Modify: `src/lib.rs` (add `pub mod cl_swap;`), `src/main.rs` (add `mod cl_swap;`)
- Test: inline `#[cfg(test)] mod tests` in `src/cl_swap.rs`

**Interfaces:**
- Consumes: `crate::cl_math::{MIN_TICK, MAX_TICK, SwapStep, compute_swap_step, get_sqrt_ratio_at_tick, min_sqrt_ratio, max_sqrt_ratio}`, `crate::cl_sim::ClPoolState`
- Produces:
  - `pub struct TickLadder` with `pub fn new(ticks: Vec<(i32, i128)>, lower_bound: i32, upper_bound: i32) -> Self`, `pub fn covers(&self, tick: i32) -> bool`, `pub fn next_initialized(&self, from_tick: i32, zero_for_one: bool) -> LadderStep`, `pub fn is_empty(&self) -> bool`, `pub fn len(&self) -> usize`
  - `pub enum LadderStep { Initialized { tick: i32, liquidity_net: i128 }, Exhausted }`
  - `pub struct MultiTickQuote { pub amount_out: U256, pub amount_in_consumed: U256, pub sqrt_price_after: U256, pub ticks_crossed: u32, pub exhausted: bool }`
  - `pub fn quote_exact_input_multi_tick(state: &ClPoolState, ladder: &TickLadder, amount_in: U256, zero_for_one: bool, max_ticks: u32) -> Option<MultiTickQuote>`

> **SIGN CONVENTION — every test ladder in this plan depends on it.**
> `liquidity_net[t]` is the liquidity added when crossing tick `t` *upward*.
> A position `[tl, tu]` holding `L` contributes `net[tl] += L` and
> `net[tu] -= L`. So with price inside the position, the tick *below* carries a
> **positive** net and the tick *above* a **negative** one. The swap loop
> negates the net when `zero_for_one`, which is why crossing a
> positive-net tick downward *removes* liquidity. Inverting these signs makes
> liquidity rise as price falls, which silently turns
> `crossing_swap_is_below_the_constant_liquidity_estimate` into a failing test
> that looks like a bug in the swap loop. If that test fails, check the ladder
> data before touching the loop.
>
> **Design note for the reviewer.** A faithful port of v3-core's `swap` loop
> ends each iteration by recomputing `tick` from the new sqrt price via
> `getTickAtSqrtRatio` when the step stopped mid-range. That call is
> deliberately **omitted** here: a mid-range stop means the input is exhausted,
> so the loop terminates on the next condition check and the recomputed tick is
> never read. Skipping it keeps `getTickAtSqrtRatio` — a second 20-constant
> table, a second transcription hazard — out of the codebase entirely.
> `sqrt_price_after` is returned instead, which is what callers actually need.

- [ ] **Step 1: Write the failing ladder-navigation tests**

Create `src/cl_swap.rs`:

```rust
//! `TickLadder` + the pure multi-tick exact-input swap loop.
//!
//! Everything here is synchronous and total: given a ladder, the quote is a
//! deterministic function with no I/O. Tick acquisition lives in `cl_ticks`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cl_sim::ClPoolState;
    use ethers::types::U256;

    fn ladder() -> TickLadder {
        // Symmetric band around 0, 60-spaced, with liquidity added at the
        // inner ticks and removed at the outer ones.
        TickLadder::new(
            vec![
                (-180, 1_000_000_000),
                (-120, 2_000_000_000),
                (-60, 3_000_000_000),
                (60, -3_000_000_000),
                (120, -2_000_000_000),
                (180, -1_000_000_000),
            ],
            -180,
            180,
        )
    }

    #[test]
    fn next_initialized_walks_down_for_zero_for_one() {
        let l = ladder();
        // zero_for_one searches for the greatest initialized tick <= from.
        assert_eq!(
            l.next_initialized(0, true),
            LadderStep::Initialized { tick: -60, liquidity_net: 3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(-60, true),
            LadderStep::Initialized { tick: -60, liquidity_net: 3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(-61, true),
            LadderStep::Initialized { tick: -120, liquidity_net: 2_000_000_000 }
        );
    }

    #[test]
    fn next_initialized_walks_up_for_one_for_zero() {
        let l = ladder();
        // one_for_zero searches for the least initialized tick strictly > from.
        assert_eq!(
            l.next_initialized(0, false),
            LadderStep::Initialized { tick: 60, liquidity_net: -3_000_000_000 }
        );
        assert_eq!(
            l.next_initialized(60, false),
            LadderStep::Initialized { tick: 120, liquidity_net: -2_000_000_000 }
        );
    }

    /// Running off the end of proven coverage must be reported, never guessed.
    /// This is the defect the whole plan exists to remove.
    #[test]
    fn next_initialized_reports_exhaustion_past_coverage() {
        let l = ladder();
        assert_eq!(l.next_initialized(-181, true), LadderStep::Exhausted);
        assert_eq!(l.next_initialized(180, false), LadderStep::Exhausted);
    }

    #[test]
    fn empty_ladder_is_immediately_exhausted() {
        let l = TickLadder::new(Vec::new(), -60, 60);
        assert_eq!(l.next_initialized(0, true), LadderStep::Exhausted);
        assert_eq!(l.next_initialized(0, false), LadderStep::Exhausted);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --lib cl_swap
```

Expected: FAIL to compile — `cannot find type TickLadder`.

- [ ] **Step 3: Implement `TickLadder`**

Insert above the tests in `src/cl_swap.rs`:

```rust
use crate::cl_math::{
    compute_swap_step, get_sqrt_ratio_at_tick, max_sqrt_ratio, min_sqrt_ratio, MAX_TICK, MIN_TICK,
};
use crate::cl_sim::ClPoolState;
use ethers::types::U256;

/// Outcome of asking a ladder for the next initialized tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderStep {
    Initialized { tick: i32, liquidity_net: i128 },
    /// The search left the range this ladder is known-complete over. The
    /// caller must NOT assume constant liquidity beyond this point.
    Exhausted,
}

/// A materialised, sorted view of a pool's initialized ticks over a bounded
/// range, plus the bounds over which that view is known complete.
///
/// The bounds are the load-bearing part. A ladder that merely held ticks would
/// let a large swap walk off the end and silently continue at the last known
/// liquidity — exactly the constant-liquidity error in
/// `quote_exact_input_single_tick`. Recording where knowledge stops turns that
/// into a reported `Exhausted` and a caller-side fallback.
#[derive(Clone, Debug)]
pub struct TickLadder {
    /// `(tick, liquidity_net)` sorted ascending by tick, deduplicated.
    ticks: Vec<(i32, i128)>,
    lower_bound: i32,
    upper_bound: i32,
}

impl TickLadder {
    pub fn new(mut ticks: Vec<(i32, i128)>, lower_bound: i32, upper_bound: i32) -> Self {
        ticks.sort_unstable_by_key(|(t, _)| *t);
        ticks.dedup_by_key(|(t, _)| *t);
        Self {
            ticks,
            lower_bound: lower_bound.max(MIN_TICK),
            upper_bound: upper_bound.min(MAX_TICK),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ticks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    pub fn lower_bound(&self) -> i32 {
        self.lower_bound
    }

    pub fn upper_bound(&self) -> i32 {
        self.upper_bound
    }

    /// Whether `tick` falls inside the range this ladder proved complete.
    pub fn covers(&self, tick: i32) -> bool {
        tick >= self.lower_bound && tick <= self.upper_bound
    }

    /// Next initialized tick in the direction of travel.
    ///
    /// Mirrors v3-core's `nextInitializedTickWithinOneWord`: `zero_for_one`
    /// (price falling) searches for the greatest initialized tick `<= from`,
    /// the other direction for the least initialized tick `> from`.
    pub fn next_initialized(&self, from_tick: i32, zero_for_one: bool) -> LadderStep {
        if !self.covers(from_tick) {
            return LadderStep::Exhausted;
        }
        let found = if zero_for_one {
            self.ticks
                .iter()
                .rev()
                .find(|(t, _)| *t <= from_tick)
                .copied()
        } else {
            self.ticks.iter().find(|(t, _)| *t > from_tick).copied()
        };
        match found {
            Some((tick, liquidity_net)) if self.covers(tick) => {
                LadderStep::Initialized { tick, liquidity_net }
            }
            _ => LadderStep::Exhausted,
        }
    }
}
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test --lib cl_swap
```

Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cl_swap.rs src/lib.rs src/main.rs
git commit -m "feat(cl): add TickLadder with explicit coverage bounds"
```

- [ ] **Step 6: Write the failing swap-loop tests**

Append to the `tests` module in `src/cl_swap.rs`:

```rust
    fn pool_state(liquidity: u128, fee_ppm: u32) -> ClPoolState {
        ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity,
            tick: 0,
            tick_spacing: 60,
            fee_ppm,
        }
    }

    /// A swap small enough to stay inside the current range must agree with
    /// the single-tick model, which is exactly correct in that regime.
    ///
    /// NOTE ON MAGNITUDE: `amount_in` must be large enough that integer
    /// rounding is not the dominant term. At `amount_in = 1000` the output is
    /// ~997 units, so a single unit of last-place rounding is ~10 bps and
    /// exceeds any sane relative tolerance — the two models genuinely differ by
    /// 1 there (single-tick 997, `SwapMath` 996), and the SwapMath value is the
    /// correct one. Reaching tick -60 needs ~3.0e15 of input at this liquidity,
    /// so 1e12 is comfortably in-range and the two agree exactly. Verified
    /// numerically across 1e4/1e6/1e9/1e12: difference is 0 at every one.
    #[test]
    fn small_swap_matches_single_tick_model() {
        let state = pool_state(1_000_000_000_000_000_000, 3_000);
        let l = ladder();
        let amount_in = U256::from(1_000_000_000_000u64);

        let multi = quote_exact_input_multi_tick(&state, &l, amount_in, true, 128)
            .expect("multi-tick quote");
        let single = crate::cl_sim::quote_exact_input_single_tick(&state, amount_in, true, 3_000)
            .expect("single-tick call")
            .expect("single-tick quote");

        assert!(!multi.exhausted, "an in-range swap must not exhaust the ladder");
        assert_eq!(multi.ticks_crossed, 0, "an in-range swap crosses no ticks");

        // The multi-tick model rounds output DOWN, matching how the pool itself
        // rounds. It must therefore never quote ABOVE the single-tick estimate.
        assert!(
            multi.amount_out <= single,
            "multi-tick ({}) must never exceed the single-tick estimate ({})",
            multi.amount_out,
            single
        );

        let diff = single - multi.amount_out;
        assert!(
            diff * U256::from(1_000u64) <= single,
            "in-range multi ({}) and single ({}) must agree within 0.1%",
            multi.amount_out,
            single
        );
    }

    /// The headline regression: once a swap crosses out of the initial range,
    /// the constant-liquidity model overstates output. Falling liquidity on
    /// the way down means the multi-tick answer must be strictly smaller.
    #[test]
    fn crossing_swap_is_below_the_constant_liquidity_estimate() {
        // Liquidity DROPS as price falls through -60 then -120.
        // See the SIGN CONVENTION note above: ticks below the current price
        // carry POSITIVE liquidity_net, and crossing them downward negates
        // that, removing liquidity. Starting at 1000e9: -60 -> 500e9,
        // -120 -> 100e9, -180 -> 50e9, never reaching zero.
        let l = TickLadder::new(
            vec![(-180, 50_000_000_000), (-120, 400_000_000_000), (-60, 500_000_000_000)],
            -180,
            180,
        );
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(10u64).pow(U256::from(18u64));

        let multi = quote_exact_input_multi_tick(&state, &l, amount_in, true, 128)
            .expect("multi-tick quote");
        let single = crate::cl_sim::quote_exact_input_single_tick(&state, amount_in, true, 3_000)
            .expect("single-tick call")
            .expect("single-tick quote");

        assert!(multi.ticks_crossed >= 1, "this size must cross at least one tick");
        assert!(
            multi.amount_out < single,
            "multi-tick {} must be below the optimistic single-tick {}",
            multi.amount_out,
            single
        );
    }

    /// Walking past proven coverage must set the flag rather than extrapolate.
    #[test]
    fn swap_beyond_coverage_reports_exhaustion() {
        let l = TickLadder::new(vec![(-60, 900_000_000_000)], -60, 60);
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(10u64).pow(U256::from(24u64));

        let quote = quote_exact_input_multi_tick(&state, &l, amount_in, true, 128)
            .expect("multi-tick quote");

        assert!(quote.exhausted, "a swap past coverage must report exhaustion");
        assert!(
            quote.amount_in_consumed < amount_in,
            "an exhausted quote must not claim to have spent the whole input"
        );
    }

    /// `max_ticks` bounds the work per quote; hitting it is an exhaustion, not
    /// a silently truncated answer.
    #[test]
    fn tick_budget_is_enforced_as_exhaustion() {
        let l = TickLadder::new(
            (1..=40).map(|i| (-60 * i, 10_000_000_000i128)).collect(),
            -2_400,
            2_400,
        );
        let state = pool_state(1_000_000_000_000, 3_000);
        let amount_in = U256::from(10u64).pow(U256::from(24u64));

        let quote = quote_exact_input_multi_tick(&state, &l, amount_in, true, 3)
            .expect("multi-tick quote");

        assert!(quote.exhausted, "hitting the tick budget must report exhaustion");
        assert!(quote.ticks_crossed <= 3, "budget must be respected");
    }

    #[test]
    fn zero_input_yields_none() {
        let state = pool_state(1_000_000_000_000, 3_000);
        assert!(quote_exact_input_multi_tick(&state, &ladder(), U256::zero(), true, 128).is_none());
    }

    #[test]
    fn zero_liquidity_yields_none() {
        let state = pool_state(0, 3_000);
        assert!(
            quote_exact_input_multi_tick(&state, &ladder(), U256::from(1_000u64), true, 128)
                .is_none()
        );
    }
```

- [ ] **Step 7: Run to verify they fail**

```bash
cargo test --lib cl_swap
```

Expected: FAIL to compile — `cannot find function quote_exact_input_multi_tick`.

- [ ] **Step 8: Implement the swap loop**

Append to `src/cl_swap.rs`, above the tests:

```rust
/// Result of a multi-tick exact-input quote.
#[derive(Clone, Copy, Debug)]
pub struct MultiTickQuote {
    pub amount_out: U256,
    /// Input actually spent, including fee. Below the requested amount only
    /// when `exhausted` is set.
    pub amount_in_consumed: U256,
    pub sqrt_price_after: U256,
    pub ticks_crossed: u32,
    /// The swap wanted to continue past the ladder's proven coverage or past
    /// `max_ticks`. The quote is a LOWER bound on a partial fill, not a
    /// complete answer — callers must fall back rather than use it as a real
    /// output.
    pub exhausted: bool,
}

/// Apply a signed liquidity delta.
///
/// Returns `None` on overflow or underflow rather than saturating — the caller
/// treats `None` as exhaustion. Saturating here would silently invent liquidity
/// the pool does not have, which is the class of error this module exists to
/// eliminate.
fn apply_liquidity_net(liquidity: u128, liquidity_net: i128) -> Option<u128> {
    if liquidity_net >= 0 {
        liquidity.checked_add(liquidity_net.unsigned_abs())
    } else {
        liquidity.checked_sub(liquidity_net.unsigned_abs())
    }
}

/// Exact-input quote that crosses tick boundaries, updating liquidity at each
/// initialized tick.
///
/// Returns `None` for structurally invalid input (zero amount, zero liquidity,
/// unusable state). A quote that runs out of ladder returns `Some` with
/// `exhausted = true` — the distinction matters: `None` means "cannot answer",
/// `exhausted` means "answered partially, do not trust as a full fill".
pub fn quote_exact_input_multi_tick(
    state: &ClPoolState,
    ladder: &TickLadder,
    amount_in: U256,
    zero_for_one: bool,
    max_ticks: u32,
) -> Option<MultiTickQuote> {
    if amount_in.is_zero() || state.liquidity == 0 || state.sqrt_price_x96.is_zero() {
        return None;
    }

    let price_limit = if zero_for_one {
        min_sqrt_ratio()
    } else {
        max_sqrt_ratio()
    };

    let mut remaining = amount_in;
    let mut amount_out = U256::zero();
    let mut sqrt_price = state.sqrt_price_x96;
    let mut liquidity = state.liquidity;
    let mut tick = state.tick;
    let mut ticks_crossed: u32 = 0;
    let mut exhausted = false;

    while !remaining.is_zero() {
        if zero_for_one && sqrt_price <= price_limit {
            break;
        }
        if !zero_for_one && sqrt_price >= price_limit {
            break;
        }

        let (next_tick, liquidity_net) = match ladder.next_initialized(tick, zero_for_one) {
            LadderStep::Initialized { tick, liquidity_net } => (tick, liquidity_net),
            LadderStep::Exhausted => {
                exhausted = true;
                break;
            }
        };

        let sqrt_price_next_tick = get_sqrt_ratio_at_tick(next_tick)?;

        // Clamp the step target to the global price limit.
        let target = if zero_for_one {
            sqrt_price_next_tick.max(price_limit)
        } else {
            sqrt_price_next_tick.min(price_limit)
        };

        let step = compute_swap_step(sqrt_price, target, liquidity, remaining, state.fee_ppm)?;

        remaining = remaining
            .checked_sub(step.amount_in.checked_add(step.fee_amount)?)
            .unwrap_or_else(U256::zero);
        amount_out = amount_out.checked_add(step.amount_out)?;
        sqrt_price = step.sqrt_price_next;

        if sqrt_price != sqrt_price_next_tick {
            // Stopped mid-range: the input is spent. v3-core would recompute
            // `tick` here via `getTickAtSqrtRatio`; the loop exits on the next
            // condition check and never reads it, so it is omitted.
            break;
        }

        if remaining.is_zero() {
            // The input landed EXACTLY on the boundary. The fill is complete —
            // crossing only matters if there is more to swap. Without this
            // break, an exact fixed-point tie falls through to the guards
            // below and a complete, correct quote gets stamped `exhausted`,
            // contradicting `amount_in_consumed`'s contract. Confirmed
            // reachable: L=1e12, fee=3000ppm, ladder [(-60, +1e12)],
            // amount_in=3_013_394_246 lands on -60 with remaining==0 and then
            // trips the liquidity guard below.
            break;
        }

        // Landed exactly on an initialized tick — cross it.
        if ticks_crossed >= max_ticks {
            exhausted = true;
            break;
        }
        let signed = if zero_for_one { -liquidity_net } else { liquidity_net };
        liquidity = match apply_liquidity_net(liquidity, signed) {
            Some(l) if l > 0 => l,
            // Liquidity would go to zero or underflow: the ladder disagrees
            // with reality, so refuse rather than quote through a dead range.
            _ => {
                exhausted = true;
                break;
            }
        };
        // Downward crossings land on `tick - 1` so the next search makes
        // progress instead of re-finding the tick just crossed.
        tick = if zero_for_one { next_tick - 1 } else { next_tick };
        ticks_crossed += 1;
    }

    if amount_out.is_zero() {
        return None;
    }

    Some(MultiTickQuote {
        amount_out,
        amount_in_consumed: amount_in.checked_sub(remaining).unwrap_or(amount_in),
        sqrt_price_after: sqrt_price,
        ticks_crossed,
        exhausted,
    })
}
```

- [ ] **Step 9: Run to verify they pass**

```bash
cargo test --lib cl_swap
```

Expected: PASS — 10 tests.

- [ ] **Step 10: Commit**

```bash
git add src/cl_swap.rs
git commit -m "feat(cl): add multi-tick exact-input swap loop with explicit exhaustion"
```

---

### Task 3: `TickDataSource` trait and static test double (`cl_ticks.rs`)

**Files:**
- Create: `src/cl_ticks.rs`
- Modify: `src/lib.rs` (add `pub mod cl_ticks;`), `src/main.rs` (add `mod cl_ticks;`)
- Test: inline `#[cfg(test)] mod tests` in `src/cl_ticks.rs`

**Interfaces:**
- Consumes: `async_trait::async_trait`, `anyhow::Result`, `ethers::types::{Address, U256, U64}`. **Not** `cl_swap::TickLadder` — that is Task 4's dependency; do not import it here.
- Produces:
  - `pub const MAX_TICK_WORDS: usize = 8;`
  - `#[async_trait] pub trait TickDataSource: Send + Sync { async fn tick_words(&self, pool: Address, word_positions: &[i16], block: U64) -> Result<Vec<Option<U256>>>; async fn liquidity_net(&self, pool: Address, ticks: &[i32], block: U64) -> Result<Vec<Option<i128>>>; }`
  - `pub fn word_position(tick: i32, tick_spacing: i32) -> (i16, u8)`
  - `pub fn ticks_in_word(word_pos: i16, bitmap: U256, tick_spacing: i32) -> Vec<i32>`
  - `pub struct StaticTickSource` with `pub fn new(pool: Address, ticks: Vec<(i32, i128)>, tick_spacing: i32) -> Self`

- [ ] **Step 1: Write the failing bitmap-decoding tests**

Create `src/cl_ticks.rs`:

```rust
//! Tick data acquisition behind a swappable source.
//!
//! The swap loop in `cl_swap` is pure; everything that needs the network lives
//! here. `TickDataSource` is the seam: `RpcTickSource` today, a local-node
//! source later, `StaticTickSource` in tests — none of which the math layer
//! can distinguish.

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
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --lib cl_ticks
```

Expected: FAIL to compile — `cannot find function word_position`.

- [ ] **Step 3: Implement the trait, bitmap helpers and static source**

Insert above the tests in `src/cl_ticks.rs`:

```rust
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
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test --lib cl_ticks
```

Expected: PASS — 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cl_ticks.rs src/lib.rs src/main.rs
git commit -m "feat(cl): add TickDataSource trait, bitmap helpers and static test source"
```

---

### Task 4: Ladder builder

**Files:**
- Modify: `src/cl_ticks.rs` (append `build_ladder`; widen the existing
  `use anyhow::Result;` to `use anyhow::{anyhow, Result};` — the new
  tick-spacing guard needs the macro)
- Test: inline tests in `src/cl_ticks.rs`

**Interfaces:**
- Consumes: `TickDataSource`, `word_position`, `ticks_in_word`, `crate::cl_swap::TickLadder`, `crate::cl_sim::ClPoolState`
- Produces: `pub async fn build_ladder<S: TickDataSource + ?Sized>(source: &S, pool: Address, state: &ClPoolState, block: U64, words_per_side: usize) -> Result<TickLadder>`

- [ ] **Step 1: Write the failing builder tests**

Append to the `tests` module in `src/cl_ticks.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --lib cl_ticks
```

Expected: FAIL to compile — `cannot find function build_ladder`.

- [ ] **Step 3: Implement `build_ladder`**

Append to `src/cl_ticks.rs`, above the tests:

```rust
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
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test --lib cl_ticks
```

Expected: PASS — 9 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cl_ticks.rs
git commit -m "feat(cl): add ladder builder with word-derived coverage bounds"
```

---

### Task 5: `RpcTickSource` — batched on-chain reads

**Files:**
- Modify: `src/cl_ticks.rs` (append `RpcTickSource`)
- Test: inline tests in `src/cl_ticks.rs`

**Interfaces:**
- Consumes: `crate::quote_cl::multicall3_aggregate3`, `ethers::providers::{JsonRpcClient, Provider}`
- Produces:
  - `pub struct RpcTickSource<C>` with `pub fn new(provider: Arc<Provider<C>>) -> Self`
  - `pub fn decode_liquidity_net(word: &[u8]) -> Option<i128>`
  - `pub(crate) fn tick_bitmap_calldata(word_pos: i16) -> Vec<u8>`
  - `pub(crate) fn ticks_calldata(tick: i32) -> Vec<u8>`

> `ticks(int24)` returns the 8-field `Tick.Info` struct — `liquidityGross`
> (uint128), **`liquidityNet` (int128)**, `feeGrowthOutside0X128`,
> `feeGrowthOutside1X128`, `tickCumulativeOutside` (int56),
> `secondsPerLiquidityOutsideX128` (uint160), `secondsOutside` (uint32),
> `initialized` (bool) — ABI-encoded as 8 × 32-byte words = **256 bytes**.
> `liquidityNet` is the second field, i.e. bytes 32..64, sign-extended across
> the word.
>
> The decoder deliberately depends only on the first two fields and requires
> just `len >= 64`. That is what makes it portable across the UniV3 forks this
> system quotes (Slipstream, Aerodrome Slipstream, PancakeSwap V3): they may
> extend the tail of the struct, but the leading `liquidityGross`/
> `liquidityNet` pair is fixed by the AMM's own accounting.
>
> `quote_cl::multicall3_aggregate3` is `pub(crate)`, so this code must live in
> the crate — it does.

- [ ] **Step 1: Write the failing encode/decode tests**

Append to the `tests` module:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --lib cl_ticks
```

Expected: FAIL to compile — `cannot find function tick_bitmap_calldata`.

- [ ] **Step 3: Implement `RpcTickSource`**

Append to `src/cl_ticks.rs`, above the tests:

```rust
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
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test --lib cl_ticks
```

Expected: PASS — 13 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cl_ticks.rs
git commit -m "feat(cl): add batched multicall3 tick data source"
```

---

### Task 6: `CachedTickSource` — the node-ready seam

**Files:**
- Modify: `src/cl_ticks.rs` (append `CachedTickSource`)
- Test: inline tests in `src/cl_ticks.rs`

**Interfaces:**
- Consumes: `TickDataSource`, `dashmap::DashMap`
- Produces: `pub struct CachedTickSource<S>` with `pub fn new(inner: S, ttl_blocks: u64) -> Self`, `pub fn cached_words(&self) -> usize`, `pub fn invalidate_pool(&self, pool: Address)`

> **This is the piece that makes the local node a config swap.** Tick
> *liquidity* only changes when a position is minted or burned in that range —
> far less often than price moves — so words and nets are cached against a
> block epoch rather than an exact block. When RTT drops to ~1ms, construct
> `RpcTickSource` directly and the caching layer becomes optional without
> touching `cl_swap` or the callers.

- [ ] **Step 1: Write the failing caching tests**

Append to the `tests` module:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --lib cl_ticks
```

Expected: FAIL to compile — `cannot find type CachedTickSource`.

- [ ] **Step 3: Implement `CachedTickSource`**

Append to `src/cl_ticks.rs`, above the tests:

```rust
use dashmap::DashMap;

/// Caching decorator over any `TickDataSource`.
///
/// Keyed by `(pool, word|tick, epoch)` where `epoch = block / ttl_blocks`.
/// Tick liquidity changes only on mint/burn in that range, so an epoch of a
/// few dozen blocks trades a bounded staleness window for the removal of
/// nearly all tick RPC from the hot path. `invalidate_pool` is the escape
/// hatch when a mint/burn event is observed.
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
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test --lib cl_ticks
```

Expected: PASS — 18 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cl_ticks.rs
git commit -m "feat(cl): add epoch-keyed caching tick source"
```

---

### Task 7: Ladder plumbing on `ClPoolState`

**Files:**
- Modify: `src/cl_sim.rs` (flag helper, docstring, `#[allow(dead_code)]` removal)
- Test: inline tests in `src/cl_sim.rs`

**Interfaces:**
- Produces: `pub fn multi_tick_enabled() -> bool`, `pub fn cl_ladder_words() -> usize`, `pub fn cl_max_ticks_crossed() -> u32`

- [ ] **Step 1: Write the failing flag tests**

Append to the `tests` module in `src/cl_sim.rs`:

```rust
    #[test]
    fn multi_tick_defaults_off() {
        let _guard = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("ARBOT_CL_MULTI_TICK");
        assert!(
            !multi_tick_enabled(),
            "multi-tick must stay off until the parity harness is green"
        );
    }

    #[test]
    fn multi_tick_honours_the_flag() {
        let _guard = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_CL_MULTI_TICK", "1");
        assert!(multi_tick_enabled());
        std::env::remove_var("ARBOT_CL_MULTI_TICK");
    }

    #[test]
    fn ladder_words_is_clamped_to_the_word_ceiling() {
        let _guard = CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_CL_LADDER_WORDS", "999");
        assert!(cl_ladder_words() <= crate::cl_ticks::MAX_TICK_WORDS);
        std::env::remove_var("ARBOT_CL_LADDER_WORDS");
    }
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --lib cl_sim
```

Expected: FAIL to compile — `cannot find function multi_tick_enabled`.

- [ ] **Step 3: Add the flag helpers and unblock the tick fields**

In `src/cl_sim.rs`, delete both `#[allow(dead_code)]` attributes on
`ClPoolState::tick` and `ClPoolState::tick_spacing` (they are read by
`cl_swap` and `cl_ticks` now) and replace the stale field comment:

```rust
#[derive(Clone, Debug)]
pub struct ClPoolState {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    /// Current tick from `slot0`. Drives ladder navigation in `cl_swap`.
    pub tick: i32,
    /// Pool tick spacing. Drives bitmap word/bit decomposition in `cl_ticks`.
    pub tick_spacing: i32,
    /// Swap fee in hundredths of a bip (UniV3 fee tier or on-chain fee()).
    pub fee_ppm: u32,
}
```

Then append the helpers:

```rust
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
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test --lib cl_sim
```

Expected: PASS — existing `cl_sim` tests plus 3 new.

- [ ] **Step 5: Run the whole suite to confirm nothing regressed**

```bash
cargo test --lib
```

Expected: PASS, no new warnings about unused `tick`/`tick_spacing`.

- [ ] **Step 6: Commit**

```bash
git add src/cl_sim.rs
git commit -m "feat(cl): add multi-tick flags and retire dead_code on tick fields"
```

---

### Task 8: Wire multi-tick into `plan.rs` and `sizing.rs`

**Files:**
- Modify: `src/plan.rs:61-126` (`hop_expected_out`)
- Modify: `src/sizing.rs:315-350` (local CL sizing arm)
- Test: inline tests in `src/plan.rs`

**Interfaces:**
- Consumes: `crate::cl_swap::{TickLadder, quote_exact_input_multi_tick}`, `crate::cl_sim::{multi_tick_enabled, cl_max_ticks_crossed}`
- Produces: `pub fn cl_hop_out(state: &ClPoolState, ladder: Option<&TickLadder>, amount_in: U256, zero_for_one: bool) -> Option<(U256, bool)>` in `src/plan.rs` — returns `(amount_out, used_multi_tick)`; `sizing.rs` calls the same helper.

> **The behavioural change to review carefully.** The `ARBOT_CL_TICK_BUFFER_BPS`
> haircut exists to cover unmodelled tick crossing. When a multi-tick quote
> succeeds without exhausting, that crossing IS modelled and the haircut becomes
> double-counting — it would push `min_out` below what the pool pays and give up
> real edge. So the haircut applies on the single-tick path only. It is NOT
> removed: it still guards every fallback.

- [ ] **Step 1: Write the failing helper tests**

Append to the `tests` module in `src/plan.rs`:

```rust
    #[test]
    fn cl_hop_out_uses_the_ladder_when_multi_tick_is_enabled() {
        let _guard = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_CL_MULTI_TICK", "1");

        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
        };
        // Positive net below the price: crossing -60 downward removes 500e9,
        // taking liquidity 1000e9 -> 500e9. See the SIGN CONVENTION note in
        // Task 2.
        let ladder = crate::cl_swap::TickLadder::new(
            vec![(-180, 50_000_000_000), (-60, 500_000_000_000)],
            -180,
            180,
        );
        // SIZE THIS AGAINST THE FIXTURE'S LIQUIDITY, not by eyeballing. With
        // L = 1e12 it takes 3_013_394_246 to reach tick -60 and 6_040_382_047
        // to reach -180, which is the ladder's lower bound. Anything at or
        // above that upper figure runs off proven coverage, returns
        // `exhausted`, and makes `used_multi` FALSE — which would render this
        // test's own assertion unsatisfiable. 5e9 sits inside the window: it
        // crosses -60 exactly once and then stops mid-range.
        let amount_in = U256::from(5_000_000_000u64);

        let (out, used_multi) =
            cl_hop_out(&state, Some(&ladder), amount_in, true).expect("hop prices");
        assert!(used_multi, "an available ladder must be used");

        let single = crate::cl_sim::quote_exact_input_single_tick(&state, amount_in, true, 3_000)
            .expect("single call")
            .expect("single quote");
        assert!(out < single, "multi-tick must be below the optimistic estimate");

        std::env::remove_var("ARBOT_CL_MULTI_TICK");
    }

    #[test]
    fn cl_hop_out_falls_back_when_the_ladder_is_exhausted() {
        let _guard = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_CL_MULTI_TICK", "1");

        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
        };
        // Coverage far too narrow for the size below.
        let ladder = crate::cl_swap::TickLadder::new(vec![(-60, 900_000_000_000)], -60, 60);
        let amount_in = U256::from(10u64).pow(U256::from(24u64));

        let (_, used_multi) =
            cl_hop_out(&state, Some(&ladder), amount_in, true).expect("hop prices");
        assert!(!used_multi, "an exhausted ladder must fall back, not be trusted");

        std::env::remove_var("ARBOT_CL_MULTI_TICK");
    }

    #[test]
    fn cl_hop_out_ignores_the_ladder_when_the_flag_is_off() {
        let _guard = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("ARBOT_CL_MULTI_TICK");

        let state = crate::cl_sim::ClPoolState {
            sqrt_price_x96: crate::cl_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
            liquidity: 1_000_000_000_000,
            tick: 0,
            tick_spacing: 60,
            fee_ppm: 3_000,
        };
        let ladder = crate::cl_swap::TickLadder::new(vec![(-60, 500_000_000_000)], -180, 180);

        let (_, used_multi) = cl_hop_out(&state, Some(&ladder), U256::from(1_000u64), true)
            .expect("hop prices");
        assert!(!used_multi, "flag off must keep the single-tick path");
    }
```

- [ ] **Step 1b: Write the failing dispatch test for `hop_expected_out`**

The three tests above exercise `cl_hop_out` directly. That is not enough: the
branch in `hop_expected_out` that *skips* the haircut on a successful
multi-tick quote is the single most dangerous line in this task, and nothing
above executes it. Every `Edge` fixture in the repo sets `tick_ladder: None`,
so if a future edit swapped the two match arms — haircut on success, none on
fallback, the textbook inversion — the entire suite would still pass.

Mirror the existing fixture in `cl_hop_expected_out_uses_the_curve_not_the_secant`
(`src/plan.rs:803`), which already builds a CL `Edge` and calls
`hop_expected_out`. Copy it and change only: attach a ladder, enable the flag,
and invert the expectation.

```rust
    /// The one branch that skips the haircut. Pins it against the inversion
    /// that would otherwise pass the whole suite.
    #[test]
    fn hop_expected_out_does_not_haircut_a_successful_multi_tick_quote() {
        let _guard = crate::cl_sim::CL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ARBOT_CL_MULTI_TICK", "1");

        // Same fixture shape as `cl_hop_expected_out_uses_the_curve_not_the_secant`,
        // with a ladder attached. 5e9 crosses tick -60 exactly once and stops
        // mid-range, so the quote is NOT exhausted (see the sizing note in the
        // `cl_hop_out` tests above for where those bounds come from).
        let ladder = std::sync::Arc::new(crate::cl_swap::TickLadder::new(
            vec![(-180, 50_000_000_000), (-60, 500_000_000_000)],
            -180,
            180,
        ));
        let amount_in = U256::from(5_000_000_000u64);

        // Build `edge` exactly as the existing test does, then set:
        //   edge.tick_ladder = Some(ladder.clone());
        // (keep its `state: Some(cl_state)` with liquidity 1_000_000_000_000,
        // tick 0, tick_spacing 60, fee_ppm 3_000)

        let (multi_out, used_multi) =
            cl_hop_out(&cl_state, Some(ladder.as_ref()), amount_in, true).expect("multi quote");
        assert!(used_multi, "fixture must produce a non-exhausted multi-tick quote");

        let actual = hop_expected_out(&edge, edge.from, amount_in);

        assert_eq!(
            actual, multi_out,
            "a successful multi-tick quote must pass through UNDISCOUNTED"
        );
        assert!(
            actual > crate::util::apply_slippage(multi_out, cl_tick_buffer_bps()),
            "the tick buffer must NOT be applied on top of a modelled crossing — \
             if this fails, the two match arms in hop_expected_out are inverted"
        );

        std::env::remove_var("ARBOT_CL_MULTI_TICK");
    }
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --lib plan::tests::cl_hop_out
```

Expected: FAIL to compile — `cannot find function cl_hop_out`.

- [ ] **Step 3: Implement `cl_hop_out`**

Add to `src/plan.rs`, above `hop_expected_out`:

```rust
/// Price one CL hop, preferring the multi-tick model.
///
/// Returns `(amount_out, used_multi_tick)`. `used_multi_tick` is true only
/// when a ladder produced a NON-exhausted quote — i.e. when tick crossing was
/// genuinely modelled. Callers use it to decide whether the
/// `ARBOT_CL_TICK_BUFFER_BPS` haircut still applies.
pub fn cl_hop_out(
    state: &crate::cl_sim::ClPoolState,
    ladder: Option<&crate::cl_swap::TickLadder>,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<(U256, bool)> {
    if crate::cl_sim::multi_tick_enabled() {
        if let Some(ladder) = ladder {
            if let Some(quote) = crate::cl_swap::quote_exact_input_multi_tick(
                state,
                ladder,
                amount_in,
                zero_for_one,
                crate::cl_sim::cl_max_ticks_crossed(),
            ) {
                if !quote.exhausted && !quote.amount_out.is_zero() {
                    return Some((quote.amount_out, true));
                }
                tracing::debug!(
                    target: "minout",
                    ticks_crossed = quote.ticks_crossed,
                    ladder_len = ladder.len(),
                    "multi-tick quote exhausted the ladder; falling back to single-tick"
                );
            }
        }
    }
    let out = crate::cl_sim::quote_exact_input_single_tick(
        state,
        amount_in,
        zero_for_one,
        state.fee_ppm,
    )
    .ok()??;
    if out.is_zero() {
        return None;
    }
    Some((out, false))
}
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test --lib plan::tests::cl_hop_out
```

Expected: PASS — 3 tests.

- [ ] **Step 5: Route `hop_expected_out` through the helper**

Replace the body of the CL match arm at `src/plan.rs:89-126` (the arm guarded
by `local_cl_quotes_enabled() && path.len() <= 2`) with:

```rust
        } if crate::cl_sim::local_cl_quotes_enabled() && path.len() <= 2 => {
            // UniV3-family pools order tokens by address, so `from` is token0
            // exactly when it sorts below `to`.
            let zero_for_one = edge.from < edge.to;
            match cl_hop_out(cl_state, edge.tick_ladder.as_deref(), current_amount, zero_for_one) {
                // Multi-tick modelled the crossing, so the tick buffer would
                // double-count it and give up real edge. Use the quote as-is.
                Some((out, true)) => {
                    tracing::debug!(
                        target: "minout",
                        curve_out = %out,
                        linear_out = %linear,
                        "CL hop priced multi-tick"
                    );
                    out
                }
                // Single-tick fallback: liquidity was held constant, so the
                // estimate is systematically optimistic and the buffer still
                // covers the unmodelled crossing.
                Some((out, false)) => {
                    let discounted = crate::util::apply_slippage(out, cl_tick_buffer_bps());
                    tracing::debug!(
                        target: "minout",
                        curve_out = %out,
                        linear_out = %linear,
                        discounted = %discounted,
                        buffer_bps = cl_tick_buffer_bps(),
                        "CL hop priced single-tick with crossing buffer"
                    );
                    discounted
                }
                None => linear,
            }
        }
```

> `edge.tick_ladder` does not exist yet. Add it to `Edge` in `src/graph.rs`
> beside the existing per-edge CL `state`:
> ```rust
> /// Ladder for the CL pool on this edge, when one was built. `None` keeps
> /// the single-tick path with its crossing buffer.
> pub tick_ladder: Option<std::sync::Arc<crate::cl_swap::TickLadder>>,
> ```
> Initialise it to `None` at every `Edge` construction site (`cargo check` will
> enumerate them); Task 9 populates it. `Arc` because an `Edge` is cloned per
> candidate and a ladder can hold hundreds of ticks.

- [ ] **Step 6: Route the sizing arm through the same helper**

Replace the `quote_exact_input_single_tick` call at `src/sizing.rs:330-338`
with:

```rust
            let (out, used_multi) = crate::plan::cl_hop_out(
                cl_state,
                edge.tick_ladder.as_deref(),
                amount_in,
                zero_for_one,
            )?;
            if out.is_zero() {
                return None;
            }
            // Deliberately does NOT touch `ctx.quote_count` — that counter
            // tracks RPC quotes, and the point of this arm is that it issues
            // none. The split between the two is the P1 win made observable.
            debug!(
                pool = %format!("0x{}", hex::encode(pool)),
                ?amount_in,
                used_multi,
                "sized hop locally from cached CL state (no RPC)"
            );
```

- [ ] **Step 7: Run the whole suite**

```bash
cargo test --lib && cargo test --bin arb-exec
```

Expected: PASS. With `ARBOT_CL_MULTI_TICK` unset, behaviour is byte-identical
to today — every ladder is `None`, so `cl_hop_out` takes the single-tick path
and applies the buffer exactly as before.

- [ ] **Step 8: Commit**

```bash
git add src/plan.rs src/sizing.rs src/graph.rs
git commit -m "feat(cl): prefer multi-tick quotes, keep tick buffer on fallback only"
```

---

### Task 9: Ladder population and the parity harness

**Files:**
- Modify: `src/venues.rs:1738-1760` and `src/venues.rs:2364-2380` (populate `tick_ladder` alongside `prefetched_cl_state`)
- Create: `src/bin/cl_parity.rs`
- Modify: `Cargo.toml` (register the binary)

**Interfaces:**
- Consumes: `crate::cl_ticks::{CachedTickSource, RpcTickSource, build_ladder}`, `crate::cl_swap::quote_exact_input_multi_tick`, `crate::quote_univ3::UniQuoter`
- Produces: `cl_parity` binary

> **This task is the acceptance gate.** `ARBOT_CL_MULTI_TICK` must not be turned
> on for a funded run until `cl_parity` reports agreement with the on-chain
> quoter. Unit tests prove internal consistency; only this proves the model
> matches the pool.

- [ ] **Step 1: Populate ladders next to the existing CL state prefetch**

At both `venues.rs` prefetch sites, after `load_cl_pool_states_batched`
returns, build ladders for the pools that produced state:

```rust
    // Ladders ride along with the state prefetch: same block, same pool set,
    // and `CachedTickSource` collapses repeat words across the scan. Skipped
    // entirely when the flag is off, so this costs nothing until enabled.
    let tick_ladders: std::collections::HashMap<Address, std::sync::Arc<crate::cl_swap::TickLadder>> =
        if crate::cl_sim::multi_tick_enabled() {
            let source = crate::cl_ticks::CachedTickSource::new(
                crate::cl_ticks::RpcTickSource::new(provider.clone()),
                32,
            );
            let words = crate::cl_sim::cl_ladder_words();
            let mut built = std::collections::HashMap::new();
            for (pool, state) in states.iter() {
                match crate::cl_ticks::build_ladder(&source, *pool, state, block, words).await {
                    Ok(ladder) if !ladder.is_empty() => {
                        built.insert(*pool, std::sync::Arc::new(ladder));
                    }
                    Ok(_) => {}
                    Err(err) => tracing::debug!(
                        target: "cl_ticks",
                        pool = %format!("0x{}", hex::encode(pool)),
                        error = %err,
                        "ladder build failed; edge keeps the single-tick path"
                    ),
                }
            }
            built
        } else {
            std::collections::HashMap::new()
        };
```

Then set `tick_ladder: tick_ladders.get(&pool).cloned()` at the `Edge`
construction sites that already set `state: Some(cl_state)`.

- [ ] **Step 2: Verify the flag-off path is unchanged**

```bash
cargo test --lib && cargo test --bin arb-exec
```

Expected: PASS. `multi_tick_enabled()` is false by default, so the map is empty
and every edge still carries `tick_ladder: None`.

- [ ] **Step 3: Commit**

```bash
git add src/venues.rs
git commit -m "feat(cl): build tick ladders alongside the CL state prefetch"
```

- [ ] **Step 4: Write the parity harness**

Create `src/bin/cl_parity.rs`:

```rust
//! Differential harness: multi-tick model vs the deployed on-chain quoter.
//!
//! The unit tests prove the simulator is self-consistent. Only this proves it
//! matches the pool. Run it before enabling `ARBOT_CL_MULTI_TICK` anywhere
//! near funds.
//!
//! Usage:
//!   ARBOT_RPC_URL=... cargo run --bin cl_parity -- <pool> <fee_ppm> <amount_in>...

use anyhow::{anyhow, Context, Result};
use arb_exec::{cl_sim, cl_swap, cl_ticks};
use ethers::{
    contract::abigen,
    providers::{Http, Provider},
    types::{Address, U256, U64},
};
use std::{str::FromStr, sync::Arc};

abigen!(
    IClPoolTokens,
    r#"[
        function token0() external view returns (address)
        function token1() external view returns (address)
    ]"#,
);

/// The crate has no shared `pool_tokens` helper, so read the pair off the pool.
async fn pool_tokens(provider: Arc<Provider<Http>>, pool: Address) -> Result<(Address, Address)> {
    let c = IClPoolTokens::new(pool, provider);
    let token0 = c.token_0().call().await.context("pool token0()")?;
    let token1 = c.token_1().call().await.context("pool token1()")?;
    Ok((token0, token1))
}

/// Resolve an HTTP endpoint: explicit `ARBOT_RPC_URL` wins, otherwise take the
/// first usable entry from the chain's configured `BASE_RPC_URLS` list so this
/// runs against the same provider the bot already uses.
fn resolve_rpc() -> Result<String> {
    if let Ok(url) = std::env::var("ARBOT_RPC_URL") {
        if !url.trim().is_empty() {
            return Ok(url);
        }
    }
    let raw = std::env::var("BASE_RPC_URLS")
        .context("set ARBOT_RPC_URL, or BASE_RPC_URLS in .env")?;
    arb_exec::util::parse_endpoint_list(&raw)
        .into_iter()
        .find_map(|e| arb_exec::util::coerce_http_url(&e))
        .ok_or_else(|| anyhow!("no usable HTTP endpoint in BASE_RPC_URLS"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args().skip(1);
    let pool = Address::from_str(&args.next().ok_or_else(|| anyhow!("usage: cl_parity <pool> <fee_ppm> <amount_in>..."))?)
        .context("pool address")?;
    let fee_ppm: u32 = args
        .next()
        .ok_or_else(|| anyhow!("missing fee_ppm"))?
        .parse()
        .context("fee_ppm")?;
    let amounts: Vec<U256> = args
        .map(|a| U256::from_dec_str(&a).context("amount_in"))
        .collect::<Result<_>>()?;
    if amounts.is_empty() {
        return Err(anyhow!("supply at least one amount_in"));
    }

    let _ = dotenvy::dotenv();
    let rpc = resolve_rpc()?;
    let provider = Arc::new(Provider::<Http>::try_from(rpc).context("provider")?);
    let block: U64 = provider.get_block_number().await.context("block number")?;

    let state = cl_sim::load_cl_pool_state(provider.clone(), pool, block, Some(fee_ppm))
        .await?
        .ok_or_else(|| anyhow!("pool has no usable CL state"))?;

    let source = cl_ticks::CachedTickSource::new(cl_ticks::RpcTickSource::new(provider.clone()), 32);
    let ladder = cl_ticks::build_ladder(&source, pool, &state, block, 4).await?;

    println!(
        "pool=0x{} block={} tick={} spacing={} liquidity={} ladder_ticks={} coverage=[{},{}]",
        hex::encode(pool),
        block,
        state.tick,
        state.tick_spacing,
        state.liquidity,
        ladder.len(),
        ladder.lower_bound(),
        ladder.upper_bound(),
    );
    println!("amount_in,single_tick,multi_tick,ticks_crossed,exhausted,single_err_bps,multi_err_bps");

    // `UniQuoter::new` takes (provider, quoter_address, factory_address).
    // Both are already configured per chain; read them rather than hardcoding.
    let quoter_addr = Address::from_str(
        &std::env::var("BASE_UNIV3_QUOTER").context("BASE_UNIV3_QUOTER must be set")?,
    )
    .context("BASE_UNIV3_QUOTER")?;
    let factory_addr = Address::from_str(
        &std::env::var("BASE_UNIV3_FACTORY").context("BASE_UNIV3_FACTORY must be set")?,
    )
    .context("BASE_UNIV3_FACTORY")?;
    let quoter =
        arb_exec::quote_univ3::UniQuoter::new(provider.clone(), quoter_addr, factory_addr);
    let (token0, token1) = pool_tokens(provider.clone(), pool).await?;

    let mut worst_multi_bps: i64 = 0;
    let mut non_exhausted: usize = 0;
    let mut unrepresentable: usize = 0;
    let total = amounts.len();
    for amount in amounts {
        let on_chain = quoter
            .quote_path(vec![(token0, None), (token1, Some(fee_ppm))], amount, block)
            .await
            .context("on-chain quote")?;

        let single = cl_sim::quote_exact_input_single_tick(&state, amount, true, fee_ppm)?
            .unwrap_or_default();
        let multi = cl_swap::quote_exact_input_multi_tick(&state, &ladder, amount, true, 128);

        // Returns `None` when the ratio is not representable. `U256::as_u64()`
        // PANICS on a value that does not fit, and the value that would not fit
        // is precisely a catastrophic model error — a units or decimals bug
        // producing an output orders of magnitude off. That is the single most
        // important thing this harness could ever report, so it must not crash
        // there. `None` is rendered as a loud marker instead.
        let err_bps = |model: U256| -> Option<i64> {
            if on_chain.is_zero() {
                return None;
            }
            let (diff, sign) = if model >= on_chain {
                (model - on_chain, 1i64)
            } else {
                (on_chain - model, -1i64)
            };
            let scaled = diff.checked_mul(U256::from(10_000u64))? / on_chain;
            if scaled > U256::from(i64::MAX as u64) {
                return None;
            }
            Some(sign * (scaled.as_u64() as i64))
        };
        let show = |v: Option<i64>| match v {
            Some(b) => b.to_string(),
            None => "OVERFLOW".to_string(),
        };

        let (multi_out, crossed, exhausted) = match multi {
            Some(q) => (q.amount_out, q.ticks_crossed, q.exhausted),
            None => (U256::zero(), 0, true),
        };
        let multi_bps = err_bps(multi_out);
        if !exhausted {
            non_exhausted += 1;
            match multi_bps {
                Some(b) if b.abs() > worst_multi_bps.abs() => worst_multi_bps = b,
                // A non-representable error on a row we are actually judging is
                // a failure, not a curiosity.
                None => unrepresentable += 1,
                _ => {}
            }
        }

        println!(
            "{amount},{single},{multi_out},{crossed},{exhausted},{},{}",
            show(err_bps(single)),
            show(multi_bps)
        );
    }

    // A verdict drawn from zero judged samples is worthless. Without this, a
    // sweep whose every amount exhausts the ladder leaves `worst_multi_bps` at
    // its initial 0 and prints PASS having validated nothing — and anything
    // grepping for "PASS" would rubber-stamp itself.
    if non_exhausted == 0 {
        return Err(anyhow!(
            "INCONCLUSIVE: all {} samples exhausted the ladder, so nothing was validated. \
             Use smaller amounts, or raise ARBOT_CL_LADDER_WORDS / ARBOT_CL_MAX_TICKS.",
            total
        ));
    }
    if unrepresentable > 0 {
        return Err(anyhow!(
            "{unrepresentable} of {non_exhausted} judged samples produced an unrepresentable \
             error ratio — the model output is implausible. Do NOT enable ARBOT_CL_MULTI_TICK."
        ));
    }

    println!(
        "\njudged {non_exhausted} non-exhausted samples; worst multi-tick error: {worst_multi_bps} bps"
    );
    if worst_multi_bps.abs() > 5 {
        return Err(anyhow!(
            "multi-tick deviates from the on-chain quoter by {worst_multi_bps} bps (limit 5) — do NOT enable ARBOT_CL_MULTI_TICK"
        ));
    }
    println!("PASS: within 5 bps of the on-chain quoter");
    Ok(())
}
```

Register it in `Cargo.toml` after the existing `[[bin]]` block:

```toml
[[bin]]
name = "cl_parity"
path = "src/bin/cl_parity.rs"
```

> These signatures were verified against the tree before this brief was
> written: `UniQuoter::new(provider, quoter: Address, factory: Address)` at
> `src/quote_univ3.rs:81` (three args, not one, and it is not fallible), and
> `quote_path(path: Vec<(Address, Option<u32>)>, amount_in: U256, block: U64)
> -> Result<U256>` at `src/quote_univ3.rs:92`. There is no `pool_tokens`
> helper anywhere in the crate, hence the small local `abigen!` above. Do not
> add any other new abstraction for the harness.

- [ ] **Step 5: Build the harness**

```bash
cargo build --bin cl_parity
```

Expected: builds clean.

- [ ] **Step 6: Run parity against Base's deepest WETH/USDC pool**

Use the 5bps WETH/USDC pool named in `ARCHITECTURE_PIVOT_HANDOFF.md` §3 — the
one whose hand-built round trip was traced in the EVM — and sweep sizes across
the range where crossing starts to matter:

```bash
ARBOT_RPC_URL="$ARBOT_RPC_URL" cargo run --bin cl_parity -- <weth_usdc_500_pool> 500 1000000000000000 10000000000000000 100000000000000000 1000000000000000000 10000000000000000000
```

Expected: `PASS: within 5 bps of the on-chain quoter`, and — the point of the
whole plan — `single_err_bps` growing large and positive at the bigger sizes
while `multi_err_bps` stays near zero. That divergence is the 1070 bps
overstatement being closed. Record the output in the commit message.

- [ ] **Step 7: Commit**

```bash
git add src/bin/cl_parity.rs Cargo.toml
git commit -m "feat(cl): add on-chain parity harness for the multi-tick simulator"
```

- [ ] **Step 8: Enable the flag only after parity passes**

Add to `.env` **only if Step 6 printed PASS**:

```
ARBOT_CL_MULTI_TICK=1
ARBOT_CL_LADDER_WORDS=2
ARBOT_CL_MAX_TICKS=128
```

Then re-run the suite and a shadow (non-broadcasting) funnel pass, and confirm
in the logs that `CL hop priced multi-tick` appears and
`multi-tick quote exhausted the ladder` is rare. A high exhaustion rate means
`ARBOT_CL_LADDER_WORDS` is too low for the sizes being quoted — raise it before
concluding anything about profitability.

- [ ] **Step 9: Commit**

```bash
git add .env
git commit -m "chore(cl): enable multi-tick simulation after parity validation"
```

---

## Self-Review

**Spec coverage** — against `ARCHITECTURE_PIVOT_HANDOFF.md` §2c caveat 2 and §7 item 0:

| Spec requirement | Task |
|---|---|
| "A correct multi-tick CL simulator" | 1, 2 |
| "`cl_sim::quote_exact_input_single_tick` holds liquidity CONSTANT" | 2 (replaced), 8 (fallback retained) |
| "`tick`, `tick_spacing` are carried but `#[allow(dead_code)]`" | 7 |
| "prerequisite for BOTH correct min_out AND the convex program" | 8 (min_out); ladder is the per-pool state the optimiser will consume |
| "pure computation, unit-testable offline against the on-chain quoter" | 1-7 offline, 9 on-chain |
| "no RPC or deploy needed" to build | Tasks 1-8 need neither; 9 needs RPC to validate |
| Tick data must not sit in the hot loop at 240ms RTT | 6 (epoch cache), 9 (built with the existing prefetch, 2 round trips per pool) |
| Local node must be a config swap | 3 (trait), 6 (decorator) — swap the constructor, math untouched |

**Deliberately out of scope**, with reasons stated at the point of omission:
`getTickAtSqrtRatio` (Task 2 design note), exact-output swaps (Global
Constraints), `backrun_state::advance_cl_state` (unchanged — it is
`#[allow(dead_code)]` and behind the backrun flag; migrating it is not needed
for `min_out` and would widen this plan's blast radius).

**Not addressed by this plan** — these are the other six subsystems and each
needs its own: flashblock ingestion, chain comparison, cycle-set precompute,
the convex optimiser, the local node, multi-loan execution.

**Placeholder scan:** every code step carries complete compilable code; no
"add error handling", no "similar to Task N", no "TBD". The two places that
defer to the codebase — `Edge` construction sites in Task 8 Step 5, and the
quoter constructor in Task 9 Step 4 — say explicitly how to find the answer
(`cargo check`, mirror `venues.rs`) rather than leaving it open.

**Type consistency check:** `ClPoolState` fields (`sqrt_price_x96`, `liquidity`,
`tick`, `tick_spacing`, `fee_ppm`) match `cl_sim.rs:26-37` exactly and are used
identically in Tasks 2, 4, 7, 8, 9. `TickLadder::new(Vec<(i32, i128)>, i32, i32)`
is constructed with that signature in Tasks 2, 4, 8. `LadderStep::Initialized {
tick, liquidity_net }` field names are consistent across Tasks 2 and 4.
`MultiTickQuote` fields (`amount_out`, `amount_in_consumed`, `sqrt_price_after`,
`ticks_crossed`, `exhausted`) are read consistently in Tasks 2, 8, 9.
`TickDataSource::{tick_words, liquidity_net}` signatures match across the trait
and all three implementations. `build_ladder`'s `words_per_side: usize` matches
`cl_ladder_words() -> usize`. `cl_hop_out` returns `(U256, bool)` and both call
sites destructure it that way.

**Two defects found and fixed during review:**

1. **`.as_ref()` vs `.as_deref()`.** Both call sites originally passed
   `edge.tick_ladder.as_ref()`, which yields `Option<&Arc<TickLadder>>` where
   `cl_hop_out` wants `Option<&TickLadder>`. Deref coercion does not reach
   through `Option`, so this does not compile in either module regardless of
   where the call lives. Both sites now use `.as_deref()`.

2. **Inverted `liquidity_net` signs in four test ladders.** The ladders in
   `crossing_swap_is_below_the_constant_liquidity_estimate`,
   `swap_beyond_coverage_reports_exhaustion`,
   `tick_budget_is_enforced_as_exhaustion` and the three `cl_hop_out` tests
   used negative nets on ticks *below* the current price. Because the swap loop
   negates the net when `zero_for_one`, that made liquidity **rise** as price
   fell — the opposite of the modelled scenario — which would have made the
   headline regression test fail while looking like a bug in the swap loop.
   Verified numerically before correcting: with the intended convention,
   crossing `net[-60] = +500e9` downward takes liquidity `1000e9 -> 500e9`;
   with the sign inverted it goes to `1500e9`. All ladders now use positive
   nets below the price, and the convention is documented in the SIGN
   CONVENTION note in Task 2 so a reviewer does not re-invert them. The
   `ladder()` helper was already correct and is unchanged.
