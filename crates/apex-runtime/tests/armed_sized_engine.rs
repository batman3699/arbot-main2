//! R40.1: the sized engine. Its fill chooses by marginal expected net under
//! the senders and the budget; on a book, it arms `LeanExecutor3` and renews
//! what has not changed.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::lines::ArmedLine;
use apex_runtime::armed::sized::{fill, fill_spike, leads, spike_rank, Candidate, Reserve, SizedEngine, SizedSettings, LAGGARD_RANK};
use apex_runtime::armed::threshold::{Pricing, SpikeTier, SPIKE_TIER_BASE};
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::book::{Move, PoolSnapshot};
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::gas::{LEAN, SIZED};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_search::frontier::RouteId;
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

fn route(n: u8) -> RouteId {
    RouteId(u64::from(n))
}

fn cand(n: u8, group: u8, ev: f64, worst_wei: u128) -> Candidate {
    Candidate { key: (route(n), 0, 1), group: (Address::repeat_byte(group), true), other: Address::repeat_byte(100 + n), ev, worst_wei, rank: 0.0 }
}

/// A spike candidate of tier `SPIKE_TIER_BASE + k`, across from a pool of
/// its own route's.
fn spike(n: u8, group: u8, k: u8, rank: f64, worst_wei: u128) -> Candidate {
    Candidate { key: (route(n), 0, SPIKE_TIER_BASE + k), group: (Address::repeat_byte(group), true), other: Address::repeat_byte(100 + n), ev: 1.0, worst_wei, rank }
}

/// R48: one spike arm for each trigger, direction and pool across from it.
/// A move on a deep pool opens a gap to each shallower partner, and arbing one
/// moves the partner, not the trigger: on 2026-11-14 at 02:28:34 UTC one swap
/// on Uniswap's WETH/USDC pool (index 2 of block 57,164,692) was followed by
/// $2,913, $2,510 and $793 of arbitrage against three other pools (indices 12,
/// 15 and 29). A second arm across from the same pool is the same gap.
#[test]
fn spike_senders_go_to_the_best_ranked_pairs_one_a_trigger_and_partner() {
    // On trigger 1: routes 1 and 2 across from pools of their own, ranked 8
    // and 9, and route 5 across from route 2's pool, ranked 7. One on trigger
    // 2, ranked 5; one unranked.
    let mut same_gap = spike(5, 1, 0, 7.0, 1);
    same_gap.other = Address::repeat_byte(102);
    let cands = vec![spike(1, 1, 0, 8.0, 1), spike(2, 1, 0, 9.0, 1), spike(3, 2, 0, 5.0, 1), spike(4, 3, 0, 0.0, 1), same_gap];
    assert_eq!(fill_spike(&cands, &[2], 1_000), vec![1, 0], "one move gaps both partners: both arm");
    assert_eq!(fill_spike(&cands, &[8], 1_000), vec![1, 0, 2, 3], "the same trigger, direction and partner is passed over; an unranked pair comes last");
}

#[test]
fn each_spike_tier_fills_its_own_senders() {
    let cands = vec![spike(1, 1, 0, 9.0, 1), spike(2, 1, 1, 9.0, 1), spike(3, 2, 1, 5.0, 1), cand(4, 4, 99.0, 1)];
    assert_eq!(fill_spike(&cands, &[1, 1], 1_000), vec![0, 1], "one a tier; the same trigger may hold one of each");
    assert_eq!(fill_spike(&cands, &[0, 2], 1_000), vec![1, 2]);
    assert!(fill_spike(&cands, &[], 1_000).is_empty(), "no tiers, no spike arms; an ordinary arm is never one");
}

#[test]
fn a_spike_arm_is_held_to_the_budget_and_to_a_positive_value() {
    let mut worthless = spike(3, 3, 0, 9.5, 1);
    worthless.ev = 0.0;
    let cands = vec![spike(1, 1, 0, 9.0, 600), spike(2, 2, 0, 8.0, 600), worthless, spike(4, 4, 0, 1.0, 400)];
    assert_eq!(fill_spike(&cands, &[4], 1_000), vec![0, 3], "the second's reservation does not fit; the fourth's does");
}

/// R43.2: nor is a spike arm valued there. Re-valuing one replays trades on a
/// trigger moved hundreds of ticks; done for every spike candidate of every
/// group chosen from, it made a pass with nothing to price four times longer
/// (4.4 ms to 17 ms on twelve slots).
#[test]
fn the_ordinary_fill_never_values_a_spike_arm() {
    let cands = vec![cand(1, 1, 5.0, 1), spike(2, 1, 0, 9.0, 1), cand(3, 1, 4.0, 1), spike(4, 1, 1, 9.0, 1)];
    let mut asked = Vec::new();
    let chosen = fill(&cands, 4, 1_000, |c, _| {
        asked.push(c.key.2);
        c.ev
    });
    assert_eq!(chosen, vec![0, 2]);
    assert_eq!(asked, vec![1], "the other ordinary arm of the group, and no spike arm");
}

#[test]
fn the_ordinary_fill_never_takes_a_spike_arm() {
    let cands = vec![spike(1, 1, 0, 9.0, 1), cand(2, 2, 3.0, 1)];
    assert_eq!(fill(&cands, 4, 1_000, |c, _| c.ev), vec![1]);
}

#[test]
fn the_best_value_goes_first_whatever_its_group() {
    let cands = vec![cand(1, 1, 5.0, 1), cand(2, 2, 9.0, 1), cand(3, 3, 1.0, 1)];
    assert_eq!(fill(&cands, 2, 1_000, |c, _| c.ev), vec![1, 0]);
}

