//! Task 8.5 R4 — the live frontier and the local pricer.
//!
//! Pools here are built from real Base shapes (WETH/USDC at tick ≈ −197,350,
//! 18/6 decimals) with a known price gap between them, so whether a cycle pays
//! is decided by arithmetic the test can state, not by a fixture's say-so.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::{quote_exact_input_multi_tick, TickLadder};
use apex_math::finite_size::{NoSize, SearchBudget, SizedRoute, Surplus};
use apex_runtime::econ::{RouteCosts, SettledRoute};
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::gas::{self, HopSteps};
use apex_runtime::live::near_miss::{NearMisses, LADDER_WEI};
use apex_runtime::live::pricing::{CostedCycle, LiveCycle, LivePricer, MAX_TICKS};
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
        dynamic_fee: None,
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

/// A cost every size shares, with gas free: what pricing was before R12.
fn flat(wei: u128) -> RouteCosts {
    RouteCosts { other_wei: wei, wei_per_gas: 0 }
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
    let pricer = LivePricer::new(b, cycles.clone(), flat(3_000_000_000_000), gas::MEASURED);
    let wins: Vec<_> = cycles
        .keys()
        .filter_map(|id| pricer.best_size(*id, &fp(), budget()).ok())
        .collect();
    assert_eq!(wins.len(), 1, "exactly one direction buys low and sells high");
    assert!(wins[0].output > wins[0].amount_in, "it returns more than it took");
}

/// **The pricer sizes against the cost it was last given.** The same gap, once
/// its cost is more than any size grosses, has no profitable size.
#[test]
fn the_pricer_sizes_against_the_cost_it_was_last_given() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
    ]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = LivePricer::new(b, cycles.clone(), flat(0), gas::MEASURED);
    let (id, win) = cycles
        .keys()
        .find_map(|id| pricer.best_size(*id, &fp(), budget()).ok().map(|w| (*id, w)))
        .expect("the gap pays");

    pricer.set_costs(flat((win.output - win.amount_in).as_u128() * 1_000));
    assert_eq!(pricer.costs(), flat((win.output - win.amount_in).as_u128() * 1_000));
    let got = pricer.best_size(id, &fp(), budget());
    assert!(matches!(got, Err(NoSize::NoProfitableSize { .. })), "{got:?}");
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
    let pricer = LivePricer::new(b, cycles.clone(), flat(3_000_000_000_000), gas::MEASURED);
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
    let pricer = LivePricer::new(b, cycles.clone(), flat(0), gas::MEASURED);
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
    let live = LiveCycle::new(cycle, &snap).expect("both pools on their ladders");
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
    assert_eq!(LiveCycle::new(&c, &snap).unwrap().max_input(), U256::from(10u128.pow(21)));

    slip.state.balance0 = None;
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), slip]);
    let snap = b.snapshot();
    assert_eq!(LiveCycle::new(&c, &snap).unwrap().max_input(), U256::zero());
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
    let pricer = LivePricer::new(b.clone(), cycles.clone(), flat(0), gas::MEASURED);
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

// ------------------------------------------------------------------ gas (R12)

/// The pool with a zero-net initialized tick every `every` ticks across its
/// band: each is a step v3-core takes and a tick it writes, so a larger trade
/// crosses more of them.
fn ticked(mut p: PoolSnapshot, every: i32) -> PoolSnapshot {
    let ticks = p.ladder.ticks().to_vec();
    let (lower, upper) = (ticks[0].0, ticks[ticks.len() - 1].0);
    let mut all = ticks.clone();
    let mut t = lower + every;
    while t < upper {
        all.push((t, 0));
        t += every;
    }
    p.ladder = TickLadder::new(all, p.ladder.lower_bound(), p.ladder.upper_bound());
    p
}

