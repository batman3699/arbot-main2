//! R26: the arming engine and its ledger lines.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::PoolSnapshot;
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use ethers_core::types::U256;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const SLIP: Address = address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const L: u128 = 1_400_000_000_000_000_000;
const EXECUTOR2: Address = address!("00000000000000000000000000000000000e2e02");

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

use apex_runtime::armed::engine::{Engine, EngineSettings, REPRICE_BUDGET};
use apex_runtime::armed::eval::Words;
use apex_runtime::armed::lines::{Arm, ArmedLine, Word};
use apex_runtime::armed::moves::MoveStats;
use apex_runtime::armed::predicate::{layout_of, Predicate};
use apex_runtime::live::book::Move;
use apex_runtime::armed::threshold::Pricing;
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::frontier;
use apex_runtime::live::gas;
use apex_types::ids::ChainId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");

fn setup(max_arms: usize) -> (Engine, BTreeMap<Address, Arc<PoolSnapshot>>) {
    setup_with(max_arms, REPRICE_BUDGET, 3)
}

fn setup_with(max_arms: usize, reprice_budget: usize, rungs: u8) -> (Engine, BTreeMap<Address, Arc<PoolSnapshot>>) {
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let b = pool(SLIP, Venue::Slipstream, -197_350, 400);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a)), (SLIP, Arc::new(b))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = EngineSettings {
        max_arms,
        rungs,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        executor2: None,
        chain_id: 8453,
        reprice_budget,
        reprice_for: Duration::MAX,
        workers: 1,
        max_priority_wei_per_gas: 20_000_000_000,
        max_size_wei: u128::MAX,
        max_size_usdc: u128::MAX,
    };
    (Engine::new(cycles, settings, BTreeSet::new()), snap)
}

const CAKE: Address = address!("72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38");

/// Three pools of one pair: six cycles, twelve slots, every sender to spare.
fn setup_three(reprice_budget: usize, reprice_for: Duration, workers: usize) -> (Engine, BTreeMap<Address, Arc<PoolSnapshot>>) {
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [
        (UNI, pool(UNI, Venue::UniswapV3, -197_350, 500)),
        (SLIP, pool(SLIP, Venue::Slipstream, -197_350, 400)),
        (CAKE, pool(CAKE, Venue::PancakeV3, -197_350, 100)),
    ]
    .into_iter()
    .map(|(a, p)| (a, Arc::new(p)))
    .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = EngineSettings {
        max_arms: 256,
        rungs: 6,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        executor2: None,
        chain_id: 8453,
        reprice_budget,
        reprice_for,
        workers,
        max_priority_wei_per_gas: 20_000_000_000,
        max_size_wei: u128::MAX,
        max_size_usdc: u128::MAX,
    };
    (Engine::new(cycles, settings, BTreeSet::new()), snap)
}

/// `snap` with `pool` one tick up, as version `seq` of the book.
fn one_moved(snap: &BTreeMap<Address, Arc<PoolSnapshot>>, pool: Address, seq: u64) -> BTreeMap<Address, Arc<PoolSnapshot>> {
    let mut b = snap[&pool].moved(Move::Tick(snap[&pool].state.tick + 1)).unwrap();
    b.seq = seq;
    let mut moved = snap.clone();
    moved.insert(pool, Arc::new(b));
    moved
}

/// The slots (route and trigger leg) `lines` arm or renew.
fn slots_covered(lines: &[ArmedLine], armed: &mut BTreeMap<u64, (String, u8)>) -> BTreeSet<(String, u8)> {
    lines
        .iter()
        .map(|l| match l {
            ArmedLine::Arm(a) => {
                armed.insert(a.arm_id, (a.route_hash.clone(), a.trigger_leg));
                (a.route_hash.clone(), a.trigger_leg)
            }
            ArmedLine::Renew { arm_id, .. } => armed[arm_id].clone(),
        })
        .collect()
}

fn px() -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: gas::MEASURED,
        lenders: Some(apex_runtime::live::lenders::Lenders { balancer: U256::from(10u128.pow(21)), morpho: None }),
        margin_wei: U256::zero(),
        start_per_eth: None,
        rate: None,
        usdc_lenders: None,
        pool_manager: None,
    }
}

