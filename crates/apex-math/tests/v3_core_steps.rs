//! The multi-tick quote takes v3-core's steps, because each step rounds its fee
//! up on its own: a step ends at every initialized tick — **a zero net
//! included** — and at every bitmap word's edge. A quote that took fewer steps
//! over-states its output by about a unit of the input token per step skipped,
//! and a hop whose minimum is that quote reverts.
//!
//! Found 2026-10-04 by simulating a PancakeSwap hop through the deployed
//! executor: the ladder had dropped two zero-net ticks, and the book's quote
//! beat PancakeSwap's own quoter by one unit of USDC's worth of WETH.

use apex_math::cl_math::{compute_swap_step, get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::{quote_exact_input_multi_tick, word_edge, TickLadder};
use ethers_core::types::U256;
use serde_json::Value;

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pancake_zero_net_ticks.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn dec(v: &Value) -> U256 {
    U256::from_dec_str(v.as_str().unwrap()).unwrap()
}

/// The fixture's state, and its ladder — whole, or with `keep` applied.
fn recorded(keep: impl Fn(i128) -> bool) -> (ClPoolState, TickLadder, Vec<(U256, U256)>) {
    let f = fixture();
    let state = ClPoolState {
        sqrt_price_x96: dec(&f["sqrt_price_x96"]),
        liquidity: f["liquidity"].as_str().unwrap().parse().unwrap(),
        tick: f["tick"].as_i64().unwrap() as i32,
        tick_spacing: f["tick_spacing"].as_i64().unwrap() as i32,
        fee_ppm: f["fee_ppm"].as_u64().unwrap() as u32,
        balance0: None,
        balance1: None,
    };
    let ticks = f["ladder"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e[0].as_i64().unwrap() as i32, e[1].as_str().unwrap().parse::<i128>().unwrap()))
        .filter(|(_, net)| keep(*net))
        .collect();
    let band = &f["ladder_band"];
    let ladder = TickLadder::new(ticks, band[0].as_i64().unwrap() as i32, band[1].as_i64().unwrap() as i32);
    let quotes = f["quotes"].as_array().unwrap().iter().map(|q| (dec(&q["usdc_in"]), dec(&q["weth_out"]))).collect();
    (state, ladder, quotes)
}

/// **To the wei against PancakeSwap's quoter**, on paths through the pool's
/// zero-net ticks (-197425, -197423), recorded on Base.
#[test]
fn quotes_through_zero_net_ticks_match_the_venues_quoter() {
    let (state, ladder, quotes) = recorded(|_| true);
    for (usdc_in, weth_out) in quotes {
        let q = quote_exact_input_multi_tick(&state, &ladder, usdc_in, false, 64).unwrap();
        assert!(!q.exhausted, "{usdc_in}");
        assert_eq!(q.amount_out, weth_out, "{usdc_in} USDC in");
    }
}

/// And the ladder as it was built before — zero nets dropped — over-states
/// exactly the quotes whose paths cross them, which is what made a hop's
/// minimum unreachable.
#[test]
fn without_its_zero_net_ticks_the_quote_over_states() {
    let (state, ladder, quotes) = recorded(|net| net != 0);
    let mut over = 0;
    for (usdc_in, weth_out) in quotes {
        let q = quote_exact_input_multi_tick(&state, &ladder, usdc_in, false, 64).unwrap();
        assert!(q.amount_out >= weth_out, "never under: {usdc_in}");
        if q.amount_out > weth_out {
            over += 1;
        }
    }
    assert!(over > 0, "no path crossed a zero-net tick: the fixture no longer tests it");
}

/// v3-core's arithmetic, transliterated: Solidity's division truncates toward
/// zero and its `%` takes the dividend's sign, `>>` on a signed value is
/// arithmetic, and `uint8(int8(x))` is two's complement.
fn v3_core_edge(tick: i32, spacing: i32, lte: bool) -> i32 {
    let mut compressed = tick / spacing;
    if tick < 0 && tick % spacing != 0 {
        compressed -= 1;
    }
    let position = |t: i32| -> (i32, i32) { (t >> 8, i32::from((t % 256) as i8 as u8)) };
    let edge = if lte {
        let (_, bit) = position(compressed);
        (compressed - bit) * spacing
    } else {
        let (_, bit) = position(compressed + 1);
        (compressed + 1 + (255 - bit)) * spacing
    };
    edge.clamp(MIN_TICK, MAX_TICK)
}

#[test]
fn word_edge_is_v3_cores_uninitialized_fallback() {
    for (tick, spacing, lte, want) in [
        (0, 1, true, 0),
        (0, 1, false, 255),
        (-1, 1, true, -256),
        (-1, 1, false, 255),
        (255, 1, true, 0),
        (255, 1, false, 511),
        (-256, 1, true, -256),
        (-256, 1, false, -1),
        (-197_339, 10, true, -199_680),
        (-197_339, 10, false, -197_130),
        (MAX_TICK, 1, false, MAX_TICK),
        (MIN_TICK, 1, true, MIN_TICK),
    ] {
        assert_eq!(word_edge(tick, spacing, lte), want, "tick {tick} spacing {spacing} lte {lte}");
    }
    for spacing in [1, 10, 60, 100, 200] {
        for tick in (-200_000..200_000).step_by(997) {
            for lte in [true, false] {
                assert_eq!(word_edge(tick, spacing, lte), v3_core_edge(tick, spacing, lte), "{tick} {spacing} {lte}");
            }
        }
    }
}

