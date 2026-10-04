//! How close each priced route came to paying (Task 8.5 R14).
//!
//! # A count of refusals says nothing about the distance
//!
//! The shadow run reported "priced, no profitable size" as a count, so after
//! ~11,000 priced events it could say none paid and not how far any came. The
//! atomic census (R13) settled the sealed-block question — the best route after
//! a large swap nets −1.07 bps — and left the sub-block window, which only the
//! shadow's preconfirmed book can see. Whether that window is worth chasing
//! with speed depends on exactly the figure the count threw away: a route
//! fractions of a basis point from paying is a latency problem, one several
//! basis points away is not a problem speed can solve.
//!
//! # The census's measure
//!
//! A route's near miss is its best net over [`LADDER_WEI`] — each size at its
//! own cost, gas included — as a share of the size, so the figure is directly
//! comparable to R13's. Not the net at the size Engine C's search settles on:
//! for a route with no gap that is its smallest size, where the fixed cost alone
//! reads as thousands of basis points and says nothing about the price.

use serde::Serialize;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// The sizes a near miss is the best of, in wei of WETH: 0.01 to 10, from
/// where the fixed cost stops dominating to where price impact starts to. A
/// size past what the route's paying pool holds is left out.
pub const LADDER_WEI: [u128; 7] = [
    10_000_000_000_000_000,
    30_000_000_000_000_000,
    100_000_000_000_000_000,
    300_000_000_000_000_000,
    1_000_000_000_000_000_000,
    3_000_000_000_000_000_000,
    10_000_000_000_000_000_000,
];

/// Every route's near miss, counted by how far it was from paying.
///
/// Atomic, because the search prices from more than one worker and the report
/// reads concurrently; nothing here is read back on the capture path.
#[derive(Debug)]
pub struct NearMisses {
    measured: AtomicU64,
    /// Pays, then within 0.5, 1, 2, 5 and 10 bps, then beyond.
    bands: [AtomicU64; 7],
    /// The closest yet, in hundredths of a basis point; `i64::MIN` before any.
    best: AtomicI64,
}

impl Default for NearMisses {
    fn default() -> Self {
        Self { measured: AtomicU64::new(0), bands: Default::default(), best: AtomicI64::new(i64::MIN) }
    }
}

/// Upper edges of the bands below "pays", in hundredths of a basis point: a
/// near miss above −50 is within half a basis point, and so on.
const EDGES: [i64; 5] = [-50, -100, -200, -500, -1_000];

impl NearMisses {
    /// One priced route's near miss, in hundredths of a basis point of size.
    pub fn record(&self, centi_bps: i64) {
        let band = if centi_bps >= 0 {
            0
        } else {
            1 + EDGES.iter().take_while(|edge| centi_bps <= **edge).count()
        };
        self.bands[band].fetch_add(1, Ordering::Relaxed);
        self.measured.fetch_add(1, Ordering::Relaxed);
        self.best.fetch_max(centi_bps, Ordering::Relaxed);
    }

    pub fn report(&self) -> NearMissReport {
        let band = |i: usize| self.bands[i].load(Ordering::Relaxed);
        let best = self.best.load(Ordering::Relaxed);
        NearMissReport {
            measured: self.measured.load(Ordering::Relaxed),
            #[allow(clippy::cast_precision_loss)]
            best_bps: (best != i64::MIN).then(|| best as f64 / 100.0),
            pays: band(0),
            within_0_5: band(1),
            within_1: band(2),
            within_2: band(3),
            within_5: band(4),
            within_10: band(5),
            beyond_10: band(6),
        }
    }
}

/// How close the priced routes came to paying. Each band holds the routes
/// short of paying by at most its figure and more than the band before.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NearMissReport {
    pub measured: u64,
    /// The closest any route came, in basis points: `None` before the first.
    pub best_bps: Option<f64>,
    pub pays: u64,
    pub within_0_5: u64,
    pub within_1: u64,
    pub within_2: u64,
    pub within_5: u64,
    pub within_10: u64,
    pub beyond_10: u64,
}
