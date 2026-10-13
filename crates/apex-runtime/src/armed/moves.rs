//! How far each pool's price moves within an arming window (R26), learned
//! from the book as it runs: the ranking's `P(move ≥ m within W)`.
//!
//! A pool's position is its tick (1 tick ≈ 1 bp) or, for a constant-product
//! pool, its log-price in bps. Statistics only; nothing priced reads them.

use alloy_primitives::Address;
use std::collections::{BTreeMap, VecDeque};

/// A day of 10-second windows.
const KEEP: usize = 8_640;

/// How fast large moves thin out: P(move >= m) falls as m^-2.87, the log-log
/// slope for m from 10 to 100 ticks over 33,917 five-block windows on 38 pools
/// (2026-10-12/13). Used only past the largest move a pool has been seen to make.
pub const TAIL_EXPONENT: f64 = 2.87;

#[derive(Debug, Default)]
struct PoolMoves {
    window_start_ns: u64,
    start: f64,
    max_up: f64,
    max_down: f64,
    windows: VecDeque<(f64, f64)>,
}

#[derive(Debug)]
pub struct MoveStats {
    window_ns: u64,
    pools: BTreeMap<Address, PoolMoves>,
}

impl MoveStats {
    pub fn new(window_ns: u64) -> Self {
        Self { window_ns: window_ns.max(1), pools: BTreeMap::new() }
    }

    pub fn observe(&mut self, pool: Address, position: f64, now_ns: u64) {
        let w = self.window_ns;
        let p = self
            .pools
            .entry(pool)
            .or_insert_with(|| PoolMoves { window_start_ns: now_ns, start: position, ..Default::default() });
        if now_ns >= p.window_start_ns.saturating_add(w) {
            p.windows.push_back((p.max_up, p.max_down));
            if p.windows.len() > KEEP {
                p.windows.pop_front();
            }
            let windows = std::mem::take(&mut p.windows);
            *p = PoolMoves { window_start_ns: now_ns, start: position, windows, ..Default::default() };
        }
        p.max_up = p.max_up.max(position - p.start);
        p.max_down = p.max_down.max(p.start - position);
    }

    /// The share of closed windows in which `pool` moved at least `step`
    /// that way: (hits + 1) / (windows + 2), for a move no larger than the
    /// pool has been seen to make. Past that, the share at the largest seen,
    /// thinned by the measured tail ([`TAIL_EXPONENT`]): a uniform prior gave
    /// rungs for 2% moves in ten seconds every sender (the smoke run,
    /// 2026-10-12 UTC), and none at all left spike rungs without one (R26.3).
    /// A pool seen not to move that way gets nothing.
    pub fn p_at_least(&self, pool: Address, up: bool, step: u32) -> f64 {
        let Some(p) = self.pools.get(&pool) else { return 0.5 };
        let way = |(u, d): &(f64, f64)| if up { *u } else { *d };
        let n = p.windows.len() as f64 + 2.0;
        let share = |m: f64| (p.windows.iter().filter(|w| way(w) >= m).count() as f64 + 1.0) / n;
        let step = f64::from(step);
        let seen = p.windows.iter().map(way).fold(way(&(p.max_up, p.max_down)), f64::max);
        // The two agree at `seen`, and a pool seen not to move gets 0^k = 0.
        if step <= seen {
            share(step)
        } else {
            share(seen) * (seen / step).powf(TAIL_EXPONENT)
        }
    }
}