#[test]
fn the_engine_arms_its_best_slots_and_renews_them() {
    let (engine, snap) = setup(4);
    let first = engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
    let arms: Vec<_> = first.iter().filter_map(|l| if let ArmedLine::Arm(a) = l { Some(a) } else { None }).collect();
    assert_eq!(arms.len(), 4, "four senders, more than four armable rungs");
    for a in &arms {
        assert_eq!(a.to_block, 1_005, "10 s of 2 s blocks");
        assert_eq!(a.armed_block, 1_000);
        assert!(a.predicates.iter().any(|p| matches!(p, Predicate::BlockNumber { .. })));
        assert!(a.calldata.starts_with("0x") && a.calldata.len() > 10);
        assert!(a.sender_slot < 4);
        assert_eq!(a.executor, Some(EXECUTOR), "no LeanExecutor2: built for v1, and named");
        assert_eq!(a.start_words.len(), 4, "price and liquidity of both pools");
        // The fire lies past the threshold the ceiling was priced at (fires
        // used up to 7.4% over it), and execution runs to 1.25x the net gas
        // before its refund: the limit is the shared formula over 1.25x the
        // ceiling, at least 1.75x the expected gas. Four fires on 2026-10-12
        // ran out at 1.25x the ceiling.
        assert!(u128::from(a.gas_limit) * 4 >= u128::from(a.gas_expected) * 7, "limit {} for {} expected", a.gas_limit, a.gas_expected);
    }
    assert!(arms.iter().any(|a| a.trigger_leg == 1), "triggers on hop 2's pool are armed too");
    let senders: BTreeSet<_> = arms.iter().map(|a| a.sender_slot).collect();
    assert_eq!(senders.len(), arms.len(), "one sender per live arm");
    // Same head, same book: nothing new.
    assert!(engine.tick(&snap, 1_000, 5_000_000, &px(), 2).is_empty());
    // A head is a sealed block: at 1,004 the window still holds block 1,005.
    assert!(engine.tick(&snap, 1_004, 5_000_000, &px(), 3).is_empty());
    // Block 1,005 sealed 12 s after arming, the book unchanged: renewed at
    // once, so block 1,006 is covered too; not re-armed. A renewal is a new
    // transaction: its deadline runs from now, as the arm's did (0 + 10 + 1).
    let later = engine.tick(&snap, 1_005, 5_000_000, &px(), 12_000_000_000);
    assert_eq!(later.len(), 4);
    // `run(bytes)`: the plan starts at byte 68, and its deadline is the five
    // bytes at plan offset 17 (R29).
    let deadline_of = |calldata: &str| {
        let data = alloy_primitives::hex::decode(calldata.trim_start_matches("0x")).unwrap();
        data[85..90].iter().fold(0u64, |acc, b| acc << 8 | u64::from(*b))
    };
    for l in &later {
        let ArmedLine::Renew { arm_id, to_block: 1_010, deadline: Some(23), calldata: Some(c), .. } = l else { panic!("a renewal: {l:?}") };
        let a = arms.iter().find(|a| a.arm_id == *arm_id).unwrap();
        assert_eq!(a.deadline, 11);
        assert_eq!(deadline_of(&a.calldata), 11);
        assert_eq!(deadline_of(c), 23, "re-signed with the new deadline");
    }
    let s = engine.stats();
    assert_eq!((s.arms, s.renewals), (4, 4));
}

#[test]
fn a_line_round_trips_through_json() {
    let (engine, snap) = setup(1);
    let lines = engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
    let text = serde_json::to_string(&lines[0]).unwrap();
    let back: ArmedLine = serde_json::from_str(&text).unwrap();
    assert_eq!(back, lines[0]);
}

/// A pool the boot check found unreadable is never armed.
#[test]
fn an_unarmable_pool_arms_nothing() {
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let b = pool(SLIP, Venue::Slipstream, -197_350, 400);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a)), (SLIP, Arc::new(b))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let settings = EngineSettings {
        max_arms: 4, rungs: 3, window_s: 10, block_time_ms: 2_000, band_cap_bps: 5,
        liquidity_floor_bps: 9_800, executor: EXECUTOR, executor2: None, chain_id: 8453, reprice_budget: REPRICE_BUDGET,
        reprice_for: Duration::MAX, workers: 1, max_priority_wei_per_gas: 20_000_000_000, max_size_wei: u128::MAX, max_size_usdc: u128::MAX,
    };
    let engine = Engine::new(cycles, settings, [SLIP].into_iter().collect());
    assert!(engine.tick(&snap, 1_000, 5_000_000, &px(), 1).is_empty());
    let s = engine.stats();
    assert_eq!((s.slots, s.slots_priced, s.stale_waiting), (0, 0, 0), "its slots are not the engine's to price");
}

