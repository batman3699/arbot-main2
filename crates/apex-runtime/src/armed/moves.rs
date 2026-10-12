//! How far each pool's price moves within an arming window (R26), learned
//! from the book as it runs: the ranking's `P(move ≥ m within W)`.
//!
//! A pool's position is its tick (1 tick ≈ 1 bp) or, for a constant-product
//! pool, its log-price in bps. Statistics only; nothing priced reads them.

use alloy_primitives::Address;
use std::collections::{BTreeMap, VecDeque};

/// A day of 10-second windows.
const KEEP: usize = 8_640;

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
    /// that way: (hits + prior) / (windows + 2). The prior is one pseudo-hit
    /// for a move no larger than the pool has been seen to make, and nothing
    /// beyond: a move never seen gets no credit, or rungs for 2% moves in ten
    /// seconds would fill every sender (the smoke run, 2026-10-12 UTC).
    pub fn p_at_least(&self, pool: Address, up: bool, step: u32) -> f64 {
        let Some(p) = self.pools.get(&pool) else { return 0.5 };
        let way = |(u, d): &(f64, f64)| if up { *u } else { *d };
        let step = f64::from(step);
        let hits = p.windows.iter().filter(|w| way(w) >= step).count();
        let seen = p.windows.iter().map(way).fold(way(&(p.max_up, p.max_down)), f64::max);
        let prior = if step <= seen { 1.0 } else { 0.0 };
        (hits as f64 + prior) / (p.windows.len() as f64 + 2.0)
    }
}