#[test]
fn a_low_value_group_does_not_outrank_a_better_second_arm() {
    // Group 1 has two arms worth 10 and, after the first, 6; group 2 one worth 2.
    let cands = vec![cand(1, 1, 10.0, 1), cand(2, 1, 8.0, 1), cand(3, 2, 2.0, 1)];
    let chosen = fill(&cands, 2, 1_000, |c, before| if before.is_empty() { c.ev } else { 6.0 });
    assert_eq!(chosen, vec![0, 1], "a group-first pass would have spent a sender on the 2");
}

#[test]
fn a_second_arm_worth_nothing_after_the_first_leaves_the_sender_free() {
    let cands = vec![cand(1, 1, 10.0, 1), cand(2, 1, 8.0, 1), cand(3, 2, 2.0, 1)];
    let chosen = fill(&cands, 2, 1_000, |c, before| if before.is_empty() { c.ev } else { 0.0 });
    assert_eq!(chosen, vec![0, 2], "the closed gap holds no sender; the next group gets it");
}

#[test]
fn it_stops_at_no_positive_value_and_at_the_budget() {
    let cands = vec![cand(1, 1, 3.0, 400), cand(2, 2, 2.0, 400), cand(3, 3, 1.0, 400), cand(4, 4, -1.0, 1)];
    assert_eq!(fill(&cands, 8, 1_000, |c, _| c.ev), vec![0, 1], "the third's worst case does not fit");
    assert_eq!(fill(&cands, 8, 10_000, |c, _| c.ev), vec![0, 1, 2], "and nothing of no value is armed");
}

#[test]
fn the_budget_term_prefers_the_cheaper_reservation_at_equal_value() {
    let cands = vec![cand(1, 1, 5.0, 900), cand(2, 2, 5.0, 10)];
    assert_eq!(fill(&cands, 1, 1_000, |c, _| c.ev), vec![1]);
}

#[test]
fn sixty_four_tier_one_arms_reserve_under_a_tenth_of_the_budget() {
    // The spec's table: a tier-1 arm at 0.01 gwei, base fee 0.0003 gwei, the
    // fork-measured limit, and the L1 bound of 1e12.
    let worst = u128::from(SIZED.limit) * (2 * 300_000 + 10_000_000) + 1_000_000_000_000;
    assert!(64 * worst < 1_000_000_000_000_000, "{} wei", 64 * worst);
}

// ------------------------------------------------------------- on a book

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const CAKE: Address = address!("72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38");
const EXECUTOR3: Address = address!("00000000000000000000000000000000000e3e03");
const L: u128 = 1_400_000_000_000_000_000_000;

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32) -> PoolSnapshot {
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
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
        seq: 1,
        reserves: None,
    }
}

fn px() -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: LEAN,
        lenders: Some(apex_runtime::live::lenders::Lenders { balancer: U256::from(10u128.pow(21)), morpho: None }),
        margin_wei: U256::zero(),
        start_per_eth: None,
        rate: None,
        usdc_lenders: None,
        pool_manager: None,
    }
}

fn engine(max_senders: usize) -> (SizedEngine, BTreeMap<Address, Arc<PoolSnapshot>>) {
    engine_with(max_senders, Vec::new(), Reserve::WorstCase)
}

fn sized_settings(max_senders: usize, tiers: Vec<SpikeTier>, reserve: Reserve) -> SizedSettings {
    SizedSettings {
        max_senders,
        window_s: 10,
        block_time_ms: 200,
        band_cap_bps: 20,
        liquidity_floor_bps: 9_800,
        tier2_multiple: 10,
        executor3: EXECUTOR3,
        chain_id: 8453,
        max_size_wei: 5 * 10u128.pow(18),
        max_size_usdc: 12_000_000_000,
        max_priority_wei_per_gas: 20_000_000_000,
        min_net_wei: 1,
        max_steps: 8,
        p_no_trade_ppm: 20_000,
        p_revert_ppm: 0,
        reprice_for: Duration::MAX,
        workers: 1,
        l1_fee_bound_wei: 1_000_000_000_000,
        tiers,
        reserve,
        ordinary_budget_wei: u128::MAX,
    }
}

fn engine_with(max_senders: usize, tiers: Vec<SpikeTier>, reserve: Reserve) -> (SizedEngine, BTreeMap<Address, Arc<PoolSnapshot>>) {
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> =
        [(UNI, pool(UNI, Venue::UniswapV3, -197_350, 500)), (CAKE, pool(CAKE, Venue::PancakeV3, -197_350, 100))]
            .into_iter()
            .map(|(a, p)| (a, Arc::new(p)))
            .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = sized_settings(max_senders, tiers, reserve);
    (SizedEngine::new(cycles, settings, BTreeSet::new()), snap)
}

const GWEI: u128 = 1_000_000_000;

/// A tier at 0.01 ETH, tipping a quarter, with 50 WETH to size to.
fn tier(senders: usize) -> SpikeTier {
    SpikeTier {
        least_net_wei: 10u128.pow(16),
        bid_share_bps: 2_500,
        cap_wei_per_gas: 200 * GWEI,
        senders,
        max_size_wei: 50 * 10u128.pow(18),
        max_size_usdc: 120_000_000_000,
    }
}

const BUDGET: u128 = 10u128.pow(18);

