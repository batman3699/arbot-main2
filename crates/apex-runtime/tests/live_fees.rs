//! Slipstream's dynamic fee (`apex_runtime::live::book::DynamicFee`).
//!
//! Held to the chain two ways: one Base block's raw answers, recorded
//! (`fixtures/slipstream_fee_base.json`, block 52,004,239, WETH/USDC CL100),
//! composed by the module's rules must reproduce that block's `fee()`; and eleven
//! (tick, TWAP, `fee()`) triples sampled from Base the same morning must each be
//! reproduced by the formula.

use alloy_primitives::{address, hex, Address, B256, U256};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::abi;
use apex_runtime::live::book::{compose_dynamic_fee, compose_hook_fee, DynamicFee, ModuleDefaults, PoolBook, PoolSnapshot};
use apex_runtime::live::frontier::WETH;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::state::ReconstructionStatus;
use serde_json::Value;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const SLIP: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");

/// WETH/USDC CL100's regime, as the fixture composes it.
const REGIME: DynamicFee = DynamicFee {
    base: 535,
    cap: 2_000,
    scaling: 14_900_000,
    initial: Some(150),
    seconds_ago: 600,
    twap_tick: None,
};

fn fixture() -> Value {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/slipstream_fee_base.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture");
    serde_json::from_str(&text).unwrap()
}

fn answer(f: &Value, k: &str) -> Vec<u8> {
    hex::decode(f["returns"][k].as_str().unwrap()).unwrap()
}

/// **One real block, end to end.** The module's configuration, defaults and
/// window, the factory's fee for the spacing, the pool's `observe` and `slot0`,
/// all as Base answered them at block 52,004,239 — composed here, they give that
/// block's `fee()`: 698.
#[test]
fn the_model_reproduces_the_module_on_a_real_block() {
    let f = fixture();
    let uint = |k: &str| U256::from_be_slice(&answer(&f, k)).to::<u128>();
    let defaults = ModuleDefaults {
        scaling: uint("defaultScalingFactor"),
        cap: uint("defaultFeeCap"),
        seconds_ago: uint("secondsAgo"),
    };
    let d = compose_dynamic_fee(&answer(&f, "dynamicFeeConfig"), defaults, Some(uint("tickSpacingToFee"))).unwrap();
    assert_eq!(d, REGIME);

    // The request is the one Base was sent — `cast`'s encoding of `observe([600, 0])`.
    let sent = f["calldata"]["observe"]["data"].as_str().unwrap();
    assert_eq!(format!("0x{}", hex::encode(abi::call_observe(600))), sent);
    let twap = abi::decode_twap(&answer(&f, "observe"), d.seconds_ago).expect("two cumulatives");
    let tick = i32::try_from(abi::word_int(&answer(&f, "slot0"), 1, 24).unwrap()).unwrap();
    let d = DynamicFee { twap_tick: Some(twap), ..d };
    assert_eq!(u128::from(d.after_first(tick)), uint("fee"));
    assert_eq!(d.first_in_block(tick), 150, "the block's first swap pays the initial fee");
}

/// Eleven (tick, TWAP, `fee()`) triples sampled from Base 2026-10-01, blocks
/// 52,004,199–52,004,239: the formula reproduces every one.
#[test]
fn the_formula_reproduces_eleven_sampled_blocks() {
    for (tick, twap, fee) in [
        (-197_414, -197_403, 698),
        (-197_415, -197_403, 713),
        (-197_415, -197_403, 713),
        (-197_415, -197_403, 713),
        (-197_415, -197_404, 698),
        (-197_415, -197_404, 698),
        (-197_416, -197_404, 713),
        (-197_416, -197_405, 698),
        (-197_417, -197_405, 713),
        (-197_417, -197_405, 713),
        (-197_417, -197_406, 698),
    ] {
        assert_eq!(DynamicFee { twap_tick: Some(twap), ..REGIME }.after_first(tick), fee, "tick {tick} twap {twap}");
    }
}

