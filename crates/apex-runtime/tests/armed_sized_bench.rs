//! R43.2: where a sized pass's time goes, on ladders the size of real ones.
//! A measurement, not a check: it asserts nothing about time and is ignored.
//!
//! ```text
//! cargo test --release -p apex-runtime --test armed_sized_bench -- --ignored --nocapture
//! ```
//!
//! With two spike tiers the host's pass went from about 5 ms to 29-54 ms
//! (2026-11-13). Measured here, on four pools and 24 slots, one thread:
//!
//! | 4,000 ticks a side          | before   | after   |
//! |-----------------------------|----------|---------|
//! | one quote, a 10-tick move   | 393 µs   | 7 µs    |
//! | a pass with nothing stale   | 36.6 ms  | 2.3 ms  |
//! | a pass after one pool swaps | 322 ms   | 33 ms   |
//!
//! Three causes: the tick a target price is at was searched for over the
//! whole tick range, twice a move (41 of a small quote's 45 µs); a pool's
//! ladder was copied three times a quote and scanned from one end at every
//! crossing; and the ordinary fill re-valued spike arms it could never choose.
use alloy_primitives::{address, Address};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::armed::engine::BidInputs;
use apex_runtime::armed::plan::SizedTerms;
use apex_runtime::armed::sized::{Reserve, SizedEngine, SizedSettings};
use apex_runtime::armed::threshold::{net_at, sized_points, spike_point, Pricing, SpikeTier, SPIKE_TIER_BASE};
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::book::PoolSnapshot;
use apex_runtime::live::frontier::{self, WETH};
use apex_runtime::live::gas::LEAN;
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_types::ids::ChainId;
use ethers_core::types::U256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const L: u128 = 14_000_000_000_000_000_000;

fn pool(n: u8, venue: Venue, tick: i32, fee_ppm: u32, ticks: usize) -> PoolSnapshot {
    // `ticks` initialized ticks either side, ten apart, each a small position edge.
    let mut rows = Vec::new();
    for i in 1..=ticks as i32 {
        let net = (L / 400) as i128;
        rows.push((tick - 5 - 10 * i, (L / 400), net));
        rows.push((tick - 5 + 10 * i, (L / 400), -net));
    }
    PoolSnapshot {
        spec: PoolSpec { pool: Address::repeat_byte(n), venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5_000_000.0, v4: None },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: 10,
            fee_ppm,
            balance0: Some(U256::from(10u128.pow(24))),
            balance1: Some(U256::from(10u128.pow(16))),
        },
        ladder: TickLadder::with_gross(rows, tick - 40_000, tick + 40_000),
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

fn tiers() -> Vec<SpikeTier> {
    let t = |least: u128, cap: u128, senders: usize| SpikeTier {
        least_net_wei: least,
        bid_share_bps: 2_500,
        cap_wei_per_gas: cap,
        senders,
        max_size_wei: 18 * 10u128.pow(18),
        max_size_usdc: 100_000_000_000,
    };
    vec![t(47_000_000_000_000_000, 20_000_000_000, 6), t(470_000_000_000_000_000, 157_000_000_000, 2)]
}

fn timed<T>(label: &str, n: u32, mut f: impl FnMut() -> T) {
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(f());
    }
    let each = t.elapsed() / n;
    eprintln!("{label:<44} {each:?}");
}