#[test]
fn moves_are_counted_over_windows() {
    let mut m = MoveStats::new(10_000_000_000);
    let s = 1_000_000_000u64;
    // Two closed windows: one moving up 30, one flat.
    for (t, x) in [(0, 0.0), (5, 30.0), (10, 30.0), (19, 30.0), (21, 30.0)] {
        m.observe(UNI, x, t * s);
    }
    assert_eq!(m.p_at_least(UNI, true, 20), 0.5, "seen: (1 hit + 1) / (2 windows + 2)");
    assert_eq!(m.p_at_least(UNI, true, 30), 0.5, "the largest seen");
    // Past the largest move seen, the measured tail: P(>= m) falls as m^-2.87
    // (38 pools, 2026-10-12/13), anchored at the largest seen. Not zero, or a
    // spike rung never gets a sender; not the prior, or it gets them all.
    let tail = |m: f64| 0.5 * (30.0 / m).powf(2.87);
    assert!((m.p_at_least(UNI, true, 40) - tail(40.0)).abs() < 1e-12);
    assert!((m.p_at_least(UNI, true, 300) - tail(300.0)).abs() < 1e-12);
    assert!(m.p_at_least(UNI, true, 300) < 0.0014 * m.p_at_least(UNI, true, 30), "ten times the largest seen: about 1/740");
    assert_eq!(m.p_at_least(UNI, false, 1), 0.0, "no downward move seen yet");
    assert_eq!(m.p_at_least(SLIP, true, 1), 0.5, "an unseen pool is a coin toss");
}

/// The words `arm` watches, as `snap` holds them now.
fn words_now(arm: &Arm, snap: &BTreeMap<Address, Arc<PoolSnapshot>>) -> Words {
    let mut now = arm.clone();
    now.start_words = arm
        .pools
        .iter()
        .flat_map(|a| {
            let p = &snap[&a.parse::<Address>().unwrap()];
            let l = layout_of(p);
            [(l.price_slot.unwrap(), p.state.sqrt_price_x96), (l.liquidity_slot.unwrap(), U256::from(p.state.liquidity))]
                .map(|(slot, v)| Word { address: format!("{:#x}", l.address), slot: format!("{slot:#x}"), value: format!("{v:#x}") })
        })
        .collect();
    Words::from_arm(&now).unwrap()
}

/// Whether `arm`, against the book `snap`, has its other pool inside its band
/// and its trigger not yet met: what an arm must be when it is armed or renewed.
fn starts_short_of_its_trigger(arm: &Arm, snap: &BTreeMap<Address, Arc<PoolSnapshot>>, head: u64) -> bool {
    let trigger_pool = &arm.pools[usize::from(arm.trigger_leg)];
    let (trigger, band): (Vec<Predicate>, Vec<Predicate>) = arm
        .predicates
        .iter()
        .cloned()
        .partition(|p| matches!(p, Predicate::Storage { address, .. } if address == trigger_pool));
    let w = words_now(arm, snap);
    w.holds(&band, head) && !w.holds(&trigger, head)
}

/// An arm is built from rungs priced at the book it is armed on. Rungs priced
/// before a pool moved are neither armed nor renewed until they are repriced:
/// in the 2026-10-12 smoke run, with 308 slots and 24 repriced a tick, arms
/// went out with bands ticks away from the other pool's price, and none fired.
#[test]
fn arms_start_inside_their_band_and_short_of_their_trigger() {
    let (engine, snap) = setup_with(4, 1, 3);
    let mut armed: BTreeMap<u64, Arm> = BTreeMap::new();
    for t in 1..=4 {
        for l in engine.tick(&snap, 1_000, 5_000_000, &px(), t) {
            let ArmedLine::Arm(a) = l else { panic!("nothing to renew yet") };
            assert!(starts_short_of_its_trigger(&a, &snap, 1_000), "arm {} at the book it was priced on", a.arm_id);
            armed.insert(a.arm_id, *a);
        }
    }
    assert_eq!(armed.len(), 4, "one slot priced a tick, every slot armed by the fourth");

    // The Slipstream pool moves 40 ticks, past every band, as block 1,005
    // (every window's last) seals: the senders are free for the next block.
    let mut b = snap[&SLIP].moved(Move::Tick(-197_310)).unwrap();
    b.seq = 1;
    let mut moved = snap.clone();
    moved.insert(SLIP, Arc::new(b));
    let mut fresh = 0;
    for (i, l) in engine.tick(&moved, 1_005, 5_000_000, &px(), 5).into_iter().enumerate() {
        match l {
            ArmedLine::Arm(a) => {
                assert!(starts_short_of_its_trigger(&a, &moved, 1_005), "line {i}: arm {} from the moved book", a.arm_id);
                fresh += 1;
            }
            ArmedLine::Renew { arm_id, .. } => {
                assert!(starts_short_of_its_trigger(&armed[&arm_id], &moved, 1_005), "line {i}: arm {arm_id} renewed on a stale band");
            }
        }
    }
    assert!(fresh >= 1, "the one slot repriced arms at the moved price");
}