fn config(base: u32, cap: u32, k: u64, initial_on: bool, initial: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [u128::from(base), u128::from(cap), u128::from(k), u128::from(initial_on), u128::from(initial)] {
        out.extend_from_slice(&U256::from(v).to_be_bytes::<32>());
    }
    out
}

const DEFAULTS: ModuleDefaults = ModuleDefaults { scaling: 0, cap: 30_000, seconds_ago: 600 };

/// The module's rules, each one: an unset base is the spacing's; an unset
/// scaling takes the default scaling *and* the default cap; an unset initial fee
/// is the base; the zero-fee indicator charges nothing at all; the cap binds.
#[test]
fn the_modules_rules() {
    let compose = |c: Vec<u8>, d: ModuleDefaults| compose_dynamic_fee(&c, d, Some(500)).unwrap();

    assert_eq!(compose(config(0, 2_000, 14_900_000, false, 0), DEFAULTS).base, 500, "unset base");
    assert_eq!(compose_dynamic_fee(&config(0, 2_000, 1, false, 0), DEFAULTS, None), Err("tickSpacingToFee"));

    let defaults = ModuleDefaults { scaling: 7_000_000, cap: 900, seconds_ago: 600 };
    let d = compose(config(535, 2_000, 0, false, 0), defaults);
    assert_eq!((d.scaling, d.cap), (7_000_000, 900), "unset scaling takes both defaults");

    assert_eq!(compose(config(535, 2_000, 1, true, 0), DEFAULTS).initial, Some(535), "unset initial is the base");
    assert_eq!(compose(config(535, 2_000, 1, true, 420), DEFAULTS).initial, Some(0), "zero-fee initial");
    assert_eq!(compose(config(535, 2_000, 1, false, 150), DEFAULTS).initial, None, "disabled");

    let free = compose(config(420, 2_000, 14_900_000, true, 150), DEFAULTS);
    assert_eq!(free, DynamicFee::FREE);
    assert_eq!((free.after_first(-197_000), free.first_in_block(-197_000)), (0, 0));

    // The cap binds, and without a TWAP the module adds nothing.
    let d = DynamicFee { twap_tick: Some(0), ..REGIME };
    assert_eq!(d.after_first(1_000), 2_000);
    assert_eq!(DynamicFee { twap_tick: None, ..REGIME }.after_first(1_000), 535);
    // With the initial fee disabled, the first swap pays what every other does.
    let d = DynamicFee { initial: None, twap_tick: Some(-197_406), ..REGIME };
    assert_eq!(d.first_in_block(-197_417), d.after_first(-197_417));
}

fn observe_answer(c0: i64, c1: i64) -> Vec<u8> {
    let word = |v: i128| {
        let mut w = if v < 0 { [0xffu8; 32] } else { [0u8; 32] };
        w[16..].copy_from_slice(&v.to_be_bytes());
        w
    };
    let mut out = Vec::new();
    for v in [0x40, 0xa0, 2, i128::from(c0), i128::from(c1), 2, 0, 0] {
        out.extend_from_slice(&word(v));
    }
    out
}

