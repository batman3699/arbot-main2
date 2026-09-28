//! Task 8.2 — **INV-41**, `obs::auditor_detects_injected_miss`. §2.7, §28.

use alloy_primitives::B256;
use apex_obs::coverage::{
    AuditWindow, CoverageAuditor, CoverageResponse, Discovery, OpportunityKey, Recall, RouteClass,
};
use apex_types::time::{DurationNanos, UnixNanos};

const T0: UnixNanos = UnixNanos(1_700_000_000_000_000_000);

fn class(hops: u8) -> RouteClass {
    RouteClass { hops, venue_family: 1 }
}

fn found(n: u8, ev: i128, hops: u8) -> Discovery {
    Discovery { key: OpportunityKey(B256::repeat_byte(n)), ev, class: class(hops) }
}

fn window(fast: Vec<Discovery>, oracle: Vec<Discovery>) -> AuditWindow {
    AuditWindow {
        block: 50_684_845,
        observed_at: T0,
        audited_at: UnixNanos(T0.0 + 8_000_000_000),
        fast_path: fast,
        oracle,
    }
}

/// **The plan's Step 1 test.** §28: inject a synthetic opportunity the frontier
/// provably cannot see, and the auditor reports it.
#[test]
fn auditor_detects_injected_miss() {
    let seen = found(1, 1_000, 2);
    let injected = found(2, 900_000, 4); // a 4-hop route the fast path never built

    let report = CoverageAuditor::new().audit(&window(vec![seen], vec![seen, injected]));

    assert_eq!(report.missed, vec![injected], "the injected opportunity was not reported");
    assert_eq!(report.recall, Recall::Measured(0.5));
    assert!(report.oracle_was_complete());

    // And it says which class is blind, because "we are blind to 4-hop routes"
    // and "we are blind generally" are different diagnoses.
    assert_eq!(report.by_route_class[&class(4)], Recall::Measured(0.0));
    assert_eq!(report.by_route_class[&class(2)], Recall::Measured(1.0));
}

/// The counterweight. An auditor that reported everything as missed would pass
/// the test above; this is what stops that being enough.
#[test]
fn perfect_coverage_reports_no_misses() {
    let a = found(1, 1_000, 2);
    let b = found(2, 5_000, 3);
    let report = CoverageAuditor::new().audit(&window(vec![a, b], vec![a, b]));

    assert!(report.missed.is_empty());
    assert_eq!(report.recall, Recall::Measured(1.0));
    assert_eq!(report.high_ev_miss_rate, Recall::Measured(0.0));
    assert_eq!(report.response(0.95), CoverageResponse::None);
}

/// **An oracle that found nothing scores `Undefined`, not 1.0.**
///
/// "We both found nothing" and "we found everything there was" are the same
/// arithmetic and opposite facts, and the first is what a broken oracle
/// produces on every window. This is the single most important behaviour in
/// the module: a gauge reading perfect while the oracle is dead is worse than
/// no gauge.
#[test]
fn an_empty_oracle_is_undefined_not_perfect() {
    let report = CoverageAuditor::new().audit(&window(vec![], vec![]));

    assert_eq!(report.recall, Recall::Undefined);
    assert_eq!(report.recall.value(), None);
    assert!(!report.recall.meets(0.95), "an absent measurement must not clear the floor");
    assert_eq!(report.high_ev_miss_rate, Recall::Undefined);

    // And it triggers no response: escalating on an absent measurement would
    // make a dead oracle look like a discovery problem.
    assert_eq!(report.response(0.95), CoverageResponse::None);
}

/// **An oracle that misses what the fast path found is not an oracle.**
///
/// Recall would read 1.0 here on the naive arithmetic -- the fast path found
/// everything the oracle did -- and that number would be measured against a
/// reference that is not a superset.
#[test]
fn an_oracle_that_is_not_a_superset_is_reported() {
    let shared = found(1, 1_000, 2);
    let only_fast = found(9, 40_000, 2);

    let report = CoverageAuditor::new().audit(&window(vec![shared, only_fast], vec![shared]));

    assert_eq!(report.recall, Recall::Measured(1.0), "the arithmetic still says 1.0");
    assert_eq!(report.oracle_gaps, vec![only_fast], "but the reference has a hole");
    assert!(
        !report.oracle_was_complete(),
        "a recall of 1.0 against an incomplete oracle must not read as trustworthy"
    );
}

