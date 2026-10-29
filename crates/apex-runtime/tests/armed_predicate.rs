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
    let slots = |v: Venue| {
        let l = layout(v, UNI);
        (l.price_slot.map(|s| s.as_u64()), l.liquidity_slot.map(|s| s.as_u64()))
    };
    assert_eq!(slots(Venue::UniswapV3), (Some(0), Some(4)));
    assert_eq!(slots(Venue::PancakeV3), (Some(0), Some(5)));
    for v in [Venue::Slipstream, Venue::SlipstreamV3] {
        assert_eq!(slots(v), (Some(6), Some(16)));
    }
    assert_eq!(layout(Venue::AerodromeV2, UNI).reserve_slots, Some((U256::from(20), U256::from(21))));
    assert_eq!(slots(Venue::AerodromeV2), (None, None));
    assert_eq!(layout(Venue::UniswapV3, UNI).address, UNI, "a pool's own storage");
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
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6, v4: None };
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

/// A fire's gross is what the armed plan returns on the fire's book, as the
/// lean executor runs it (R36): hop 2 sells all hop 1 returns, so a trigger
/// overshot past the threshold is sold too, not left as residue. The
/// evaluator priced hop 2 at `mid` and fell short of every simulation since
/// the lean deploy.
#[test]
fn a_fire_s_gross_is_what_the_armed_plan_returns_on_the_fire_s_book() {
    use apex_runtime::armed::plan::armed_gross;
    use apex_runtime::live::pricing::LiveCycle;
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [
        (UNI, Arc::new(pool(UNI, Venue::UniswapV3, -197_350, 500))),
        (SLIP, Arc::new(pool(SLIP, Venue::Slipstream, -197_300, 400))),
    ]
    .into_iter()
    .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let size = U256::from(10u128.pow(18));
    let (c, live) = cycles
        .values()
        .map(|c| (c, LiveCycle::new(c, &snap).unwrap()))
        .find(|(_, l)| l.quote(size).unwrap().outputs[1] > size)
        .expect("one direction pays");
    let chained = live.quote(size).unwrap();
    let mid = chained.outputs[0] * 99 / 100;
    let at_mid = live.quote_fixed(size, mid).unwrap().outputs[1];
    assert_eq!(armed_gross(c, &snap, size, mid, at_mid).unwrap(), chained.outputs[1] - size);
    assert!(chained.outputs[1] > at_mid, "the overshoot pays");
    let err = armed_gross(c, &snap, size, chained.outputs[0] + 1, at_mid).unwrap_err();
    assert!(err.contains("minimum"), "{err}");
    // A book where the cycle cannot repay the loan: hop 2 is short of its floor.
    let short = cycles.values().find(|o| o.commitment.route_hash != c.commitment.route_hash).unwrap();
    let back = LiveCycle::new(short, &snap).unwrap().quote(size).unwrap();
    assert!(armed_gross(short, &snap, size, back.outputs[0], back.outputs[1]).is_err(), "it would revert");
}

/// Closing on Aerodrome v2, the plan's WETH is exactly the armed output the
/// pool is asked for, whatever hop 1 overshoots by.
#[test]
fn a_fire_closing_on_aerodrome_grosses_exactly_the_armed_output() {
    use apex_runtime::armed::plan::armed_gross;
    use apex_runtime::live::cp::{self, Loaded, Reserves};
    use apex_runtime::live::pricing::LiveCycle;
    const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
    let spec = PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4.9e6, v4: None };
    // Reserves pricing WETH ~1% above the Uniswap pool: buying it back pays.
    let reserves = Reserves { reserve0: U256::from(1_823_383_892_520_317_644_689u128), reserve1: U256::from(4_600_000_000_000u128) };
    let loaded = Loaded { reserves, fee_ppm: 3_000, decimals: (18, 6), factory: Venue::AerodromeV2.factory() };
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [
        (UNI, Arc::new(pool(UNI, Venue::UniswapV3, -197_350, 500))),
        (AERO, Arc::new(cp::snapshot(&spec, loaded, Default::default(), 100))),
    ]
    .into_iter()
    .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    let c = cycles.values().find(|c| c.legs[1].pool == AERO).unwrap();
    let size = U256::from(10u128.pow(17));
    let chained = LiveCycle::new(c, &snap).unwrap().quote(size).unwrap();
    let output = size + (chained.outputs[1] - size) / 2;
    let mid = chained.outputs[0] * 99 / 100;
    assert_eq!(armed_gross(c, &snap, size, mid, output).unwrap(), output - size);
    assert!(armed_gross(c, &snap, size, mid, size).is_err(), "no profit: the executor's floor reverts");
}

