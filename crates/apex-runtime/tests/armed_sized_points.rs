//! R40.1: pricing a slot through the mirror: the threshold at the band's
//! adverse edge, the tiers, and the expected net across the move.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::engine::BidInputs;
use apex_runtime::armed::plan::SizedTerms;
use apex_runtime::armed::targets::RouteWinners;
use apex_runtime::armed::threshold::{adverse_edge, expected_net, net_at, sized_points, Pricing, MAX_STEP};
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::book::PoolSnapshot;
use apex_runtime::live::frontier::{self, Cycle, WETH};
use apex_runtime::live::gas::LEAN;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use std::collections::BTreeMap;
use std::sync::Arc;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const CAKE: Address = address!("72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38");
const L: u128 = 1_400_000_000_000_000_000_000;

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32, liquidity: u128) -> PoolSnapshot {
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity,
            tick,
            tick_spacing: 10,
            fee_ppm,
            balance0: Some(U256::from(10u128.pow(24))),
            balance1: Some(U256::from(10u128.pow(16))),
        },
        ladder: TickLadder::new(vec![], tick - 30_000, tick + 30_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: Default::default(),
        block: 100,
        last_log: None,
        through: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

fn px() -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: LEAN,
        lenders: None,
        margin_wei: U256::zero(),
        start_per_eth: None,
        rate: None,
        usdc_lenders: None,
        pool_manager: None,
    }
}

fn terms() -> SizedTerms {
    SizedTerms { cap: U256::exp10(18) * 5, rate: None, overhead: 60_000, swap_gas: 250_000, l1_bound: 3_000_000_000, min_net: 1, max_steps: 8, read_gas: 30_000 }
}

/// Both pools at one price: a slot whose trigger must move to open a gap.
fn slot(liquidity: u128) -> ([Arc<PoolSnapshot>; 2], Cycle) {
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500, liquidity);
    let b = pool(CAKE, Venue::PancakeV3, -197_350, 100, liquidity);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a)), (CAKE, Arc::new(b))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let c = cycles.into_values().next().unwrap();
    ([Arc::clone(&snap[&c.legs[0].pool]), Arc::clone(&snap[&c.legs[1].pool])], c)
}

const NONE: &[u128] = &[];

#[test]
fn the_threshold_pays_at_the_adverse_edge_and_not_a_step_before() {
    let (pair, c) = slot(L);
    let points = sized_points(&c, &pair, 0, &px(), &terms(), 5, 10, &BidInputs::winners(NONE), 20_000_000_000);
    let one = &points[0];
    assert_eq!(one.tier, 1);
    let edge = adverse_edge(&c, &pair, 0, 5).unwrap();
    let bid = one.bid.sent.per_gas;
    assert!(net_at(&c, &pair, 0, &edge, one.step, &px(), &terms(), bid).is_some(), "it pays at its threshold");
    assert!(net_at(&c, &pair, 0, &edge, one.step - 1, &px(), &terms(), bid).is_none(), "and not a step before");
}

#[test]
fn a_wider_band_raises_the_threshold() {
    let (pair, c) = slot(L);
    let step = |band| sized_points(&c, &pair, 0, &px(), &terms(), band, 10, &BidInputs::winners(NONE), 20_000_000_000)[0].step;
    assert!(step(10) > step(5));
    assert!(step(20) > step(10));
}

#[test]
fn the_adverse_edge_closes_the_gap_for_either_leg() {
    let (pair, c) = slot(L);
    for leg in 0..2 {
        let edge = adverse_edge(&c, &pair, leg, 20).unwrap();
        let other = &pair[1 - leg];
        let near = net_at(&c, &pair, leg, other, 60, &px(), &terms(), 0).expect("60 steps pay where the other pool is");
        let far = net_at(&c, &pair, leg, &edge, 60, &px(), &terms(), 0).map_or(U256::zero(), |n| n.net_wei);
        assert!(far < near.net_wei, "leg {leg}: the adverse edge pays less");
    }
}

