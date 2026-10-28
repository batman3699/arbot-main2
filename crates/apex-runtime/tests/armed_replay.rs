//! Task 8.5 R36, lever 1 — `apex armed-replay`: a recorded fire's route quoted
//! on the fire's own book at other sizes, to ask whether a smaller size that
//! crosses no tick would have paid more per gas than the size armed.

use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::replay::{size_point, sizes, single_tick_size};
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::gas;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::pricing::LiveCycle;
use apex_types::ids::ChainId;
use apex_types::state::ReconstructionStatus;
use ethers_core::types::U256;

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP: Address = address!("dbc6998296caA1652A810dc8D3BaF4A8294330f1");
const L: u128 = 1_400_000_000_000_000_000;

/// A WETH/USDC pool, liquidity `L` over ±2,000 ticks, with a zero-net
/// initialized tick every `every`: a larger trade crosses more of them.
fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32, every: i32) -> PoolSnapshot {
    let (lower, upper) = ((tick - 2_000) / 10 * 10, (tick + 2_000) / 10 * 10);
    let mut ticks = vec![(lower, L as i128), (upper, -(L as i128))];
    let mut t = lower + every;
    while t < upper {
        ticks.push((t, 0));
        t += every;
    }
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: 10,
            fee_ppm,
            balance0: Some(U256::from(10u128.pow(21))),
            balance1: Some(U256::from(3_000_000_000_000u128)),
        },
        ladder: TickLadder::new(ticks, lower - 5_000, upper + 5_000),
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

fn cycle_on(every: i32) -> (frontier::Cycle, std::collections::BTreeMap<Address, std::sync::Arc<PoolSnapshot>>) {
    let book = PoolBook::from_snapshots(
        [pool(UNI, Venue::UniswapV3, -197_355, 500, every), pool(SLIP, Venue::Slipstream, -197_305, 400, every)],
        ReconstructionStatus::Verified,
    );
    let snap = book.snapshot();
    let c = frontier::cycles(ChainId(8453), WETH, &snap).into_iter().next().unwrap();
    (c, (*snap).clone())
}

/// The single-tick size is the largest that crosses nothing on either hop:
/// one wei more crosses a tick.
#[test]
fn the_single_tick_size_is_the_largest_that_crosses_nothing() {
    let (c, snap) = cycle_on(20);
    let live = LiveCycle::new(&c, &snap).unwrap();
    let cap = U256::exp10(21);
    let s = single_tick_size(&live, cap);
    let crossed = |x: U256| live.quote(x).unwrap().hops.map(|h| h.crossed);
    assert!(s > U256::zero() && s < cap);
    assert_eq!(crossed(s), [0, 0]);
    assert_ne!(crossed(s + 1), [0, 0]);
}

/// With no tick inside the cap's reach, the cap is the answer.
#[test]
fn a_cap_that_crosses_nothing_is_its_own_single_tick_size() {
    let (c, snap) = cycle_on(1_000);
    let live = LiveCycle::new(&c, &snap).unwrap();
    let cap = U256::exp10(15);
    assert_eq!(live.quote(cap).unwrap().hops.map(|h| h.crossed), [0, 0]);
    assert_eq!(single_tick_size(&live, cap), cap);
}

/// A size's point: the cycle's output, each hop's crossings and the lean
/// model's gas for them, the figure the engine arms with.
#[test]
fn a_size_point_carries_its_output_crossings_and_lean_gas() {
    let (c, snap) = cycle_on(20);
    let live = LiveCycle::new(&c, &snap).unwrap();
    let x = U256::exp10(19);
    let q = live.quote(x).unwrap();
    let p = size_point(&live, &gas::LEAN, x).unwrap();
    assert_eq!((p.size, p.output), (x, q.outputs[1]));
    assert_eq!(p.crossed, q.hops.map(|h| h.crossed));
    assert_eq!(p.gas, gas::LEAN.estimate(&q.hops).expected.0);
    assert!(p.crossed[0] + p.crossed[1] > 0);
    assert_eq!(size_point(&live, &gas::LEAN, U256::exp10(30)), None, "past what the pools can fill");
}

/// The ladder: the armed size times 2^(k/2) for k from -12 to 4, the armed
/// size itself and the single-tick size; ascending, each once, none zero.
#[test]
fn the_sizes_span_a_sixty_fourth_to_four_times_the_armed_size() {
    let armed = U256::exp10(18);
    let s = sizes(armed, U256::from(123_456u64));
    assert_eq!(s.first(), Some(&U256::from(123_456u64)));
    assert_eq!(s.last(), Some(&(armed * 4)));
    assert!(s.contains(&armed) && s.contains(&(armed / 64)));
    assert!(s.windows(2).all(|w| w[0] < w[1]), "ascending, each once");
    assert_eq!(sizes(armed, U256::zero()).len(), 17, "no single-tick size: none added");
}