/// R43: with a tier configured, its arms take its own senders, the last of
/// the lane's, and carry its size cap and its share bid; ordinary arms keep
/// to the rest.
#[test]
fn spike_arms_take_their_reserved_senders_and_their_own_terms() {
    let (e, snap) = engine_with(6, vec![tier(2)], Reserve::WorstCase);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    let armed = arms(&lines);
    let (spikes, ordinary): (Vec<_>, Vec<_>) = armed.iter().partition(|a| a.rung >= SPIKE_TIER_BASE);
    assert_eq!(spikes.len(), 2, "two reserved senders");
    assert_eq!(ordinary.len(), 4, "tier 1 on each slot");
    let slots = |v: &[&&apex_runtime::armed::lines::Arm]| v.iter().map(|a| a.sender_slot).collect::<BTreeSet<u16>>();
    assert_eq!(slots(&spikes), BTreeSet::from([4, 5]));
    assert_eq!(slots(&ordinary), BTreeSet::from([0, 1, 2, 3]));
    for a in &spikes {
        let sized = a.sized.as_ref().unwrap();
        assert_eq!(sized.tier, SPIKE_TIER_BASE);
        assert_eq!(sized.cap, (50 * 10u128.pow(18)).to_string(), "the tier's size cap is the plan's");
        let bid: u128 = a.bid_wei_per_gas.parse().unwrap();
        assert!(bid > 0 && bid <= 200 * GWEI, "{bid}");
        assert_eq!(a.bid_model.as_deref(), Some("tier_share"));
        let ordinary_step = ordinary.iter().find(|o| o.route_hash == a.route_hash && o.trigger_leg == a.trigger_leg).map(|o| o.step).unwrap();
        assert!(a.step > ordinary_step, "a spike arm waits for a larger move than tier 1's");
    }
}

/// R43: an ordinary arm never takes a spike tier's sender, however many it
/// could fill.
#[test]
fn ordinary_arms_never_take_a_spike_tiers_sender() {
    let (e, snap) = engine_with(4, vec![tier(2)], Reserve::WorstCase);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    let armed = arms(&lines);
    let ordinary: Vec<_> = armed.iter().filter(|a| a.rung < SPIKE_TIER_BASE).collect();
    assert_eq!(ordinary.len(), 2, "four slots could pay, two senders are theirs");
    assert!(ordinary.iter().all(|a| a.sender_slot < 2));
    assert_eq!(armed.len(), 4);
}

/// R43: the spike senders go to the pairs the census values most.
#[test]
fn spike_senders_follow_the_census_rank() {
    let (e, snap) = engine_with(6, vec![tier(2)], Reserve::WorstCase);
    let e = e.with_rank(Arc::new(|c| if c.legs[0].pool == CAKE { 7.0 } else { 0.0 }));
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    let spikes: Vec<_> = arms(&lines).into_iter().filter(|a| a.rung >= SPIKE_TIER_BASE).collect();
    assert_eq!(spikes.len(), 2);
    assert!(spikes.iter().all(|a| a.pools[0] == format!("{CAKE:#x}")), "both legs of the route that starts in the ranked pool");
}

/// R48: a spike arm waits on the deeper pool of its pair. Its predicates hold
/// the other pool where it was, so it fires only when its own trigger moves
/// first, and in 2026-11-13..18's arbitrage of $50 or more on the census
/// routes (where both pools' depth was known) the deeper pool moved first in
/// 89% of the value. Here the ranked route's two legs tie on rank, and the
/// one sender goes to the leg whose trigger holds four times the liquidity,
/// whichever pool that is.
#[test]
fn a_spike_sender_waits_on_the_deeper_pool() {
    for (deep, shallow) in [(UNI, CAKE), (CAKE, UNI)] {
        let (e, mut snap) = engine_with(5, vec![tier(1)], Reserve::WorstCase);
        let mut p = (*snap[&deep]).clone();
        p.state.liquidity = 4 * L;
        snap.insert(deep, Arc::new(p));
        let e = e.with_rank(Arc::new(|c| if c.legs[0].pool == CAKE { 7.0 } else { 0.0 }));
        let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
        let spikes: Vec<_> = arms(&lines).into_iter().filter(|a| a.rung >= SPIKE_TIER_BASE).collect();
        assert_eq!(spikes.len(), 1);
        let a = spikes[0];
        assert_eq!(a.pools[0], format!("{CAKE:#x}"), "the ranked route");
        let trigger = &a.pools[usize::from(a.trigger_leg)];
        assert_eq!(trigger, &format!("{deep:#x}"), "the trigger is the deeper pool, not {shallow:#x}");
    }
}

/// R49: a spike leg's rank is its route's value split by where the census saw
/// the route's trades start, with R48's depth rule counting for
/// `TRIGGER_PRIOR_USD` of them, so one small trade does not flip a leg.
#[test]
fn a_spike_legs_rank_follows_the_census_trigger_split() {
    assert_eq!(spike_rank(1_000.0, None, 0, true), 1_000.0, "without a split, R48's");
    assert_eq!(spike_rank(1_000.0, None, 1, false), 1_000.0 * LAGGARD_RANK);
    assert_eq!(spike_rank(1_000.0, Some([0.0, 0.0]), 1, false), 1_000.0 * LAGGARD_RANK, "an empty split says nothing");
    // Route #2 on 2026-11-20: its $2,169 trade began on the shallower pool.
    let deep = spike_rank(2_224.0, Some([0.0, 2_169.0]), 0, true);
    let shallow = spike_rank(2_224.0, Some([0.0, 2_169.0]), 1, false);
    assert!(shallow > 10.0 * deep, "{shallow} against {deep}");
    assert!(shallow <= 2_224.0, "never more than the route's value");
    let leader = spike_rank(1_000.0, Some([20.0, 0.0]), 1, true);
    let one_trade = spike_rank(1_000.0, Some([20.0, 0.0]), 0, false);
    assert!(leader > one_trade, "$20 of trades does not outweigh depth: {leader} against {one_trade}");
}

/// R49: where the census saw a route's trades start outranks depth: the one
/// sender waits on Pancake's pool although Uniswap's holds four times its
/// liquidity.
#[test]
fn a_spike_sender_waits_where_the_census_saw_the_trades_start() {
    let (e, mut snap) = engine_with(5, vec![tier(1)], Reserve::WorstCase);
    let mut p = (*snap[&UNI]).clone();
    p.state.liquidity = 4 * L;
    snap.insert(UNI, Arc::new(p));
    let e = e
        .with_rank(Arc::new(|c| if c.legs[0].pool == CAKE { 7.0 } else { 0.0 }))
        .with_trigger_usd(Arc::new(|c| (c.legs[0].pool == CAKE).then_some([500.0, 0.0])));
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    let spikes: Vec<_> = arms(&lines).into_iter().filter(|a| a.rung >= SPIKE_TIER_BASE).collect();
    assert_eq!(spikes.len(), 1);
    let a = spikes[0];
    assert_eq!(a.pools[0], format!("{CAKE:#x}"), "the ranked route");
    assert_eq!(a.pools[usize::from(a.trigger_leg)], format!("{CAKE:#x}"), "its trades began on Pancake's pool");
}

