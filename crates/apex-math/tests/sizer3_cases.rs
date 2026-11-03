//! Task 8.5 R40.1: `contracts/lean/LeanSizer3.sol` is held to `apex_math::walk`
//! through `apex-exec/tests/fixtures/sizer3_cases.json`: the spec's matrix of
//! pool pairs, and seeded random ones, each with the size, the quoted outputs,
//! the boundaries and the flags the walk gives. Rust writes the file
//! (`cargo test -p apex-math --test sizer3_cases -- --ignored`), and this test
//! and `test/lean/LeanSizer3.t.sol` both check every case in it.

use apex_math::cl_math::{get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_math::walk::{quote, walk, WalkLeg, WalkPrice, MAX_STEPS, MAX_STEPS_LIMIT};
use ethers_core::types::U256;
use serde_json::{json, Value};
use std::path::PathBuf;

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../apex-exec/tests/fixtures/sizer3_cases.json")
}

/// The executor's hop kinds: 0 Uniswap v3, 1 PancakeSwap, 2 Aerodrome v2,
/// 3 Uniswap v4, 4 Slipstream.
#[derive(Clone)]
enum Pool {
    Cl { kind: u8, tick: i32, liquidity: u128, fee: u32, spacing: i32, cross_gas: u64, ticks: Vec<(i32, i128)>, hooks: &'static str },
    Reserves { reserve_in: u128, reserve_out: u128, fee: u32 },
}

#[derive(Clone)]
struct Case {
    name: String,
    /// Hop 0 sells token0 (the start is the lower address).
    zero_for_one: bool,
    pools: [Pool; 2],
    cap: U256,
    wei_per_gas: u64,
    rate: Option<u64>,
    max_steps: u32,
}

const L: u128 = 1_000_000_000_000_000_000_000_000;
const NO_HOOK: &str = "0x0000000000000000000000000000000000000000";
/// beforeSwap and afterSwap, neither returning a delta.
const HOOK: &str = "0x00000000000000000000000000000000000010c0";

fn cl(kind: u8, tick: i32, fee: u32, spacing: i32, ticks: Vec<(i32, i128)>) -> Pool {
    Pool::Cl { kind, tick, liquidity: L, fee, spacing, cross_gas: 30_000, ticks, hooks: NO_HOOK }
}

fn case(name: &str, zero_for_one: bool, pools: [Pool; 2]) -> Case {
    Case { name: name.into(), zero_for_one, pools, cap: U256::MAX, wei_per_gas: 0, rate: None, max_steps: MAX_STEPS }
}

fn state(tick: i32, liquidity: u128, fee: u32, spacing: i32) -> ClPoolState {
    ClPoolState {
        sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
        liquidity,
        tick,
        tick_spacing: spacing,
        fee_ppm: fee,
        balance0: None,
        balance1: None,
    }
}

/// Hop 0 falls when it sells token0, so its pool sits dear; hop 1 rises.
fn matrix() -> Vec<Case> {
    let half = (L / 2) as i128;
    let whole = L as i128;
    let mut out = vec![
        case("single_tick", true, [cl(0, 6_100, 500, 60, vec![]), cl(0, 6_000, 500, 60, vec![])]),
        case("one_for_zero", false, [cl(0, -6_100, 500, 60, vec![]), cl(0, -6_000, 500, 60, vec![])]),
        case(
            "multi_tick_walk",
            true,
            [cl(0, 6_300, 500, 60, vec![(6_240, -half), (6_180, -half), (6_060, half)]), cl(0, 6_000, 500, 60, vec![(6_060, half), (6_120, -half)])],
        ),
        case("negative_net_deepens", true, [cl(0, 6_100, 500, 60, vec![(6_060, -whole)]), cl(0, 6_000, 500, 60, vec![])]),
        case("positive_net_narrows", true, [cl(0, 6_100, 500, 60, vec![(6_060, half)]), cl(0, 6_000, 500, 60, vec![])]),
        case("tick_exactly_at_boundary", true, [cl(0, 6_120, 500, 60, vec![(6_120, -half)]), cl(0, 6_000, 500, 60, vec![])]),
        case("liquidity_runs_out", true, [cl(0, 6_100, 500, 60, vec![(6_060, whole)]), cl(0, 6_000, 500, 60, vec![])]),
        case("fee_rounding_small_gap", true, [cl(0, 6_011, 500, 1, vec![]), cl(0, 6_000, 500, 1, vec![])]),
        case("word_edge_step", true, [cl(0, 300, 100, 1, vec![]), cl(0, 200, 100, 1, vec![])]),
        case("zero_net_tick", true, [cl(0, 6_100, 500, 60, vec![(6_060, 0)]), cl(0, 6_000, 500, 60, vec![])]),
        case("several_words_negative_ticks", true, [cl(0, -200, 100, 1, vec![]), cl(0, -1_000, 100, 1, vec![])]),
        case("pancake_then_slipstream", true, [cl(1, 6_100, 100, 1, vec![(6_080, -half)]), cl(4, 6_000, 400, 1, vec![(6_030, half)])]),
        case("v4_no_hook", true, [cl(3, 6_100, 3_000, 60, vec![(6_060, -half)]), cl(0, 5_900, 500, 60, vec![])]),
        case(
            "v4_allowed_hook",
            false,
            // Hop 0 buys token0 where it is cheap; hop 1, the hooked v4 pool, sells it dear.
            [cl(0, -6_100, 500, 60, vec![]), Pool::Cl { kind: 3, tick: -5_900, liquidity: L, fee: 3_000, spacing: 60, cross_gas: 25_000, ticks: vec![(-5_940, -half)], hooks: HOOK }],
        ),
        case("aero_volatile_first", true, [Pool::Reserves { reserve_in: L / 1_000, reserve_out: L / 1_000 * 185 / 100, fee: 3_000 }, cl(0, 6_000, 500, 60, vec![])]),
        case("aero_volatile_second", true, [cl(0, 6_400, 500, 60, vec![(6_300, -half)]), Pool::Reserves { reserve_in: L * 18 / 10, reserve_out: L, fee: 3_000 }]),
    ];
    let mut usdc = case(
        "usdc_six_decimals",
        true,
        [Pool::Cl { kind: 0, tick: 6_100, liquidity: 2_000_000_000_000, fee: 500, spacing: 10, cross_gas: 30_000, ticks: vec![(6_070, -1_000_000_000_000)], hooks: NO_HOOK },
         Pool::Cl { kind: 0, tick: 6_000, liquidity: 3_000_000_000_000, fee: 500, spacing: 10, cross_gas: 30_000, ticks: vec![], hooks: NO_HOOK }],
    );
    usdc.wei_per_gas = 10_000_000;
    usdc.rate = Some(3_150_000_000);
    out.push(usdc);
    let mut rated = case("usdc_start_rate_charges_crossings", true, [cl(0, 6_100, 500, 60, vec![(6_060, -whole)]), cl(0, 6_000, 500, 60, vec![])]);
    rated.wei_per_gas = 1_000_000_000;
    rated.rate = Some(3_150_000_000);
    out.push(rated);
    let mut refused = case("crossing_refused", true, [cl(0, 6_100, 500, 60, vec![(6_060, -whole)]), cl(0, 6_000, 500, 60, vec![])]);
    refused.wei_per_gas = 1_000_000_000_000_000_000;
    out.push(refused);
    let mut short = case("max_steps_clamp", true, [cl(0, -200, 100, 1, vec![]), cl(0, -1_000, 100, 1, vec![])]);
    short.max_steps = 1;
    out.push(short);
    let mut most = case("max_steps_limit", true, [cl(0, -200, 100, 1, vec![]), cl(0, -1_000, 100, 1, vec![])]);
    most.max_steps = MAX_STEPS_LIMIT;
    out.push(most);
    let mut capped = case("cap_clamp", true, [cl(0, 6_100, 500, 60, vec![]), cl(0, 6_000, 500, 60, vec![])]);
    capped.cap = U256::exp10(21);
    out.push(capped);
    // The input that takes the first pool exactly to its tick at 6,060.
    let mut exact = case("exact_boundary_amount", true, [cl(0, 6_100, 500, 60, vec![(6_060, -whole)]), cl(0, 6_000, 500, 60, vec![])]);
    let at = apex_math::cl_math::get_amount0_delta(get_sqrt_ratio_at_tick(6_060).unwrap(), get_sqrt_ratio_at_tick(6_100).unwrap(), L, true).unwrap();
    exact.cap = apex_math::cl_math::mul_div_rounding_up(at, U256::from(1_000_000u64), U256::from(999_500u64)).unwrap();
    out.push(exact);
    out
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
}

fn random(n: usize) -> Vec<Case> {
    let mut rng = Rng(0xC0FFEE51CE);
    let mut out = Vec::new();
    while out.len() < n {
        let spacing = [1, 10, 60][rng.range(0, 2) as usize];
        let high = rng.range(-30_000, 30_000) as i32 / spacing * spacing;
        let gap = rng.range(2, 40) as i32 * spacing;
        let down = rng.next().is_multiple_of(2);
        let pool = |rng: &mut Rng, tick: i32, falls: bool| {
            let kind = [0u8, 1, 3, 4][rng.range(0, 3) as usize];
            let liquidity = 10u128.pow(rng.range(21, 25) as u32) * rng.range(1, 9) as u128;
            let mut ticks = Vec::new();
            let (mut at, mut l) = (tick, liquidity as f64);
            for _ in 0..rng.range(0, 4) {
                at = if falls { at - rng.range(1, 6) as i32 * spacing } else { at + rng.range(1, 6) as i32 * spacing };
                let after = l * [0.4, 0.7, 1.5, 2.5][rng.range(0, 3) as usize];
                ticks.push((at, (if falls { l - after } else { after - l }) as i128));
                l = after;
            }
            Pool::Cl { kind, tick, liquidity, fee: [100, 500, 3_000][rng.range(0, 2) as usize], spacing, cross_gas: 30_000, ticks, hooks: NO_HOOK }
        };
        let (one, two) = if down { (high, high - gap) } else { (high - gap, high) };
        let pools = [pool(&mut rng, one, down), pool(&mut rng, two, !down)];
        let mut c = case(&format!("random_{}", out.len()), down, pools);
        c.wei_per_gas = [0, 10_000_000, 1_000_000_000][rng.range(0, 2) as usize];
        out.push(c);
    }
    out
}

fn expected(c: &Case) -> Option<Value> {
    let states: Vec<Option<(ClPoolState, TickLadder)>> = c
        .pools
        .iter()
        .map(|p| match p {
            Pool::Cl { tick, liquidity, fee, spacing, ticks, .. } => {
                Some((state(*tick, *liquidity, *fee, *spacing), TickLadder::new(ticks.clone(), MIN_TICK, MAX_TICK)))
            }
            Pool::Reserves { .. } => None,
        })
        .collect();
    let legs: Vec<WalkLeg> = c
        .pools
        .iter()
        .zip(&states)
        .enumerate()
        .map(|(i, (p, s))| {
            let zero_for_one = if i == 0 { c.zero_for_one } else { !c.zero_for_one };
            match (p, s) {
                (Pool::Cl { cross_gas, .. }, Some((state, ladder))) => WalkLeg::Cl { state, ladder, zero_for_one, cross_gas: *cross_gas },
                (Pool::Reserves { reserve_in, reserve_out, fee }, _) => {
                    WalkLeg::Reserves { reserve_in: U256::from(*reserve_in), reserve_out: U256::from(*reserve_out), fee_ppm: *fee }
                }
                _ => unreachable!(),
            }
        })
        .collect();
    let legs = [legs[0], legs[1]];
    let price = WalkPrice { wei_per_gas: U256::from(c.wei_per_gas), rate: c.rate.map(U256::from) };
    let (size, mid, out, steps, capped, truncated, cross_gas) = match walk(legs, c.cap, price, c.max_steps) {
        Some(w) => {
            let [mid, out] = quote(legs, w.size)?;
            (w.size, mid, out, w.steps, w.capped, w.truncated, w.cross_gas)
        }
        None => (U256::zero(), U256::zero(), U256::zero(), 0, false, false, 0),
    };
    Some(json!({
        "size": size.to_string(), "mid": mid.to_string(), "out": out.to_string(), "steps": steps,
        "flags": u8::from(capped) | (u8::from(truncated) << 1), "cross_gas": cross_gas,
    }))
}

fn to_json(c: &Case) -> Value {
    let pools: Vec<Value> = c
        .pools
        .iter()
        .map(|p| match p {
            Pool::Cl { kind, tick, liquidity, fee, spacing, cross_gas, ticks, hooks } => json!({
                "kind": kind, "tick": tick, "liquidity": liquidity.to_string(), "fee": fee, "spacing": spacing,
                "cross_gas": cross_gas, "hooks": hooks,
                "ticks": ticks.iter().map(|t| t.0).collect::<Vec<_>>(),
                "nets": ticks.iter().map(|t| t.1.to_string()).collect::<Vec<_>>(),
            }),
            Pool::Reserves { reserve_in, reserve_out, fee } => json!({
                "kind": 2, "reserve_in": reserve_in.to_string(), "reserve_out": reserve_out.to_string(), "fee": fee,
            }),
        })
        .collect();
    json!({
        "zero_for_one": c.zero_for_one, "pools": pools, "cap": c.cap.to_string(), "wei_per_gas": c.wei_per_gas.to_string(),
        "rate": c.rate.unwrap_or(0).to_string(), "max_steps": c.max_steps, "expected": expected(c),
    })
}

fn cases() -> Value {
    let all: Vec<Case> = matrix().into_iter().chain(random(24)).collect();
    let names: Vec<String> = all.iter().map(|c| c.name.clone()).collect();
    let mut by_name = serde_json::Map::new();
    for c in &all {
        by_name.insert(c.name.clone(), to_json(c));
    }
    json!({
        "_provenance": "Task 8.5 R40.1. Written by crates/apex-math/tests/sizer3_cases.rs from apex_math::walk; \
            that test and test/lean/LeanSizer3.t.sol check every case. Hop 0 sells token0 when zero_for_one. \
            Regenerate only when the Rust is right.",
        "names": names, "cases": by_name,
    })
}

#[test]
#[ignore = "writes the fixture"]
fn regenerate_sizer3_cases() {
    std::fs::write(path(), serde_json::to_string_pretty(&cases()).unwrap() + "\n").unwrap();
}

#[test]
fn the_fixture_is_still_what_the_walk_computes() {
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(on_disk, cases());
}

#[test]
fn every_case_has_an_answer_and_most_trade() {
    let all: Vec<Case> = matrix().into_iter().chain(random(24)).collect();
    let mut trading = 0;
    for c in &all {
        let e = expected(c).unwrap_or_else(|| panic!("{}: the quote refused its size", c.name));
        if e["size"] != "0" {
            trading += 1;
        }
    }
    assert!(trading * 10 >= all.len() * 8, "{trading} of {} trade", all.len());
}