fn ratio(tick: i32) -> U256 {
    get_sqrt_ratio_at_tick(tick).unwrap()
}

/// Constant liquidity from -600 to 600 with nothing initialized between, at
/// tick 250, spacing `spacing`, plus `extra` ticks.
fn wide(spacing: i32, extra: &[(i32, i128)]) -> (ClPoolState, TickLadder) {
    let l = 10u128.pow(18);
    let state = ClPoolState {
        sqrt_price_x96: ratio(250) + U256::from(12_345u64),
        liquidity: l,
        tick: 250,
        tick_spacing: spacing,
        fee_ppm: 500,
        balance0: None,
        balance1: None,
    };
    let mut ticks = vec![(-600, l as i128), (600, -(l as i128))];
    ticks.extend_from_slice(extra);
    (state, TickLadder::new(ticks, -768, 767))
}

/// v3-core's steps, composed by hand: to each boundary in turn, the last being
/// where the input runs out short of it.
fn stepped(state: &ClPoolState, boundaries: &[i32], amount: U256) -> U256 {
    let (mut price, mut left, mut out) = (state.sqrt_price_x96, amount, U256::zero());
    for b in boundaries {
        let s = compute_swap_step(price, ratio(*b), state.liquidity, left, state.fee_ppm).unwrap();
        out += s.amount_out;
        left -= s.amount_in + s.fee_amount;
        price = s.sqrt_price_next;
        if price != ratio(*b) {
            return out;
        }
    }
    out
}

/// **A word's edge ends a step.** At spacing 1, from tick 250 the price meets
/// word 0's edge, 255, before any initialized tick, and v3-core stops there.
/// The quote is that composition exactly — and, for some sizes, not the
/// single step a quote that ignored the edge would take.
#[test]
fn a_word_edge_ends_a_step() {
    let (state, ladder) = wide(1, &[]);
    let mut differs = false;
    // Tick 255 is ~2.5e14 of input away, tick 511 ~1.4e16: the smaller sizes
    // stop before the edge, the rest cross it.
    for amount in [3u64, 10, 50, 100, 120].map(|n| U256::from(n) * U256::exp10(13)) {
        let q = quote_exact_input_multi_tick(&state, &ladder, amount, false, 64).unwrap();
        // Word 0's edge, then word 1's: the initialized tick at 600 lies past it.
        assert_eq!(q.amount_out, stepped(&state, &[255, 511], amount), "{amount}");
        assert_eq!(q.ticks_crossed, 0, "an edge is not a tick: nothing crossed, nothing counted");
        differs |= q.amount_out != stepped(&state, &[600], amount);
    }
    assert!(differs, "no size distinguishes one step from two: the test proves nothing");
}

/// **The quote counts the steps that cost gas.** A settlement's gas follows
/// v3-core's loop: each initialized tick it crosses writes that tick, and each
/// word's edge it passes reads the next word of the bitmap. The two are counted
/// apart, because they cost different amounts: an edge is a step that crosses
/// nothing.
#[test]
fn the_quote_counts_crossings_and_word_edges_apart() {
    // Spacing 1, tick 250, a zero-net tick at 300: up from 250 the steps end at
    // 255 (word 0's edge), 300 (initialized), 511 (word 1's edge), then short
    // of 600. Tick 255 is ~2.5e14 of input away, 300 ~2.8e15, 511 ~1.4e16.
    let (state, ladder) = wide(1, &[(300, 0)]);
    for (amount, edges, crossed) in [(3u64, 0, 0), (50, 1, 0), (500, 1, 1), (1_500, 2, 1)] {
        let amount = U256::from(amount) * U256::exp10(13);
        let q = quote_exact_input_multi_tick(&state, &ladder, amount, false, 64).unwrap();
        assert!(!q.exhausted, "{amount}");
        assert_eq!((q.word_steps, q.ticks_crossed), (edges, crossed), "{amount}");
    }
}

/// **A zero-net tick ends a step** too, and changes nothing else: at spacing
/// 10 no word edge is near, so the only extra stop is the tick at 300.
#[test]
fn a_zero_net_tick_ends_a_step() {
    let (state, ladder) = wide(10, &[(300, 0)]);
    let (_, without) = wide(10, &[]);
    let mut differs = false;
    // Tick 300 is ~2.5e15 of input away, tick 600 ~1.8e16: every size here
    // crosses the zero-net tick and stops short of the next.
    for amount in [3u64, 5, 8, 10, 15].map(|n| U256::from(n) * U256::exp10(15)) {
        let q = quote_exact_input_multi_tick(&state, &ladder, amount, false, 64).unwrap();
        assert_eq!(q.amount_out, stepped(&state, &[300, 600], amount), "{amount}");
        let plain = quote_exact_input_multi_tick(&state, &without, amount, false, 64).unwrap();
        assert_eq!(plain.amount_out, stepped(&state, &[600], amount));
        differs |= q.amount_out != plain.amount_out;
    }
    assert!(differs, "no size distinguishes the split: the test proves nothing");
}