/// **A larger trade crosses more ticks, and is charged their gas.** The quote
/// that prices a size counts the ticks each hop crosses, and the cost a size is
/// sized against is the route's other costs plus that size's expected gas at the
/// gas price — never one figure every size shares.
#[test]
fn a_size_that_crosses_more_ticks_is_charged_its_gas() {
    let b = book(vec![
        ticked(pool(UNI, Venue::UniswapV3, -197_350, 500, L), 10),
        ticked(pool(SLIP, Venue::Slipstream, -197_300, 80, L), 10),
    ]);
    let snap = b.snapshot();
    let cycle = &frontier::cycles(BASE, WETH, &snap)[0];
    let costs = RouteCosts { other_wei: 7_000_000_000, wei_per_gas: 5_000_000 };
    let route = CostedCycle::new(LiveCycle::new(cycle, &snap).unwrap(), costs, gas::MEASURED);
    let quote = |x: U256| LiveCycle::new(cycle, &snap).unwrap().quote(x).unwrap();
    let crossed = |hops: [HopSteps; 2]| hops[0].crossed + hops[1].crossed;

    // 0.001 WETH crosses at most the tick hop 1's price sits on; 100 WETH
    // crosses several more.
    let (small, large) = (U256::exp10(15), U256::exp10(20));
    assert!(crossed(quote(small).hops) <= 1, "{:?}", quote(small).hops);
    assert!(crossed(quote(large).hops) >= crossed(quote(small).hops) + 4, "{:?}", quote(large).hops);

    // Each hop's steps are its own pool's, in its own direction: hop 1 sells
    // WETH down its pool, hop 2 buys it back up the other.
    let mut amount = large;
    for (i, leg) in cycle.legs.iter().enumerate() {
        let p = snap.get(&leg.pool).unwrap();
        let direct = quote_exact_input_multi_tick(&p.state, &p.ladder, amount, leg.zero_for_one, MAX_TICKS).unwrap();
        let want = HopSteps {
            venue: leg.venue,
            zero_for_one: leg.zero_for_one,
            crossed: direct.ticks_crossed,
            word_steps: direct.word_steps,
        };
        assert_eq!(quote(large).hops[i], want, "hop {i}");
        amount = direct.amount_out;
    }
    assert_ne!(cycle.legs[0].zero_for_one, cycle.legs[1].zero_for_one);

    let (at_small, at_large) = (route.gas_at(small).unwrap(), route.gas_at(large).unwrap());
    assert_eq!(at_small, gas::MEASURED.estimate(&quote(small).hops));
    assert_eq!(at_large, gas::MEASURED.estimate(&quote(large).hops));
    assert!(at_large.expected > at_small.expected && at_large.ceiling > at_small.ceiling);
    for x in [small, large] {
        let priced = route.priced(x).unwrap();
        assert_eq!(priced.output, quote(x).outputs[1]);
        assert_eq!(priced.cost, U256::from(costs.with_gas(route.gas_at(x).unwrap().expected)));
    }
    // The least any size costs: a settlement that crosses nothing.
    let nothing = quote(small).hops.map(|h| HopSteps { crossed: 0, word_steps: 0, ..h });
    assert_eq!(route.fixed_cost(), U256::from(costs.with_gas(gas::MEASURED.estimate(&nothing).expected)));
    assert!(route.fixed_cost() < route.priced(large).unwrap().cost);
}

