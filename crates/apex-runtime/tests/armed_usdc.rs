//! Task 8.5 R37: USDC as a cycle's start and profit token. Costs are paid in
//! ETH; a USDC cycle's are converted at the book's deepest WETH/USDC pool's
//! mid price, as a ratio of integers, rounded against the arm.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::threshold::{start_per_eth, to_start, to_wei};
use apex_runtime::live::book::PoolSnapshot;
use apex_runtime::live::frontier::{build_starts, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use std::collections::BTreeMap;
use std::sync::Arc;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const AXIS: Address = address!("5be75e3B0aFF5d1a042A5363Ae36acB1d6D755Ea");
const DEEP: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const THIN: Address = address!("6c561B446416E1A00E8E93E221854d6eA4171372");
const AXIS_A: Address = address!("e1bfc5ada1125718b53b2affc638ba9981f6070f");
const AXIS_B: Address = address!("e381d2ec7996d9fa1a72e461fb4470db9230f37a");

fn pool(addr: Address, token0: Address, token1: Address, tick: i32, l: u128) -> PoolSnapshot {
    let (lower, upper) = ((tick - 2_000) / 10 * 10, (tick + 2_000) / 10 * 10);
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue: Venue::UniswapV3, token0, token1, fee_ppm: 500, depth_usd: 5e6, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: l,
            tick,
            tick_spacing: 10,
            fee_ppm: 500,
            balance0: Some(U256::exp10(24)),
            balance1: Some(U256::exp10(14)),
        },
        ladder: TickLadder::new(vec![(lower, l as i128), (upper, -(l as i128))], lower - 5_000, upper + 5_000),
        decimals: (18, 6),
        factory: Venue::UniswapV3.factory(),
        code_hash: Default::default(),
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

fn book(pools: Vec<PoolSnapshot>) -> BTreeMap<Address, Arc<PoolSnapshot>> {
    pools.into_iter().map(|p| (p.spec.pool, Arc::new(p))).collect()
}

/// `sqrtP² / 2^192`: USDC units a wei, WETH being token0.
fn two_192() -> U256 {
    U256::one() << 192
}

#[test]
fn a_usdc_arm_s_costs_are_converted_at_the_mid_price() {
    // The deep pool sets the rate, not the thin one at another price.
    let snap = book(vec![pool(DEEP, WETH, USDC, -197_350, 10u128.pow(19)), pool(THIN, WETH, USDC, -197_000, 10u128.pow(15))]);
    let rate = start_per_eth(&snap).expect("a WETH/USDC pool");
    let sqrt = get_sqrt_ratio_at_tick(-197_350).unwrap();
    let expected = (U256::exp10(15) * sqrt * sqrt + two_192() - 1) / two_192();
    assert_eq!(to_start(U256::exp10(15), rate), expected, "rounded up: a cost");
    assert!(expected > U256::from(2_680_000u64) && expected < U256::from(2_700_000u64), "{expected}: $2.68–2.70");
}

#[test]
fn a_surplus_converts_back_rounding_down() {
    let snap = book(vec![pool(DEEP, WETH, USDC, -197_350, 10u128.pow(19))]);
    let rate = start_per_eth(&snap).unwrap();
    let sqrt = get_sqrt_ratio_at_tick(-197_350).unwrap();
    let units = U256::from(2_685_000u64);
    assert_eq!(to_wei(units, rate), units * two_192() / (sqrt * sqrt), "rounded down: a surplus");
    assert!(to_wei(to_start(U256::exp10(15), rate), rate) >= U256::exp10(15) - 1);
}

#[test]
fn no_weth_usdc_pool_no_rate() {
    assert_eq!(start_per_eth(&book(vec![pool(AXIS_A, AXIS, USDC, -289_960, 10u128.pow(19))])), None);
}

#[test]
fn usdc_cycles_start_and_end_in_usdc() {
    let snap = book(vec![
        pool(AXIS_A, AXIS, USDC, -289_960, 10u128.pow(19)),
        pool(AXIS_B, AXIS, USDC, -289_900, 10u128.pow(19)),
        pool(DEEP, WETH, USDC, -197_350, 10u128.pow(19)),
        pool(THIN, WETH, USDC, -197_300, 10u128.pow(19)),
    ]);
    let (_, cycles) = build_starts(ChainId(8453), &[WETH, USDC], &snap);
    let usdc: Vec<_> = cycles.values().filter(|c| c.start == USDC).collect();
    let weth: Vec<_> = cycles.values().filter(|c| c.start == WETH).collect();
    assert!(usdc.iter().any(|c| [c.legs[0].pool, c.legs[1].pool].contains(&AXIS_A)), "AXIS/USDC cycles from USDC");
    for c in &usdc {
        assert_eq!((c.legs[0].token_in, c.legs[1].token_out), (USDC, USDC));
    }
    assert!(weth.iter().all(|c| c.legs[0].token_in == WETH && c.legs[1].token_out == WETH));
    assert!(!weth.is_empty(), "the WETH/USDC pair still makes WETH cycles");
    assert_eq!(cycles.len(), usdc.len() + weth.len());
}

/// A USDC cycle is sized in USDC's own units: its search starts at a dollar,
/// not at 10^18 units. With the 18-decimal default its smallest size was
/// 10^12 units, $1,000,000, over every AXIS/USDC pool's holding: no USDC
/// route armed in the shadow (2026-10-29).
#[test]
fn a_usdc_cycle_is_sized_in_usdc_s_units() {
    use apex_runtime::armed::threshold::{best_at, Pricing};
    use apex_runtime::econ::RouteCosts;
    use apex_runtime::live::gas;
    use apex_runtime::live::lenders::Lenders;
    let held = |mut p: PoolSnapshot| {
        p.state.balance1 = Some(U256::from(50_000_000_000u64)); // $50,000 of USDC
        p
    };
    let snap = book(vec![held(pool(DEEP, WETH, USDC, -197_350, 10u128.pow(17))), held(pool(THIN, WETH, USDC, -197_000, 10u128.pow(17)))]);
    let (_, cycles) = build_starts(ChainId(8453), &[USDC], &snap);
    let px = Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: gas::LEAN,
        lenders: Some(Lenders { balancer: U256::exp10(21), morpho: None }),
        margin_wei: U256::zero(),
        start_per_eth: start_per_eth(&snap),
        rate: None,
        usdc_lenders: Some(Lenders { balancer: U256::zero(), morpho: Some(U256::exp10(13)) }),
        pool_manager: None,
    };
    let points: Vec<_> = cycles
        .values()
        .filter_map(|c| {
            let pair = [Arc::clone(&snap[&c.legs[0].pool]), Arc::clone(&snap[&c.legs[1].pool])];
            best_at(c, &pair, 0, (*pair[0]).clone(), &px.for_cycle(c)?)
        })
        .collect();
    assert_eq!(points.len(), 1, "the direction across the gap pays");
    let size = points[0].size;
    assert!(size > U256::from(1_000_000u64) && size < U256::from(50_000_000_000u64), "{size}: between $1 and the pools' $50,000");
}
