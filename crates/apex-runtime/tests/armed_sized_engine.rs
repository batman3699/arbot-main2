//! R40.1: the sized engine. Its fill chooses by marginal expected net under
//! the senders and the budget; on a book, it arms `LeanExecutor3` and renews
//! what has not changed.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::lines::ArmedLine;
use apex_runtime::armed::sized::{fill, fill_spike, Candidate, Reserve, SizedEngine, SizedSettings};
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
    Candidate { key: (route(n), 0, 1), group: (Address::repeat_byte(group), true), ev, worst_wei, rank: 0.0 }
}

/// A spike candidate of tier `SPIKE_TIER_BASE + k`.
fn spike(n: u8, group: u8, k: u8, rank: f64, worst_wei: u128) -> Candidate {
    Candidate { key: (route(n), 0, SPIKE_TIER_BASE + k), group: (Address::repeat_byte(group), true), ev: 1.0, worst_wei, rank }
}

#[test]
fn spike_senders_go_to_the_best_ranked_pairs_one_a_trigger() {
    // Two on trigger 1, ranked 9 and 8; one on trigger 2, ranked 5; one unranked.
    let cands = vec![spike(1, 1, 0, 8.0, 1), spike(2, 1, 0, 9.0, 1), spike(3, 2, 0, 5.0, 1), spike(4, 3, 0, 0.0, 1)];
    assert_eq!(fill_spike(&cands, &[2], 1_000), vec![1, 2], "the second arm on trigger 1 is passed over");
    assert_eq!(fill_spike(&cands, &[8], 1_000), vec![1, 2, 3], "and an unranked pair comes last");
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

fn engine_with(max_senders: usize, tiers: Vec<SpikeTier>, reserve: Reserve) -> (SizedEngine, BTreeMap<Address, Arc<PoolSnapshot>>) {
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> =
        [(UNI, pool(UNI, Venue::UniswapV3, -197_350, 500)), (CAKE, pool(CAKE, Venue::PancakeV3, -197_350, 100))]
            .into_iter()
            .map(|(a, p)| (a, Arc::new(p)))
            .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = SizedSettings {
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
    };
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
    let e = e.with_rank(Arc::new(|start_in, _| if start_in == CAKE { 7.0 } else { 0.0 }));
    let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, BUDGET, 1_500_000_000);
    let spikes: Vec<_> = arms(&lines).into_iter().filter(|a| a.rung >= SPIKE_TIER_BASE).collect();
    assert_eq!(spikes.len(), 2);
    assert!(spikes.iter().all(|a| a.pools[0] == format!("{CAKE:#x}")), "both legs of the route that starts in the ranked pool");
}

/// R43: what a pending sized arm holds of the day's budget. Here a spike arm
/// bids 16.7 gwei: its gas limit at that is 0.0167 ETH, over a budget of
/// 0.006; what an inclusion that does not trade costs is 0.0023, and two fit.
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
    let e = SizedEngine::new(cycles, built.settings_for_test().clone(), BTreeSet::new()).with_rank(Arc::new(move |start_in, other| {
        let first = start_in == UNI && other == CAKE;
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