/// Solidity's signed division truncates toward zero, and the module's TWAP is
/// `int24((c1 − c0) / secondsAgo)`. Base's ticks are negative, so a floor would
/// put every inexact TWAP one tick low — 15 ppm on this pool.
#[test]
fn the_twap_divides_as_solidity_does() {
    assert_eq!(abi::decode_twap(&observe_answer(0, -1_001), 10), Some(-100));
    assert_eq!(abi::decode_twap(&observe_answer(0, 1_001), 10), Some(100));
    assert_eq!(abi::decode_twap(&observe_answer(0, -118_443_900), 600), Some(-197_406));
    // Not an int24 — though an i32 —, not two cumulatives, no window: no TWAP.
    assert_eq!(abi::decode_twap(&observe_answer(0, 1 << 30), 1), None);
    assert_eq!(abi::decode_twap(&observe_answer(0, (1 << 23) - 1), 1), Some((1 << 23) - 1));
    assert_eq!(abi::decode_twap(&observe_answer(0, 10)[..32 * 4], 600), None, "cut before the second cumulative");
    assert_eq!(abi::decode_twap(&observe_answer(0, 10)[..32 * 5], 600), Some(0), "the second array is not read");
    assert_eq!(abi::decode_twap(&observe_answer(0, 10), 0), None);
}

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32, dynamic_fee: Option<DynamicFee>) -> PoolSnapshot {
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5e6, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: 1_400_000_000_000_000_000,
            tick,
            tick_spacing: 100,
            fee_ppm,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::new(vec![(-199_000, 1), (-196_000, -1)], -200_000, -195_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: B256::ZERO,
        block: 100,
        last_log: None,
        through: None,
        dynamic_fee,
        seq: 0,
        reserves: None,
    }
}

/// **The book states a Slipstream fee from the tick, on every write.** A
/// snapshot entering the book gets the fee its tick implies, whatever it
/// carried; a swap that moves the tick off the TWAP raises the fee at once —
/// the trigger swap, exactly when pricing needs it — and a Uniswap pool's fee
/// does not move at all.
#[test]
fn the_book_prices_slipstream_by_its_tick() {
    let regime = DynamicFee { twap_tick: Some(-197_406), ..REGIME };
    let book = PoolBook::from_snapshots(
        [pool(SLIP, Venue::Slipstream, -197_417, 1, Some(regime)), pool(UNI, Venue::UniswapV3, -197_350, 500, None)],
        ReconstructionStatus::Verified,
    );
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 698, "11 ticks off the TWAP");

    let sqrt = |t: i32| alloy_primitives::U256::from_be_bytes({
        let mut b = [0u8; 32];
        get_sqrt_ratio_at_tick(t).unwrap().to_big_endian(&mut b);
        b
    });
    // A large swap moves Slipstream 34 ticks off its TWAP: 535 + 34 × 14.9.
    book.apply_swap(SLIP, sqrt(-197_440), 1_400_000_000_000_000_000, -197_440, (101, 0));
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 1_041);
    // Back to the TWAP: the base.
    book.apply_swap(SLIP, sqrt(-197_406), 1_400_000_000_000_000_000, -197_406, (102, 0));
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 535);

    book.apply_swap(UNI, sqrt(-197_500), 1_400_000_000_000_000_000, -197_500, (103, 0));
    assert_eq!(book.get(UNI).unwrap().state.fee_ppm, 500, "a fixed fee stays fixed");
}

// ------------------------------------------------------------- R44: the swap hook's fee

/// The hook's `ZERO_FEE_INDICATOR`: `type(uint24).max`.
const FLAT: u32 = 0x00ff_ffff;
const HOOK_DEFAULTS: ModuleDefaults = ModuleDefaults { scaling: 0, cap: 50_000, seconds_ago: 600 };

