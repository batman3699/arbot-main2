//! The coverage auditor (Blueprint §2.7, §28). **INV-41.**
//!
//! > A fast path can only capture what it discovers.
//!
//! The auditor runs a delayed, broader, more expensive search on **reserved
//! slow-path resources** and compares its best executable candidates against
//! what the hot path produced. §28 is explicit about the goal, and it is a
//! modest one: *"not to pretend the auditor is omniscient — it is to prevent
//! the engine from silently losing known opportunity classes."*
//!
//! That sentence decides the whole design. An auditor built to be an authority
//! would report a number and invite trust in it. This one is built to be
//! **falsifiable**, and three things follow:
//!
//! 1. **Recall is [`Recall::Undefined`] when the oracle found nothing**, never
//!    `1.0`. "We both found nothing" and "we found everything there was" are
//!    the same arithmetic and opposite facts, and the first is what a broken
//!    oracle produces on every window. A gauge that reads perfect while the
//!    oracle is dead is worse than no gauge.
//!
//! 2. **An oracle that misses what the fast path found is not an oracle.**
//!    [`CoverageReport::oracle_gaps`] reports those, and their presence means
//!    the recall figure beside them is measuring an incomplete reference. §28
//!    never claims the auditor is complete; this is where that honesty is
//!    mechanical rather than stated.
//!
//! 3. **The oracle's results arrive as data.** This module cannot call a
//!    search: it takes two lists and compares them. That is what keeps
//!    `apex-obs` free of any dependency on the search crates, and it is the
//!    structural form of §2.7's "**independent** broad-search oracle" -- an
//!    auditor sharing code with the thing it audits would hide a bug in both.
//!
//! # Misses are weighted by EV, not counted
//!
//! Same reasoning as the miss ledger: fifty missed dust opportunities matter
//! less than one missed large one, and a count-based rate says the opposite.
//! `high_EV_miss_rate` is the share of *value* the fast path did not see.

use apex_types::time::{DurationNanos, UnixNanos};
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What makes two discoveries the same opportunity. The route hash: §25's
/// dedup key, so the two planes agree on identity without agreeing on how they
/// found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OpportunityKey(pub B256);

/// §28 tracks `route_class_miss_rate` and `venue_class_miss_rate` separately,
/// because "we are blind to 4-hop routes" and "we are blind to Balancer" are
/// different diagnoses with different fixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RouteClass {
    pub hops: u8,
    /// A venue family, not a pool. Blindness is rarely to one pool.
    pub venue_family: u16,
}

/// One opportunity, as either plane reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovery {
    pub key: OpportunityKey,
    /// Signed: an oracle may surface something that was not worth taking, and
    /// that is still a discovery the fast path either saw or did not.
    pub ev: i128,
    pub class: RouteClass,
}

/// A rate that may not exist.
///
/// The variant that matters is `Undefined`. Every arithmetic definition of
/// recall divides by what the oracle found, and an oracle that found nothing
/// makes the quotient `0/0` — which `1.0` is the tempting answer to and the
/// wrong one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Recall {
    /// The oracle found nothing over this window. Not a perfect score: no
    /// score. A window where the oracle is broken looks exactly like this.
    Undefined,
    Measured(f64),
}

impl Recall {
    /// The figure, or `None`. Deliberately not `unwrap_or(1.0)` anywhere -- a
    /// caller that wants a number has to decide what an absent one means.
    pub const fn value(self) -> Option<f64> {
        match self {
            Self::Undefined => None,
            Self::Measured(v) => Some(v),
        }
    }

    /// Whether this clears a floor. `Undefined` **does not**: acceptance
    /// criterion 2 is `hot_path_recall >= 0.95`, and a window with no
    /// measurement has not met it.
    pub fn meets(self, floor: f64) -> bool {
        matches!(self, Self::Measured(v) if v >= floor)
    }
}

/// §28's automatic response ladder, in order. The derived `Ord` is the
/// severity, so taking a max over several windows gives the strongest response
/// any of them called for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CoverageResponse {
    /// Recall is within tolerance.
    None,
    /// 1. Expand route templates in the frontier.
    ExpandTemplates,
    /// 2. Raise slow-path resource allocation.
    RaiseSlowPathBudget,
    /// 3. Disable the affected strategy class.
    DisableClass(RouteClass),
}

/// One window, as both planes saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditWindow {
    pub block: u64,
    /// When the fast path saw this window.
    pub observed_at: UnixNanos,
    /// When the audit finished. The difference is `coverage_audit_lag`, and it
    /// is reported because a recall figure from a stale window says less about
    /// the system running now.
    pub audited_at: UnixNanos,
    pub fast_path: Vec<Discovery>,
    pub oracle: Vec<Discovery>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoverageReport {
    pub block: u64,
    pub recall: Recall,
    /// How many distinct opportunities the oracle found. Reported rather than
    /// implied, for two reasons: pooling across windows needs the denominator
    /// as an integer, and a recall of 1.0 over one opportunity is a different
    /// statement from the same figure over a hundred.
    pub oracle_count: usize,
    /// EV-weighted: the share of discoverable *value* the fast path did not
    /// see. Fifty dust misses matter less than one large one.
    pub high_ev_miss_rate: Recall,
    pub by_route_class: BTreeMap<RouteClass, Recall>,
    pub lag: DurationNanos,
    /// What the oracle found and the fast path did not. The point of the whole
    /// exercise.
    pub missed: Vec<Discovery>,
    /// **What the fast path found and the oracle did not.**
    ///
    /// Not a success. It means the reference is incomplete, so every figure
    /// above it is measured against something that is not a superset -- and a
    /// recall of 1.0 computed from a gappy oracle is the most misleading
    /// number this module can produce.
    pub oracle_gaps: Vec<Discovery>,
}

