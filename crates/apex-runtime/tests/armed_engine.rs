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

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32) -> PoolSnapshot {
    let spacing = 10;
    let lower = (tick - 2_000) / spacing * spacing;
    let upper = (tick + 2_000) / spacing * spacing;
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0 },
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
use apex_runtime::armed::predicate::{layout, Predicate};
use apex_runtime::live::book::Move;
use apex_runtime::armed::threshold::Pricing;
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::frontier;
use apex_runtime::live::gas;
use apex_types::ids::ChainId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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
        chain_id: 8453,
        reprice_budget,
    };
    (Engine::new(cycles, settings, BTreeSet::new()), snap)
}

fn px() -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: gas::MEASURED,
        lenders: Some(apex_runtime::live::lenders::Lenders::balancer_only(U256::from(10u128.pow(21)))),
        margin_wei: U256::zero(),
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
    let word = |d: u64| format!("{d:064x}");
    for l in &later {
        let ArmedLine::Renew { arm_id, to_block: 1_010, deadline: Some(23), calldata: Some(c), .. } = l else { panic!("a renewal: {l:?}") };
        let a = arms.iter().find(|a| a.arm_id == *arm_id).unwrap();
        assert_eq!(a.deadline, 11);
        assert!(a.calldata.contains(&word(11)) && !a.calldata.contains(&word(23)));
        assert!(c.contains(&word(23)), "re-signed with the new deadline");
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
        liquidity_floor_bps: 9_800, executor: EXECUTOR, chain_id: 8453, reprice_budget: REPRICE_BUDGET,
    };
    let engine = Engine::new(cycles, settings, [SLIP].into_iter().collect());
    assert!(engine.tick(&snap, 1_000, 5_000_000, &px(), 1).is_empty());
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
            let l = layout(p.spec.venue);
            [(l.price_slot.unwrap(), p.state.sqrt_price_x96), (l.liquidity_slot.unwrap(), U256::from(p.state.liquidity))]
                .map(|(slot, v)| Word { address: a.clone(), slot: format!("{slot:#x}"), value: format!("{v:#x}") })
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

/// R27: a rung sized past Balancer's holding borrows from the Morpho lender,
/// and its calldata names that lender; one within it borrows from Balancer.
#[test]
fn a_rung_past_balancers_holding_borrows_from_morpho() {
    use apex_runtime::live::lenders::Lenders;
    let lender = Address::repeat_byte(0xee);
    let (engine, snap) = setup(16);
    let small = U256::from(10u128.pow(17)); // 0.1 WETH
    let px = Pricing { lenders: Some(Lenders { balancer: small, morpho: Some((lender, U256::from(10u128.pow(21)))) }), ..px() };
    let arms: Vec<Arm> = engine
        .tick(&snap, 1_000, 5_000_000, &px, 1)
        .into_iter()
        .filter_map(|l| if let ArmedLine::Arm(a) = l { Some(*a) } else { None })
        .collect();
    let needle = format!("{lender:#x}").trim_start_matches("0x").to_string();
    assert!(arms.iter().any(|a| a.lender == "morpho"), "some rung sizes past 0.1 WETH");
    for a in &arms {
        let size = U256::from_dec_str(&a.size_wei).unwrap();
        assert_eq!(a.lender, if size <= small { "balancer" } else { "morpho" }, "arm {} at {size}", a.arm_id);
        assert_eq!(a.calldata.contains(&needle), a.lender == "morpho");
    }
}