/// The Slipstream pool one tick up, as a new version of the book.
fn slip_moved(snap: &BTreeMap<Address, Arc<PoolSnapshot>>) -> BTreeMap<Address, Arc<PoolSnapshot>> {
    let mut b = snap[&SLIP].moved(Move::Tick(-197_349)).unwrap();
    b.seq = 1;
    let mut moved = snap.clone();
    moved.insert(SLIP, Arc::new(b));
    moved
}

/// A rung whose plan changed is re-armed only once its arm's window has
/// ended, even with senders free: a rung never has two transactions pending.
#[test]
fn a_changed_rung_waits_for_its_window() {
    let key = |a: &Arm| (a.route_hash.clone(), a.trigger_leg, a.rung);
    // Senders to spare: every armable rung is armed.
    let (engine, snap) = setup_with(64, REPRICE_BUDGET, 3);
    let first: BTreeMap<u64, Arm> = engine
        .tick(&snap, 1_000, 5_000_000, &px(), 1)
        .into_iter()
        .map(|l| match l {
            ArmedLine::Arm(a) => (a.arm_id, *a),
            ArmedLine::Renew { .. } => panic!("nothing to renew yet"),
        })
        .collect();
    assert!(first.len() >= 4);
    let keys: BTreeSet<_> = first.values().map(key).collect();
    let moved = slip_moved(&snap);
    for l in engine.tick(&moved, 1_002, 5_000_000, &px(), 2) {
        let ArmedLine::Arm(a) = l else { panic!("windows open: nothing to renew") };
        assert!(!keys.contains(&key(&a)), "rung {:?} armed twice while its first arm is pending", key(&a));
    }
    // Block 1,005 sealed: every rung armed again at the new book, or renewed.
    let mut covered = BTreeSet::new();
    let mut rearmed = 0;
    for l in engine.tick(&moved, 1_005, 5_000_000, &px(), 3) {
        match l {
            ArmedLine::Arm(a) => {
                rearmed += u32::from(keys.contains(&key(&a)));
                covered.insert(key(&a));
            }
            ArmedLine::Renew { arm_id, .. } => {
                covered.insert(key(&first[&arm_id]));
            }
        }
    }
    assert!(keys.is_subset(&covered), "every rung armed at 1,000 is armed or renewed at 1,005");
    assert!(rearmed >= 1, "a one-tick move changes some rung's plan");
}

/// R29: a rung borrows from Morpho Blue, the cheaper loan, while Morpho holds
/// its size, and from Balancer's vault past it; the plan's lender byte says
/// which.
#[test]
fn a_rung_past_morphos_holding_borrows_from_balancer() {
    use apex_runtime::live::lenders::Lenders;
    let (engine, snap) = setup(16);
    let small = U256::from(10u128.pow(17)); // 0.1 WETH
    let lenders = Lenders { balancer: U256::from(10u128.pow(21)), morpho: Some(small) };
    let px = Pricing { lenders: Some(lenders), ..px() };
    let arms: Vec<Arm> = engine
        .tick(&snap, 1_000, 5_000_000, &px, 1)
        .into_iter()
        .filter_map(|l| if let ArmedLine::Arm(a) = l { Some(*a) } else { None })
        .collect();
    assert!(arms.iter().any(|a| a.lender == "balancer"), "some rung sizes past 0.1 WETH");
    for a in &arms {
        let size = U256::from_dec_str(&a.size_wei).unwrap();
        assert_eq!(a.lender, if size <= small { "morpho" } else { "balancer" }, "arm {} at {size}", a.arm_id);
        // `run(bytes)`: the selector, the offset, the length, then the plan,
        // whose first byte is its lender: 0 Balancer, 1 Morpho.
        let data = alloy_primitives::hex::decode(a.calldata.trim_start_matches("0x")).unwrap();
        assert_eq!(data[68], u8::from(a.lender == "morpho"), "arm {}", a.arm_id);
    }
}

