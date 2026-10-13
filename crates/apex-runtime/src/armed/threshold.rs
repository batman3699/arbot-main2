//! Where an armed backrun's trigger sits, and what it trades there (R26).
//!
//! A slot is a cycle and the pool whose move opens its gap. The move goes
//! against the cycle's own swap on that pool. Profit at the best size grows
//! with the move, so the smallest move that clears the margin is found by
//! doubling and then bisecting. Rungs sit 5 and 20 steps past it: additive,
//! because a threshold that is large only because the pools already sit
//! apart says nothing about how far triggers move (the smoke run's 62-tick
//! threshold put a multiplied rung 3 at 248).

use crate::econ::{GasEstimate, RouteCosts, SettledRoute};
use crate::live::book::{Move, PoolSnapshot};
use crate::live::frontier::Cycle;
use crate::live::gas::SettlementGas;
use crate::live::pricing::{CostedCycle, LiveCycle};
use alloy_primitives::Address;
use apex_math::finite_size::{best_size, SearchBudget, Surplus};
use ethers_core::types::U256;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The furthest a trigger is searched: 2,000 ticks (about 20% of price) or
/// 2,000 bps.
pub const MAX_STEP: u32 = 2_000;

/// Each rung's move past the threshold, in ticks or bps.
///
/// Spaced about two apart from +20, so a move between two rungs still pays the
/// lower one at least ~75% of its best. The larger rungs are for spikes (R26.3):
/// over 10.6 hours on 38 pools (2026-10-12/13), 89 single transactions moved a
/// price 20 ticks or more, 9 moved it 50, and 2 moved it 200. The 21:00 UTC
/// spike, $80 of gaps the reactive lane saw, was a 43-tick jump that the rungs
/// up to +20 could take only a corner of.
pub const RUNG_OFFSETS: [u32; 6] = [0, 5, 20, 50, 100, 200];

#[derive(Clone, Copy, Debug)]
pub struct Pricing {
    pub costs: RouteCosts,
    pub gas: SettlementGas,
    /// What the lenders hold: the most any size may borrow, and from whom
    /// (R21, R27).
    pub lenders: Option<crate::live::lenders::Lenders>,
    /// What the surplus over base costs must exceed (`armed.margin_wei`).
    pub margin_wei: U256,
}

/// The best size at one state, and what it would settle for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub size: U256,
    /// Hop 1's output at `size`: hop 2's fixed input.
    pub mid: U256,
    pub output: U256,
    /// Base gas and the L1 fee at `size`.
    pub cost: U256,
    /// `output − size − cost`.
    pub surplus: U256,
    pub gas: GasEstimate,
}

#[derive(Clone, Debug)]
pub struct Rung {
    /// 1 to 6: the threshold, and [`RUNG_OFFSETS`] past it.
    pub index: u8,
    /// The move, in ticks or bps.
    pub step: u32,
    pub point: Point,
    /// The trigger pool at this rung's threshold.
    pub trigger: PoolSnapshot,
}

/// The trigger moves against the cycle's own swap on its pool: up when the
/// cycle sells token0 there.
pub fn trigger_up(cycle: &Cycle, leg: usize) -> bool {
    cycle.legs[leg].zero_for_one
}

/// `step` ticks (or bps) from `p`'s price, up or down.
pub fn move_of(p: &PoolSnapshot, up: bool, step: u32) -> Move {
    if p.reserves.is_some() {
        Move::Bps { up, bps: step }
    } else {
        let s = i32::try_from(step).unwrap_or(i32::MAX);
        Move::Tick(if up { p.state.tick.saturating_add(s) } else { p.state.tick.saturating_sub(s) })
    }
}

fn map_of(cycle: &Cycle, pools: [Arc<PoolSnapshot>; 2]) -> BTreeMap<Address, Arc<PoolSnapshot>> {
    cycle.legs.iter().map(|l| l.pool).zip(pools).collect()
}

/// The best size with the trigger leg's pool replaced by `trigger`, if its
/// surplus exceeds the margin.
pub fn best_at(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, trigger: PoolSnapshot, px: &Pricing) -> Option<Point> {
    let mut pools = pair.clone();
    pools[leg] = Arc::new(trigger);
    let live = LiveCycle::new(cycle, &map_of(cycle, pools))?;
    let route = CostedCycle::new(live, px.costs, px.gas).lent(px.lenders);
    let best = best_size(&route, SearchBudget::default()).ok()?;
    let Surplus::Gain(surplus) = best.net else { return None };
    if surplus <= px.margin_wei {
        return None;
    }
    let gas = route.gas_at(best.amount_in)?;
    let [mid, output] = route.cycle().hop_outputs(best.amount_in)?;
    let cost = U256::from(px.costs.with_gas(gas.expected));
    Some(Point { size: best.amount_in, mid, output, cost, surplus, gas })
}

