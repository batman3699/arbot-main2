//! Where an armed backrun's trigger sits, and what it trades there (R26).
//!
//! A slot is a cycle and the pool whose move opens its gap. The move goes
//! against the cycle's own swap on that pool. Profit at the best size grows
//! with the move, so the smallest move that clears the margin is found by
//! doubling and then bisecting. Rungs sit a few steps past it (R36): additive,
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
/// R36: where ordinary moves land. R26's `0, 5, 20, 50, 100, 200` waited for
/// spikes: over 10.6 hours on 38 pools (2026-10-12/13) only 89 transactions
/// moved a price 20 ticks or more, and the canary period saw 8 fires. The
/// bots that take trigger-block arbitrage (R35) fire on ordinary moves, so
/// the rungs start at the threshold and double: a move between two still pays
/// the lower one.
pub const RUNG_OFFSETS: [u32; 6] = [0, 1, 2, 4, 8, 16];

#[derive(Clone, Copy, Debug)]
pub struct Pricing {
    pub costs: RouteCosts,
    pub gas: SettlementGas,
    /// What the lenders hold: the most any size may borrow, and from whom
    /// (R21, R27).
    pub lenders: Option<crate::live::lenders::Lenders>,
    /// What the surplus over base costs must exceed (`armed.margin_wei`);
    /// after [`Pricing::for_cycle`], in the cycle's start token.
    pub margin_wei: U256,
    /// R37: the book's USDC units a wei ([`start_per_eth`]); a USDC cycle is
    /// not priced without it.
    pub start_per_eth: Option<(U256, U256)>,
    /// R37: set by [`Pricing::for_cycle`] for a USDC cycle: its costs are
    /// converted at this rate.
    pub rate: Option<(U256, U256)>,
    /// R37: Morpho's USDC, a USDC cycle's lender.
    pub usdc_lenders: Option<crate::live::lenders::Lenders>,
    /// R37: PoolManager's WETH and USDC, a v4 route's lender.
    pub pool_manager: Option<(U256, U256)>,
}

impl Pricing {
    /// This pricing, for one cycle (R37). A USDC cycle is costed in USDC at
    /// the book's rate, its margin converted, Morpho's USDC its lender; a
    /// route with a v4 hop borrows from PoolManager, as `LeanExecutor2`
    /// does, up to its holding (the v4 hop's base gas carries the loan). None
    /// for a USDC cycle when the book has no rate.
    pub fn for_cycle(&self, cycle: &Cycle) -> Option<Self> {
        let usdc = cycle.start == apex_exec::lean2::USDC;
        let v4 = cycle.legs.iter().any(|l| l.venue == crate::live::inventory::Venue::UniswapV4);
        let rate = if usdc { Some(self.start_per_eth?) } else { None };
        let lenders = if v4 {
            self.pool_manager.map(|(weth, usdc_held)| crate::live::lenders::Lenders {
                balancer: U256::zero(),
                morpho: Some(if usdc { usdc_held } else { weth }),
            })
        } else if usdc {
            self.usdc_lenders
        } else {
            self.lenders
        };
        let margin_wei = rate.map_or(self.margin_wei, |r| to_start(self.margin_wei, r));
        Some(Self { rate, lenders, margin_wei, ..*self })
    }

    /// A cost or tip in wei, in the start token (rounded up).
    pub fn in_start(&self, wei: U256) -> U256 {
        self.rate.map_or(wei, |r| to_start(wei, r))
    }

    /// A surplus in the start token, in wei (rounded down): what a bid per
    /// gas is measured against.
    pub fn in_wei(&self, start: U256) -> U256 {
        self.rate.map_or(start, |r| to_wei(start, r))
    }
}

/// R37: USDC units a wei, as `(sqrtP², 2^192)` (or its inverse when USDC is
/// token0), from the WETH/USDC pool with the most liquidity the book holds.
/// None when it holds none.
pub fn start_per_eth(snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>) -> Option<(U256, U256)> {
    let usdc = apex_exec::lean2::USDC;
    let p = snapshot
        .values()
        .filter(|p| p.reserves.is_none() && p.spec.pair() == (crate::live::frontier::WETH.min(usdc), crate::live::frontier::WETH.max(usdc)))
        .max_by_key(|p| p.state.liquidity)?;
    // Exact: sqrtP² fits 256 bits for any price below 2^64 a unit, and a
    // rate that would not fit is no rate.
    let sq = U256::try_from(p.state.sqrt_price_x96.full_mul(p.state.sqrt_price_x96)).ok()?;
    let two_192 = U256::one() << 192;
    if p.spec.token0 == crate::live::frontier::WETH { Some((sq, two_192)) } else { Some((two_192, sq)) }
}

/// A wei amount in the start token, rounded up: a cost.
pub fn to_start(wei: U256, (num, den): (U256, U256)) -> U256 {
    let v = wei.full_mul(num);
    let d = ethers_core::types::U512::from(den);
    U256::try_from((v + d - 1) / d).unwrap_or(U256::MAX)
}