/// R27.1: a slot whose pool changed is not armed until it is repriced, while
/// one whose window merely passed still is, on its last pricing. So the
/// changed slots are repriced first. On 2026-10-13 the pass was pegged (12 h
/// of CPU in 13 h up) and took its slots oldest-priced first, which put a slot
/// whose pool had just moved behind every slot that had only aged.
#[test]
fn a_slot_whose_pool_changed_is_repriced_before_one_that_only_aged() {
    for changed in [UNI, SLIP, CAKE] {
        // Eight slots a pass: two passes price all twelve.
        let (engine, snap) = setup_three(8, Duration::MAX, 1);
        let mut armed = BTreeMap::new();
        let mut all = slots_covered(&engine.tick(&snap, 1_000, 5_000_000, &px(), 1), &mut armed);
        all.extend(slots_covered(&engine.tick(&snap, 1_000, 5_000_000, &px(), 2), &mut armed));
        assert_eq!(all.len(), 12, "six cycles, either pool the trigger");

        // 11 s on, block 1,005 sealed: every window has passed, every sender
        // is free, and one pool has moved. Eight slots run through it.
        let moved = one_moved(&snap, changed, 1);
        let covered = slots_covered(&engine.tick(&moved, 1_005, 5_000_000, &px(), 11_000_000_002), &mut armed);
        assert_eq!(covered, all, "the eight changed slots repriced; the four that only aged still armed ({changed:#x} moved)");
        let s = engine.stats();
        assert_eq!((s.slots, s.slots_current, s.stale_waiting), (12, 12, 4));
    }
}

/// R27.1: a pass prices its slots on several threads, and writes what one
/// thread would. R27's larger sizes cost a third of the pass's throughput (83
/// to 58 slots a second) with three of the host's four cores idle.
#[test]
fn a_pass_on_several_threads_writes_what_one_thread_writes() {
    let run = |workers: usize| {
        let (engine, snap) = setup_three(usize::MAX, Duration::MAX, workers);
        let mut lines = engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
        let moved = one_moved(&snap, SLIP, 1);
        lines.extend(engine.tick(&moved, 1_005, 5_000_000, &px(), 2));
        lines.extend(engine.tick(&one_moved(&moved, CAKE, 2), 1_010, 5_000_000, &px(), 11_000_000_000));
        (lines, engine.stats().slots_priced)
    };
    let (one, priced) = run(1);
    assert!(one.len() >= 12 && priced == 12 + 8 + 12, "{} lines, {priced} slots priced", one.len());
    for workers in [2, 3, 8] {
        assert_eq!(run(workers), (one.clone(), priced), "{workers} threads");
    }
}

/// R27.1: a pass stops starting slots when its time is up, so the arms it has
/// priced go out; but it always prices one, or a slow book would never be
/// priced at all. What it leaves is counted.
#[test]
fn a_pass_stops_starting_slots_when_its_time_is_up() {
    for workers in [1, 3] {
        let (engine, snap) = setup_three(usize::MAX, Duration::ZERO, workers);
        for pass in 1..=3u64 {
            engine.tick(&snap, 1_000, 5_000_000, &px(), pass);
            let s = engine.stats();
            assert_eq!((s.slots_priced, s.slots_current, s.stale_waiting), (pass, pass, 12 - pass), "{workers} threads, pass {pass}");
        }
        let s = engine.stats();
        assert_eq!((s.passes, s.waiting_total, s.current_total), (3, 11 + 10 + 9, 1 + 2 + 3));
    }
    // The count is a bound too.
    let (engine, snap) = setup_three(5, Duration::MAX, 3);
    engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
    let s = engine.stats();
    assert_eq!((s.slots_priced, s.slots_current, s.stale_waiting), (5, 5, 7));
}

/// R27.1: a pass that left slots waiting is followed at once by the next. On
/// 2026-10-13 a 150 ms pass waited out the rest of a 200 ms tick with 72 slots
/// still stale. With nothing waiting, the next pass comes soon enough to meet
/// a flashblock's writes (200 ms apart) early, and an empty pass costs 0.4 ms.
#[test]
fn a_pass_that_left_slots_waiting_is_followed_at_once() {
    use apex_runtime::armed::engine::{pause_after, IDLE_PAUSE};
    assert_eq!(pause_after(1), Duration::ZERO);
    assert_eq!(pause_after(106), Duration::ZERO);
    assert_eq!(pause_after(0), IDLE_PAUSE);
    assert_eq!(IDLE_PAUSE, Duration::from_millis(50));
}

