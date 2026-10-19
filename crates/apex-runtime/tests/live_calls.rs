//! Task 8.5 R6, R29 — the production call builder (`apex_runtime::live::calls`).
//!
//! Pools are real Base shapes with a price gap between them, so which cycle
//! pays is arithmetic the test can state. The lean plan's encoder is held to
//! the contract in `apex-exec`; here the question is whether the builder
//! composes the plan the candidate was priced as — each hop's pool and kind in
//! order, exact minimums, the floor, and the commitment's lender.

mod support;

use alloy_primitives::{address, Address, B256, U256};
use apex_exec::call::CallPlan;
use apex_exec::lean::{lean_commitment, run_calldata, HopKind, LeanHop, LeanLender, LeanPlan};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::calls::{lean_hop, LiveCallBuilder};
use apex_runtime::live::frontier::{self, Cycle, Leg, BALANCER_FLASH, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::lenders::MORPHO_FLASH;
use apex_runtime::live::pricing::LiveCycle;
use apex_runtime::plane::{CallBuilder, Decline};
use apex_types::candidate::{Candidate, DiscreteRefined, DiscreteSize};
use apex_types::commitment::ExecutionCommitment;
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::ids::{ChainId, FlashProviderId};
use apex_types::state::ReconstructionStatus;
use std::collections::BTreeMap;
use std::sync::Arc;

const BASE: ChainId = ChainId(8453);
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const EXECUTOR: Address = address!("1c3d856D29eA2118c8d955070a6AD83C984586f3");
const DEADLINE: u64 = 1_790_000_000;
const L: u128 = 1_400_000_000_000_000_000;

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32, spacing: i32) -> PoolSnapshot {
    let lower = (tick - 2_000) / spacing * spacing;
    let upper = (tick + 2_000) / spacing * spacing;
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5e6 },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: spacing,
            fee_ppm,
            balance0: Some(ethers_core::types::U256::from(10u128.pow(21))),
            balance1: Some(ethers_core::types::U256::from(3_000_000_000_000u128)),
        },
        ladder: TickLadder::new(vec![(lower, L as i128), (upper, -(L as i128))], lower - 5_000, upper + 5_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: B256::ZERO,
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
        reserves: None,
    }
}

const PANCAKE: Address = address!("72ab388e2E2F6FaceF59E3C3FA2C4E29011c2D38");

/// WETH is cheaper on Uniswap than on Slipstream when `uni_tick` is below `slip_tick`.
///
/// The Uniswap pool's inventory fee (3,000) is not its chain fee (500): the
/// router finds a pool by tokens and fee, so the builder must encode the fee
/// read from the chain.
fn book(uni_tick: i32, slip_tick: i32) -> Arc<PoolBook> {
    let mut uni = pool(UNI, Venue::UniswapV3, uni_tick, 500, 10);
    uni.spec.fee_ppm = 3_000;
    Arc::new(PoolBook::from_snapshots(
        [uni, pool(SLIP, Venue::Slipstream, slip_tick, 400, 100)],
        ReconstructionStatus::Verified,
    ))
}

/// The cycle that pays, and a candidate for it priced against the book.
fn priced(b: &Arc<PoolBook>) -> (Cycle, Candidate) {
    let cycles = frontier::cycles(BASE, WETH, &b.snapshot());
    let input = U256::from(10u128.pow(18));
    let (cycle, out) = cycles
        .into_iter()
        .map(|c| {
            let out = LiveCycle::new(&c, &b.snapshot()).unwrap().output(u256_to_ethers(input)).unwrap();
            (c, u256_to_alloy(out))
        })
        .find(|(_, out)| *out > input)
        .expect("one direction pays");
    let mut c = support::candidate(1, 100, 1);
    c.route = cycle.commitment.clone();
    c.input_amount = DiscreteSize::from_refinement(input, DiscreteRefined::new());
    c.expected_output = out;
    (cycle, c)
}

fn commitment(c: &Candidate, min_profit: u64) -> ExecutionCommitment {
    ExecutionCommitment {
        chain_id: BASE,
        executor_address: EXECUTOR,
        executor_version: 1,
        venue_fingerprints: BTreeMap::new(),
        flash_source: BALANCER_FLASH,
        state_fingerprint_hash: B256::ZERO,
        route_hash: c.route.route_hash,
        exact_inputs: vec![c.input_amount.get()],
        min_profit: U256::from(min_profit),
        slippage_constraints: vec![30, 30],
        deadline: DEADLINE,
        submission_policy: c.submission_policy,
    }
}

/// The second factory's $3.8M WETH/USDC pool (tick spacing 50), from
/// `data/base/aerodrome_slipstream_v3/pools.jsonl`.
const SLIP3: Address = address!("3FE04A59Ebd38cF06080a6F60a98D124eb59392A");

const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");

fn aero(r1: u128) -> PoolSnapshot {
    use apex_runtime::live::cp::{self, Loaded, Reserves};
    let reserves = Reserves {
        reserve0: ethers_core::types::U256::from(1_823_383_892_520_317_644_689u128),
        reserve1: ethers_core::types::U256::from(r1),
    };
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6 };
    let loaded = Loaded { reserves, fee_ppm: 3_000, decimals: (18, 6), factory: Venue::AerodromeV2.factory() };
    cp::snapshot(&spec, loaded, B256::ZERO, 100)
}

