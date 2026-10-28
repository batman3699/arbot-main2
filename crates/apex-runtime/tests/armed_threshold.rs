//! R26: an armed backrun's threshold, rungs and band.

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

use apex_runtime::armed::threshold::{band, best_at, fixed, move_of, rungs, trigger_up, Band, Pricing, RUNG_OFFSETS};
use apex_runtime::live::pricing::LiveCycle;
use apex_runtime::econ::RouteCosts;
use apex_runtime::live::frontier::{self, Cycle};
use apex_runtime::live::gas;
use apex_types::ids::ChainId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

fn pair_and_cycles() -> (Vec<Cycle>, BTreeMap<Address, Arc<PoolSnapshot>>) {
    let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
    let b = pool(SLIP, Venue::Slipstream, -197_350, 400);
    let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a)), (SLIP, Arc::new(b))].into_iter().collect();
    let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
    (cycles.into_values().collect(), snap)
}

fn px(margin: u128) -> Pricing {
    Pricing {
        costs: RouteCosts { other_wei: 20_000_000_000, wei_per_gas: 5_000_000 },
        gas: gas::MEASURED,
        lenders: Some(apex_runtime::live::lenders::Lenders { balancer: U256::from(10u128.pow(21)), morpho: None }),
        margin_wei: U256::from(margin),
        start_per_eth: None,
        rate: None,
        usdc_lenders: None,
        pool_manager: None,
    }
}

fn pair(c: &Cycle, snap: &BTreeMap<Address, Arc<PoolSnapshot>>) -> [Arc<PoolSnapshot>; 2] {
    [Arc::clone(&snap[&c.legs[0].pool]), Arc::clone(&snap[&c.legs[1].pool])]
}

/// Two pools at the same price pay nothing; moving the trigger opens the gap
/// R36: the rungs sit where ordinary moves land, 1, 2, 4, 8 and 16 steps past
/// the threshold, not R26's 5 to 200 that only spikes reached.
#[test]
fn rungs_step_one_two_four_eight_sixteen_past_the_threshold() {
    let (cycles, snap) = pair_and_cycles();
    let c = &cycles[0];
    let found = rungs(c, &pair(c, &snap), 0, &px(0), 6);
    let h = found[0].step;
    assert_eq!(found.iter().map(|r| r.step).collect::<Vec<_>>(), vec![h, h + 1, h + 2, h + 4, h + 8, h + 16]);
}

/// at a threshold, and one step less does not clear it.
#[test]
fn the_threshold_is_the_smallest_move_that_clears_the_margin() {
    let (cycles, snap) = pair_and_cycles();
    for c in &cycles {
        for leg in 0..2 {
            let pair = pair(c, &snap);
            let found = rungs(c, &pair, leg, &px(0), 6);
            assert!(!found.is_empty(), "a big enough move opens the gap");
            let first = &found[0];
            let up = trigger_up(c, leg);
            let one_less = pair[leg].moved(move_of(&pair[leg], up, first.step - 1)).unwrap();
            assert!(best_at(c, &pair, leg, one_less, &px(0)).is_none(), "minimal at step {}", first.step);
            // Rungs: the threshold, then 1, 2, 4, 8 and 16 steps past it, each
            // paying more: ordinary moves land there (R36).
            assert_eq!(RUNG_OFFSETS, [0, 1, 2, 4, 8, 16]);
            assert_eq!(found.iter().map(|r| r.step).collect::<Vec<_>>(), RUNG_OFFSETS.map(|o| first.step + o).to_vec());
            assert!(found.windows(2).all(|w| w[1].point.surplus > w[0].point.surplus));
            assert_eq!(found.iter().map(|r| r.index).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 6]);
            assert_eq!(rungs(c, &pair, leg, &px(0), 9).len(), 6, "no more rungs than offsets");
            // A margin raises the threshold.
            let stricter = rungs(c, &pair, leg, &px(first.point.surplus.as_u128() * 2), 1);
            assert!(stricter[0].step > first.step);
            // Hop 2's fixed input is hop 1's output at the size.
            assert!(first.point.mid > U256::zero() && first.point.output > first.point.size);
        }
    }
}