/// A start-token amount in wei, rounded down: a surplus.
pub fn to_wei(start: U256, (num, den): (U256, U256)) -> U256 {
    if num.is_zero() {
        return U256::zero();
    }
    U256::try_from(start.full_mul(den) / ethers_core::types::U512::from(num)).unwrap_or(U256::MAX)
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
    // R37: sized in the start token's own units (USDC's six decimals).
    let first = &pools[0];
    let decimals = if first.spec.token0 == cycle.start { first.decimals.0 } else { first.decimals.1 };
    let live = LiveCycle::new(cycle, &map_of(cycle, pools))?;
    let route = CostedCycle::new(live, px.costs, px.gas).lent(px.lenders).in_start(px.rate);
    let best = best_size(&route, SearchBudget::for_decimals(decimals)).ok()?;
    let Surplus::Gain(surplus) = best.net else { return None };
    if surplus <= px.margin_wei {
        return None;
    }
    let gas = route.gas_at(best.amount_in)?;
    let [mid, output] = route.cycle().hop_outputs(best.amount_in)?;
    let cost = px.in_start(U256::from(px.costs.with_gas(gas.expected)));
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

/// The rung's band when it must pay `tip_wei` on top of the margin (R36):
/// a fire anywhere inside it nets at least nothing after its own tip.
pub fn band_paying(
    cycle: &Cycle,
    pair: &[Arc<PoolSnapshot>; 2],
    leg: usize,
    rung: &Rung,
    px: &Pricing,
    cap_steps: u32,
    tip_wei: U256,
) -> Option<Band> {
    let paying = Pricing { margin_wei: px.margin_wei.saturating_add(tip_wei), ..*px };
    band(cycle, pair, leg, rung, &paying, cap_steps)
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
    let cost = px.in_start(U256::from(px.costs.with_gas(gas.expected)));
    let surplus = q.outputs[1].checked_sub(size)?.checked_sub(cost)?;
    (surplus > px.margin_wei).then_some(Point { size, mid, output: q.outputs[1], cost, surplus, gas })
}

// ------------------------------------------------- R40.1: sized arms

/// How far past the no-tip threshold tier 1's bid is priced: where ordinary
/// moves land (R36's rungs).
pub const TIER1_BID_STEPS: u32 = 4;
/// The bands the engine chooses among, in steps, up to `band_cap_bps`.
pub const BAND_WIDTHS: [u32; 3] = [5, 10, 20];
/// Past the threshold, where a slot's expected net is measured.
pub const NET_GRID: [u32; 6] = RUNG_OFFSETS;

/// A sized trade at one state: what it nets in wei after its gas at the base
/// fee plus its bid and the L1 and other costs, its gas, and its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetAt {
    pub net_wei: U256,
    pub gas: u64,
    pub size: U256,
}

/// The trade `LeanExecutor3` would make with the trigger `step` from where it
/// is and the other pool as `other`, bidding `bid_wei_per_gas` over the base
/// fee: the mirror's walk, quote and early exit. None when it would not trade,
/// or would net nothing.
#[allow(clippy::too_many_arguments)]
pub fn net_at(
    cycle: &Cycle,
    pair: &[Arc<PoolSnapshot>; 2],
    leg: usize,
    other: &PoolSnapshot,
    step: u32,
    px: &Pricing,
    terms: &super::plan::SizedTerms,
    bid_wei_per_gas: u128,
) -> Option<NetAt> {
    let trigger = pair[leg].moved(move_of(&pair[leg], trigger_up(cycle, leg), step))?;
    let book = map_of(cycle, with(pair, leg, &trigger, other));
    let price = U256::from(px.costs.wei_per_gas.saturating_add(bid_wei_per_gas));
    let super::plan::SizedOutcome::Trade { gross, walked, size, .. } = super::plan::sized_trade(cycle, &book, terms, price, &px.gas)
    else {
        return None;
    };
    let gas = terms.read_gas + u64::from(terms.swap_gas) + u64::from(terms.overhead) + walked.cross_gas;
    let cost = U256::from(gas).saturating_mul(price).saturating_add(U256::from(px.costs.other_wei));
    let net_wei = px.in_wei(gross).checked_sub(cost).filter(|n| !n.is_zero())?;
    Some(NetAt { net_wei, gas, size })
}

/// The other pool at its adverse edge: `band` steps the way that closes the
/// gap, against the cycle's swap there.
pub fn adverse_edge(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, band: u32) -> Option<PoolSnapshot> {
    let other = 1 - leg;
    pair[other].moved(move_of(&pair[other], !trigger_up(cycle, other), band))
}