// ------------------------------------------------------------------ R37: v4

const AXIS: Address = address!("5be75e3b0aff5d1a042a5363ae36acb1d6d755ea");
const AXIS_POOL: alloy_primitives::B256 =
    alloy_primitives::b256!("c19d91d99afc0332e5013a569c9721cd9ecb51a14c851f7a189a140950a627fa");

/// The AXIS/USDC v4 pool: keyed in the book by its key address, its words
/// PoolManager's.
fn v4_pool(tick: i32) -> PoolSnapshot {
    use apex_runtime::live::v4::{key_address, V4Key, POOL_MANAGER};
    let mut p = pool(key_address(AXIS_POOL), Venue::UniswapV4, tick, 100);
    p.spec.token0 = AXIS;
    // AXIS is worth ~5e-7 USDC a unit: a few ticks move ~1e21 of it.
    p.state.balance0 = Some(U256::exp10(30));
    p.spec.v4 = Some(V4Key { pool_id: AXIS_POOL, fee: 100, tick_spacing: 1, hooks: address!("b0b24b89db0dafbe43c5b40226b63a179f592880") });
    p.factory = POOL_MANAGER;
    p
}

#[test]
fn a_v4_trigger_watches_pool_manager_storage() {
    use apex_runtime::armed::predicate::layout_of;
    use apex_runtime::live::v4::POOL_MANAGER;
    let p = v4_pool(-289_960);
    let at = p.moved(Move::Tick(-289_950)).unwrap();
    let v = serde_json::to_value(trigger(&p, &at, true).unwrap()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["params"]["address"], "0x498581ff718922c3f8e6a244956af099b2652b2b", "PoolManager, not the key address");
    assert_eq!(v[0]["params"]["slot"], "0xa446a125276052ec378b11eacacc4812f36f5e00ab66ee87cd802ad56ce21aa5");
    assert_eq!(v[0]["params"]["mask"], "0xffffffffffffffffffffffffffffffffffffffff", "slot0's low 160 bits: the price");
    assert_eq!(v[0]["params"]["value"], format!("{:#x}", at.state.sqrt_price_x96));
    assert_eq!(layout_of(&p).address, POOL_MANAGER);
    let uni = pool(UNI, Venue::UniswapV3, -197_350, 500);
    assert_eq!((layout_of(&uni).address, layout_of(&uni).price_slot, layout_of(&uni).liquidity_slot), (UNI, Some(U256::zero()), Some(U256::from(4))));
    let mut keyless = v4_pool(-289_960);
    keyless.spec.v4 = None;
    assert_eq!(trigger(&keyless, &at, true), None, "a v4 pool without its key watches nothing");
}

#[test]
fn a_v4_band_s_liquidity_word_is_state_plus_three() {
    let p = v4_pool(-289_960);
    let edges = [p.moved(Move::Tick(-289_965)).unwrap(), p.moved(Move::Tick(-289_955)).unwrap()];
    let v = serde_json::to_value(band(&p, &edges, 9_800).unwrap()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);
    for i in 0..3 {
        assert_eq!(v[i]["params"]["address"], "0x498581ff718922c3f8e6a244956af099b2652b2b");
    }
    assert_eq!(v[0]["params"]["slot"], "0xa446a125276052ec378b11eacacc4812f36f5e00ab66ee87cd802ad56ce21aa5");
    assert_eq!(v[2]["params"]["slot"], "0xa446a125276052ec378b11eacacc4812f36f5e00ab66ee87cd802ad56ce21aa8");
    assert_eq!(v[2]["params"]["mask"], "0xffffffffffffffffffffffffffffffff");
}

/// A node holding storage words by `(address, slot)`; any other word is 0.
struct Storage(BTreeMap<(Address, U256), U256>);