impl CoverageReport {
    /// Whether the numbers above can be trusted as a recall measurement.
    ///
    /// False when the oracle turned out not to be a superset of the fast path.
    /// Reported separately from the figures rather than folded into them,
    /// because a caller charting `hot_path_recall` needs to know the series
    /// has a hole in it, not to receive a silently adjusted value.
    pub fn oracle_was_complete(&self) -> bool {
        self.oracle_gaps.is_empty()
    }

    /// §28's ladder. `floor` is acceptance criterion 2's 0.95.
    ///
    /// An `Undefined` recall produces no response: there is nothing to respond
    /// to, and escalating on an absent measurement would make a dead oracle
    /// look like a discovery problem.
    pub fn response(&self, floor: f64) -> CoverageResponse {
        let Some(recall) = self.recall.value() else { return CoverageResponse::None };
        if recall >= floor {
            return CoverageResponse::None;
        }
        // A class that is entirely invisible is a different problem from one
        // that is patchy: no template expansion finds what the frontier cannot
        // represent, so it goes straight to the end of the ladder.
        for (class, r) in &self.by_route_class {
            if matches!(r, Recall::Measured(v) if *v == 0.0) {
                return CoverageResponse::DisableClass(*class);
            }
        }
        // Wide but not total: more search, then more budget for it.
        if recall < floor / 2.0 {
            CoverageResponse::RaiseSlowPathBudget
        } else {
            CoverageResponse::ExpandTemplates
        }
    }
}

#[derive(Debug, Default)]
pub struct CoverageAuditor {
    reports: Vec<CoverageReport>,
}

impl CoverageAuditor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compare one window's two lists.
    pub fn audit(&mut self, window: &AuditWindow) -> CoverageReport {
        let fast: BTreeSet<OpportunityKey> = window.fast_path.iter().map(|d| d.key).collect();
        let oracle_keys: BTreeSet<OpportunityKey> = window.oracle.iter().map(|d| d.key).collect();

        let missed: Vec<Discovery> =
            window.oracle.iter().filter(|d| !fast.contains(&d.key)).copied().collect();
        let oracle_gaps: Vec<Discovery> =
            window.fast_path.iter().filter(|d| !oracle_keys.contains(&d.key)).copied().collect();

        let recall = ratio(oracle_keys.len() - missed.len(), oracle_keys.len());

        // EV-weighted, over positive EV only: an oracle finding a losing
        // opportunity the fast path skipped is the fast path working.
        let value = |ds: &[Discovery]| -> i128 {
            ds.iter().map(|d| d.ev.max(0)).fold(0i128, i128::saturating_add)
        };
        let oracle_value = value(&window.oracle);
        let missed_value = value(&missed);
        let high_ev_miss_rate = if oracle_value == 0 {
            Recall::Undefined
        } else {
            Recall::Measured(missed_value as f64 / oracle_value as f64)
        };

        let mut per_class: BTreeMap<RouteClass, (usize, usize)> = BTreeMap::new();
        for d in &window.oracle {
            per_class.entry(d.class).or_insert((0, 0)).1 += 1;
            if fast.contains(&d.key) {
                per_class.entry(d.class).or_insert((0, 0)).0 += 1;
            }
        }
        let by_route_class =
            per_class.into_iter().map(|(c, (hit, total))| (c, ratio(hit, total))).collect();

        let report = CoverageReport {
            block: window.block,
            recall,
            oracle_count: oracle_keys.len(),
            high_ev_miss_rate,
            by_route_class,
            lag: DurationNanos(window.audited_at.0.saturating_sub(window.observed_at.0)),
            missed,
            oracle_gaps,
        };
        self.reports.push(report.clone());
        report
    }

    pub fn reports(&self) -> &[CoverageReport] {
        &self.reports
    }

    /// Recall across every window audited so far, pooled rather than averaged.
    ///
    /// Averaging per-window rates weights a window with one opportunity the
    /// same as one with a hundred, which is how a quiet block full of nothing
    /// drags a real measurement around.
    pub fn pooled_recall(&self) -> Recall {
        let mut hit = 0usize;
        let mut total = 0usize;
        for r in &self.reports {
            // Integer arithmetic over the reported counts. The first version
            // reconstructed each window's denominator from its rate and miss
            // count -- `missed / (1 - recall)` -- which is exactly zero at
            // `recall == 1.0` and silently dropped every window the fast path
            // covered completely. Deriving a count from a rounded float was
            // the mistake; the count is now carried.
            total += r.oracle_count;
            hit += r.oracle_count.saturating_sub(r.missed.len());
        }
        ratio(hit, total)
    }

    /// Windows whose oracle was not a superset of the fast path. A non-empty
    /// answer means the pooled figure above is measured against an incomplete
    /// reference.
    pub fn windows_with_an_incomplete_oracle(&self) -> Vec<u64> {
        self.reports.iter().filter(|r| !r.oracle_was_complete()).map(|r| r.block).collect()
    }
}

/// `hit / total`, or `Undefined` when there was nothing to recall.
fn ratio(hit: usize, total: usize) -> Recall {
    if total == 0 {
        Recall::Undefined
    } else {
        Recall::Measured(hit as f64 / total as f64)
    }
}
