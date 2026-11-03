//! R40.1: the sized call for `LeanExecutor3`, and the mirror's prediction of
//! what the executor will do with it.

use alloy_primitives::{address, Address, B256};
use apex_exec::call::CallPlan;
use apex_exec::lean2::{Lean2Lender, StartToken, USDC};
use apex_exec::lean3::{run3_calldata, Hop3Kind};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_math::walk::{walk, WalkLeg, WalkPrice};
use apex_runtime::armed::plan::{cross_gas, sized_call, sized_outcome, SizedOutcome, SizedTerms};
use apex_runtime::live::book::PoolSnapshot;
use apex_runtime::live::calls::lean3_hop;
use apex_runtime::live::frontier::{self, Cycle, WETH};
use apex_runtime::live::gas::LEAN;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use std::collections::BTreeMap;
use std::sync::Arc;

const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const CAKE: Address = address!("72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38");
const EXECUTOR3: Address = address!("00000000000000000000000000000000000e3e03");
const AXIS: Address = address!("5be75e3b0aff5d1a042a5363ae36acb1d6d755ea");
const ALPHA: Address = address!("b0b24b89db0dafbe43c5b40226b63a179f592880");
const L: u128 = 1_400_000_000_000_000_000_000;

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32) -> PoolSnapshot {
    let spacing = 10;
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: spacing,
            fee_ppm,
            balance0: Some(U256::from(10u128.pow(24))),
            balance1: Some(U256::from(10u128.pow(16))),
        },
        ladder: TickLadder::new(vec![], tick - 20_000, tick + 20_000),
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

fn book(pools: Vec<PoolSnapshot>) -> BTreeMap<Address, Arc<PoolSnapshot>> {
    pools.into_iter().map(|p| (p.spec.pool, Arc::new(p))).collect()
}

/// WETH/USDC on Uniswap v3 and PancakeSwap, 60 ticks apart.
fn weth_route() -> (BTreeMap<Address, Arc<PoolSnapshot>>, Cycle) {
    let snap = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500), pool(CAKE, Venue::PancakeV3, -197_290, 100)]);
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    // Of its two directions, the one that pays: sold on PancakeSwap, dear there.
    let c = cycles.into_values().find(|c| walked(&snap, c, U256::MAX, 0).is_some()).expect("a paying direction");
    (snap, c)
}

fn terms(cap: U256) -> SizedTerms {
    SizedTerms { cap, rate: None, overhead: 60_000, swap_gas: 250_000, l1_bound: 3_000_000_000, min_net: 1, max_steps: 8, read_gas: 30_000 }
}

#[test]
fn a_sized_call_carries_the_cycle_and_its_terms() {
    let (snap, c) = weth_route();
    let t = terms(U256::exp10(18) * 5);
    let call = sized_call(&c, &snap, &t, EXECUTOR3, 8453, 1_793_900_000, Lean2Lender::Balancer, &LEAN).unwrap();
    let CallPlan::Lean3(plan) = call.plan() else { panic!("a LeanExecutor3 plan") };
    assert_eq!((plan.lender, plan.start, plan.cap), (Lean2Lender::Balancer, StartToken::Weth, 5 * 10u128.pow(18)));
    assert_eq!((plan.rate, plan.gas_overhead, plan.swap_gas, plan.l1_bound, plan.min_net, plan.max_steps), (0, 60_000, 250_000, 3_000_000_000, 1, 8));
    assert_eq!(plan.hops[1].token_out, WETH, "ends in what it borrowed");
    assert_eq!(plan.hops[0].tick_spacing, 10, "the spacing is the book's");
    assert_eq!(call.data(), &run3_calldata(plan).unwrap()[..]);
    assert_eq!((call.to(), call.chain_id()), (EXECUTOR3, 8453));
}

#[test]
fn the_hop_kind_and_fee_follow_the_venue() {
    let (_, c) = weth_route();
    let mut leg = c.legs[0].clone();
    let kinds = [
        (Venue::UniswapV3, Hop3Kind::UniswapV3, leg.fee_ppm),
        (Venue::PancakeV3, Hop3Kind::PancakeV3, leg.fee_ppm),
        (Venue::Slipstream, Hop3Kind::Slipstream, 0),
        (Venue::SlipstreamV3, Hop3Kind::Slipstream, 0),
    ];
    for (venue, kind, fee) in kinds {
        leg.venue = venue;
        let h = lean3_hop(&leg, 60, 30_000).unwrap();
        assert_eq!((h.kind, h.fee, h.tick_spacing, h.cross_gas), (kind, fee, 60, 30_000), "{venue:?}");
    }
    leg.venue = Venue::AerodromeV2;
    let h = lean3_hop(&leg, 60, 30_000).unwrap();
    assert_eq!((h.kind, h.fee, h.tick_spacing), (Hop3Kind::AerodromeV2, 0, 0), "its fee and reserves are read at execution");
    assert!(lean3_hop(&leg, 60, 70_000).is_err(), "a crossing's gas past two bytes");
}