/// R29 review: an arm closing on an Aerodrome v2 pool asks it for the arm's
/// output, and one closing on a v3 pool for the loan plus one. The lean
/// executor asks an Aerodrome pool for exactly the hop's minimum.
#[test]
fn an_arm_closing_on_aerodrome_asks_for_its_output() {
    use apex_runtime::live::cp::{self, Loaded, Reserves};
    const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6, v4: None };
    // Uniswap's price at tick −197,350.
    let reserves = Reserves {
        reserve0: U256::from(1_823_383_892_520_317_644_689u128),
        reserve1: U256::from(4_903_486_964_987u128),
    };
    let loaded = Loaded { reserves, fee_ppm: 3_000, decimals: (18, 6), factory: Venue::AerodromeV2.factory() };
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [
        (UNI, Arc::new(pool(UNI, Venue::UniswapV3, -197_350, 500))),
        (AERO, Arc::new(cp::snapshot(&spec, loaded, Default::default(), 100))),
    ]
    .into_iter()
    .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let closes_on_aero: BTreeSet<String> =
        cycles.values().filter(|c| c.legs[1].pool == AERO).map(|c| format!("{:#x}", c.commitment.route_hash)).collect();
    let settings = EngineSettings {
        max_arms: 256,
        rungs: 6,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        executor2: None,
        chain_id: 8453,
        reprice_budget: REPRICE_BUDGET,
        reprice_for: Duration::MAX,
        workers: 1,
        max_priority_wei_per_gas: 20_000_000_000,
        max_size_wei: u128::MAX,
        max_size_usdc: u128::MAX,
    };
    let engine = Engine::new(cycles, settings, BTreeSet::new());
    let arms: Vec<Arm> = engine
        .tick(&snap, 1_000, 5_000_000, &px(), 1)
        .into_iter()
        .filter_map(|l| if let ArmedLine::Arm(a) = l { Some(*a) } else { None })
        .collect();
    assert!(arms.iter().any(|a| closes_on_aero.contains(&a.route_hash)), "an arm closes on Aerodrome");
    for a in &arms {
        // `run(bytes)`: the plan at byte 68, its 38-byte header, hop 0's 57
        // bytes, then hop 1, whose minimum is its last 16.
        let data = alloy_primitives::hex::decode(a.calldata.trim_start_matches("0x")).unwrap();
        let min = data[68 + 38 + 57 + 41..68 + 38 + 57 + 57].iter().fold(0u128, |acc, b| acc << 8 | u128::from(*b));
        let want = if closes_on_aero.contains(&a.route_hash) {
            a.output_wei.parse::<u128>().unwrap()
        } else {
            a.size_wei.parse::<u128>().unwrap() + 1
        };
        assert_eq!(min, want, "arm {} on {}", a.arm_id, a.route_hash);
    }
}

/// R36: what the ranking learned of each pool's moves is the engine's to give
/// and take back, so a restart does not begin it again.
#[test]
fn the_engine_hands_over_its_moves_and_takes_them_back() {
    use apex_runtime::armed::moves::MovesSnapshot;
    let (engine, _) = setup(2);
    let snap = MovesSnapshot { window_ns: 10_000_000_000, saved_at_ns: 5, pools: [(UNI, vec![(12.0, 0.0), (0.0, 3.0)])].into_iter().collect() };
    assert_eq!(engine.restore_moves(&snap), Ok(1));
    assert_eq!(engine.moves_snapshot(5), snap);
    let other = MovesSnapshot { window_ns: 1, ..snap };
    assert!(engine.restore_moves(&other).is_err(), "another window length");
}

/// R36: a fire is a gap between the route's pools, not the trigger pool's
/// move: the market moves both. Two engines with the same pools' history (100
/// ticks a window): one whose spreads never moved, so no rung can fire (all
/// tie, the first armable one is armed), and one whose spreads moved 100,
/// where the surplus decides. Read from the pool, both would be the second.
#[test]
fn the_ranking_reads_the_route_s_spread_not_its_pool() {
    use apex_runtime::armed::moves::{spread_key, MovesSnapshot};
    let armed_rung = |spread_move: f64| {
        let (engine, snap) = setup_with(1, REPRICE_BUDGET, 6);
        let big = vec![(100.0, 100.0); 400];
        let small = vec![(spread_move, spread_move); 400];
        let pools = [(UNI, big.clone()), (SLIP, big), (spread_key(UNI, SLIP), small.clone()), (spread_key(SLIP, UNI), small)];
        let history = MovesSnapshot { window_ns: 10_000_000_000, saved_at_ns: 0, pools: pools.into_iter().collect() };
        engine.restore_moves(&history).unwrap();
        let lines = engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
        let rungs: Vec<(u8, u32)> = lines.iter().filter_map(|l| if let ArmedLine::Arm(a) = l { Some((a.rung, a.step)) } else { None }).collect();
        assert_eq!(rungs.len(), 1, "one sender");
        rungs[0]
    };
    let (still, moving) = (armed_rung(0.0), armed_rung(100.0));
    assert!(still.0 < moving.0, "a spread that never moved arms {still:?}; one of 100, {moving:?}");
}