/// R48: the depth a spike arm's leg is ranked by: in-range liquidity, or for
/// a constant-product pool the same quantity, the square root of its reserves'
/// product. Both pools of a pair trade the same two tokens, so the two compare.
#[test]
fn a_pools_depth_is_its_liquidity_at_the_price() {
    let cl = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let mut shallow = cl.clone();
    shallow.state.liquidity = L / 2;
    assert!(leads(&cl, &shallow) && !leads(&shallow, &cl));
    assert!(leads(&cl, &cl), "a tie leads both ways");
    let mut cp = pool(CAKE, Venue::AerodromeV2, 0, 3_000);
    cp.state.liquidity = 0;
    // sqrt(1e30 * 4e18) = 2e24, over L = 1.4e24.
    cp.reserves = Some(apex_runtime::live::cp::Reserves { reserve0: U256::from(10u128.pow(30)), reserve1: U256::from(4 * 10u128.pow(18)) });
    assert!(leads(&cp, &cl) && !leads(&cl, &cp));
}

/// R43: what a pending sized arm holds of the day's budget. Here a spike arm
/// bids 16.7 gwei: its gas limit at that is 0.0167 ETH, over a budget of
/// 0.006; what an inclusion that does not trade costs on its route (R44:
/// 81,700 gas through these two venues) is 0.0014, and two fit.
#[test]
fn a_no_trade_reservation_lets_a_spike_arm_into_a_small_budget() {
    let small = 6_000_000_000_000_000;
    let (e, snap) = engine_with(6, vec![tier(2)], Reserve::WorstCase);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, small, 1_500_000_000);
    assert!(arms(&lines).iter().all(|a| a.rung < SPIKE_TIER_BASE), "a million gas at the tier's bid is over the budget");
    let (e, snap) = engine_with(6, vec![tier(2)], Reserve::NoTrade);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, small, 1_500_000_000);
    assert_eq!(arms(&lines).iter().filter(|a| a.rung >= SPIKE_TIER_BASE).count(), 2);
}

fn arms(lines: &[ArmedLine]) -> Vec<&apex_runtime::armed::lines::Arm> {
    lines.iter().filter_map(|l| if let ArmedLine::Arm(a) = l { Some(a.as_ref()) } else { None }).collect()
}

/// R51: a hookless v4 WETH/USDC pool keyed at 500 pips; `fee_ppm` is the
/// book's composed fee, above the key's while a protocol fee is on.
fn v4_pool(fee_ppm: u32) -> PoolSnapshot {
    use apex_runtime::live::v4::{key_address, V4Key};
    let id = alloy_primitives::B256::repeat_byte(0x22);
    let mut p = pool(key_address(id), Venue::UniswapV4, -197_350, 500);
    p.spec.v4 = Some(V4Key { pool_id: id, fee: 500, tick_spacing: 10, hooks: Address::ZERO });
    p.state.fee_ppm = fee_ppm;
    p
}

/// R51: PoolManager adds a v4 swap's protocol fee to its input currency's
/// accrued slot, so a sized arm through a pool with one on carries that write
/// in its swap gas, at the dearer case: the slot written from zero.
#[test]
fn a_v4_pool_with_a_protocol_fee_adds_its_fee_write_to_the_swap_gas() {
    use apex_runtime::live::gas::PROTOCOL_FEE_WRITE;
    let swap_gas = |fee_ppm: u32| -> Vec<u32> {
        let snap: BTreeMap<Address, Arc<PoolSnapshot>> =
            [pool(UNI, Venue::UniswapV3, -197_350, 500), v4_pool(fee_ppm)].into_iter().map(|p| (p.spec.pool, Arc::new(p))).collect();
        let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
        let e = SizedEngine::new(cycles, sized_settings(16, Vec::new(), Reserve::WorstCase), BTreeSet::new());
        let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, 10_000_000_000_000_000, 1_500_000_000);
        arms(&lines).iter().map(|a| a.sized.as_ref().expect("a sized arm").swap_gas).collect()
    };
    let hops = u32::try_from(LEAN.hop.uniswap_v3 + LEAN.hop.uniswap_v4).unwrap();
    let plain = swap_gas(500);
    assert!(!plain.is_empty(), "the route arms");
    assert!(plain.iter().all(|g| *g == hops), "{plain:?}: no protocol fee, the hops alone ({hops})");
    // 100 pips each way on the key's 500: 600.
    let with_fee = swap_gas(600);
    assert!(!with_fee.is_empty(), "the route arms with its protocol fee on");
    let want = hops + u32::try_from(PROTOCOL_FEE_WRITE).unwrap();
    assert!(with_fee.iter().all(|g| *g == want), "{with_fee:?}: the hops and the fee's write ({want})");
}

#[test]
fn it_arms_every_slot_for_the_sized_executor() {
    let (e, snap) = engine(16);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, 10_000_000_000_000_000, 1_500_000_000);
    let armed = arms(&lines);
    assert_eq!(armed.len(), 4, "two cycles, a trigger leg each way, tier 1 each");
    for a in &armed {
        let sized = a.sized.as_ref().expect("a sized arm");
        assert_eq!(sized.tier, 1);
        assert_eq!(a.executor, Some(EXECUTOR3));
        assert_eq!(a.gas_limit, SIZED.limit);
        assert_eq!(sized.cap, (5 * 10u128.pow(18)).to_string());
        assert_eq!(sized.l1_bound, 3_000_000_000, "twice the oracle's bound");
        assert!(a.calldata.starts_with("0x3a276523"), "run(bytes)");
        assert_eq!(a.calldata.len(), 2 + 2 * (68 + 192), "a 162-byte plan");
        assert!(a.predicates.len() >= 3);
    }
}

