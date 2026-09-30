//! Task 8.5 R4 — the live frontier and the local pricer.
//!
//! Pools here are built from real Base shapes (WETH/USDC at tick ≈ −197,350,
//! 18/6 decimals) with a known price gap between them, so whether a cycle pays
//! is decided by arithmetic the test can state, not by a fixture's say-so.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_math::finite_size::{NoSize, SearchBudget, SizedRoute};
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::pricing::{LiveCycle, LivePricer};
use apex_search::engine_c::TemplatePricer;
use apex_types::ids::ChainId;
use apex_types::state::{ReconstructionStatus, StateFingerprint};
use ethers_core::types::U256;
use std::collections::BTreeMap;
use std::sync::Arc;

const BASE: ChainId = ChainId(8453);
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP: Address = address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1");
const ELSEWHERE: Address = address!("00000000000000000000000000000000000000ee");

/// A WETH/USDC pool with constant liquidity `l` over [tick−2000, tick+2000].
fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32, l: u128) -> PoolSnapshot {
    let spacing = 10;
    let lower = (tick - 2_000) / spacing * spacing;
    let upper = (tick + 2_000) / spacing * spacing;
    PoolSnapshot {
        spec: PoolSpec {
            pool: addr,
            venue,
            token0: WETH,
            token1: USDC,
            fee_ppm,
            depth_usd: 5_000_000.0,
        },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: l,
            tick,
            tick_spacing: spacing,
            fee_ppm,
            balance0: Some(U256::from(10u128.pow(21))),
            balance1: Some(U256::from(3_000_000_000_000u128)),
        },
        ladder: TickLadder::new(vec![(lower, l as i128), (upper, -(l as i128))], lower - 5_000, upper + 5_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: Default::default(),
        block: 100,
        last_log: None,
    }
}

const L: u128 = 1_400_000_000_000_000_000;

fn book(pools: Vec<PoolSnapshot>) -> Arc<PoolBook> {
    Arc::new(PoolBook::from_snapshots(pools, ReconstructionStatus::Verified))
}

// ------------------------------------------------------------------ the frontier

/// Two pools on one pair are two cycles, one each way, and every leg chains:
/// start → A → other → B → start.
#[test]
fn two_pools_make_two_cycles_that_chain_back_to_the_start() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
    ]);
    let cycles = frontier::cycles(BASE, WETH, &b.snapshot());
    assert_eq!(cycles.len(), 2);
    for c in &cycles {
        assert_eq!(c.legs[0].token_in, WETH);
        assert_eq!(c.legs[0].token_out, c.legs[1].token_in);
        assert_eq!(c.legs[1].token_out, WETH);
        assert_ne!(c.legs[0].pool, c.legs[1].pool);
        // WETH is token0, so selling it is zero-for-one, and buying it back is not.
        assert!(c.legs[0].zero_for_one && !c.legs[1].zero_for_one);
        assert_eq!(c.commitment.hops.len(), 2);
    }
    assert_ne!(cycles[0].commitment.route_hash, cycles[1].commitment.route_hash, "directions differ");
}

/// A pair with one pool has nothing to arbitrage against, and a pair without the
/// start token is not a native-token cycle.
#[test]
fn a_lone_pool_and_a_non_native_pair_make_no_cycles() {
    let mut other = pool(ELSEWHERE, Venue::UniswapV3, 5, 100, L);
    other.spec.token0 = address!("00000000000000000000000000000000000000a1");
    other.spec.token1 = address!("00000000000000000000000000000000000000b2");
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), other]);
    assert!(frontier::cycles(BASE, WETH, &b.snapshot()).is_empty());
}

#[test]
fn the_frontier_holds_every_cycle_it_prices() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
    ]);
    let (front, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    assert_eq!(front.len(), 2);
    for id in cycles.keys() {
        assert!(front.get(*id).is_some());
    }
}

// ------------------------------------------------------------------ the pricer

fn fp() -> StateFingerprint {
    StateFingerprint {
        chain_id: BASE,
        parent_block_hash: Default::default(),
        confirmed_block_number: 100,
        preconf_sequence: None,
        flashblock_index: None,
        state_root_or_equivalent: None,
        block_hash_if_available: None,
        state_delta_hash: Default::default(),
        venue_state_version: BTreeMap::new(),
        external_dependency_fingerprint: None,
    }
}

fn budget() -> SearchBudget {
    SearchBudget { max_evaluations: 128, min_input: U256::from(10u64.pow(12)) }
}

/// **A gap wider than both fees pays; the pricer finds it.** Fifty ticks is
/// ≈ 50 bps between the pools against 5 + 0.8 bps of fees.
#[test]
fn a_gap_wider_than_the_fees_has_a_profitable_size() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
    ]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = LivePricer::new(b, cycles.clone(), 3_000_000_000_000);
    let wins: Vec<_> = cycles
        .keys()
        .filter_map(|id| pricer.best_size(*id, &fp(), budget()).ok())
        .collect();
    assert_eq!(wins.len(), 1, "exactly one direction buys low and sells high");
    assert!(wins[0].output > wins[0].amount_in, "it returns more than it took");
}

/// The same price in both pools pays nothing once the fees are taken, and the
/// refusal is the economics' — not the pool's.
#[test]
fn equal_prices_have_no_profitable_size() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_350, 80, L),
    ]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = LivePricer::new(b, cycles.clone(), 3_000_000_000_000);
    for id in cycles.keys() {
        match pricer.best_size(*id, &fp(), budget()) {
            Err(NoSize::NoProfitableSize { .. }) | Err(NoSize::RangeEmpty) => {}
            other => panic!("expected no profitable size, got {other:?}"),
        }
    }
}

/// A pool whose price has left its ladder is **refused**, not priced at the last
/// known liquidity — the constant-liquidity error behind the legacy fast path's
/// ~140 bps.
#[test]
fn a_pool_off_its_ladder_is_unpriceable() {
    let mut off = pool(SLIP, Venue::Slipstream, -197_300, 80, L);
    off.state.tick = -150_000; // far outside the proven range
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), off]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = LivePricer::new(b, cycles.clone(), 0);
    for id in cycles.keys() {
        assert_eq!(pricer.best_size(*id, &fp(), budget()).unwrap_err(), NoSize::Unpriceable);
    }
}

/// A size that would run off a ladder is refused as a partial fill, never
/// completed at the last liquidity it knew.
#[test]
fn a_size_past_the_ladder_is_refused_not_completed() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
    ]);
    let snap = b.snapshot();
    let cycle = &frontier::cycles(BASE, WETH, &snap)[0];
    let live = LiveCycle::new(cycle, &snap, 0).expect("both pools on their ladders");
    assert!(live.output(U256::from(10u64.pow(16))).is_some(), "0.01 WETH fills");
    assert!(live.output(U256::exp10(26)).is_none(), "100M WETH runs off every ladder");
}

/// The capacity bound is the paying pool's real holding, and an unread balance
/// fails closed.
#[test]
fn max_input_is_the_real_holding_and_fails_closed() {
    let mut slip = pool(SLIP, Venue::Slipstream, -197_300, 80, L);
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), slip.clone()]);
    let snap = b.snapshot();
    let c = frontier::cycles(BASE, WETH, &snap)
        .into_iter()
        .find(|c| c.legs[1].pool == SLIP)
        .unwrap();
    assert_eq!(LiveCycle::new(&c, &snap, 0).unwrap().max_input(), U256::from(10u128.pow(21)));

    slip.state.balance0 = None;
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), slip]);
    let snap = b.snapshot();
    assert_eq!(LiveCycle::new(&c, &snap, 0).unwrap().max_input(), U256::zero());
}
