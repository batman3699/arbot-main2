//! P&L attribution (Blueprint §32, §26's Attribution family, §7.9).
//!
//! Realized profit, split by chain, strategy, venue, route and
//! [`OptimizationLayer`]. The whole difficulty is in one word of §26:
//! *incremental* P&L by layer.
//!
//! # Some dimensions partition a trade's profit and some do not
//!
//! A trade has exactly one chain and one strategy, so those columns **add up to
//! the total**: they partition it. A trade has several venues and several
//! optimization layers, so those columns **do not**. Adding the per-layer
//! column gives a number larger than the profit that was actually made, and
//! every trade touching two layers is counted twice in it.
//!
//! That is not a flaw to be corrected by splitting profit between layers.
//! Splitting requires a rule -- equally? by some weight? -- and every such rule
//! invents a fact. A trade that used a parallel split *and* a V4 route did not
//! earn 60% of its profit from one of them; it earned all of it from both, and
//! the honest report says so.
//!
//! So the two kinds are different types. [`Partition`] can be summed and its
//! total checked against the trades. [`Overlapping`] cannot: it reports
//! [`Overlapping::double_counted`] alongside its rows, and a caller that adds
//! the column anyway has been told in the type what it is holding.
//!
//! # Where a real incremental number exists, it is measured
//!
//! §26's Optimization family records `single_route_ev` **and**
//! `split_route_ev` for the same opportunity. Their difference is what the
//! split layer actually contributed, against a counterfactual the system
//! already computed rather than one this module invents. [`Counterfactual`]
//! carries it, and `incremental` is `None` when nobody supplied one -- absent,
//! not zero, because "this layer contributed nothing" and "nobody measured what
//! this layer contributed" are different claims.

use apex_types::ids::{ChainId, StrategyId, VenueId};
use apex_types::pnl::{OptimizationLayer, PnlAttribution};
use alloy_primitives::B256;
use std::collections::BTreeMap;

/// What an optimization layer would have earned without it, from a baseline
/// the system computed at decision time (§26: `single_route_ev` against
/// `split_route_ev`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counterfactual {
    pub layer: OptimizationLayer,
    /// Net profit the same opportunity was priced at without this layer.
    pub without_layer: i128,
}

/// A dimension where each trade lands in exactly one bucket. The rows sum to
/// the total, and [`Partition::sums_to`] checks that rather than assuming it.
#[derive(Clone, Debug, PartialEq)]
pub struct Partition<K: Ord> {
    rows: BTreeMap<K, Row>,
}

// Hand-written: `#[derive(Default)]` would add a `K: Default` bound, and a map
// key has no business needing one.
impl<K: Ord> Default for Partition<K> {
    fn default() -> Self {
        Self { rows: BTreeMap::new() }
    }
}

/// A dimension where one trade lands in several buckets. The rows **do not**
/// sum to the total, and the type will not let a reader forget it.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlapping<K: Ord> {
    rows: BTreeMap<K, Row>,
    total_net: i128,
    total_trades: usize,
}

