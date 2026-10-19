//! R26: an armed transaction's predicates and break-even calldata.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{Move, PoolSnapshot};
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

use apex_runtime::armed::plan::armed_call;
use apex_runtime::armed::predicate::{band, expiry, layout, trigger, Predicate};
use apex_runtime::live::frontier;
use apex_types::compat::u256_to_alloy;
use apex_types::ids::ChainId;
use std::collections::BTreeMap;
use std::sync::Arc;

const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");

/// The JSON is exactly what `base_sendRawTransactionValidity` takes.
#[test]
fn predicates_encode_as_the_rpc_takes_them() {
    let p = pool(SLIP, Venue::Slipstream, -197_350, 400);
    let at = p.moved(Move::Tick(-197_300)).unwrap();
    let preds = trigger(&p, &at, true).unwrap();
    let v = serde_json::to_value(&preds).unwrap();
    assert_eq!(v[0]["type"], "storage");
    assert_eq!(v[0]["params"]["address"], "0xdbc6998296caa1652a810dc8d3baf4a8294330f1");
    assert_eq!(v[0]["params"]["slot"], "0x6");
    assert_eq!(v[0]["params"]["mask"], "0xffffffffffffffffffffffffffffffffffffffff");
    assert_eq!(v[0]["params"]["op"], ">=");
    assert_eq!(v[0]["params"]["value"], format!("{:#x}", at.state.sqrt_price_x96));
    let down = serde_json::to_value(trigger(&p, &p.moved(Move::Tick(-197_400)).unwrap(), false).unwrap()).unwrap();
    assert_eq!(down[0]["params"]["op"], "<=");
    let e = serde_json::to_value(expiry(52_510_000)).unwrap();
    assert_eq!(e, serde_json::json!({"type": "block_number", "params": {"op": "<=", "value": "0x3213d30"}}));
    let back: Vec<Predicate> = serde_json::from_value(v).unwrap();
    assert_eq!(back, preds);
}

#[test]
fn each_venue_watches_its_verified_slots() {
    assert_eq!((layout(Venue::UniswapV3).price_slot, layout(Venue::UniswapV3).liquidity_slot), (Some(0), Some(4)));
    assert_eq!((layout(Venue::PancakeV3).price_slot, layout(Venue::PancakeV3).liquidity_slot), (Some(0), Some(5)));
    for v in [Venue::Slipstream, Venue::SlipstreamV3] {
        assert_eq!((layout(v).price_slot, layout(v).liquidity_slot), (Some(6), Some(16)));
    }
    assert_eq!(layout(Venue::AerodromeV2).reserve_slots, Some((20, 21)));
    assert_eq!(layout(Venue::AerodromeV2).price_slot, None);
}

/// The other pool: a price band and a liquidity floor.
#[test]
fn the_band_is_two_bounds_and_a_floor() {
    let p = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let edges = [p.moved(Move::Tick(-197_355)).unwrap(), p.moved(Move::Tick(-197_345)).unwrap()];
    let v = serde_json::to_value(band(&p, &edges, 9_800).unwrap()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);
    assert_eq!((v[0]["params"]["op"].as_str(), v[1]["params"]["op"].as_str()), (Some(">="), Some("<=")));
    assert_eq!(v[0]["params"]["value"], format!("{:#x}", edges[0].state.sqrt_price_x96));
    assert_eq!(v[1]["params"]["value"], format!("{:#x}", edges[1].state.sqrt_price_x96));
    assert_eq!(v[2]["params"]["slot"], "0x4");
    assert_eq!(v[2]["params"]["mask"], "0xffffffffffffffffffffffffffffffff");
    assert_eq!(v[2]["params"]["value"], format!("{:#x}", U256::from(L) * U256::from(9_800) / U256::from(10_000)));
}

/// An armed cycle closing on Aerodrome v2 asks the pool for the armed output,
/// not the loan plus one (R29 review): the lean executor asks an Aerodrome
/// pool for exactly the hop's minimum, so a break-even ask would leave all of
/// the profit but a wei in the pool.
#[test]
fn an_armed_aerodrome_closing_hop_is_asked_for_the_armed_output() {
    use apex_exec::call::CallPlan;
    use apex_runtime::live::cp::{self, Loaded, Reserves};
    const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6 };
    let reserves = Reserves {
        reserve0: U256::from(1_823_383_892_520_317_644_689u128),
        reserve1: U256::from(4_541_987_609_188u128),
    };
    let loaded = Loaded { reserves, fee_ppm: 3_000, decimals: (18, 6), factory: Venue::AerodromeV2.factory() };
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> =
        [(UNI, Arc::new(a)), (AERO, Arc::new(cp::snapshot(&spec, loaded, Default::default(), 100)))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let c = cycles.values().find(|c| c.legs[1].pool == AERO).expect("a cycle closing on Aerodrome");
    let (size, mid, output) = (U256::from(10u128.pow(18)), U256::from(3_000_000_000u64), U256::from(10u128.pow(18) + 5_000_000_000_000));
    let call = armed_call(c, size, mid, output, EXECUTOR, 8453, 1_791_800_000, &apex_runtime::live::lenders::MORPHO).unwrap();
    let CallPlan::Lean(plan) = call.plan() else { panic!("a lean plan") };
    assert_eq!(plan.hops[1].min_out, 10u128.pow(18) + 5_000_000_000_000, "asked for the armed output");
    assert_eq!(plan.hops[0].min_out, 3_000_000_000, "the v3 hop at its quote");
}

/// Hop 1's floor is `mid`, hop 2's the loan plus one, the plan's profit floor 1,
/// as a lean plan (R29) from the chosen lender.
#[test]
fn the_armed_plan_carries_break_even_floors() {
    use apex_exec::call::CallPlan;
    use apex_exec::lean::{LeanLender, LeanPlan};
    use apex_runtime::live::calls::lean_hop;
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let b = pool(SLIP, Venue::Slipstream, -197_300, 400);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a.clone())), (SLIP, Arc::new(b.clone()))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let c = cycles.values().next().unwrap();
    let (size, mid) = (U256::from(10u128.pow(18)), U256::from(3_000_000_000u64));
    let output = size + U256::from(7u64);
    let call = armed_call(c, size, mid, output, EXECUTOR, 8453, 1_791_800_000, &apex_runtime::live::lenders::MORPHO).unwrap();
    let (s, m) = (u256_to_alloy(size), u256_to_alloy(mid));
    let want = LeanPlan {
        lender: LeanLender::Morpho,
        amount: 10u128.pow(18),
        deadline: 1_791_800_000,
        min_profit: 1,
        hops: vec![
            lean_hop(&c.legs[0], m, m).unwrap(),
            lean_hop(&c.legs[1], u256_to_alloy(output), s + alloy_primitives::U256::from(1)).unwrap(),
        ],
    };
    assert_eq!(call.plan(), &CallPlan::Lean(want));
    assert_eq!((call.to(), call.chain_id()), (EXECUTOR, 8453));
}