#[test]
fn a_slot_nothing_pays_is_empty() {
    // A pool so thin no move within MAX_STEP pays its gas.
    let (pair, c) = slot(1_000_000);
    assert!(sized_points(&c, &pair, 0, &px(), &terms(), 5, 10, &BidInputs::winners(NONE), 20_000_000_000).is_empty());
    assert!(net_at(&c, &pair, 0, &pair[1], MAX_STEP, &px(), &terms(), 0).is_none());
}

#[test]
fn tier_two_needs_a_bid_at_least_twice_tier_ones() {
    let (pair, c) = slot(L);
    // No measured winners: every bid is zero, so tier 2 adds nothing.
    let flat = sized_points(&c, &pair, 0, &px(), &terms(), 5, 10, &BidInputs::winners(NONE), 20_000_000_000);
    assert_eq!(flat.len(), 1);
    // Winners bidding across three orders of magnitude: bids rise with the surplus.
    let winners: Vec<u128> = (0u32..60).map(|i| 1_000_000u128 * 10u128.pow(i % 4) * u128::from(1 + i / 4)).collect();
    let mut sorted = winners.clone();
    sorted.sort_unstable();
    let points = sized_points(&c, &pair, 0, &px(), &terms(), 5, 10, &BidInputs::winners(&sorted), 20_000_000_000);
    assert_eq!(points.len(), 2, "a second tier");
    let (one, two) = (&points[0], &points[1]);
    assert_eq!(two.tier, 2);
    assert!(two.step > one.step);
    assert!(two.bid.sent.per_gas >= one.bid.sent.per_gas * 2);
}

/// R41.2: a point bids against the winners of gaps its size: under an edge
/// above every net, that band's; over an edge at nothing, the band above's;
/// never R36's list beside them.
#[test]
fn a_point_bids_against_its_own_bands_winners() {
    let (pair, c) = slot(L);
    let winners: Vec<u128> = (0u32..60).map(|i| 1_000_000u128 * 10u128.pow(i % 4) * u128::from(1 + i / 4)).collect();
    let mut sorted = winners.clone();
    sorted.sort_unstable();
    let sent = |inputs: &BidInputs| -> Vec<u128> {
        sized_points(&c, &pair, 0, &px(), &terms(), 5, 10, inputs, 20_000_000_000).iter().map(|p| p.bid.sent.per_gas).collect()
    };
    let banded = |edge: u128, under: &[u128], over: &[u128]| RouteWinners {
        all: NONE.to_vec().into(),
        bands: Some((vec![edge].into(), vec![under.to_vec().into(), over.to_vec().into()])),
    };
    let by = |w: &RouteWinners| sent(&BidInputs { winners: &w.all, banded: Some(w), ..BidInputs::winners(NONE) });
    let plain = sent(&BidInputs::winners(&sorted));
    assert!(plain.iter().any(|b| *b > 0), "the winners price some bid");
    assert_eq!(by(&banded(u128::MAX, &sorted, NONE)), plain, "under the edge: that band's winners");
    assert_eq!(by(&banded(0, NONE, &sorted)), plain, "over an edge at nothing: the band above's");
}

#[test]
fn the_grid_grows_with_the_move_until_the_cap_holds_it() {
    let (pair, c) = slot(L);
    let p = &sized_points(&c, &pair, 0, &px(), &terms(), 5, 10, &BidInputs::winners(NONE), 20_000_000_000)[0];
    for w in p.grid.windows(2) {
        assert!(w[1].1 >= w[0].1, "{:?}", p.grid);
        assert!(w[1].0 > w[0].0);
    }
    let mut small = terms();
    small.cap = U256::exp10(15);
    let capped = &sized_points(&c, &pair, 0, &px(), &small, 5, 10, &BidInputs::winners(NONE), 20_000_000_000)[0];
    assert!(capped.grid[5].1 < p.grid[5].1, "the cap holds the biggest move's net down");
}

#[test]
fn expected_net_weights_each_bucket_by_its_chance() {
    let grid = [(1, 1.0), (2, 5.0), (4, 10.0)];
    let p = |s: u32| match s {
        1 => 0.5,
        2 => 0.2,
        _ => 0.1,
    };
    // 0.3 × 1 + 0.1 × 5 + 0.1 × 10.
    assert!((expected_net(&grid, p) - 1.8).abs() < 1e-12);
    assert_eq!(expected_net(&grid, |_| 0.0), 0.0);
}
