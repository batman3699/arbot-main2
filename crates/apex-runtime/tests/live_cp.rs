//! R24: Aerodrome v2's volatile pools, quoted as the pool quotes.

use apex_runtime::live::cp::{quote_out, Reserves};
use ethers_core::types::U256;

/// `getReserves()` and `getAmountOut` of the WETH/USDC pool 0xcDAC0d6c…, block
/// 52,364,894, fee 30 bps; token0 WETH.
const R0: u128 = 1_823_383_892_520_317_644_689;
const R1: u128 = 4_541_987_609_188;
const VECTORS: [(bool, u128, u128); 8] = [
    (true, 10_000_000_000_000_000, 24_834_797),
    (true, 100_000_000_000_000_000, 248_335_749),
    (true, 1_000_000_000_000_000_000, 2_482_136_085),
    (true, 123_456_789_012_345_678, 306_583_410),
    (false, 25_000_000, 10_006_102_620_585_099),
    (false, 250_000_000, 100_056_084_545_959_744),
    (false, 2_500_000_000, 1_000_066_947_794_155_070),
    (false, 1_234_567_891, 493_997_360_556_505_433),
];

fn pool() -> Reserves {
    Reserves { reserve0: U256::from(R0), reserve1: U256::from(R1) }
}

/// **To the unit**, both directions, against the chain.
#[test]
fn the_quote_is_the_pools_get_amount_out_to_the_unit() {
    for (zero_for_one, amount_in, out) in VECTORS {
        assert_eq!(
            quote_out(&pool(), 3_000, U256::from(amount_in), zero_for_one),
            Some(U256::from(out)),
            "{amount_in}"
        );
    }
}

/// The pool takes `floor(in · fee)` off the input; `in · (1 − fee)` floored
/// would keep a unit less, and one of the eight vectors above sees it.
#[test]
fn the_fee_comes_off_the_input_rounded_down() {
    let r = Reserves { reserve0: U256::from(10u64), reserve1: U256::from(1_000u64) };
    // 1 · 0.003 floors to 0, so the whole unit swaps: 1000 · 1 / 11.
    assert_eq!(quote_out(&r, 3_000, U256::one(), true), Some(U256::from(90u64)));
}

#[test]
fn nothing_in_or_an_empty_side_quotes_nothing() {
    assert_eq!(quote_out(&pool(), 3_000, U256::zero(), true), None);
    let empty = Reserves { reserve0: U256::zero(), reserve1: U256::from(R1) };
    assert_eq!(quote_out(&empty, 3_000, U256::from(10u64), true), None);
    assert_eq!(quote_out(&empty, 3_000, U256::from(10u64), false), None);
}
