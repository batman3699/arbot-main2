//! R40.1: the sized engine. Its fill chooses by marginal expected net under
//! the senders and the budget; on a book, it arms `LeanExecutor3` and renews
//! what has not changed.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::lines::ArmedLine;
use apex_runtime::armed::sized::{fill, Candidate, SizedEngine, SizedSettings};
use apex_runtime::armed::threshold::Pricing;
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
    Candidate { key: (route(n), 0, 1), group: (Address::repeat_byte(group), true), ev, worst_wei }
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
        l1_fee_bound_wei: 1_000_000_000_000,
    };
    (SizedEngine::new(cycles, settings, BTreeSet::new()), snap)
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