/// **Engine C sizes against each size's own cost.** The net the pricer reports
/// is the size's output, less what it took, the other costs, and the gas that
/// size's crossings use — the figure the economics will charge.
#[test]
fn the_pricer_charges_each_size_its_own_gas() {
    let b = book(vec![
        ticked(pool(UNI, Venue::UniswapV3, -197_350, 500, L), 10),
        ticked(pool(SLIP, Venue::Slipstream, -197_300, 80, L), 10),
    ]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let costs = RouteCosts { other_wei: 1_000_000_000_000, wei_per_gas: 5_000_000 };
    let pricer = LivePricer::new(b.clone(), cycles.clone(), costs, gas::MEASURED);
    let (id, win) = cycles
        .keys()
        .find_map(|id| pricer.best_size(*id, &fp(), budget()).ok().map(|w| (*id, w)))
        .expect("the gap pays");

    let cycle = LiveCycle::new(pricer.cycle(id).unwrap(), &b.snapshot()).unwrap();
    let hops = cycle.quote(win.amount_in).unwrap().hops;
    assert!(hops[0].crossed + hops[1].crossed > 0, "the size found crosses nothing: the test proves nothing");
    let route = CostedCycle::new(cycle, costs, gas::MEASURED);
    let gas = route.gas_at(win.amount_in).unwrap();
    assert_eq!(
        win.net,
        Surplus::Gain(win.output - win.amount_in - U256::from(costs.with_gas(gas.expected))),
        "the reported net is not the size's own"
    );
}

// ------------------------------------------------------------------ near misses (R14)

/// **Near misses are counted by how far they were from paying**, in disjoint
/// bands of basis points, with the closest kept.
#[test]
fn near_misses_are_counted_by_how_far_they_were_from_paying() {
    let m = NearMisses::default();
    assert_eq!(m.report().best_bps, None, "nothing measured, nothing closest");
    // Hundredths of a basis point, on and inside each band's edges — and a
    // different count in each band, so no two bands can trade places unseen.
    for c in [
        0, -1, -49, -50, -75, -99, -100, -120, -150, -199, -200, -250, -300, -400, -499, -500, -600, -700, -800,
        -900, -999, -1_000, -1_001, -2_000, -3_000, -4_000, -5_000, -10_000,
    ] {
        m.record(c);
    }
    let r = m.report();
    assert_eq!(r.measured, 28);
    assert_eq!(
        (r.pays, r.within_0_5, r.within_1, r.within_2, r.within_5, r.within_10, r.beyond_10),
        (1, 2, 3, 4, 5, 6, 7)
    );
    assert_eq!(r.best_bps, Some(0.0));
    m.record(120);
    let r = m.report();
    assert_eq!(r.best_bps, Some(1.2));

    // The names `scripts/shadow-status.sh` reads from the report.
    let json = serde_json::to_value(&r).unwrap();
    let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["best_bps", "beyond_10", "measured", "pays", "within_0_5", "within_1", "within_10", "within_2", "within_5"]
    );
}

/// **A route's near miss is its best net over the ladder**: each size at its
/// own cost, as a share of the size — the census's measure. A 50-tick gap
/// (50.1 bps) against 5.8 bps of fees nets about 44 bps the paying way, and
/// about -56 the other.
#[test]
fn a_routes_near_miss_is_its_best_net_over_the_ladder() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
    ]);
    let snap = b.snapshot();
    let costs = RouteCosts { other_wei: 100_000_000_000, wei_per_gas: 5_000_000 };
    let mut seen = Vec::new();
    for cycle in frontier::cycles(BASE, WETH, &snap) {
        let route = CostedCycle::new(LiveCycle::new(&cycle, &snap).unwrap(), costs, gas::MEASURED);
        let at = |x: u128| {
            let p = route.priced(U256::from(x)).unwrap();
            (p.output.as_u128() as i128 - x as i128 - p.cost.as_u128() as i128) * 1_000_000 / x as i128
        };
        let want = LADDER_WEI.iter().map(|x| at(*x)).max().unwrap();
        let got = route.near_miss_centi_bps().expect("every ladder size prices");
        assert_eq!(i128::from(got), want);
        seen.push(got);
    }
    seen.sort_unstable();
    assert!((-5_700..-5_500).contains(&seen[0]), "the losing way: {seen:?}");
    assert!((4_300..4_500).contains(&seen[1]), "the paying way: {seen:?}");
}