/// R44: `DynamicSwapFeeHook`'s rules (Slipstream's third factory, `0x9cb2…`),
/// each one. They are the old module's but for its indicator: there it is 420
/// and means free; here it is `type(uint24).max` and means the tick spacing's
/// fee, flat, whatever else the pool is configured with. An unset base is the
/// spacing's; an unset scaling takes the default scaling *and* cap; an initial
/// fee of nothing is the base, and of the indicator the spacing's.
#[test]
fn the_hooks_rules() {
    let compose = |c: Vec<u8>| compose_hook_fee(&c, HOOK_DEFAULTS, Some(3_000)).unwrap();
    // As configured: WETH/USDC at spacing 50 on 2026-11-16.
    let d = compose(config(220, 7_500, 32_200_000, false, 0));
    assert_eq!((d.base, d.cap, d.scaling, d.initial, d.seconds_ago), (220, 7_500, 32_200_000, None, 600));
    // An unset base is the spacing's fee, and needs it.
    assert_eq!(compose(config(0, 7_500, 1, false, 0)).base, 3_000);
    assert_eq!(compose_hook_fee(&config(0, 7_500, 1, false, 0), HOOK_DEFAULTS, None), Err("tickSpacingToFee"));
    // An unset scaling takes the defaults for both: the pool's own cap is not read.
    let unset = compose(config(220, 7_500, 0, false, 0));
    assert_eq!((unset.scaling, unset.cap), (0, 50_000));
    // The indicator: the spacing's fee and nothing else, an initial fee included.
    let flat = compose(config(FLAT, 7_500, 32_200_000, true, 150));
    assert_eq!((flat.base, flat.cap, flat.scaling, flat.initial), (3_000, 3_000, 0, None));
    assert_eq!((flat.after_first(-5_000), flat.first_in_block(-5_000)), (3_000, 3_000));
    // It is not the old module's 420, which here is a base fee like any other.
    assert_eq!(compose(config(420, 7_500, 1, false, 0)).base, 420);
    // An initial fee: nothing set is the base, the indicator the spacing's, else its own.
    assert_eq!(compose(config(220, 7_500, 1, true, 0)).initial, Some(220));
    assert_eq!(compose(config(0, 7_500, 1, true, 0)).initial, Some(3_000), "the base the pool ends up with");
    assert_eq!(compose(config(220, 7_500, 1, true, FLAT)).initial, Some(3_000));
    assert_eq!(compose(config(220, 7_500, 1, true, 90)).initial, Some(90));
    assert_eq!(compose(config(220, 7_500, 1, false, 90)).initial, None, "not enabled");
    assert_eq!(compose_hook_fee(&[0u8; 31], HOOK_DEFAULTS, Some(3_000)), Err("dynamicFeeConfig"));
}

/// R44: the model is the hook's `fee()`, on twelve Base blocks in which the
/// pool had not yet swapped (58,230,447 to 58,230,468, 2026-11-16): three
/// pools, each `(base, cap, K)` as configured, its tick and ten-minute TWAP
/// as read, and the fee the pool answered.
#[test]
fn the_hook_model_reproduces_twelve_sampled_blocks() {
    // A pool's `(base, cap, K)`, and four blocks' `(tick, TWAP, fee)`.
    type Pool = ((u32, u32, u64), [(i32, i32, u32); 4]);
    let sampled: [Pool; 3] = [
        // WETH/USDC, spacing 50.
        ((220, 7_500, 32_200_000), [(-201_354, -201_351, 316), (-201_354, -201_351, 316), (-201_355, -201_351, 348), (-201_357, -201_351, 413)]),
        // WETH/cbBTC, spacing 10.
        ((250, 50_000, 100_000_000), [(-266_013, -266_011, 450), (-266_013, -266_011, 450), (-266_013, -266_011, 450), (-266_013, -266_011, 450)]),
        // USDC/cbBTC, spacing 100.
        ((400, 50_000, 12_000_000), [(-64_661, -64_664, 436), (-64_661, -64_664, 436), (-64_659, -64_664, 460), (-64_659, -64_664, 460)]),
    ];
    for ((base, cap, k), rows) in sampled {
        let d = compose_hook_fee(&config(base, cap, k, false, 0), HOOK_DEFAULTS, Some(3_000)).unwrap();
        for (tick, twap, fee) in rows {
            assert_eq!(DynamicFee { twap_tick: Some(twap), ..d }.after_first(tick), fee, "base {base}, tick {tick}, TWAP {twap}");
        }
    }
}