#[test]
fn a_v4_hop_carries_its_key_and_the_route_borrows_from_pool_manager() {
    use apex_runtime::live::v4::{key_address, V4Key, POOL_MANAGER};
    let id = B256::repeat_byte(0x11);
    let mut v4 = pool(key_address(id), Venue::UniswapV4, -289_960, 100);
    (v4.spec.token0, v4.factory) = (AXIS, POOL_MANAGER);
    v4.spec.v4 = Some(V4Key { pool_id: id, fee: 100, tick_spacing: 1, hooks: ALPHA });
    let mut v3 = pool(UNI, Venue::UniswapV3, -289_900, 500);
    v3.spec.token0 = AXIS;
    let snap = book(vec![v4, v3]);
    let (_, cycles) = frontier::build_starts(ChainId(8453), &[USDC], &snap);
    let c = cycles.into_values().find(|c| c.legs[0].venue == Venue::UniswapV4).unwrap();
    let mut t = terms(U256::from(12_000_000_000u64));
    t.rate = Some((U256::from(3u64), U256::from(1_000_000_000u64)));
    let call = sized_call(&c, &snap, &t, EXECUTOR3, 8453, 1_793_900_000, Lean2Lender::Morpho, &LEAN).unwrap();
    let CallPlan::Lean3(plan) = call.plan() else { panic!("a LeanExecutor3 plan") };
    assert_eq!((plan.lender, plan.start), (Lean2Lender::PoolManager, StartToken::Usdc));
    let h = plan.hops[0];
    assert_eq!((h.kind, h.pool, h.fee, h.tick_spacing), (Hop3Kind::UniswapV4, ALPHA, 100, 1), "the key's hooks, fee and spacing");
}

#[test]
fn the_plan_rate_is_the_books_plus_five_percent_rounded_up() {
    let mut t = terms(U256::one());
    assert_eq!(t.plan_rate(), None, "WETH carries none");
    // 3e-9 USDC units a wei: 3e9 a 1e18, then 5% more.
    t.rate = Some((U256::from(3u64), U256::from(1_000_000_000u64)));
    assert_eq!(t.plan_rate(), Some(U256::from(3_150_000_000u64)));
    t.rate = Some((U256::from(1u64), U256::from(3u64)));
    // 1e18 / 3 × 1.05 = 350,000,000,000,000,000 exactly: no rounding.
    assert_eq!(t.plan_rate(), Some(U256::from(350_000_000_000_000_000u64)));
    t.rate = Some((U256::from(1u64), U256::from(7u64)));
    // 1e18 × 105 / 700 = 150,000,000,000,000,000 exactly; 1/7 × 1.05 rounds up past it.
    assert_eq!(t.plan_rate(), Some(U256::from(150_000_000_000_000_000u64)));
    t.rate = Some((U256::from(2u64), U256::from(7u64)));
    assert_eq!(t.plan_rate(), Some(U256::from(300_000_000_000_000_000u64)));
    t.rate = Some((U256::from(1u64), U256::from(9u64)));
    assert_eq!(t.plan_rate(), Some(U256::from(116_666_666_666_666_667u64)), "rounded up");
}

fn walked(snap: &BTreeMap<Address, Arc<PoolSnapshot>>, c: &Cycle, cap: U256, wei_per_gas: u64) -> Option<apex_math::walk::Walked> {
    let leg = |i: usize| {
        let p = &snap[&c.legs[i].pool];
        WalkLeg::Cl { state: &p.state, ladder: &p.ladder, zero_for_one: c.legs[i].zero_for_one, cross_gas: cross_gas(&c.legs[i], &LEAN) }
    };
    walk([leg(0), leg(1)], cap, WalkPrice { wei_per_gas: U256::from(wei_per_gas), rate: None }, 8)
}

#[test]
fn the_outcome_is_the_walks_size_and_reports_what_the_limits_left() {
    let (snap, c) = weth_route();
    let free = walked(&snap, &c, U256::MAX, 10_000_000).unwrap();
    let SizedOutcome::Trade { size, gross, beyond_cap, beyond_steps, .. } =
        sized_outcome(&c, &snap, &terms(U256::MAX), U256::from(10_000_000u64), &LEAN)
    else {
        panic!("a trade")
    };
    assert_eq!(size, free.size);
    assert!(gross > U256::zero());
    assert_eq!(beyond_cap, U256::zero(), "nothing left under no cap");
    assert_eq!(beyond_steps, U256::zero(), "the walk finished within its steps");

    let cap = free.size / 4;
    let SizedOutcome::Trade { size, walked, beyond_cap, .. } = sized_outcome(&c, &snap, &terms(cap), U256::from(10_000_000u64), &LEAN) else {
        panic!("a capped trade")
    };
    assert_eq!(size, cap);
    assert!(walked.capped);
    assert!(beyond_cap > U256::zero(), "the cap left value");
}

#[test]
fn no_gap_and_a_floor_too_high_are_no_trades() {
    let (snap, c) = weth_route();
    let flat = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500), pool(CAKE, Venue::PancakeV3, -197_350, 100)]);
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &flat);
    for cycle in cycles.values() {
        assert_eq!(sized_outcome(cycle, &flat, &terms(U256::MAX), U256::from(10_000_000u64), &LEAN), SizedOutcome::NoTrade(0));
    }
    // At a million gwei a gas, the reads and swaps cost more than the gap pays.
    let dear = U256::from(1_000_000_000_000_000u64);
    assert_eq!(sized_outcome(&c, &snap, &terms(U256::MAX), dear, &LEAN), SizedOutcome::NoTrade(1));
    let mut gone = snap.clone();
    gone.remove(&c.legs[1].pool);
    assert_eq!(sized_outcome(&c, &gone, &terms(U256::MAX), U256::one(), &LEAN), SizedOutcome::NoTrade(2));
}