#[async_trait::async_trait]
impl apex_chain::rpc::RpcTransport for Storage {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, apex_chain::rpc::RpcError> {
        assert_eq!(method, "eth_getStorageAt");
        let a: Address = params[0].as_str().unwrap().parse().unwrap();
        let slot = U256::from_str_radix(params[1].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
        Ok(serde_json::json!(format!("{:#x}", self.0.get(&(a, slot)).copied().unwrap_or_default())))
    }
}

#[tokio::test]
async fn verify_layouts_reads_v4_words_from_pool_manager() {
    use apex_runtime::armed::engine::verify_layouts;
    use apex_runtime::live::reads::ChainReads;
    use apex_runtime::live::v4::{liquidity_slot, state_slot, POOL_MANAGER};
    let p = v4_pool(-289_960);
    let word = |b: alloy_primitives::B256| U256::from_big_endian(b.as_slice());
    // slot0 with fees above the price: masked off, as the predicate masks it.
    let slot0 = p.state.sqrt_price_x96 | (U256::from(100u64) << 208);
    let at_pm = |liquidity: u128| {
        Storage(
            [((POOL_MANAGER, word(state_slot(AXIS_POOL))), slot0), ((POOL_MANAGER, word(liquidity_slot(AXIS_POOL))), U256::from(liquidity))]
                .into_iter()
                .collect(),
        )
    };
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(p.spec.pool, Arc::new(p.clone()))].into_iter().collect();
    let reads = ChainReads::new(Arc::new(at_pm(L)));
    assert!(verify_layouts(&reads, &snap, 100).await.is_empty(), "PoolManager's words are the book's");
    let reads = ChainReads::new(Arc::new(at_pm(L - 1)));
    assert_eq!(verify_layouts(&reads, &snap, 100).await, [p.spec.pool].into_iter().collect(), "a word the book does not hold");
}

// ------------------------------------------------- R37: arms for LeanExecutor2

const EXECUTOR2: Address = address!("00000000000000000000000000000000000e2e02");
const ALPHA: Address = address!("b0b24b89db0dafbe43c5b40226b63a179f592880");
const AXIS_A: Address = address!("e1bfc5ada1125718b53b2affc638ba9981f6070f");
const AXIS_B: Address = address!("e381d2ec7996d9fa1a72e461fb4470db9230f37a");

/// An AXIS/USDC pool: AXIS token0, with AXIS-sized balances.
fn axis_pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32) -> PoolSnapshot {
    let mut p = pool(addr, venue, tick, fee_ppm);
    p.spec.token0 = AXIS;
    p.state.balance0 = Some(U256::exp10(30));
    p
}

fn axis_book(pools: Vec<PoolSnapshot>) -> BTreeMap<Address, Arc<PoolSnapshot>> {
    pools.into_iter().map(|p| (p.spec.pool, Arc::new(p))).collect()
}

/// The v4 AXIS pool against a v3 one, priced 60 ticks apart: USDC cycles.
fn v4_and_v3() -> (BTreeMap<Address, Arc<PoolSnapshot>>, Vec<frontier::Cycle>) {
    let snap = axis_book(vec![v4_pool(-289_960), axis_pool(AXIS_A, Venue::UniswapV3, -289_900, 500)]);
    let (_, cycles) = frontier::build_starts(ChainId(8453), &[apex_exec::lean2::USDC], &snap);
    (snap, cycles.into_values().collect())
}

#[test]
fn a_v4_hop_carries_its_pool_key() {
    use apex_exec::lean2::{Hop2Kind, V4HopKey};
    use apex_runtime::live::calls::lean2_hop;
    let (_, cycles) = v4_and_v3();
    let c = cycles.iter().find(|c| c.legs[0].venue == Venue::UniswapV4).unwrap();
    let h = lean2_hop(&c.legs[0], alloy_primitives::U256::from(43u64), alloy_primitives::U256::from(42u64)).unwrap();
    assert_eq!(h.kind, Hop2Kind::UniswapV4);
    assert_eq!(h.v4, Some(V4HopKey { fee: 100, tick_spacing: 1, hooks: ALPHA }), "the spec's key");
    assert_eq!((h.token_out, h.min_out), (AXIS, 42), "exact input, held to its minimum");
    let v3 = lean2_hop(&c.legs[1], alloy_primitives::U256::from(43u64), alloy_primitives::U256::from(42u64)).unwrap();
    assert_eq!((v3.kind, v3.pool, v3.v4), (Hop2Kind::UniswapV3Style, AXIS_A, None));
    let mut keyless = c.legs[0].clone();
    keyless.v4 = None;
    assert!(lean2_hop(&keyless, alloy_primitives::U256::ZERO, alloy_primitives::U256::ZERO).is_err(), "a v4 hop without its key");
}