#[test]
fn the_engine_watches_each_armed_route_s_spread() {
    use apex_runtime::armed::moves::spread_key;
    let (engine, snap) = setup(1);
    engine.tick(&snap, 1_000, 5_000_000, &px(), 1);
    let seen = engine.moves_snapshot(1).pools;
    assert!(seen.contains_key(&spread_key(UNI, SLIP)) && seen.contains_key(&spread_key(SLIP, UNI)), "{:?}", seen.keys());
}

/// R37: a USDC arm borrows Morpho's USDC, and its renewal does too: renewed
/// with the cycle's own pricing, not the WETH lenders'. `run(bytes)`'s plan
/// starts at byte 68 with its lender (0 Balancer, 1 Morpho).
#[test]
fn a_renewed_usdc_arm_keeps_its_lender() {
    use apex_runtime::armed::threshold::start_per_eth;
    use apex_runtime::live::lenders::Lenders;
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> =
        [(UNI, Arc::new(pool(UNI, Venue::UniswapV3, -197_350, 500))), (SLIP, Arc::new(pool(SLIP, Venue::Slipstream, -197_350, 400)))]
            .into_iter()
            .collect();
    let (_, cycles) = frontier::build_starts(ChainId(8453), &[WETH, USDC], &snap);
    let settings = EngineSettings {
        max_arms: 64,
        rungs: 3,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        executor2: Some(EXECUTOR2),
        chain_id: 8453,
        reprice_budget: REPRICE_BUDGET,
        reprice_for: Duration::MAX,
        workers: 1,
        max_priority_wei_per_gas: 20_000_000_000,
        max_size_wei: u128::MAX,
        max_size_usdc: u128::MAX,
    };
    let engine = Engine::new(cycles, settings, BTreeSet::new());
    let px = Pricing {
        start_per_eth: start_per_eth(&snap),
        usdc_lenders: Some(Lenders { balancer: U256::zero(), morpho: Some(U256::from(10u128.pow(13))) }),
        ..px()
    };
    let lender_of = |calldata: &str| alloy_primitives::hex::decode(calldata.trim_start_matches("0x")).unwrap()[68];
    let first = engine.tick(&snap, 1_000, 5_000_000, &px, 1);
    let usdc: Vec<&Arm> = first.iter().filter_map(|l| if let ArmedLine::Arm(a) = l { Some(&**a) } else { None }).filter(|a| a.start == Some(USDC)).collect();
    assert!(!usdc.is_empty(), "USDC cycles arm");
    for a in &usdc {
        assert_eq!((lender_of(&a.calldata), a.lender.as_str()), (1, "morpho"), "Morpho's USDC");
        assert_eq!(a.executor, Some(EXECUTOR2), "built for LeanExecutor2, and simulated against it");
        let (num, den) = start_per_eth(&snap).unwrap();
        assert_eq!(a.start_per_eth, Some([num.to_string(), den.to_string()]), "the rate its costs were converted at");
    }
    let later = engine.tick(&snap, 1_005, 5_000_000, &px, 12_000_000_000);
    let mut renewed = 0;
    for l in &later {
        if let ArmedLine::Renew { arm_id, calldata: Some(c), .. } = l {
            if usdc.iter().any(|a| a.arm_id == *arm_id) {
                assert_eq!(lender_of(c), 1, "renewed on Morpho's USDC, not Balancer's WETH");
                renewed += 1;
            }
        }
    }
    assert_eq!(renewed, usdc.len());
}