/// **The pricer records a near miss for every route it prices**, and none for
/// one it cannot: a pool off its ladder prices nothing, so there is nothing to
/// be near.
#[test]
fn the_pricer_records_a_near_miss_for_every_route_it_prices() {
    let b = book(vec![
        pool(UNI, Venue::UniswapV3, -197_350, 500, L),
        pool(SLIP, Venue::Slipstream, -197_300, 80, L),
    ]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = LivePricer::new(b, cycles.clone(), flat(3_000_000_000_000), gas::MEASURED);
    for id in cycles.keys() {
        let _ = pricer.best_size(*id, &fp(), budget());
    }
    let r = pricer.near_misses().report();
    assert_eq!((r.measured, r.pays, r.beyond_10), (2, 1, 1), "{r:?}");

    let mut off = pool(SLIP, Venue::Slipstream, -197_300, 80, L);
    off.state.tick = -150_000;
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), off]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = LivePricer::new(b, cycles.clone(), flat(0), gas::MEASURED);
    for id in cycles.keys() {
        assert_eq!(pricer.best_size(*id, &fp(), budget()).unwrap_err(), NoSize::Unpriceable);
    }
    assert_eq!(pricer.near_misses().report().measured, 0);
}

/// **A near miss is never at a size the paying pool cannot pay out.** Each
/// pool here holds 0.02 WETH, so of the ladder only 0.01 WETH can execute, and
/// the near miss is that size's net — not a larger size's, which would describe
/// a trade nothing could make.
#[test]
fn a_near_miss_is_never_at_a_size_the_pool_cannot_pay() {
    let thin = |mut p: PoolSnapshot| {
        p.state.balance0 = Some(U256::from(20_000_000_000_000_000u128));
        p
    };
    let b = book(vec![
        thin(pool(UNI, Venue::UniswapV3, -197_350, 500, L)),
        thin(pool(SLIP, Venue::Slipstream, -197_300, 80, L)),
    ]);
    let snap = b.snapshot();
    let costs = RouteCosts { other_wei: 100_000_000_000, wei_per_gas: 5_000_000 };
    for cycle in frontier::cycles(BASE, WETH, &snap) {
        let route = CostedCycle::new(LiveCycle::new(&cycle, &snap).unwrap(), costs, gas::MEASURED);
        let x = 10_000_000_000_000_000u128;
        let p = route.priced(U256::from(x)).unwrap();
        let only = (p.output.as_u128() as i128 - x as i128 - p.cost.as_u128() as i128) * 1_000_000 / x as i128;
        assert_eq!(route.near_miss_centi_bps().map(i128::from), Some(only));
    }
}

/// **A pool's depth must not hide a size that pays.** The same 50-tick gap,
/// the paying pool holding 1,000 WETH or a million: a million puts the search's
/// first probes far past the pools' ladders, where the route cannot be priced.
/// A size that pays is still a size that pays.
#[test]
fn a_deeper_paying_pool_does_not_hide_a_size_that_pays() {
    for held in [10u128.pow(21), 10u128.pow(24)] {
        let deep = |mut p: PoolSnapshot| {
            p.state.balance0 = Some(U256::from(held));
            p
        };
        let b = book(vec![
            deep(pool(UNI, Venue::UniswapV3, -197_350, 500, L)),
            deep(pool(SLIP, Venue::Slipstream, -197_300, 80, L)),
        ]);
        let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
        let pricer = LivePricer::new(b, cycles.clone(), flat(3_000_000_000_000), gas::MEASURED);
        let wins = cycles.keys().filter_map(|id| pricer.best_size(*id, &fp(), budget()).ok()).count();
        assert_eq!(wins, 1, "holding {held} wei: {:?}", pricer.near_misses().report());
    }
}

