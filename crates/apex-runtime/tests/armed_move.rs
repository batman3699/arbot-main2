//! R26: pools moved to a price, for pricing an armed backrun before its trigger.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{Move, PoolBook, PoolSnapshot, SwapWrite};
use apex_runtime::live::cp::{move_reserves, Reserves};
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::compat::u256_to_alloy;
use apex_types::state::ReconstructionStatus;
use ethers_core::types::U256;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const L: u128 = 1_400_000_000_000_000_000;

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32) -> PoolSnapshot {
    let spacing = 10;
    let lower = (tick - 2_000) / spacing * spacing;
    let upper = (tick + 2_000) / spacing * spacing;
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0 },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: spacing,
            fee_ppm,
            balance0: Some(U256::from(10u128.pow(21))),
            balance1: Some(U256::from(3_000_000_000_000u128)),
        },
        ladder: TickLadder::new(vec![(lower, L as i128), (upper, -(L as i128))], lower - 5_000, upper + 5_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: Default::default(),
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

#[test]
fn reserves_move_along_the_curve_and_never_gain() {
    let r = Reserves { reserve0: U256::from(10u128.pow(21)), reserve1: U256::from(3 * 10u128.pow(12)) };
    let k = r.reserve0 * r.reserve1;
    for (up, bps) in [(true, 100u32), (false, 100), (true, 1), (false, 2_000)] {
        let m = move_reserves(&r, up, bps).unwrap();
        let before = r.reserve1.as_u128() as f64 / r.reserve0.as_u128() as f64;
        let after = m.reserve1.as_u128() as f64 / m.reserve0.as_u128() as f64;
        let want = if up { 1.0 + f64::from(bps) / 1e4 } else { 1.0 - f64::from(bps) / 1e4 };
        assert!((after / before - want).abs() < 1e-6, "{up} {bps}: {}", after / before);
        assert!(m.reserve0 * m.reserve1 <= k, "the product never grows");
    }
    assert!(move_reserves(&r, false, 10_000).is_none());
    assert!(move_reserves(&Reserves { reserve0: U256::zero(), reserve1: U256::one() }, true, 1).is_none());
}

#[test]
fn a_snapshot_moves_by_its_kind() {
    let p = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let up = p.moved(Move::Tick(-197_300)).unwrap();
    assert_eq!(up.state.tick, -197_300);
    assert_eq!(up.state.sqrt_price_x96, get_sqrt_ratio_at_tick(-197_300).unwrap());
    assert!(up.state.balance0.unwrap() < p.state.balance0.unwrap(), "price up: token0 leaves the pool");
    assert!(p.moved(Move::Bps { up: true, bps: 10 }).is_none(), "a tick pool takes ticks");
    assert!(p.moved(Move::Tick(-100_000)).is_none(), "outside the ladder");
}

/// Every Swap the book applies is checked against `move_to_price` from the
/// state before it: the move model is validated on every real swap.
#[test]
fn applied_swaps_are_checked_against_the_move_model() {
    let book = PoolBook::from_snapshots(vec![pool(UNI, Venue::UniswapV3, -197_350, 500)], ReconstructionStatus::Verified);
    let sqrt = u256_to_alloy(get_sqrt_ratio_at_tick(-197_340).unwrap());
    book.apply_swaps(&[SwapWrite { pool: UNI, sqrt_price_x96: sqrt, liquidity: L, tick: -197_340, at: (101, 1) }]);
    assert_eq!(book.move_checks(), (1, 0, 0));
    // A liquidity the model does not predict, at the tick it does: a mint or
    // burn the book has not applied (live, mostly liquidity added around swaps).
    book.apply_swaps(&[SwapWrite { pool: UNI, sqrt_price_x96: sqrt, liquidity: L + 1, tick: -197_340, at: (102, 1) }]);
    assert_eq!(book.move_checks(), (2, 0, 1));
    // A tick the model does not predict is the model's error.
    book.apply_swaps(&[SwapWrite { pool: UNI, sqrt_price_x96: sqrt, liquidity: L + 1, tick: -197_339, at: (103, 1) }]);
    assert_eq!(book.move_checks(), (3, 1, 1));
    // An older log is refused by the book and not checked.
    book.apply_swaps(&[SwapWrite { pool: UNI, sqrt_price_x96: sqrt, liquidity: L, tick: -197_340, at: (99, 1) }]);
    assert_eq!(book.move_checks(), (3, 1, 1));
}