fn kind(v: Venue) -> HopKind {
    match v {
        Venue::UniswapV3 | Venue::Slipstream | Venue::SlipstreamV3 => HopKind::UniswapV3Style,
        Venue::PancakeV3 => HopKind::PancakeV3,
        Venue::AerodromeV2 => HopKind::AerodromeV2,
    }
}

/// The plan the builder must produce: hop 0 at least its quote, hop 1 at
/// least the larger of the loan plus the floor and its slippage bound.
fn expected(b: &PoolBook, cycle: &Cycle, c: &Candidate, min_profit: u128, lender: LeanLender) -> LeanPlan {
    let input = c.input_amount.get();
    let mid = u256_to_alloy(LiveCycle::new(cycle, &b.snapshot()).unwrap().hop_outputs(u256_to_ethers(input)).unwrap()[0]);
    let floor = (input + U256::from(min_profit)).max(c.expected_output * U256::from(9_970u64) / U256::from(10_000u64));
    let hop = |l: &Leg, min: U256| LeanHop {
        kind: kind(b.get(l.pool).unwrap().spec.venue),
        pool: l.pool,
        token_out: l.token_out,
        min_out: u128::try_from(min).unwrap(),
    };
    LeanPlan {
        lender,
        amount: u128::try_from(input).unwrap(),
        deadline: DEADLINE,
        min_profit,
        hops: vec![hop(&cycle.legs[0], mid), hop(&cycle.legs[1], floor)],
    }
}

/// The call carries exactly `want`: its calldata, its commitment, its plan.
fn assert_builds(call: &apex_exec::call::ExecutorCall, want: &LeanPlan) {
    assert_eq!((call.to(), call.chain_id()), (EXECUTOR, 8453));
    assert_eq!(call.data(), &run_calldata(want).unwrap()[..]);
    assert_eq!(call.commitment(), lean_commitment(want, 8453, EXECUTOR).unwrap());
    assert_eq!(call.plan(), &CallPlan::Lean(want.clone()));
}

/// **A priced cycle becomes one `run` call**, in both venue orders: a Balancer
/// loan of the input, each hop's pool and kind in order, hop 0 at least its
/// quote, hop 1 at least the larger of its slippage bound and the loan plus the
/// minimum profit, and the commitment the signer recomputes.
#[test]
fn a_priced_cycle_becomes_one_run_call_in_either_venue_order() {
    // A lower tick is fewer USDC per WETH. The cycle starts in WETH, so it sells
    // WETH first where WETH is dear — the higher tick — and buys it back cheap.
    for (uni, slip, first) in [(-197_350, -197_300, Venue::Slipstream), (-197_300, -197_350, Venue::UniswapV3)] {
        let b = book(uni, slip);
        let (cycle, c) = priced(&b);
        assert_eq!(b.get(cycle.legs[0].pool).unwrap().spec.venue, first, "WETH is sold first where it is dear");
        let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &commitment(&c, 1)).expect("builds");
        assert_builds(&call, &expected(&b, &cycle, &c, 1, LeanLender::Balancer));

        // With a minimum profit of half the gain, what the cycle owes is above
        // the slippage bound, and it is hop 1's minimum.
        let half = u128::try_from((c.expected_output - c.input_amount.get()) / U256::from(2u64)).unwrap();
        let call = LiveCallBuilder::new(b.clone(), [cycle.clone()])
            .build(&c, &commitment(&c, u64::try_from(half).unwrap()))
            .expect("builds");
        let want = expected(&b, &cycle, &c, half, LeanLender::Balancer);
        assert_eq!(want.hops[1].min_out, u128::try_from(c.input_amount.get()).unwrap() + half);
        assert_builds(&call, &want);
    }
}

/// Every venue's cycle builds, its hop of that venue's kind: PancakeSwap's
/// pool pays through its own callback, both Slipstream factories through
/// Uniswap's, Aerodrome v2 by the amount it is asked for.
#[test]
fn every_venues_cycle_builds_its_hop_kind() {
    let mut cake = pool(PANCAKE, Venue::PancakeV3, -197_300, 100, 1);
    cake.spec.fee_ppm = 500;
    for (uni, other) in [
        (-197_350, cake.clone()),
        (-197_350, pool(SLIP3, Venue::SlipstreamV3, -197_300, 400, 100)),
        (-197_350, aero(4_541_987_609_188)),
        (-199_000, aero(4_541_987_609_188)),
    ] {
        let venue = other.spec.venue;
        let b = Arc::new(PoolBook::from_snapshots(
            [pool(UNI, Venue::UniswapV3, uni, 500, 10), other],
            ReconstructionStatus::Verified,
        ));
        let (cycle, c) = priced(&b);
        let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &commitment(&c, 1)).expect("builds");
        let want = expected(&b, &cycle, &c, 1, LeanLender::Balancer);
        assert!(want.hops.iter().any(|h| h.kind == kind(venue)), "{venue:?}");
        assert_builds(&call, &want);
    }
}