/// R44: a hooked pool's fee is fixed for a block by the block's first swap,
/// at the tick the pool stood at before it. A backrun follows the trigger in
/// its block, so the pool as the trigger leaves it (`triggered`) still
/// charges the fee of where it stood; on the older factories the module
/// reads the tick at every swap, and the fee moves with it. A pool merely
/// standing elsewhere when its block begins (`moved`: a band's edge) charges
/// the fee of that tick on every venue.
#[test]
fn a_hooked_pool_a_trigger_moved_keeps_the_fee_of_where_it_stood() {
    use apex_runtime::live::book::Move;
    let regime = DynamicFee { base: 220, cap: 7_500, scaling: 32_200_000, initial: None, seconds_ago: 600, twap_tick: Some(-197_350) };
    let book = PoolBook::from_snapshots(
        [pool(SLIP, Venue::SlipstreamHook, -197_350, 1, Some(regime)), pool(UNI, Venue::Slipstream, -197_350, 1, Some(regime))],
        ReconstructionStatus::Verified,
    );
    let (hooked, older) = (book.get(SLIP).unwrap(), book.get(UNI).unwrap());
    assert_eq!((hooked.state.fee_ppm, older.state.fee_ppm), (220, 220), "at the TWAP: the base");
    // A trigger moves each 40 ticks: 220 + 40 x 32.2 = 1,508 where the fee follows the tick.
    let (h, o) = (hooked.triggered(Move::Tick(-197_310)).unwrap(), older.triggered(Move::Tick(-197_310)).unwrap());
    assert_eq!((h.state.tick, o.state.tick), (-197_310, -197_310));
    assert_eq!(o.state.fee_ppm, 1_508, "the module reads the tick at every swap");
    assert_eq!(h.state.fee_ppm, 220, "the block's fee was set before the trigger moved it");
    assert_eq!(h.state.sqrt_price_x96, o.state.sqrt_price_x96, "the same move, but for the fee");
    // Standing there when the block begins: its first swap reads that tick.
    assert_eq!(hooked.moved(Move::Tick(-197_310)).unwrap().state.fee_ppm, 1_508);
    assert_eq!(older.moved(Move::Tick(-197_310)).unwrap().state.fee_ppm, 1_508);
    // A swap the book applies is a later block's start: the fee is that tick's.
    let sqrt = alloy_primitives::U256::from_be_bytes({
        let mut b = [0u8; 32];
        get_sqrt_ratio_at_tick(-197_310).unwrap().to_big_endian(&mut b);
        b
    });
    book.apply_swap(SLIP, sqrt, 1_400_000_000_000_000_000, -197_310, (101, 0));
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 1_508, "the next block's first swap reads the new tick");
}

/// R44: the evaluator rebuilds a fire's book from the parent block and the
/// fire block's own swaps, and the book restates a fee from the tick at every
/// swap. Within the block a hooked pool charges what its first swap found, so
/// its fee is put back to the parent's; the older factory's stays the tick's.
#[test]
fn a_fire_blocks_replay_keeps_a_hooked_pools_fee() {
    use apex_runtime::armed::run::keep_block_fees;
    let regime = DynamicFee { base: 220, cap: 7_500, scaling: 32_200_000, initial: None, seconds_ago: 600, twap_tick: Some(-197_350) };
    let book = PoolBook::from_snapshots(
        [pool(SLIP, Venue::SlipstreamHook, -197_350, 1, Some(regime)), pool(UNI, Venue::Slipstream, -197_350, 1, Some(regime))],
        ReconstructionStatus::Verified,
    );
    let parent = book.snapshot();
    let sqrt = alloy_primitives::U256::from_be_bytes({
        let mut b = [0u8; 32];
        get_sqrt_ratio_at_tick(-197_310).unwrap().to_big_endian(&mut b);
        b
    });
    for p in [SLIP, UNI] {
        book.apply_swap(p, sqrt, 1_400_000_000_000_000_000, -197_310, (101, 0));
    }
    let kept = keep_block_fees(&parent, &book.snapshot());
    assert_eq!((kept[&SLIP].state.tick, kept[&SLIP].state.fee_ppm), (-197_310, 220), "the trigger's price, the block's fee");
    assert_eq!((kept[&UNI].state.tick, kept[&UNI].state.fee_ppm), (-197_310, 1_508));
}