/// The other pool's band holds the rung's fixed size above the margin at
/// both edges; a band is never wider than the cap. With the trigger on hop 2's
/// pool, the band is on hop 1's: `mid` is then the least hop 1 returns across
/// the band, so the plan's fixed hop-2 input is always there.
#[test]
fn the_band_keeps_the_fixed_size_paying_at_both_edges() {
    let (cycles, snap) = pair_and_cycles();
    for c in &cycles {
        for leg in 0..2 {
            let pair = pair(c, &snap);
            let rung = rungs(c, &pair, leg, &px(0), 3).pop().unwrap(); // the largest: room for a band
            let b = band(c, &pair, leg, &rung, &px(0), 5).unwrap_or_else(|| panic!("a band for leg {leg}"));
            let [lo, hi] = &b.edges;
            let other = &pair[1 - leg];
            assert!(lo.state.sqrt_price_x96 < other.state.sqrt_price_x96 && hi.state.sqrt_price_x96 > other.state.sqrt_price_x96);
            assert!(other.state.tick - lo.state.tick <= 5 && hi.state.tick - other.state.tick <= 5);
            assert!(b.point.surplus > U256::zero() && b.point.size == rung.point.size);
            for edge in [lo, hi] {
                let mut pools = pair.clone();
                pools[leg] = Arc::new(rung.trigger.clone());
                pools[1 - leg] = Arc::new(edge.clone());
                let map: BTreeMap<Address, Arc<PoolSnapshot>> = c.legs.iter().map(|l| l.pool).zip(pools).collect();
                let live = LiveCycle::new(c, &map).unwrap();
                let q = live.quote_fixed(b.point.size, b.point.mid).expect("hop 1 covers mid at every edge");
                assert!(q.outputs[1] > b.point.size, "and the loan is repaid");
            }
            if leg == 0 {
                assert_eq!(b.point.mid, rung.point.mid, "a band on hop 2's pool leaves hop 1 alone");
            } else {
                assert!(b.point.mid < rung.point.mid, "hop 1's band edge returns less");
            }
            assert!(band(c, &pair, leg, &rung, &px(rung.point.surplus.as_u128()), 5).is_none(), "no room at the margin");
            assert!(band(c, &pair, leg, &rung, &px(0), 0).is_none(), "a zero cap allows no band");
        }
    }
}

#[test]
fn the_trigger_moves_against_the_cycles_own_swap() {
    let (cycles, _) = pair_and_cycles();
    assert_eq!(cycles.len(), 2);
    for c in &cycles {
        for leg in 0..2 {
            assert_eq!(trigger_up(c, leg), c.legs[leg].zero_for_one);
        }
    }
}

/// Whatever the pools' offset, the threshold is the smallest move that pays:
/// the bisection is exact at every threshold, not only at those its doubling
/// lands on.
#[test]
fn the_threshold_is_minimal_at_every_offset() {
    let mut seen = BTreeSet::new();
    for d in -12..=12 {
        let a = pool(UNI, Venue::UniswapV3, -197_350, 500);
        let b = pool(SLIP, Venue::Slipstream, -197_350 + d, 400);
        let snap: BTreeMap<Address, Arc<PoolSnapshot>> = [(UNI, Arc::new(a)), (SLIP, Arc::new(b))].into_iter().collect();
        let (_, cycles) = frontier::build(ChainId(8453), WETH, &snap);
        for c in cycles.values() {
            for leg in 0..2 {
                let pair = pair(c, &snap);
                let Some(first) = rungs(c, &pair, leg, &px(0), 1).into_iter().next() else { continue };
                if first.step > 1 {
                    let one_less = pair[leg].moved(move_of(&pair[leg], trigger_up(c, leg), first.step - 1)).unwrap();
                    assert!(best_at(c, &pair, leg, one_less, &px(0)).is_none(), "offset {d}: minimal at step {}", first.step);
                }
                seen.insert(first.step);
            }
        }
    }
    assert!(seen.len() >= 8, "thresholds over a range: {seen:?}");
}

fn edge_ticks(b: &Band) -> [i32; 2] {
    [b.edges[0].state.tick, b.edges[1].state.tick]
}

/// The band's hurdle is strict, as the threshold's is: a margin equal to the
/// least an edge pays narrows the band, one wei less keeps it.
#[test]
fn the_bands_margin_is_strict() {
    let (cycles, snap) = pair_and_cycles();
    for c in &cycles {
        for leg in 0..2 {
            let pair = pair(c, &snap);
            let rung = rungs(c, &pair, leg, &px(0), 3).pop().unwrap();
            let b = band(c, &pair, leg, &rung, &px(0), 5).unwrap();
            let pays = |edge: &PoolSnapshot| fixed(c, &pair, leg, &rung.trigger, edge, b.point.size, b.point.mid, &px(0)).unwrap().surplus;
            let least = pays(&b.edges[0]).min(pays(&b.edges[1])).as_u128();
            let kept = band(c, &pair, leg, &rung, &px(least - 1), 5).unwrap();
            assert_eq!(edge_ticks(&kept), edge_ticks(&b), "leg {leg}: one wei under the least edge keeps the band");
            let at = band(c, &pair, leg, &rung, &px(least), 5);
            assert!(at.is_none_or(|n| edge_ticks(&n) != edge_ticks(&b)), "leg {leg}: a margin equal to it does not");
        }
    }
}