/// **And the economics sizes it too.** Engine C's search and the economics'
/// two stages are separate searches over the same curve: with the paying pool a
/// million deep, the economics — golden section, then the climb — refines the
/// same route to a profitable size, through the real pricer.
#[tokio::test]
async fn the_economics_sizes_a_route_through_a_deep_pool() {
    use apex_econ::cost::failure::FailureProfile;
    use apex_econ::cost::l1_data::{L1FeeModel, L1FeeParameters};
    use apex_runtime::econ::{ChainCosts, LiveEconomics, ScenarioPriors};
    use apex_runtime::plane::Economics;
    use apex_search::frontier::{ProposalOrigin, RouteProposal};

    let deep = |mut p: PoolSnapshot| {
        p.state.balance0 = Some(U256::from(10u128.pow(24)));
        p
    };
    let b = book(vec![
        deep(pool(UNI, Venue::UniswapV3, -197_350, 500, L)),
        deep(pool(SLIP, Venue::Slipstream, -197_300, 80, L)),
    ]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let pricer = Arc::new(LivePricer::new(b, cycles.clone(), flat(3_000_000_000_000), gas::MEASURED));
    let costs = ChainCosts {
        gas_price_wei: alloy_primitives::U256::from(5_000_000u64),
        l1: L1FeeParameters {
            l1_base_fee: U256::from(82_228_355u64),
            l1_blob_base_fee: U256::from(4_300_268u64),
            base_fee_scalar: 2_269,
            blob_base_fee_scalar: 1_055_762,
        },
        l1_model: L1FeeModel::unvalidated(),
        failure: FailureProfile { gas_on_failure: apex_types::cost::GasUsed(411_945), failure_ppm: 50_000 },
    };
    let econ = LiveEconomics::new(pricer.clone(), ScenarioPriors::default(), costs, apex_types::ids::StrategyId(1));

    let (id, _) = cycles
        .keys()
        .find_map(|id| pricer.best_size(*id, &fp(), budget()).ok().map(|w| (*id, w)))
        .expect("Engine C finds the paying direction");
    let proposal = RouteProposal {
        chain: BASE,
        route: pricer.commitment(id).expect("a commitment"),
        venue_set: Vec::new(),
        state_fingerprint: fp(),
        found_at: apex_types::time::UnixNanos(0),
        origin: ProposalOrigin::FiniteSize,
        flash_source: None,
        size_hint: None,
        net_hint: None,
    };
    let size = econ.size(&proposal).await.expect("the economics sizes it");
    assert!(size.get() > alloy_primitives::U256::ZERO);
    assert!(size.get() <= alloy_primitives::U256::from(10u128.pow(24)));
}

/// R21: a route sizes within what the lender holds. The gap of
/// `a_gap_wider_than_the_fees_has_a_profitable_size`, capped at half the size
/// it finds unbounded.
#[test]
fn a_route_sizes_within_the_lenders_holding() {
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), pool(SLIP, Venue::Slipstream, -197_300, 80, L)]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let free = LivePricer::new(b.clone(), cycles.clone(), flat(3_000_000_000_000), gas::MEASURED);
    let (id, unbounded) = cycles
        .keys()
        .find_map(|id| free.best_size(*id, &fp(), budget()).ok().map(|s| (*id, s.amount_in)))
        .expect("one direction pays");
    let cap = unbounded / 2;
    let (_tx, rx) = tokio::sync::watch::channel(Some(cap));
    let capped = LivePricer::new(b, cycles, flat(3_000_000_000_000), gas::MEASURED).with_lender(rx);
    let sized = capped.best_size(id, &fp(), budget()).expect("still pays at half the size");
    assert!(sized.amount_in <= cap, "{} > {}", sized.amount_in, cap);
}

/// R21: an unread lender holding sizes nothing, as an unread pool balance does.
#[test]
fn an_unread_lender_sizes_nothing() {
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), pool(SLIP, Venue::Slipstream, -197_300, 80, L)]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let (_tx, rx) = tokio::sync::watch::channel(None);
    let pricer = LivePricer::new(b, cycles.clone(), flat(3_000_000_000_000), gas::MEASURED).with_lender(rx);
    assert!(cycles.keys().all(|id| pricer.best_size(*id, &fp(), budget()).is_err()));
}