#[test]
fn each_venue_maps_to_its_hop_kind() {
    for (venue, want) in [
        (Venue::UniswapV3, HopKind::UniswapV3Style),
        (Venue::Slipstream, HopKind::UniswapV3Style),
        (Venue::SlipstreamV3, HopKind::UniswapV3Style),
        (Venue::PancakeV3, HopKind::PancakeV3),
        (Venue::AerodromeV2, HopKind::AerodromeV2),
    ] {
        let leg = Leg { pool: UNI, venue, token_in: WETH, token_out: USDC, zero_for_one: true, fee_ppm: 500 };
        assert_eq!(
            lean_hop(&leg, U256::from(42u64)),
            Ok(LeanHop { kind: want, pool: UNI, token_out: USDC, min_out: 42 })
        );
    }
    let leg = Leg { pool: UNI, venue: Venue::UniswapV3, token_in: WETH, token_out: USDC, zero_for_one: true, fee_ppm: 500 };
    assert!(lean_hop(&leg, U256::from(u128::MAX) + U256::from(1u64)).is_err(), "a minimum beyond 128 bits");
}

/// The book moved after pricing: the candidate's price is not the price of any
/// plan buildable now.
#[test]
fn a_book_that_moved_since_pricing_is_stale() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    b.apply_swap(cycle.legs[1].pool, U256::from(1u64) << 96, L, -197_310, (101, 0));
    let err = LiveCallBuilder::new(b.clone(), [cycle]).build(&c, &commitment(&c, 1)).unwrap_err();
    assert!(matches!(err, Decline::StaleState { .. }), "{err:?}");
}

/// A pool off its ladder cannot be quoted, so no plan can be built on it.
#[test]
fn a_pool_off_its_ladder_is_stale() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    b.apply_swap(cycle.legs[0].pool, U256::from(1u64) << 96, L, -150_000, (101, 0));
    let err = LiveCallBuilder::new(b.clone(), [cycle]).build(&c, &commitment(&c, 1)).unwrap_err();
    assert!(matches!(err, Decline::StaleState { .. }), "{err:?}");
}

/// A cycle that cannot return the loan plus its minimum profit is refused
/// here, not built into a plan the executor reverts.
#[test]
fn a_cycle_below_its_floor_is_refused() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    let gain = c.expected_output - c.input_amount.get();
    let k = commitment(&c, u64::try_from(gain).unwrap() + 1);
    let err = LiveCallBuilder::new(b, [cycle]).build(&c, &k).unwrap_err();
    assert!(matches!(&err, Decline::Uncommittable { detail } if detail.contains("minimum profit")), "{err:?}");
}

/// The commitment's lender is the plan's: Balancer's vault, or Morpho Blue
/// directly (R29); nothing else.
#[test]
fn the_plan_borrows_from_the_commitments_lender() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    let k = ExecutionCommitment { flash_source: MORPHO_FLASH, ..commitment(&c, 1) };
    let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &k).expect("builds");
    assert_builds(&call, &expected(&b, &cycle, &c, 1, LeanLender::Morpho));
    for source in [FlashProviderId(0), FlashProviderId(3)] {
        let k = ExecutionCommitment { flash_source: source, ..commitment(&c, 1) };
        let err = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &k).unwrap_err();
        assert!(matches!(err, Decline::Uncommittable { .. }), "{source:?}: {err:?}");
    }
}

/// A commitment for another route, an unknown route, and shapes a two-hop
/// cycle does not have are all refused before anything is encoded.
#[test]
fn a_commitment_that_does_not_describe_this_cycle_is_refused() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    let builder = LiveCallBuilder::new(b.clone(), [cycle.clone()]);
    let k = commitment(&c, 1);
    // The other direction's cycle is one the builder knows: a commitment for it
    // must not be built against this candidate's price.
    let other = frontier::cycles(BASE, WETH, &b.snapshot()).into_iter().find(|o| o.id != cycle.id).unwrap();
    let both = LiveCallBuilder::new(b.clone(), [cycle.clone(), other.clone()]);
    let err = both.build(&c, &ExecutionCommitment { route_hash: other.commitment.route_hash, ..k.clone() }).unwrap_err();
    assert!(matches!(&err, Decline::Uncommittable { detail } if detail.contains("another route")), "{err:?}");
    for bad in [
        ExecutionCommitment { exact_inputs: vec![], ..k.clone() },
        ExecutionCommitment { exact_inputs: vec![U256::from(1u64); 2], ..k.clone() },
        ExecutionCommitment { slippage_constraints: vec![30], ..k.clone() },
    ] {
        let err = builder.build(&c, &bad).unwrap_err();
        assert!(matches!(err, Decline::Uncommittable { .. }), "{err:?}");
    }
    // A route the builder holds no cycle for.
    let err = LiveCallBuilder::new(b, []).build(&c, &k).unwrap_err();
    assert!(matches!(&err, Decline::Uncommittable { detail } if detail.contains("no live cycle")), "{err:?}");
}
