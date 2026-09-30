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
/// Uniswap v3's WETH/USDC 0.3% pool: a second pool of the same venue as `UNI`.
const UNI_30: Address = address!("6c561B446416E1A00E8E93E221854d6eA4171372");

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
        seq: 0,
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

/// A proposal's venue versions are its **route's** pools, read from the book: a
/// swap on a pool the route does not touch leaves them unchanged, and a swap on
/// one it does moves them. The event's other fields are kept.
#[test]
fn route_versions_move_only_with_the_routes_own_pools() {
    let mut far = pool(ELSEWHERE, Venue::UniswapV3, -197_340, 100, L);
    far.spec.token1 = address!("00000000000000000000000000000000000000c3");
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
        far,
    ]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = LivePricer::new(b.clone(), cycles.clone(), 0);
    let id = *cycles.keys().next().unwrap();
    let versions = || pricer.route_fingerprint(id, &fp()).venue_state_version;
    let before = versions();
    assert_eq!(before.len(), 2, "one version for each venue on the route: {before:?}");
    assert_eq!(pricer.route_fingerprint(id, &fp()).confirmed_block_number, 100);

    // Another Uniswap pool swaps: same venue, not on this route.
    b.apply_swap(ELSEWHERE, alloy_primitives::U256::from(1u64) << 96, 1, -197_341, (200, 0));
    assert_eq!(versions(), before, "a pool off the route moved the version");

    // A pool on the route swaps.
    b.apply_swap(UNI, alloy_primitives::U256::from(1u64) << 96, 1, -197_351, (201, 3));
    assert_ne!(versions(), before);
}

/// **A write behind another pool's position still moves the version.** The
/// first version was the latest `(block, log index)` among the route's pools,
/// and a maximum of positions stays put while a pool changes underneath it:
/// here a swap the preconfirmed feed missed arrives confirmed, at an earlier
/// position than one it delivered on the route's other pool of the same venue.
#[test]
fn a_write_behind_another_pools_position_still_moves_the_version() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(UNI_30, Venue::UniswapV3, -197_300, 3_000, L),
    ]);
    let both = [UNI, UNI_30];
    let sqrt = alloy_primitives::U256::from(1u64) << 96;

    // Preconfirmed: the 0.3% pool at (201, 7).
    b.apply_swap(UNI_30, sqrt, 1, -197_301, (201, 7));
    let before = b.versions_for(&both);
    // Confirmed only: the 0.05% pool at (201, 3) -- newer for that pool, older
    // than the route's latest position.
    b.apply_swap(UNI, sqrt, 1, -197_351, (201, 3));
    assert_ne!(b.versions_for(&both), before, "the 0.05% pool moved and the version did not");
}

/// The write sequence is the book's own. A snapshot entering with a large `seq`
/// has it replaced — kept, it would put every later write to the route's other
/// pool behind it, and the route's version would stop moving.
#[test]
fn a_callers_seq_is_replaced() {
    let mut garbage = pool(UNI, Venue::UniswapV3, -197_350, 500, L);
    garbage.seq = u64::MAX;
    let b = book(vec![garbage, pool(UNI_30, Venue::UniswapV3, -197_300, 3_000, L)]);
    let before = b.versions_for(&[UNI, UNI_30]);
    b.apply_swap(UNI_30, alloy_primitives::U256::from(1u64) << 96, 1, -197_301, (201, 0));
    assert_ne!(b.versions_for(&[UNI, UNI_30]), before);
}

/// A pool the book no longer carries is not "unchanged": the reading is empty,
/// which last-mile reads as every venue gone (§5.6).
#[test]
fn a_pool_the_book_does_not_carry_empties_the_reading() {
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L)]);
    assert_eq!(b.versions_for(&[UNI]).len(), 1);
    assert!(b.versions_for(&[UNI, ELSEWHERE]).is_empty());
}