#[test]
fn an_unchanged_arm_renews_and_a_size_change_alone_does_not_rearm() {
    let (e, snap) = engine(16);
    let first = e.tick(&snap, 1_000, 300_000, &px(), 1, 10_000_000_000_000_000, 1_500_000_000);
    assert_eq!(arms(&first).len(), 4);
    // The window passes with nothing changed: renewals, no new arms.
    let later = 1_000 + 51;
    let again = e.tick(&snap, later, 300_000, &px(), 11_000_000_000, 10_000_000_000_000_000, 1_500_000_000);
    assert!(arms(&again).is_empty());
    assert_eq!(again.iter().filter(|l| matches!(l, ArmedLine::Renew { .. })).count(), 4);
    // Liquidity added to a pool at the same price moves no threshold.
    let mut deeper = (*snap[&UNI]).clone();
    deeper.state.liquidity += 1;
    deeper.seq = 2;
    let mut moved = snap.clone();
    moved.insert(UNI, Arc::new(deeper));
    let after = e.tick(&moved, later + 51, 300_000, &px(), 21_000_000_000, 10_000_000_000_000_000, 1_500_000_000);
    assert!(arms(&after).is_empty(), "the executor sizes: a deeper pool is the same arm");
}

#[test]
fn a_moved_price_rearms_once_its_sender_is_free() {
    let (e, snap) = engine(16);
    let first = e.tick(&snap, 1_000, 300_000, &px(), 1, 10_000_000_000_000_000, 1_500_000_000);
    let ids: BTreeSet<u64> = arms(&first).iter().map(|a| a.arm_id).collect();
    let mut moved = snap.clone();
    let mut p = snap[&UNI].moved(Move::Tick(-197_330)).unwrap();
    p.seq = 2;
    moved.insert(UNI, Arc::new(p));
    let busy = e.tick(&moved, 1_010, 300_000, &px(), 2_000_000_000, 10_000_000_000_000_000, 1_500_000_000);
    assert!(arms(&busy).is_empty(), "changed, but its sender is pending");
    let freed = e.tick(&moved, 1_000 + 51, 300_000, &px(), 11_000_000_000, 10_000_000_000_000_000, 1_500_000_000);
    let new: Vec<_> = arms(&freed).into_iter().filter(|a| !ids.contains(&a.arm_id)).collect();
    assert!(!new.is_empty(), "re-armed at the new prices");
}

#[test]
fn the_senders_and_the_budget_bound_what_is_armed() {
    let (e, snap) = engine(2);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, 10_000_000_000_000_000, 1_500_000_000);
    assert_eq!(arms(&lines).len(), 2, "two senders");
    let (e, snap) = engine(16);
    let worst = u128::from(SIZED.limit) * (2 * 300_000) + 1_000_000_000_000;
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, worst * 3 + 1, 1_500_000_000);
    assert_eq!(arms(&lines).len(), 3, "the budget holds three worst cases");
}

#[test]
fn a_book_with_no_gap_to_open_arms_nothing() {
    let (e, mut snap) = engine(16);
    // So thin that no move within reach pays its gas.
    for p in snap.values_mut() {
        let mut thin = (**p).clone();
        thin.state.liquidity = 1_000_000;
        *p = Arc::new(thin);
    }
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, 10_000_000_000_000_000, 1_500_000_000);
    assert!(arms(&lines).is_empty());
}

#[test]
fn the_l1_bound_is_read_with_one_oracle_call() {
    use apex_runtime::armed::sized::{l1_upper_call, l1_upper_of, GAS_PRICE_ORACLE, SIZED_TX_BYTES};
    assert_eq!(format!("{GAS_PRICE_ORACLE:#x}"), "0x420000000000000000000000000000000000000f");
    let call = l1_upper_call(SIZED_TX_BYTES);
    // getL1FeeUpperBound(uint256): 0xf1c7a58b, then 512 as a word.
    assert_eq!(alloy_primitives::hex::encode(&call[..4]), "f1c7a58b");
    assert_eq!(call.len(), 36);
    assert_eq!(&call[34..], &[0x02, 0x00]);
    let mut answer = [0u8; 32];
    answer[28..].copy_from_slice(&1_482_055_036u32.to_be_bytes());
    assert_eq!(l1_upper_of(&answer), Some(1_482_055_036));
    assert_eq!(l1_upper_of(&answer[..31]), None, "not a word");
    assert_eq!(l1_upper_of(&[0xff; 32]), None, "past 128 bits");
}

/// R40.1: a pass's pricing, timed: three pools of one pair, every slot stale.
#[test]
fn a_full_pass_is_timed() {
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [
        (UNI, pool(UNI, Venue::UniswapV3, -197_350, 500)),
        (CAKE, pool(CAKE, Venue::PancakeV3, -197_350, 100)),
        (address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1"), pool(address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1"), Venue::Slipstream, -197_350, 400)),
    ]
    .into_iter()
    .map(|(a, p)| (a, Arc::new(p)))
    .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let slots = 2 * cycles.len();
    let (e, _) = engine(64);
    let mut settings = e.settings_for_test().clone();
    settings.workers = 6;
    let e = SizedEngine::new(cycles, settings, BTreeSet::new());
    let started = std::time::Instant::now();
    e.tick(&snap, 1_000, 300_000, &px(), 1, 10_000_000_000_000_000, 1_500_000_000);
    let took = started.elapsed();
    eprintln!("{slots} slots priced in {took:?}: {:?} a slot", took / u32::try_from(slots).unwrap_or(1));
}