#[test]
fn an_armed_v4_route_borrows_from_the_pool_manager() {
    use apex_exec::call::CallPlan;
    use apex_exec::lean2::{Lean2Lender, StartToken};
    use apex_runtime::armed::plan::armed_call2;
    let (_, cycles) = v4_and_v3();
    let c = cycles.iter().find(|c| c.legs.iter().any(|l| l.venue == Venue::UniswapV4)).unwrap();
    let (size, mid) = (U256::from(100_000_000u64), U256::exp10(20));
    for asked in [Lean2Lender::Balancer, Lean2Lender::Morpho] {
        let call = armed_call2(c, size, mid, size + U256::from(9u64), EXECUTOR2, 8453, 1_791_800_000, asked).unwrap();
        let CallPlan::Lean2(plan) = call.plan() else { panic!("a LeanExecutor2 plan") };
        assert_eq!((plan.lender, plan.start), (Lean2Lender::PoolManager, StartToken::Usdc), "a v4 hop runs inside PoolManager's unlock");
        assert_eq!((call.to(), call.chain_id()), (EXECUTOR2, 8453));
    }
}

#[test]
fn an_armed_usdc_route_starts_and_ends_in_usdc() {
    use apex_exec::call::CallPlan;
    use apex_exec::lean2::{Lean2Lender, Lean2Plan, StartToken};
    use apex_runtime::armed::plan::armed_call2;
    use apex_runtime::live::calls::lean2_hop;
    let snap = axis_book(vec![axis_pool(AXIS_A, Venue::UniswapV3, -289_960, 500), axis_pool(AXIS_B, Venue::Slipstream, -289_900, 400)]);
    let (_, cycles) = frontier::build_starts(ChainId(8453), &[apex_exec::lean2::USDC], &snap);
    let c = cycles.values().next().unwrap();
    let (size, mid) = (U256::from(100_000_000u64), U256::exp10(20));
    let output = size + U256::from(9u64);
    let call = armed_call2(c, size, mid, output, EXECUTOR2, 8453, 1_791_800_000, Lean2Lender::Morpho).unwrap();
    let (s, m) = (u256_to_alloy(size), u256_to_alloy(mid));
    let want = Lean2Plan {
        lender: Lean2Lender::Morpho,
        start: StartToken::Usdc,
        amount: 100_000_000,
        deadline: 1_791_800_000,
        min_profit: 1,
        hops: vec![
            lean2_hop(&c.legs[0], m, m).unwrap(),
            lean2_hop(&c.legs[1], u256_to_alloy(output), s + alloy_primitives::U256::from(1)).unwrap(),
        ],
    };
    assert_eq!(call.plan(), &CallPlan::Lean2(want));
    let CallPlan::Lean2(plan) = call.plan() else { unreachable!() };
    assert_eq!(plan.hops[1].token_out, apex_exec::lean2::USDC, "ends in what it borrowed");
    // A WETH cycle through the same builder starts in WETH.
    let weth = axis_book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500), pool(SLIP, Venue::Slipstream, -197_300, 400)]);
    let (_, wc) = frontier::build(ChainId(8453), WETH, &weth);
    let call = armed_call2(wc.values().next().unwrap(), size, mid, output, EXECUTOR2, 8453, 1_791_800_000, Lean2Lender::Balancer).unwrap();
    let CallPlan::Lean2(plan) = call.plan() else { panic!("a LeanExecutor2 plan") };
    assert_eq!((plan.start, plan.lender), (StartToken::Weth, Lean2Lender::Balancer));
}

/// A v4 hop is exact input, as a v3 hop is: the lean quote's rules apply
/// unchanged, and the gross is the chained quote's.
#[test]
fn armed_gross_quotes_a_v4_hop_as_the_executor_runs_it() {
    use apex_runtime::armed::plan::armed_gross;
    use apex_runtime::live::pricing::LiveCycle;
    let (snap, cycles) = v4_and_v3();
    let size = U256::from(100_000_000u64);
    let (c, live) = cycles
        .iter()
        .filter(|c| c.legs.iter().any(|l| l.venue == Venue::UniswapV4))
        .map(|c| (c, LiveCycle::new(c, &snap).unwrap()))
        .find(|(_, l)| l.quote(size).unwrap().outputs[1] > size)
        .expect("one direction pays");
    let chained = live.quote(size).unwrap();
    let mid = chained.outputs[0] * 99 / 100;
    let at_mid = live.quote_fixed(size, mid).unwrap().outputs[1];
    assert_eq!(armed_gross(c, &snap, size, mid, at_mid).unwrap(), chained.outputs[1] - size);
    assert!(armed_gross(c, &snap, size, chained.outputs[0] + 1, at_mid).unwrap_err().contains("minimum"));
}