fn at(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, px: &Pricing, step: u32) -> Option<(Point, PoolSnapshot)> {
    let moved = pair[leg].moved(move_of(&pair[leg], trigger_up(cycle, leg), step))?;
    Some((best_at(cycle, pair, leg, moved.clone(), px)?, moved))
}

/// The threshold and up to `count` rungs past it ([`RUNG_OFFSETS`]). Empty
/// when no move within [`MAX_STEP`] clears the margin.
pub fn rungs(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, px: &Pricing, count: u8) -> Vec<Rung> {
    let mut hi = 1u32;
    let mut found = None;
    while hi <= MAX_STEP {
        if let Some(x) = at(cycle, pair, leg, px, hi) {
            found = Some(x);
            break;
        }
        hi = hi.saturating_mul(2);
    }
    let Some((mut point, mut trigger)) = found else { return Vec::new() };
    let mut lo = hi / 2;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match at(cycle, pair, leg, px, mid) {
            Some((p, t)) => (hi, point, trigger) = (mid, p, t),
            None => lo = mid,
        }
    }
    let mut out = vec![Rung { index: 1, step: hi, point, trigger }];
    for index in 2..=count.min(RUNG_OFFSETS.len() as u8) {
        let step = hi.saturating_add(RUNG_OFFSETS[usize::from(index - 1)]);
        let Some((point, trigger)) = (step <= MAX_STEP).then(|| at(cycle, pair, leg, px, step)).flatten() else { break };
        out.push(Rung { index, step, point, trigger });
    }
    out
}

/// The other pool's band, and the plan that pays across it.
#[derive(Clone, Debug)]
pub struct Band {
    /// The other pool at its lower and upper edge, in price.
    pub edges: [PoolSnapshot; 2],
    /// The rung's size with the plan's `mid`, as expected with the trigger at
    /// its threshold and the other pool where it is now.
    pub point: Point,
}

fn with(pair: &[Arc<PoolSnapshot>; 2], leg: usize, trigger: &PoolSnapshot, other: &PoolSnapshot) -> [Arc<PoolSnapshot>; 2] {
    let mut pools = pair.clone();
    pools[leg] = Arc::new(trigger.clone());
    pools[1 - leg] = Arc::new(other.clone());
    pools
}

/// The other pool's band: the widest of 5, 2 and 1 steps (at most
/// `cap_steps`) at which the rung's fixed size, with the trigger at its
/// threshold, still clears the margin at both edges.
///
/// With the trigger on hop 2's pool, the band is on hop 1's, and hop 1
/// returns less at its adverse edge. The plan's `mid` (hop 2's fixed input)
/// is then the least hop 1 returns across the band, so hop 1 always covers
/// it while the band's predicates hold; otherwise every adverse edge would
/// revert. With the trigger on hop 1's pool, `mid` is the rung's.
pub fn band(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, rung: &Rung, px: &Pricing, cap_steps: u32) -> Option<Band> {
    let other = 1 - leg;
    let size = rung.point.size;
    for j in [5u32, 2, 1].into_iter().filter(|j| *j <= cap_steps) {
        let (Some(lo), Some(hi)) =
            (pair[other].moved(move_of(&pair[other], false, j)), pair[other].moved(move_of(&pair[other], true, j)))
        else {
            continue;
        };
        let mid = if other == 0 {
            let first = |edge: &PoolSnapshot| {
                LiveCycle::new(cycle, &map_of(cycle, with(pair, leg, &rung.trigger, edge)))?.hop_outputs(size).map(|[m, _]| m)
            };
            let (Some(a), Some(b)) = (first(&lo), first(&hi)) else { continue };
            a.min(b).min(rung.point.mid)
        } else {
            rung.point.mid
        };
        if [&lo, &hi].iter().all(|edge| fixed(cycle, pair, leg, &rung.trigger, edge, size, mid, px).is_some()) {
            let point = fixed(cycle, pair, leg, &rung.trigger, &pair[other], size, mid, px)?;
            return Some(Band { edges: [lo, hi], point });
        }
    }
    None
}

/// The plan at `size` and `mid` with the trigger and the other pool as given,
/// if it clears the margin: strictly, as `best_at` requires.
#[allow(clippy::too_many_arguments)]
pub fn fixed(
    cycle: &Cycle,
    pair: &[Arc<PoolSnapshot>; 2],
    leg: usize,
    trigger: &PoolSnapshot,
    other: &PoolSnapshot,
    size: U256,
    mid: U256,
    px: &Pricing,
) -> Option<Point> {
    let live = LiveCycle::new(cycle, &map_of(cycle, with(pair, leg, trigger, other)))?;
    let q = live.quote_fixed(size, mid)?;
    let gas = px.gas.estimate(&q.hops);
    let cost = U256::from(px.costs.with_gas(gas.expected));
    let surplus = q.outputs[1].checked_sub(size)?.checked_sub(cost)?;
    (surplus > px.margin_wei).then_some(Point { size, mid, output: q.outputs[1], cost, surplus, gas })
}