/// **EV-weighted, not counted.** Fifty dust misses matter less than one large
/// one, and a count-based rate says the opposite.
#[test]
fn the_miss_rate_weights_by_value_not_by_count() {
    let mut oracle = vec![found(200, 5_000_000, 2)]; // the one that matters
    for i in 0..50u8 {
        oracle.push(found(i, 10, 2)); // dust the fast path did see
    }
    let fast: Vec<Discovery> = oracle.iter().skip(1).copied().collect();

    let report = CoverageAuditor::new().audit(&window(fast, oracle));

    // By count the fast path saw 50 of 51 -- a rate that looks excellent.
    assert_eq!(report.recall, Recall::Measured(50.0 / 51.0));
    assert!(report.recall.meets(0.95), "the count-based rate clears the floor");

    // By value it missed essentially everything, which is the truth.
    let by_value = report.high_ev_miss_rate.value().expect("measured");
    assert!(by_value > 0.99, "EV-weighted miss rate was {by_value}");
}

/// A losing opportunity the fast path skipped is the fast path working, so it
/// does not count against the value-weighted rate.
#[test]
fn a_missed_negative_ev_opportunity_costs_nothing_by_value() {
    let seen = found(1, 1_000, 2);
    let skipped = found(2, -50_000, 2);
    let report = CoverageAuditor::new().audit(&window(vec![seen], vec![seen, skipped]));

    assert_eq!(report.missed, vec![skipped], "it is still recorded as unseen");
    assert_eq!(report.high_ev_miss_rate, Recall::Measured(0.0), "but it cost no value");
}

/// `coverage_audit_lag`. The audit is delayed by construction, and a recall
/// figure from a stale window says less about the system running now.
#[test]
fn the_audit_lag_is_reported() {
    let report = CoverageAuditor::new().audit(&window(vec![found(1, 1, 2)], vec![found(1, 1, 2)]));
    assert_eq!(report.lag, DurationNanos(8_000_000_000));
}

/// §28's ladder, in order. A class that is *entirely* invisible goes straight
/// to the end: no template expansion finds what the frontier cannot represent.
#[test]
fn the_response_ladder_escalates_in_order() {
    let seen = found(1, 1_000, 2);

    // Patchy: below the floor, but well above half of it. Four of five seen.
    let mut oracle = vec![seen];
    for i in 2..6u8 {
        oracle.push(found(i, 1_000, 2));
    }
    let mut fast = vec![seen];
    for i in 2..5u8 {
        fast.push(found(i, 1_000, 2));
    }
    let patchy = CoverageAuditor::new().audit(&window(fast, oracle));
    assert_eq!(patchy.recall, Recall::Measured(0.8));
    assert_eq!(patchy.response(0.95), CoverageResponse::ExpandTemplates);

    // Wide: less than half the floor.
    let mut wide_oracle = vec![seen];
    for i in 2..20u8 {
        wide_oracle.push(found(i, 1_000, 2));
    }
    let wide = CoverageAuditor::new().audit(&window(vec![seen], wide_oracle));
    assert_eq!(wide.response(0.95), CoverageResponse::RaiseSlowPathBudget);

    // Total blindness to a class: nothing of it was ever seen.
    let blind = CoverageAuditor::new()
        .audit(&window(vec![seen], vec![seen, found(7, 900_000, 4), found(8, 900_000, 4)]));
    assert_eq!(blind.response(0.95), CoverageResponse::DisableClass(class(4)));
}

/// Pooled, not averaged. A quiet window with one opportunity must not weigh the
/// same as a busy one with a hundred.
#[test]
fn recall_pools_across_windows_rather_than_averaging_rates() {
    let mut auditor = CoverageAuditor::new();

    // Window 1: one opportunity, missed. Rate 0.0.
    auditor.audit(&window(vec![], vec![found(1, 100, 2)]));

    // Window 2: twenty opportunities, all seen. Rate 1.0.
    let many: Vec<Discovery> = (10..30u8).map(|i| found(i, 100, 2)).collect();
    auditor.audit(&window(many.clone(), many));

    // Averaging the two rates gives 0.5. Pooling gives 20/21.
    let pooled = auditor.pooled_recall().value().expect("measured");
    assert!((pooled - 20.0 / 21.0).abs() < 1e-9, "pooled recall was {pooled}");
    assert!(pooled > 0.95, "the averaged figure would have failed the floor");
}

/// A window whose oracle was incomplete is nameable afterwards, so a chart of
/// pooled recall can be read with the holes marked.
#[test]
fn windows_with_a_gappy_oracle_are_nameable() {
    let mut auditor = CoverageAuditor::new();
    let shared = found(1, 100, 2);
    auditor.audit(&window(vec![shared], vec![shared]));
    auditor.audit(&window(vec![shared, found(9, 100, 2)], vec![shared]));

    assert_eq!(auditor.windows_with_an_incomplete_oracle(), vec![50_684_845]);
    assert_eq!(auditor.reports().len(), 2);
}