/// R42: the live worker's senders as the engine's lines leave them: each
/// slot's arm and the last block of its window. An arm takes its slot only
/// once the window of the arm there has ended; a renewal re-signs the arm its
/// slot still carries, or the worker has nothing to re-sign.
#[derive(Default)]
struct Worker {
    senders: BTreeMap<u16, (u64, u64)>,
    slot_of: BTreeMap<u64, u16>,
}

impl Worker {
    fn take(&mut self, lines: &[ArmedLine], head: u64) {
        for l in lines {
            match l {
                ArmedLine::Arm(a) => {
                    if let Some((held, to)) = self.senders.get(&a.sender_slot) {
                        assert!(*to <= head, "head {head}: arm {} took sender {} from arm {held}, open to {to}", a.arm_id, a.sender_slot);
                    }
                    self.senders.insert(a.sender_slot, (a.arm_id, a.to_block));
                    self.slot_of.insert(a.arm_id, a.sender_slot);
                }
                ArmedLine::Renew { arm_id, to_block, .. } => {
                    let slot = self.slot_of[arm_id];
                    let held = self.senders[&slot].0;
                    assert_eq!(held, *arm_id, "head {head}: a renewal of arm {arm_id}, whose sender {slot} carries arm {held}");
                    self.senders.insert(slot, (*arm_id, *to_block));
                }
            }
        }
    }
}

/// R42: the sized lane renewed arms the worker had replaced as R39's did, 7,459
/// of them on 2026-11-08, and believed them live: 26% of its coverage. Here
/// the spread moves one way for two windows and then the other, on one to
/// three senders, so the trigger legs trade the senders on prices that do not
/// change; the worker must hold every arm the engine renews.
#[test]
fn the_worker_holds_every_arm_the_sized_engine_renews() {
    use apex_runtime::armed::moves::{spread_key, MovesSnapshot};
    let history = |uni_first: bool| {
        let (a, b) = if uni_first { (300.0, 0.0) } else { (0.0, 300.0) };
        let pools = [(spread_key(UNI, CAKE), vec![(a, a); 400]), (spread_key(CAKE, UNI), vec![(b, b); 400])];
        MovesSnapshot { window_ns: 10_000_000_000, saved_at_ns: 0, pools: pools.into_iter().collect() }
    };
    for max_senders in 1..=3 {
        let (e, snap) = engine(max_senders);
        let mut worker = Worker::default();
        let (mut armed, mut renewed) = (0, 0);
        for (i, head) in (1_000..1_400u64).step_by(25).enumerate() {
            e.restore_moves(&history(i % 8 < 4)).unwrap();
            // Inside the first pricing's window: the plans stay as priced.
            let lines = e.tick(&snap, head, 300_000, &px(), 1 + i as u64, 10_000_000_000_000_000, 1_500_000_000);
            worker.take(&lines, head);
            armed += arms(&lines).len();
            renewed += lines.len() - arms(&lines).len();
        }
        assert!(armed > max_senders && renewed > 0, "{max_senders} senders: {armed} arms, {renewed} renewals");
    }
}

/// R43: a spike tier whose senders are all pending holds nothing else up.
/// The census's rank moves to another route while both spike senders carry
/// the first route's arms, and a third pool's slots appear with ordinary
/// senders free: those are armed in the same pass.
#[test]
fn a_spike_tier_with_no_sender_free_does_not_stop_the_ordinary_arms() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const SLIP: Address = address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1");
    let all: BTreeMap<Address, Arc<PoolSnapshot>> = [
        (UNI, pool(UNI, Venue::UniswapV3, -197_350, 500)),
        (CAKE, pool(CAKE, Venue::PancakeV3, -197_350, 100)),
        (SLIP, pool(SLIP, Venue::Slipstream, -197_350, 400)),
    ]
    .into_iter()
    .map(|(a, p)| (a, Arc::new(p)))
    .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &all);
    let (built, _) = engine_with(16, vec![tier(2)], Reserve::WorstCase);
    let flipped = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&flipped);
    let e = SizedEngine::new(cycles, built.settings_for_test().clone(), BTreeSet::new()).with_rank(Arc::new(move |c| {
        let first = c.legs[0].pool == UNI && c.legs[1].pool == CAKE;
        if first != seen.load(Ordering::Relaxed) { 9.0 } else { 0.0 }
    }));
    let mut two = all.clone();
    two.remove(&SLIP);
    let first = e.tick(&two, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    assert_eq!(arms(&first).iter().filter(|a| a.rung >= SPIKE_TIER_BASE).count(), 2);
    flipped.store(true, Ordering::Relaxed);
    let second = e.tick(&all, 1_010, 300_000, &px(), 2_000_000_000, BUDGET, 1_500_000_000);
    let armed = arms(&second);
    assert!(armed.iter().all(|a| a.rung < SPIKE_TIER_BASE), "both spike senders are pending");
    assert!(armed.iter().any(|a| a.pools.contains(&format!("{SLIP:#x}"))), "the new pool's ordinary arms went out: {}", armed.len());
}

