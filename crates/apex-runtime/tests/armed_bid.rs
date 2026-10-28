//! Task 8.5 R36 — the bid, per gas: what the winners on a route paid per gas,
//! the bid that maximises the chance of winning times what the win keeps
//! (a validity transaction that loses is never included, so the tip is paid
//! only on a win), and bands that pay the arm's own tip.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::bid::{best_bid, rank_value, Bid};
use apex_runtime::armed::engine::{bid_and_band, Engine, EngineSettings, REPRICE_BUDGET};
use apex_runtime::armed::lines::ArmedLine;
use apex_runtime::armed::threshold::{band_paying, fixed, rungs, Pricing};
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::book::PoolSnapshot;
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::gas;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

const BIDS: [u128; 4] = [0, 1_000_000, 10_000_000, 50_000_000];

#[test]
fn the_bid_maximises_the_chance_times_what_is_left() {
    // net 0.00005 ETH, 500k gas: b = 10_000_001 keeps 0.75 × (5e13 − 5e12) = 3.375e13, beating
    // 50_000_001 (1.0 × 2.5e13), 1_000_001 (0.5 × 4.95e13) and 1 (0.25 × 5e13).
    let b = best_bid(&BIDS, U256::from(50_000_000_000_000u64), 500_000, u128::MAX);
    assert_eq!((b.per_gas, b.p_win), (10_000_001, 0.75));
}

#[test]
fn the_cap_bounds_the_bid() {
    assert!(best_bid(&BIDS, U256::from(10u64.pow(17)), 500_000, 2_000_000).per_gas <= 2_000_000);
    assert_eq!(best_bid(&BIDS, U256::from(10u64.pow(17)), 500_000, 1_000_001).per_gas, 1_000_001, "the cap itself may be bid");
}

#[test]
fn a_tie_keeps_the_lower_bid() {
    // net 3, gas 1: b = 1 keeps 1 × 2, b = 2 keeps 2 × 1.
    assert_eq!(best_bid(&[0, 1], U256::from(3u64), 1, u128::MAX), Bid { per_gas: 1, p_win: 0.5 });
}

#[test]
fn nothing_measured_bids_nothing() {
    assert_eq!(best_bid(&[], U256::from(10u64.pow(15)), 500_000, u128::MAX), Bid { per_gas: 0, p_win: 0.0 });
}

#[test]
fn a_thin_net_bids_nothing() {
    assert_eq!(best_bid(&BIDS, U256::from(1_000u64), 500_000, u128::MAX).per_gas, 0);
}

#[test]
fn more_gas_never_buys_a_higher_bid_for_the_same_net() {
    for net in [10u64.pow(13), 5 * 10u64.pow(13), 10u64.pow(15), 10u64.pow(17)] {
        let light = best_bid(&BIDS, U256::from(net), 300_000, u128::MAX).per_gas;
        let heavy = best_bid(&BIDS, U256::from(net), 600_000, u128::MAX).per_gas;
        assert!(heavy <= light, "net {net}: {heavy} at 600k gas over {light} at 300k");
    }
}

#[test]
fn the_ranking_counts_the_chance_of_winning() {
    let surplus = U256::from(10u64.pow(15));
    let unlikely = Bid { per_gas: 1_000_000, p_win: 0.25 };
    let likely = Bid { per_gas: 1_000_000, p_win: 0.75 };
    assert!(rank_value(0.5, &likely, surplus, 500_000) > rank_value(0.5, &unlikely, surplus, 500_000));
    let spent = Bid { per_gas: 10u128.pow(10), p_win: 1.0 };
    assert!(rank_value(0.5, &spent, surplus, 500_000) < 0.0, "a tip over the surplus ranks below nothing");
}

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const SLIP: Address = address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const L: u128 = 1_400_000_000_000_000_000;
const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32) -> PoolSnapshot {
    let spacing = 10;
    let lower = (tick - 2_000) / spacing * spacing;
    let upper = (tick + 2_000) / spacing * spacing;
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0, v4: None },
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

fn px() -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: gas::MEASURED,
        lenders: Some(apex_runtime::live::lenders::Lenders { balancer: U256::from(10u128.pow(21)), morpho: None }),
        margin_wei: U256::zero(),
    }
}

