//! R26: a pool moved to a price is exactly the pool a swap to that price leaves.

use apex_math::cl_math::{get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio};
use apex_math::cl_move::move_to_price;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::{quote_exact_input_multi_tick, TickLadder};
use ethers_core::types::U256;

fn ladder() -> TickLadder {
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

fn state() -> ClPoolState {
    ClPoolState {
        sqrt_price_x96: get_sqrt_ratio_at_tick(0).unwrap(),
        liquidity: 6_000_000_000,
        tick: 0,
        tick_spacing: 60,
        fee_ppm: 3_000,
        balance0: Some(U256::from(10u128.pow(30))),
        balance1: Some(U256::from(10u128.pow(30))),
    }
}

/// The step-exact quoter (checked to the wei against Uniswap's and Slipstream's
/// quoters) leaves a price, a liquidity and a tick; moving there must agree.
#[test]
fn a_move_lands_where_a_swap_to_that_price_does() {
    let (s, l) = (state(), ladder());
    let mut crossed = 0;
    for zero_for_one in [true, false] {
        for amount in [10u128.pow(6), 10u128.pow(7), 2 * 10u128.pow(7), 5 * 10u128.pow(7), 10u128.pow(8)] {
            let q = quote_exact_input_multi_tick(&s, &l, U256::from(amount), zero_for_one, 128).unwrap();
            if q.exhausted || q.ended_on_tick_boundary {
                continue;
            }
            crossed += usize::from(q.ticks_crossed > 0);
            let m = move_to_price(&s, &l, q.sqrt_price_after).expect("inside the ladder");
            assert_eq!(m.sqrt_price_x96, q.sqrt_price_after);
            assert_eq!(m.liquidity, q.liquidity_after, "amount {amount} zero_for_one {zero_for_one}");
            assert_eq!(m.tick, get_tick_at_sqrt_ratio(q.sqrt_price_after).unwrap());
            // What left the pool: at least the quote's output (it rounds up), at most a unit per step more.
            let (before, after) = if zero_for_one { (s.balance1, m.balance1) } else { (s.balance0, m.balance0) };
            let out = before.unwrap() - after.unwrap();
            assert!(out >= q.amount_out && out <= q.amount_out + U256::from(8u8), "out {out} quote {}", q.amount_out);
        }
    }
    assert!(crossed >= 2, "the vectors must cross ticks, crossed {crossed}");
}

/// v3-core crosses a tick eagerly on reaching its price: downward it leaves
/// the tick one below.
#[test]
fn a_target_on_an_initialized_tick_crosses_it() {
    let (s, l) = (state(), ladder());
    let down = move_to_price(&s, &l, get_sqrt_ratio_at_tick(-60).unwrap()).unwrap();
    assert_eq!((down.tick, down.liquidity), (-61, 3_000_000_000));
    let up = move_to_price(&s, &l, get_sqrt_ratio_at_tick(60).unwrap()).unwrap();
    assert_eq!((up.tick, up.liquidity), (60, 3_000_000_000));
}

/// Past what the ladder proves, nothing is assumed.
#[test]
fn a_target_past_the_ladder_is_refused() {
    let (s, l) = (state(), ladder());
    assert!(move_to_price(&s, &l, get_sqrt_ratio_at_tick(-170).unwrap()).is_some());
    assert!(move_to_price(&s, &l, get_sqrt_ratio_at_tick(-200).unwrap()).is_none());
    assert!(move_to_price(&s, &l, get_sqrt_ratio_at_tick(250).unwrap()).is_none());
    let unchanged = move_to_price(&s, &l, s.sqrt_price_x96).unwrap();
    assert_eq!((unchanged.tick, unchanged.liquidity), (0, 6_000_000_000));
}
