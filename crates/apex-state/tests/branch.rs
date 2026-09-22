//! §5.1 / §5.4 speculative branches and rollback (INV-14).

use apex_state::branch::{BranchOutcome, SpeculativeStateTree};
use apex_types::ids::CandidateId;
use apex_types::state::StateBranchId;

#[test]
fn a_branch_matching_the_confirmed_block_is_promoted() {
    let mut t = SpeculativeStateTree::new();
    let b = t.open_branch("head-a");
    t.attach_candidate(b, CandidateId(1));

    let out = t.commit_or_rollback("head-a");

    assert_eq!(out.promoted, Some(b));
    assert!(out.invalidated_candidates.is_empty(), "a promoted branch invalidates nothing");
    assert_eq!(t.reorg_count(), 0);
}

#[test]
fn a_divergent_branch_is_rolled_back_and_names_its_candidates() {
    // §5.4: "if different -> rollback affected branch -> rebuild canonical
    // snapshot -> re-evaluate impacted candidates." The re-evaluation cannot
    // happen if the rollback does not say WHICH candidates were affected.
    let mut t = SpeculativeStateTree::new();
    let b = t.open_branch("predicted-head");
    t.attach_candidate(b, CandidateId(7));
    t.attach_candidate(b, CandidateId(8));

    let out = t.commit_or_rollback("actual-head");

    assert_eq!(out.promoted, None);
    assert_eq!(out.rolled_back, vec![b]);
    assert_eq!(out.invalidated_candidates, vec![CandidateId(7), CandidateId(8)]);
    assert_eq!(t.reorg_count(), 1);
}

#[test]
fn only_the_matching_branch_survives_a_commit() {
    let mut t = SpeculativeStateTree::new();
    let right = t.open_branch("head-a");
    let wrong1 = t.open_branch("head-b");
    let wrong2 = t.open_branch("head-c");
    t.attach_candidate(wrong1, CandidateId(1));
    t.attach_candidate(wrong2, CandidateId(2));

    let out = t.commit_or_rollback("head-a");

    assert_eq!(out.promoted, Some(right));
    assert_eq!(out.rolled_back.len(), 2);
    assert_eq!(out.invalidated_candidates.len(), 2);
    assert_eq!(t.open_branches(), 0, "a commit closes every speculative branch");
}

#[test]
fn the_canonical_branch_is_never_speculative() {
    let t = SpeculativeStateTree::new();
    assert!(StateBranchId::CANONICAL.is_canonical());
    assert_eq!(t.open_branches(), 0, "a fresh tree has canonical state only");
}

#[test]
fn divergence_rate_is_measured_not_assumed() {
    // §5.4 names preconf_to_final_divergence_rate as a thing to measure. A rate
    // nobody computes is the same as no rate.
    let mut t = SpeculativeStateTree::new();
    for i in 0..10 {
        let b = t.open_branch(if i < 3 { "wrong" } else { "right" });
        let _ = b;
        t.commit_or_rollback("right");
    }
    assert_eq!(t.commits(), 10);
    assert_eq!(t.reorg_count(), 3);
    assert!((t.divergence_rate() - 0.3).abs() < 1e-9, "got {}", t.divergence_rate());
}

#[test]
fn a_commit_with_no_open_branch_is_not_a_reorg() {
    // Quiet blocks are the common case; counting them as divergence would
    // inflate the rate and trigger the §5.4 response for no reason.
    let mut t = SpeculativeStateTree::new();
    let out = t.commit_or_rollback("head-a");

    assert_eq!(out.promoted, None);
    assert!(out.rolled_back.is_empty());
    assert_eq!(t.reorg_count(), 0);
    assert_eq!(t.divergence_rate(), 0.0);
}

#[test]
fn branch_outcome_is_exhaustive_about_what_happened() {
    let mut t = SpeculativeStateTree::new();
    let b = t.open_branch("x");
    match t.classify(b, "x") {
        BranchOutcome::Promote => {}
        other => panic!("got {other:?}"),
    }
    match t.classify(b, "y") {
        BranchOutcome::Rollback { .. } => {}
        other => panic!("got {other:?}"),
    }
}