#[test]
#[ignore = "a measurement: run in release with --ignored --nocapture"]
fn where_a_sized_pass_spends_its_time() {
    for ticks in [50usize, 1_000, 4_000] {
        eprintln!("--- {ticks} initialized ticks a side");
        let venues = [Venue::UniswapV3, Venue::PancakeV3, Venue::Slipstream, Venue::SlipstreamV3];
        let snap: BTreeMap<Address, Arc<PoolSnapshot>> = (0..4u8)
            .map(|i| pool(0xa0 + i, venues[usize::from(i)], -197_350 + i32::from(i), [500, 100, 400, 300][usize::from(i)], ticks))
            .map(|p| (p.spec.pool, Arc::new(p)))
            .collect();
        let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
        let c = cycles.values().next().unwrap().clone();
        let pair = [Arc::clone(&snap[&c.legs[0].pool]), Arc::clone(&snap[&c.legs[1].pool])];
        let terms = SizedTerms { cap: U256::exp10(18) * 5, rate: None, overhead: 110_000, swap_gas: 450_000, l1_bound: 3_000_000_000, min_net: 1, max_steps: 8, read_gas: 100_000 };
        let big = SizedTerms { cap: U256::exp10(18) * 18, ..terms };
        timed("pool clone", 2_000, || (*pair[0]).clone());
        for step in [10u32, 100, 1_000, 2_000] {
            timed(&format!("net_at step {step}"), 300, || net_at(&c, &pair, 0, &pair[1], step, &px(), &terms, 0));
        }
        {
            use apex_runtime::armed::plan::sized_trade;
            use apex_runtime::live::book::Move;
            let t10 = pair[0].state.tick + if c.legs[0].zero_for_one { 10 } else { -10 };
            timed("  moved(10 ticks)", 1_000, || pair[0].moved(Move::Tick(t10)));
            let moved = pair[0].moved(Move::Tick(t10)).unwrap();
            let book: BTreeMap<Address, Arc<PoolSnapshot>> = [(c.legs[0].pool, Arc::new(moved.clone())), (c.legs[1].pool, Arc::clone(&pair[1]))].into_iter().collect();
            timed("  sized_trade on a built book", 1_000, || sized_trade(&c, &book, &terms, U256::from(5_000_000u64), &LEAN));
            timed("  ladder clone", 1_000, || pair[0].ladder.clone());
            let target = get_sqrt_ratio_at_tick(t10).unwrap();
            timed("  get_sqrt_ratio_at_tick", 5_000, || get_sqrt_ratio_at_tick(std::hint::black_box(t10)));
            timed("  get_tick_at_sqrt_ratio", 5_000, || apex_math::cl_math::get_tick_at_sqrt_ratio(std::hint::black_box(target)));
            timed("  move_to_price(10 ticks)", 1_000, || apex_math::cl_move::move_to_price(&pair[0].state, &pair[0].ladder, target));
        }
        let none: &[u128] = &[];
        timed("sized_points (tiers 1, 2)", 100, || sized_points(&c, &pair, 0, &px(), &terms, 5, 10, &BidInputs::winners(none), 20_000_000_000));
        let ts = tiers();
        let from = sized_points(&c, &pair, 0, &px(), &terms, 5, 10, &BidInputs::winners(none), 20_000_000_000).last().map_or(1, |p| p.step);
        for (k, t) in ts.iter().enumerate() {
            let p = spike_point(&c, &pair, 0, &px(), &big, 5, SPIKE_TIER_BASE + k as u8, t, from, &BidInputs::winners(none));
            eprintln!("  spike tier {k}: from {from}, step {:?}", p.as_ref().map(|p| p.step));
            timed(&format!("spike_point tier {k}"), 100, || spike_point(&c, &pair, 0, &px(), &big, 5, SPIKE_TIER_BASE + k as u8, t, from, &BidInputs::winners(none)));
        }
        for (label, tiers) in [("no tiers", Vec::new()), ("two tiers", tiers())] {
            let settings = SizedSettings {
                max_senders: 24,
                window_s: 10,
                block_time_ms: 200,
                band_cap_bps: 20,
                liquidity_floor_bps: 9_800,
                tier2_multiple: 10,
                executor3: Address::repeat_byte(0xe3),
                chain_id: 8453,
                max_size_wei: 5 * 10u128.pow(18),
                max_size_usdc: 12_000_000_000,
                max_priority_wei_per_gas: 20_000_000_000,
                min_net_wei: 1,
                max_steps: 8,
                p_no_trade_ppm: 20_000,
                p_revert_ppm: 3_100,
                reprice_for: Duration::MAX,
                workers: 1,
                l1_fee_bound_wei: 1_000_000_000_000,
                tiers,
                reserve: Reserve::NoTrade,
                ordinary_budget_wei: 10_000_000_000_000_000,
            };
            let e = SizedEngine::new(cycles.clone(), settings, BTreeSet::new());
            let t = Instant::now();
            let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, 100_000_000_000_000_000, 1_500_000_000);
            eprintln!("{label}: first pass, {} slots: {:?}, {} lines", 2 * cycles.len(), t.elapsed(), lines.len());
            let mut now = 1u64;
            timed(&format!("{label}: a pass with nothing stale"), 20, || {
                now += 50_000_000;
                e.tick(&snap, 1_000, 300_000, &px(), now, 100_000_000_000_000_000, 1_500_000_000)
            });
            // One pool's swap: every slot through it is stale.
            let mut seq = 1u64;
            timed(&format!("{label}: a pass after one pool's swap"), 20, || {
                seq += 1;
                now += 50_000_000;
                let mut moved = snap.clone();
                let first = *snap.keys().next().unwrap();
                let mut p = (*snap[&first]).clone();
                p.seq = seq;
                moved.insert(first, Arc::new(p));
                e.tick(&moved, 1_000, 300_000, &px(), now, 100_000_000_000_000_000, 1_500_000_000)
            });
        }
    }
}