/// The smallest step from `from` at which `pays` holds, `pays` growing with
/// the step: doubling, then bisecting. None when nothing up to [`MAX_STEP`]
/// does.
fn smallest(from: u32, pays: impl Fn(u32) -> bool) -> Option<u32> {
    let from = from.max(1);
    let mut hi = from;
    while !pays(hi) {
        if hi >= MAX_STEP {
            return None;
        }
        hi = hi.saturating_mul(2).min(MAX_STEP);
    }
    // Nothing pays below `from`, nor at half of `hi`: it doubled from a step
    // that did not, or was clamped above one.
    let mut lo = (from - 1).max(hi / 2);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if pays(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

/// One sized arm a slot can hold: its tier, its threshold and band, its bid,
/// and its net across [`NET_GRID`] past the threshold, in wei.
#[derive(Clone, Debug)]
pub struct SizedArmPoint {
    pub tier: u8,
    pub step: u32,
    /// The trigger pool at the threshold.
    pub trigger: PoolSnapshot,
    /// The other pool at its band's lower and upper edge, in price.
    pub edges: [PoolSnapshot; 2],
    pub bid: crate::armed::bid::ArmBid,
    pub gas: u64,
    pub grid: [(u32, f64); 6],
}

/// Tier 1 and, when it can pay, tier 2, for the slot (`cycle`, trigger
/// `leg`) with the other pool's band `band` steps wide. Empty when no move up
/// to [`MAX_STEP`] pays.
///
/// Tier 1 sits at the smallest move that pays its own bid with the other pool
/// at its adverse edge, so whenever its predicates hold, the trade pays. Its
/// bid is priced [`TIER1_BID_STEPS`] past the no-tip threshold, since a
/// threshold's own surplus is near zero. Tier 2 sits where the net reaches
/// `tier2_multiple` times that reference, and exists only when its bid is at
/// least twice tier 1's.
#[allow(clippy::too_many_arguments)]
pub fn sized_points(
    cycle: &Cycle,
    pair: &[Arc<PoolSnapshot>; 2],
    leg: usize,
    px: &Pricing,
    terms: &super::plan::SizedTerms,
    band: u32,
    tier2_multiple: u32,
    inputs: &crate::armed::engine::BidInputs,
    cap_wei_per_gas: u128,
) -> Vec<SizedArmPoint> {
    let other = 1 - leg;
    let (Some(adverse), Some(lo), Some(hi)) = (
        adverse_edge(cycle, pair, leg, band),
        pair[other].moved(move_of(&pair[other], false, band)),
        pair[other].moved(move_of(&pair[other], true, band)),
    ) else {
        return Vec::new();
    };
    let at = |o: &PoolSnapshot, step: u32, bid: u128| net_at(cycle, pair, leg, o, step, px, terms, bid);
    let Some(bare) = smallest(1, |s| at(&adverse, s, 0).is_some()) else { return Vec::new() };
    let Some(reference) = at(&pair[other], bare + TIER1_BID_STEPS, 0) else { return Vec::new() };
    let bid_for = |n: &NetAt| {
        // R41.2: against the winners of gaps the point's size.
        crate::armed::bid::arm_bid(inputs.winners_at(n.net_wei), inputs.sample, inputs.model, n.net_wei, n.gas, cap_wei_per_gas)
    };
    let point = |tier: u8, step: u32, bid: crate::armed::bid::ArmBid| -> Option<SizedArmPoint> {
        let trigger = pair[leg].moved(move_of(&pair[leg], trigger_up(cycle, leg), step))?;
        let mut grid = [(0u32, 0f64); 6];
        for (g, off) in grid.iter_mut().zip(NET_GRID) {
            let s = step.saturating_add(off).min(MAX_STEP);
            *g = (s, at(&pair[other], s, bid.sent.per_gas).map_or(0.0, |n| n.net_wei.as_u128() as f64));
        }
        let gas = at(&pair[other], step, bid.sent.per_gas).map_or(reference.gas, |n| n.gas);
        Some(SizedArmPoint { tier, step, trigger, edges: [lo.clone(), hi.clone()], bid, gas, grid })
    };
    let one_bid = bid_for(&reference);
    let Some(one) = smallest(bare, |s| at(&adverse, s, one_bid.sent.per_gas).is_some()) else { return Vec::new() };
    let Some(first) = point(1, one, one_bid) else { return Vec::new() };
    let mut out = vec![first];
    let target = reference.net_wei.saturating_mul(U256::from(tier2_multiple));
    let big = smallest(one, |s| at(&pair[other], s, one_bid.sent.per_gas).is_some_and(|n| n.net_wei >= target));
    if let Some(two_step) = big {
        if let Some(surplus) = at(&pair[other], two_step, 0) {
            let two_bid = bid_for(&surplus);
            let doubled = two_bid.sent.per_gas >= one_bid.sent.per_gas.saturating_mul(2) && two_bid.sent.per_gas > one_bid.sent.per_gas;
            let pays = at(&adverse, two_step, two_bid.sent.per_gas).is_some();
            if doubled && pays {
                out.extend(point(2, two_step, two_bid));
            }
        }
    }
    out
}

/// Σ over the grid of P(the move ends in that bucket) × the net there:
/// `p_at_least(step)` is the chance the move reaches at least `step`.
pub fn expected_net(grid: &[(u32, f64)], p_at_least: impl Fn(u32) -> f64) -> f64 {
    let p: Vec<f64> = grid.iter().map(|(s, _)| p_at_least(*s)).collect();
    grid.iter()
        .enumerate()
        .map(|(i, (_, net))| {
            let next = p.get(i + 1).copied().unwrap_or(0.0);
            (p[i] - next).max(0.0) * net
        })
        .sum()
}