/// R43: a USDC arm's surplus is written in USDC's units, as every other
/// amount on its line is: the evaluator sweeps its bids on that figure at the
/// run's price. Written in wei, as the engine holds it, the sweep read 0.0075
/// ETH as 7.5 billion dollars, and every sized row of it came out in the
/// hundreds of millions.
#[test]
fn a_usdc_arm_s_surplus_is_written_in_usdc_units() {
    let (built, snap) = engine_with(6, vec![tier(2)], Reserve::WorstCase);
    let (_, cycles) = frontier::build(ChainId(8453), USDC, &snap);
    let e = SizedEngine::new(cycles, built.settings_for_test().clone(), BTreeSet::new());
    let usdc = Pricing {
        start_per_eth: apex_runtime::armed::threshold::start_per_eth(&snap),
        usdc_lenders: Some(apex_runtime::live::lenders::Lenders { balancer: U256::zero(), morpho: Some(U256::exp10(12)) }),
        ..px()
    };
    let lines = e.tick(&snap, 1_000, 300_000, &usdc, 1, BUDGET, 1_500_000_000);
    let spikes: Vec<_> = arms(&lines).into_iter().filter(|a| a.rung >= SPIKE_TIER_BASE).collect();
    assert_eq!(spikes.len(), 2);
    for a in spikes {
        assert_eq!(a.start, Some(USDC));
        let surplus: u128 = a.surplus_wei.parse().unwrap();
        // The tier's least net is 0.01 ETH, about $27 here, a quarter tipped.
        assert!((1_000_000..1_000_000_000).contains(&surplus), "{surplus} is not dollars in USDC's units");
        let wei = apex_runtime::armed::run::surplus_in_wei(a, 2_680.0);
        assert!(wei >= U256::exp10(15) && wei < U256::exp10(17), "{wei} wei at the run's price");
    }
}

/// An ordinary arm's worst case at a 300,000 wei base fee and no bid, and what
/// it would hold if it reserved a NoTrade's cost.
const ORDINARY_WORST: u128 = 1_000_000 * 600_000 + 1_000_000_000_000;
const ORDINARY_NO_TRADE: u128 = 81_700 * 600_000 + 1_000_000_000_000;

/// What the two spike arms of `engine_with(6, [tier(2)], NoTrade)` hold.
fn spike_held() -> u128 {
    let (e, snap) = engine_with(6, vec![tier(2)], Reserve::NoTrade);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    arms(&lines)
        .iter()
        .filter(|a| a.rung >= SPIKE_TIER_BASE)
        // A NoTrade through Uniswap's and PancakeSwap's pools: 39,800 + 41,900 gas.
        .map(|a| 81_700 * (600_000 + a.bid_wei_per_gas.parse::<u128>().unwrap()) + 1_000_000_000_000)
        .sum()
}

/// R43.1: the NoTrade reserve is the spike tiers' alone. An ordinary arm holds
/// its whole worst case, as before the tiers: on the first shadow boot
/// (2026-11-13) the lane-wide rule let the ordinary tier 2 arm at 5 to 10 gwei,
/// 57 of the first 83 arms, which no budget had admitted before.
#[test]
fn the_no_trade_reserve_is_the_spike_tiers_alone() {
    // Three NoTrade reservations would fit what the spike arms leave; three
    // worst cases do not.
    const _: () = assert!(3 * ORDINARY_NO_TRADE < 2 * ORDINARY_WORST + ORDINARY_WORST / 2);
    let budget = spike_held() + 2 * ORDINARY_WORST + ORDINARY_WORST / 2;
    let (e, snap) = engine_with(6, vec![tier(2)], Reserve::NoTrade);
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, budget, 1_500_000_000);
    let armed = arms(&lines);
    assert_eq!(armed.iter().filter(|a| a.rung >= SPIKE_TIER_BASE).count(), 2, "the spike arms hold a NoTrade's cost");
    assert_eq!(armed.iter().filter(|a| a.rung < SPIKE_TIER_BASE).count(), 2, "an ordinary arm holds its worst case");
}

/// R43.1: the lane's own budget is for its spike tiers. The ordinary tiers
/// stay inside `armed`'s, whatever the lane's is.
#[test]
fn the_ordinary_tiers_stay_inside_armeds_budget() {
    let (built, snap) = engine_with(6, vec![tier(2)], Reserve::NoTrade);
    let mut settings = built.settings_for_test().clone();
    settings.ordinary_budget_wei = 3 * ORDINARY_WORST + 1;
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let e = SizedEngine::new(cycles, settings, BTreeSet::new());
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    let armed = arms(&lines);
    assert_eq!(armed.iter().filter(|a| a.rung >= SPIKE_TIER_BASE).count(), 2);
    assert_eq!(armed.iter().filter(|a| a.rung < SPIKE_TIER_BASE).count(), 3, "four could pay; armed's budget holds three worst cases");
}

// ------------------------------------------------------------- R43.3: the lazy fill

/// The fill as it was before R43.3: after each pick, every unchosen arm of
/// the pick's group is valued again.
fn eager_fill(cands: &[Candidate], senders: usize, budget_wei: u128, mut marginal: impl FnMut(&Candidate, &[usize]) -> f64) -> Vec<usize> {
    let mut value: Vec<f64> = cands.iter().map(|c| c.ev).collect();
    let mut chosen: Vec<usize> = Vec::new();
    let mut left = budget_wei;
    let density = |v: f64, c: &Candidate| v / (1.0 / senders.max(1) as f64 + c.worst_wei as f64 / budget_wei.max(1) as f64);
    while chosen.len() < senders {
        let best = (0..cands.len())
            .filter(|i| cands[*i].key.2 < SPIKE_TIER_BASE && !chosen.contains(i) && value[*i] > 0.0 && cands[*i].worst_wei <= left)
            .max_by(|a, b| density(value[*a], &cands[*a]).total_cmp(&density(value[*b], &cands[*b])).then(b.cmp(a)));
        let Some(i) = best else { break };
        chosen.push(i);
        left -= cands[i].worst_wei;
        let group = cands[i].group;
        let in_group: Vec<usize> = chosen.iter().copied().filter(|j| cands[*j].group == group).collect();
        for j in 0..cands.len() {
            if cands[j].group == group && cands[j].key.2 < SPIKE_TIER_BASE && !chosen.contains(&j) {
                value[j] = marginal(&cands[j], &in_group);
            }
        }
    }
    chosen
}