impl<K: Ord> Default for Overlapping<K> {
    fn default() -> Self {
        Self { rows: BTreeMap::new(), total_net: 0, total_trades: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Row {
    pub trades: usize,
    pub gross_profit: i128,
    pub net_profit: i128,
    /// Measured against a supplied counterfactual. `None` means nobody
    /// supplied one -- which is not the same as a layer that contributed
    /// nothing, and the two must not print the same.
    pub incremental: Option<i128>,
}

impl<K: Ord + Copy> Partition<K> {
    pub fn rows(&self) -> &BTreeMap<K, Row> {
        &self.rows
    }
    pub fn get(&self, k: K) -> Option<Row> {
        self.rows.get(&k).copied()
    }
    pub fn total(&self) -> i128 {
        self.rows.values().map(|r| r.net_profit).fold(0i128, i128::saturating_add)
    }
    /// A partition's rows must reconstruct the total. Worth asserting rather
    /// than trusting: a trade that acquired two chains would break it silently.
    pub fn sums_to(&self, total_net: i128) -> bool {
        self.total() == total_net
    }
}

impl<K: Ord + Copy> Overlapping<K> {
    pub fn rows(&self) -> &BTreeMap<K, Row> {
        &self.rows
    }
    pub fn get(&self, k: K) -> Option<Row> {
        self.rows.get(&k).copied()
    }

    /// The real figure, from the trades rather than from these rows.
    pub const fn total_net(&self) -> i128 {
        self.total_net
    }

    /// What adding the column gives. Deliberately **not** called `total`: it is
    /// larger than the profit that was made whenever any trade touched two
    /// buckets, and naming it `total` is how it ends up on a dashboard beside
    /// one.
    pub fn sum_of_rows(&self) -> i128 {
        self.rows.values().map(|r| r.net_profit).fold(0i128, i128::saturating_add)
    }

    /// How much profit this column counts more than once. Zero only when every
    /// trade touched exactly one bucket.
    pub fn double_counted(&self) -> i128 {
        self.sum_of_rows().saturating_sub(self.total_net)
    }

    /// Whether these rows happen to partition after all -- true when no trade
    /// touched more than one bucket. Checked rather than assumed, because a
    /// single-venue day makes the venue column look like a partition and the
    /// next day it is not.
    pub fn happens_to_partition(&self) -> bool {
        self.double_counted() == 0
    }
}

/// Realized P&L, split every way §26 asks for.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AttributionReport {
    pub trades: usize,
    pub total_gross: i128,
    pub total_net: i128,
    pub by_chain: Partition<ChainId>,
    pub by_strategy: Partition<StrategyId>,
    pub by_route: Partition<B256>,
    pub by_venue: Overlapping<VenueId>,
    pub by_layer: Overlapping<OptimizationLayer>,
}

#[derive(Debug, Default)]
pub struct PnlLedger {
    trades: Vec<PnlAttribution>,
    counterfactuals: Vec<(B256, Counterfactual)>,
}

impl PnlLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, trade: PnlAttribution) {
        self.trades.push(trade);
    }

    /// Supply what a layer's absence would have been worth on one route, from
    /// a baseline the system computed at decision time.
    pub fn record_counterfactual(&mut self, route: B256, cf: Counterfactual) {
        self.counterfactuals.push((route, cf));
    }

    pub fn trades(&self) -> &[PnlAttribution] {
        &self.trades
    }

    pub fn attribute(&self) -> AttributionReport {
        let mut report = AttributionReport {
            trades: self.trades.len(),
            ..Default::default()
        };

        for t in &self.trades {
            report.total_gross = report.total_gross.saturating_add(t.gross_profit);
            report.total_net = report.total_net.saturating_add(t.net_profit_token);

            add(&mut report.by_chain.rows, t.chain, t);
            add(&mut report.by_strategy.rows, t.strategy, t);
            add(&mut report.by_route.rows, t.route_hash, t);

            // A venue listed twice on one trade is one venue, not two: the
            // trade touched it, and touching it twice does not double the
            // profit attributable to it.
            let mut venues = t.venues.clone();
            venues.sort_unstable();
            venues.dedup();
            for v in venues {
                add(&mut report.by_venue.rows, v, t);
            }

            let mut layers = t.optimization_layers.clone();
            layers.sort_unstable();
            layers.dedup();
            for l in layers {
                add(&mut report.by_layer.rows, l, t);
                // Incremental, where a counterfactual for this route and layer
                // was supplied.
                if let Some((_, cf)) = self
                    .counterfactuals
                    .iter()
                    .find(|(r, c)| *r == t.route_hash && c.layer == l)
                {
                    let row = report.by_layer.rows.entry(l).or_default();
                    let delta = t.net_profit_token.saturating_sub(cf.without_layer);
                    row.incremental =
                        Some(row.incremental.unwrap_or(0).saturating_add(delta));
                }
            }
        }

        report.by_venue.total_net = report.total_net;
        report.by_venue.total_trades = report.trades;
        report.by_layer.total_net = report.total_net;
        report.by_layer.total_trades = report.trades;
        report
    }
}

fn add<K: Ord>(rows: &mut BTreeMap<K, Row>, key: K, t: &PnlAttribution) {
    let row = rows.entry(key).or_default();
    row.trades += 1;
    row.gross_profit = row.gross_profit.saturating_add(t.gross_profit);
    row.net_profit = row.net_profit.saturating_add(t.net_profit_token);
}