fn snap() -> BTreeMap<Address, Arc<PoolSnapshot>> {
    [(UNI, Arc::new(pool(UNI, Venue::UniswapV3, -197_350, 500))), (SLIP, Arc::new(pool(SLIP, Venue::Slipstream, -197_350, 400)))]
        .into_iter()
        .collect()
}

fn settings(max_priority_wei_per_gas: u128) -> EngineSettings {
    EngineSettings {
        max_arms: 16,
        rungs: 6,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        chain_id: 8453,
        reprice_budget: REPRICE_BUDGET,
        reprice_for: Duration::MAX,
        workers: 1,
        max_priority_wei_per_gas,
    }
}

#[test]
fn every_band_clears_the_hurdle_plus_its_tip() {
    let snap = snap();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let s = settings(u128::MAX);
    let (mut checked, mut narrowed) = (0, 0);
    for c in cycles.values() {
        let pair = [Arc::clone(&snap[&c.legs[0].pool]), Arc::clone(&snap[&c.legs[1].pool])];
        for leg in 0..2 {
            for r in rungs(c, &pair, leg, &px(), 6) {
                // One winner, who paid half the rung's surplus: the arm bids
                // just over it, a tip of about half its surplus.
                let gas = r.point.gas.expected.0;
                let winner = (r.point.surplus / U256::from(2 * gas)).as_u128();
                let Some((b, bid)) = bid_and_band(c, &pair, leg, &r, &px(), &s, &[winner]) else { continue };
                assert_eq!(bid.per_gas, winner + 1);
                let tip = U256::from(bid.per_gas) * U256::from(gas);
                let paying = Pricing { margin_wei: px().margin_wei + tip, ..px() };
                for edge in &b.edges {
                    assert!(fixed(c, &pair, leg, &r.trigger, edge, r.point.size, b.point.mid, &paying).is_some(), "an edge pays the tip");
                }
                let free = band_paying(c, &pair, leg, &r, &px(), 5, U256::zero()).unwrap();
                narrowed += usize::from(free.edges[0].state.tick != b.edges[0].state.tick);
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "some band pays half its surplus");
    assert!(narrowed > 0, "the tip narrows some band: without it the edges would not pay");
}

#[test]
fn every_arm_carries_its_bid() {
    let snap = snap();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = EngineSettings {
        max_arms: 16,
        rungs: 6,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        chain_id: 8453,
        reprice_budget: REPRICE_BUDGET,
        reprice_for: Duration::MAX,
        workers: 1,
        max_priority_wei_per_gas: 20_000_000,
    };
    let engine = Engine::new(cycles, settings, BTreeSet::new()).with_bids(Arc::new(|_, _| BIDS.to_vec()));
    let arms: Vec<_> = engine
        .tick(&snap, 1_000, 5_000_000, &px(), 1)
        .into_iter()
        .filter_map(|l| if let ArmedLine::Arm(a) = l { Some(a) } else { None })
        .collect();
    assert!(!arms.is_empty());
    for a in &arms {
        let bid: u128 = a.bid_wei_per_gas.parse().unwrap();
        assert!(bid <= 20_000_000, "under the cap");
        assert!(bid == 0 || BIDS.iter().any(|b| b + 1 == bid), "a measured bid + 1 wei, or nothing: {bid}");
        let below = BIDS.iter().filter(|b| **b < bid).count() as f64 / BIDS.len() as f64;
        assert_eq!(a.p_win, below, "the chance is the share of winners it outbids");
    }
    assert!(arms.iter().any(|a| a.bid_wei_per_gas != "0"), "the fixture's surplus pays some bid");
}

#[test]
fn a_rung_that_outbids_no_one_takes_no_sender() {
    let snap = snap();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = EngineSettings {
        max_arms: 16,
        rungs: 6,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        chain_id: 8453,
        reprice_budget: REPRICE_BUDGET,
        reprice_for: Duration::MAX,
        workers: 1,
        max_priority_wei_per_gas: u128::MAX,
    };
    // Winners who paid a whole ETH per gas: no surplus here outbids them.
    let engine = Engine::new(cycles, settings, BTreeSet::new()).with_bids(Arc::new(|_, _| vec![10u128.pow(18)]));
    assert!(engine.tick(&snap, 1_000, 5_000_000, &px(), 1).is_empty());
}