/// A book of candidates from a seed: a few groups, values and reservations
/// that tie now and then, spike arms among them, and for each arm how much
/// of its value each arm chosen before it in its group leaves.
fn seeded(seed: u64, n: usize) -> (Vec<Candidate>, Vec<f64>) {
    let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    let mut next = |m: u64| {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (x >> 33) % m
    };
    let mut cands = Vec::new();
    let mut keeps = Vec::new();
    for i in 0..n {
        let tier = [1u8, 1, 2, SPIKE_TIER_BASE][next(4) as usize];
        cands.push(Candidate {
            key: (route(i as u8), 0, tier),
            group: (Address::repeat_byte(1 + next(4) as u8), next(2) == 0),
            other: Address::repeat_byte(100 + next(4) as u8),
            ev: next(6) as f64 - 1.0,
            worst_wei: 1 + next(3) as u128 * 40,
            rank: 0.0,
        });
        keeps.push([0.0, 0.3, 0.6, 0.9, 1.0][next(5) as usize]);
    }
    (cands, keeps)
}

/// R43.3: the fill values an arm again only when it is about to be chosen,
/// and chooses what valuing every arm after every pick chose: the same arms
/// in the same order, on 400 seeded books, for any senders and budget. It
/// rests on what the engine's marginal is: an arm is worth no more once
/// another of its group is armed.
#[test]
fn the_lazy_fill_chooses_what_the_eager_fill_chose() {
    for seed in 0..400u64 {
        let (cands, keeps) = seeded(seed, 6 + (seed % 30) as usize);
        let worth = |c: &Candidate, before: &[usize]| {
            let i = cands.iter().position(|x| x.key == c.key).unwrap();
            c.ev * keeps[i].powi(before.len() as i32)
        };
        for (senders, budget) in [(1usize, 1_000u128), (4, 1_000), (16, 1_000), (16, 150), (3, 90)] {
            assert_eq!(fill(&cands, senders, budget, worth), eager_fill(&cands, senders, budget, worth), "seed {seed}, {senders} senders, budget {budget}");
        }
    }
}

/// What an arm is worth after the arms of its group chosen before it, and a
/// fill over candidates asking that.
type Worth<'a> = &'a mut dyn FnMut(&Candidate, &[usize]) -> f64;
type Fill<'a> = &'a dyn Fn(&[Candidate], Worth) -> Vec<usize>;

/// R43.3: and it asks less. Two groups of six arms and four senders, an arm
/// keeping a tenth of its value for each arm of its group chosen before it.
/// Valuing every arm of the group after every pick asks 18 times
/// (5 + 5 + 4 + 4); the lazy fill does not value again what a pick left
/// unreachable, the arms of the last group chosen from: 14. The saving grows
/// with the spread of values inside a group: an arm far below the rest is
/// never valued again at all.
#[test]
fn the_lazy_fill_values_an_arm_only_when_it_may_be_chosen() {
    let mut cands: Vec<Candidate> = (0..6).map(|i| cand(i, 1, 100.0 - f64::from(i), 1)).collect();
    cands.extend((0..6).map(|i| cand(6 + i, 2, 50.0 - f64::from(i), 1)));
    // And one arm of the first group worth almost nothing beside the others.
    cands.push(cand(12, 1, 0.001, 1));
    let count = |f: Fill| {
        let mut asked = Vec::new();
        let chosen = f(&cands, &mut |c, before| {
            asked.push(c.key.0);
            c.ev * 0.1f64.powi(before.len() as i32)
        });
        (chosen, asked)
    };
    let (eager, eager_asked) = count(&|c, m| eager_fill(c, 4, 1_000, m));
    let (lazy, lazy_asked) = count(&|c, m| fill(c, 4, 1_000, m));
    assert_eq!(lazy, eager);
    assert_eq!(lazy, vec![0, 6, 1, 7]);
    assert_eq!(eager_asked.len(), 6 + 5 + 5 + 4, "every arm of the group, the small one too, after every pick");
    assert_eq!(lazy_asked.len(), 5 + 5 + 4);
    assert!(!lazy_asked.contains(&route(12)), "the arm that can never reach the top is never valued again");
}

/// The census keys winners by start token too: the sized engine asks in each
/// cycle's own, USDC's priced at the book's WETH/USDC rate.
#[test]
fn the_sized_engine_asks_each_cycle_s_winners_in_its_start_token() {
    let (_, snap) = engine(16);
    let (_, cycles) = frontier::build_starts(ChainId(8453), &[WETH, USDC], &snap);
    assert!(cycles.values().any(|c| c.start == USDC) && cycles.values().any(|c| c.start == WETH), "the fixture has both");
    let settings = sized_settings(16, Vec::new(), Reserve::WorstCase);
    let asked: Arc<std::sync::Mutex<BTreeSet<(Address, Address, Address)>>> = Arc::default();
    let seen = Arc::clone(&asked);
    let e = SizedEngine::new(cycles.clone(), settings, BTreeSet::new()).with_bids(Arc::new(move |start, a, b| {
        seen.lock().unwrap().insert((start, a, b));
        apex_runtime::armed::targets::RouteWinners::unbanded(vec![0])
    }));
    let px = Pricing { start_per_eth: apex_runtime::armed::threshold::start_per_eth(&snap), ..px() };
    e.tick(&snap, 1_000, 300_000, &px, 1, 10_000_000_000_000_000, 1_500_000_000);
    let asked = asked.lock().unwrap().clone();
    assert!(asked.iter().any(|(s, _, _)| *s == USDC), "a USDC cycle asked in USDC: {asked:?}");
    assert!(asked.iter().any(|(s, _, _)| *s == WETH), "a WETH cycle asked in WETH: {asked:?}");
    let routes: BTreeSet<_> = cycles.values().map(|c| (c.start, c.legs[0].pool, c.legs[1].pool)).collect();
    assert!(asked.is_subset(&routes), "only cycles' own (start, pools): {asked:?}");
}
