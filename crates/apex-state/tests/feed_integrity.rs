//! §5.6 feed continuity. INV-12 and INV-13.
//!
//! "A feed gap may never be silently converted into 'probably unchanged.'"

use apex_state::feed::arbiter::{FeedArbiter, Resolution};
use apex_state::feed::integrity::FeedIntegrity;
use apex_types::ids::FeedSourceId;
use apex_types::state::ReconstructionStatus;

#[test]
fn a_clean_sequence_stays_verified() {
    let mut f = FeedIntegrity::new(FeedSourceId(1));
    for seq in 100..110 {
        f.observe(seq);
    }
    assert_eq!(f.gap_count(), 0);
    assert_eq!(f.status(), ReconstructionStatus::Verified);
}

#[test]
fn a_sequence_gap_marks_the_branch_unsafe() {
    // INV-12. The counters exist so the gap is a measurement, not a guess.
    let mut f = FeedIntegrity::new(FeedSourceId(1));
    f.observe(100);
    f.observe(101);
    f.observe(104); // 102, 103 missing

    assert_eq!(f.gap_count(), 1);
    assert_eq!(f.missing_count(), 2);
    assert_eq!(f.status(), ReconstructionStatus::Unsafe);
}

#[test]
fn an_unsafe_feed_does_not_recover_by_itself() {
    // The dangerous shape: a gap, then clean traffic, and the feed quietly
    // calls itself healthy again while the missed deltas are still missing.
    let mut f = FeedIntegrity::new(FeedSourceId(1));
    f.observe(100);
    f.observe(104);
    for seq in 105..200 {
        f.observe(seq);
    }
    assert_eq!(
        f.status(),
        ReconstructionStatus::Unsafe,
        "clean traffic after a gap is not evidence the gap was repaired"
    );
}

#[test]
fn recovery_requires_an_explicit_verified_rebuild() {
    let mut f = FeedIntegrity::new(FeedSourceId(1));
    f.observe(100);
    f.observe(104);
    assert_eq!(f.status(), ReconstructionStatus::Unsafe);

    f.begin_rebuild();
    assert_eq!(f.status(), ReconstructionStatus::Rebuilding);
    assert!(!f.status().may_authorize_live_ticket(), "rebuilding is not tradeable");

    f.rebuild_verified(104);
    assert_eq!(f.status(), ReconstructionStatus::Verified);
    assert_eq!(f.gap_count(), 1, "history is kept; the counter is not reset by recovery");
}

#[test]
fn duplicates_and_out_of_order_are_counted_separately_from_gaps() {
    // They mean different things: a duplicate is harmless, a backwards delivery
    // may not be, and neither is a gap. Collapsing them loses the diagnosis.
    let mut f = FeedIntegrity::new(FeedSourceId(1));
    f.observe(100);
    f.observe(100);
    f.observe(99);
    f.observe(101);

    assert_eq!(f.duplicate_count(), 1);
    assert_eq!(f.out_of_order_count(), 1);
    assert_eq!(f.gap_count(), 0, "neither is a gap");
    assert_eq!(f.status(), ReconstructionStatus::Verified);
}

// The catch-all arms below are unreachable today -- `Resolution` has exactly two
// variants and both are matched. They stay because that is the point: the day
// someone adds `Resolution::Majority`, these tests fail instead of silently
// accepting the outcome INV-13 forbids.
#[allow(unreachable_patterns)]
#[test]
fn contradicting_feeds_are_never_resolved_by_majority_vote() {
    // INV-13. §5.6: resolve "by parentage, sequence continuity, explicit
    // reconciliation, or full state rebuild" -- never by counting agreeing
    // sources. Two feeds echoing one bad upstream are not two witnesses.
    let a = fp("parent-a", 100);
    let b = fp("parent-b", 100);
    let c = fp("parent-b", 100); // a second voice for b's parent

    match FeedArbiter::resolve(&[a, b, c]) {
        Resolution::Rebuild { .. } | Resolution::ByParentage { .. } => {}
        other => panic!("majority vote is forbidden, got {other:?}"),
    }
}

#[allow(unreachable_patterns)]
#[test]
fn agreeing_feeds_resolve_by_parentage_not_by_count() {
    let a = fp("parent-a", 100);
    let b = fp("parent-a", 100);

    match FeedArbiter::resolve(&[a, b]) {
        Resolution::ByParentage { .. } => {}
        other => panic!("unanimous feeds should resolve by parentage, got {other:?}"),
    }
}

#[allow(unreachable_patterns)]
#[test]
fn a_single_feed_is_not_a_consensus() {
    match FeedArbiter::resolve(&[fp("parent-a", 100)]) {
        Resolution::ByParentage { .. } => {}
        other => panic!("got {other:?}"),
    }
}

fn fp(parent: &str, block: u64) -> apex_state::feed::arbiter::FeedClaim {
    apex_state::feed::arbiter::FeedClaim {
        source: FeedSourceId(1),
        parent_tag: parent.to_string(),
        block,
    }
}