/// R43.3: the host's shape: many routes sharing each trigger pool. Seven pools
/// of one pair are 42 cycles and 84 slots, each pool the trigger of twelve in
/// each direction. After R43.2 the host's pass still took 35 ms (it had been
/// 45) while pricing two or three slots: the time was in choosing, not
/// pricing, which the four-pool measurement above could not show. A pass with
/// nothing stale here: 12.1 ms valuing every arm of a group after every pick,
/// 2.4 ms valuing an arm only when it may be chosen.
#[test]
#[ignore = "a measurement: run in release with --ignored --nocapture"]
fn where_a_pass_spends_its_time_when_routes_share_triggers() {
    let venues = [Venue::UniswapV3, Venue::PancakeV3, Venue::Slipstream, Venue::SlipstreamV3, Venue::UniswapV3, Venue::PancakeV3, Venue::Slipstream];
    let fees = [500, 100, 400, 300, 3000, 500, 100];
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = (0..7u8)
        .map(|i| pool(0xa0 + i, venues[usize::from(i)], -197_350 + i32::from(i), fees[usize::from(i)], 1_000))
        .map(|p| (p.spec.pool, Arc::new(p)))
        .collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    for (label, tiers) in [("no tiers", Vec::new()), ("two tiers", tiers())] {
        let settings = SizedSettings {
            max_senders: 24,
            window_s: 10,
            block_time_ms: 200,
            band_cap_bps: 20,
            liquidity_floor_bps: 9_800,
            tier2_multiple: 10,
            executor3: Address::repeat_byte(0xe3),
            chain_id: 8453,
            max_size_wei: 5 * 10u128.pow(18),
            max_size_usdc: 12_000_000_000,
            max_priority_wei_per_gas: 20_000_000_000,
            min_net_wei: 1,
            max_steps: 8,
            p_no_trade_ppm: 20_000,
            p_revert_ppm: 3_100,
            reprice_for: Duration::MAX,
            workers: 1,
            l1_fee_bound_wei: 1_000_000_000_000,
            tiers,
            reserve: Reserve::NoTrade,
            ordinary_budget_wei: 10_000_000_000_000_000,
        };
        let e = SizedEngine::new(cycles.clone(), settings, BTreeSet::new());
        let t = Instant::now();
        let lines = e.tick(&snap, 1_000, 300_000, &px(), 1, 100_000_000_000_000_000, 1_500_000_000);
        eprintln!("{label}: first pass, {} slots: {:?}, {} lines", 2 * cycles.len(), t.elapsed(), lines.len());
        let mut now = 1u64;
        timed(&format!("{label}: a pass with nothing stale"), 20, || {
            now += 50_000_000;
            e.tick(&snap, 1_000, 300_000, &px(), now, 100_000_000_000_000_000, 1_500_000_000)
        });
    }
}
