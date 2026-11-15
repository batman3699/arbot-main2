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

// ------------------------------------------------------------- R43.2: a move to a tick

/// What a state holds, for comparing two: its price, liquidity, tick and balances.
type Held = (U256, u128, i32, Option<U256>, Option<U256>);

fn held(s: Option<ClPoolState>) -> Option<Held> {
    s.map(|s| (s.sqrt_price_x96, s.liquidity, s.tick, s.balance0, s.balance1))
}

/// R43.2: a move to a tick's own price is `move_to_price` to that price, to
/// the unit, from every start and to every tick: on initialized ticks and
/// between them, both ways, to where it started, past the ladder's proof and
/// off the protocol's range. `move_to_tick` is given the tick, and so does not
/// search 1.8 million ticks for the one its target is at, twice: that search
/// was nine tenths of an armed quote's time.
#[test]
fn a_move_to_a_tick_is_a_move_to_that_ticks_price() {
    use apex_math::cl_move::move_to_tick;
    let nets: Vec<(i32, i128)> = [(-900, 4_000), (-600, 3_000), (-599, -500), (-10, 2_500), (0, 1_000), (7, -1_000), (300, -2_500), (301, 700), (900, -7_200)]
        .iter()
        .map(|(t, n)| (*t, i128::from(*n) * 1_000_000_000_000))
        .collect();
    let ladders = [TickLadder::new(nets.clone(), -1_200, 1_200), TickLadder::new(nets, -600, 300), TickLadder::new(vec![], -1_200, 1_200)];
    let mut checked = 0;
    for ladder in &ladders {
        // Every tick within two of an initialized tick or of a bound, a spread
        // between them, and the protocol's ends.
        let near = |t: i32| t - 2..=t + 2;
        let mut targets: Vec<i32> = ladder.ticks().iter().flat_map(|(t, _)| near(*t)).collect();
        targets.extend([ladder.lower_bound(), ladder.upper_bound(), -1_200, 1_200].into_iter().flat_map(near));
        targets.extend((-1_300..=1_300).step_by(37));
        targets.extend([-887_273, -887_272, 887_271, 887_272, 887_273]);
        for start in [-700, -599, -10, 0, 7, 300] {
            // On the tick's own price, and a little above it: between ticks.
            for nudge in [0u64, 12_345] {
                let state = ClPoolState {
                    sqrt_price_x96: get_sqrt_ratio_at_tick(start).unwrap() + U256::from(nudge),
                    liquidity: 9_000_000_000_000_000,
                    tick: start,
                    tick_spacing: 1,
                    fee_ppm: 500,
                    balance0: Some(U256::exp10(24)),
                    balance1: Some(U256::exp10(24)),
                };
                for tick in targets.iter().copied() {
                    let by_price = get_sqrt_ratio_at_tick(tick).and_then(|p| move_to_price(&state, ladder, p));
                    checked += usize::from(by_price.is_some());
                    assert_eq!(held(move_to_tick(&state, ladder, tick)), held(by_price), "from {start} (+{nudge}) to {tick}, proven {}..{}", ladder.lower_bound(), ladder.upper_bound());
                }
            }
        }
    }
    assert!(checked > 2_000, "{checked} moves the ladder could make");
}