/// R39: the arms of UNI, SLIP and CAKE's cycles from `starts`, six rungs
/// each, under the live worker's size caps, in the order they were armed.
/// Built for LeanExecutor2, so a USDC cycle arms too.
fn capped(max_arms: usize, max_size_wei: u128, max_size_usdc: u128, starts: &[Address]) -> Vec<Arm> {
    use apex_runtime::armed::threshold::start_per_eth;
    use apex_runtime::live::lenders::Lenders;
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [
        (UNI, pool(UNI, Venue::UniswapV3, -197_350, 500)),
        (SLIP, pool(SLIP, Venue::Slipstream, -197_350, 400)),
        (CAKE, pool(CAKE, Venue::PancakeV3, -197_350, 100)),
    ]
    .into_iter()
    .map(|(a, p)| (a, Arc::new(p)))
    .collect();
    let (_, cycles) = frontier::build_starts(ChainId(8453), starts, &snap);
    let settings = EngineSettings {
        max_arms,
        rungs: 6,
        window_s: 10,
        block_time_ms: 2_000,
        band_cap_bps: 5,
        liquidity_floor_bps: 9_800,
        executor: EXECUTOR,
        executor2: Some(EXECUTOR2),
        chain_id: 8453,
        reprice_budget: REPRICE_BUDGET,
        reprice_for: Duration::MAX,
        workers: 1,
        max_priority_wei_per_gas: 20_000_000_000,
        max_size_wei,
        max_size_usdc,
    };
    let px = Pricing {
        start_per_eth: start_per_eth(&snap),
        usdc_lenders: Some(Lenders { balancer: U256::zero(), morpho: Some(U256::from(10u128.pow(13))) }),
        ..px()
    };
    let mut arms: Vec<Arm> = Engine::new(cycles, settings, BTreeSet::new())
        .tick(&snap, 1_000, 5_000_000, &px, 1)
        .into_iter()
        .filter_map(|l| if let ArmedLine::Arm(a) = l { Some(*a) } else { None })
        .collect();
    arms.sort_by_key(|a| a.arm_id);
    arms
}

fn size_of(a: &Arm) -> u128 {
    a.size_wei.parse().unwrap()
}

fn rung_of(a: &Arm) -> (String, u8, u8) {
    (a.route_hash.clone(), a.trigger_leg, a.rung)
}

/// R39: the live worker refuses an arm over its start token's size cap, so
/// the engine ranks no such rung: every rung at or under the cap is armed,
/// and none over it.
#[test]
fn a_rung_over_the_size_cap_is_not_armed() {
    let all = capped(256, u128::MAX, u128::MAX, &[WETH]);
    let mut sizes: Vec<u128> = all.iter().map(size_of).collect();
    sizes.sort_unstable();
    let cap = sizes[sizes.len() / 2];
    assert!(cap < sizes[sizes.len() - 1], "the test needs rungs either side of the cap: {sizes:?}");
    let under: BTreeSet<_> = all.iter().filter(|a| size_of(a) <= cap).map(rung_of).collect();
    let armed: BTreeSet<_> = capped(256, cap, u128::MAX, &[WETH]).iter().map(rung_of).collect();
    assert_eq!(armed, under);
}

/// R39: no sender is left to an arm the worker would refuse. With four, the
/// best four rungs under the cap take them, though a rung that took one
/// uncapped is over it. Arms are numbered in rank order.
#[test]
fn the_senders_go_to_the_best_rungs_under_the_cap() {
    let top = capped(4, u128::MAX, u128::MAX, &[WETH]);
    let cap = top.iter().map(size_of).max().unwrap() - 1;
    let ranked = capped(256, u128::MAX, u128::MAX, &[WETH]);
    let best_under: Vec<_> = ranked.iter().filter(|a| size_of(a) <= cap).take(4).map(rung_of).collect();
    assert_eq!(best_under.len(), 4, "the test needs four rungs under the cap");
    let armed: Vec<_> = capped(4, cap, u128::MAX, &[WETH]).iter().map(rung_of).collect();
    assert_eq!(armed, best_under);
}

/// R39: each cycle is held to its start token's cap, as the worker holds its
/// arms: USDC's units for a USDC cycle, wei for a WETH one.
#[test]
fn each_start_token_has_its_own_cap() {
    let starts = |arms: Vec<Arm>| arms.iter().map(|a| a.start).collect::<BTreeSet<_>>();
    assert_eq!(starts(capped(256, u128::MAX, u128::MAX, &[WETH, USDC])), BTreeSet::from([None, Some(USDC)]));
    assert_eq!(starts(capped(256, u128::MAX, 1, &[WETH, USDC])), BTreeSet::from([None]));
    assert_eq!(starts(capped(256, 1, u128::MAX, &[WETH, USDC])), BTreeSet::from([Some(USDC)]));
}
