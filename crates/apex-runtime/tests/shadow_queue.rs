//! R21's queue: best first, newest replaces, old drops, our own pools skip.

use alloy_primitives::{Address, B256, U256};
use apex_runtime::shadow::queue::{pools_of, test_proposal, PendingQueue, MAX_PROPOSAL_AGE};
use apex_search::frontier::RouteProposal;
use apex_state::feed::event::StateEvent;
use apex_types::time::UnixNanos;
use std::sync::Arc;

const T0: u64 = 1_000_000_000_000;

fn event() -> Arc<StateEvent> {
    Arc::new(apex_search::frontier::doc_event())
}

/// A proposal for route `id` over pools `pools`, found at `found_ns`, worth `net`.
fn proposal(id: u8, pools: &[u8], found_ns: u64, net: Option<u64>) -> RouteProposal {
    let pools: Vec<Address> = pools.iter().map(|b| Address::repeat_byte(*b)).collect();
    let mut p = test_proposal(B256::repeat_byte(id), &pools);
    p.found_at = UnixNanos(found_ns);
    p.net_hint = net.map(U256::from);
    p
}

fn drain(q: &mut PendingQueue, now: u64) -> Vec<B256> {
    std::iter::from_fn(|| q.pop_best(UnixNanos(now)).best).map(|p| p.proposal.route.route_hash).collect()
}

/// The 10:07 UTC spike: the $17.98 candidate waited behind three worth cents.
#[test]
fn the_most_valuable_proposal_is_handled_first() {
    let mut q = PendingQueue::default();
    q.merge(&event(), vec![proposal(1, &[1], T0, Some(5)), proposal(2, &[2], T0, Some(1_798)), proposal(3, &[3], T0, None)]);
    assert_eq!(drain(&mut q, T0), vec![B256::repeat_byte(2), B256::repeat_byte(1), B256::repeat_byte(3)]);
}

#[test]
fn a_newer_proposal_for_a_route_replaces_the_older() {
    let mut q = PendingQueue::default();
    q.merge(&event(), vec![proposal(1, &[1], T0, Some(9))]);
    q.merge(&event(), vec![proposal(1, &[1], T0 + 10, Some(3))]);
    assert_eq!(q.len(), 1);
    let best = q.pop_best(UnixNanos(T0 + 10)).best.expect("one");
    assert_eq!(best.proposal.net_hint, Some(U256::from(3)), "the newer price, not the larger");
}

#[test]
fn an_older_proposal_does_not_replace_a_newer_one() {
    let mut q = PendingQueue::default();
    q.merge(&event(), vec![proposal(1, &[1], T0 + 10, Some(3))]);
    q.merge(&event(), vec![proposal(1, &[1], T0, Some(9))]);
    assert_eq!(q.pop_best(UnixNanos(T0 + 10)).best.expect("one").proposal.net_hint, Some(U256::from(3)));
}

#[test]
fn a_proposal_is_stale_only_past_one_second() {
    let mut q = PendingQueue::default();
    q.merge(&event(), vec![proposal(1, &[1], T0, Some(1))]);
    let at_limit = q.pop_best(UnixNanos(T0 + MAX_PROPOSAL_AGE.0));
    assert!(at_limit.stale.is_empty() && at_limit.best.is_some(), "exactly 1 s old is still fresh");

    q.merge(&event(), vec![proposal(2, &[2], T0, Some(1))]);
    let past = q.pop_best(UnixNanos(T0 + MAX_PROPOSAL_AGE.0 + 1));
    assert_eq!(past.stale.len(), 1);
    assert_eq!(past.stale[0].1 .0, MAX_PROPOSAL_AGE.0 + 1, "it carries its age");
    assert!(past.best.is_none(), "a stale entry is never the best");
}

#[test]
fn ties_go_to_the_newest_then_the_lowest_route() {
    let mut q = PendingQueue::default();
    q.merge(
        &event(),
        vec![proposal(7, &[1], T0, Some(4)), proposal(5, &[2], T0 + 1, Some(4)), proposal(6, &[3], T0 + 1, Some(4))],
    );
    assert_eq!(drain(&mut q, T0 + 1), vec![B256::repeat_byte(5), B256::repeat_byte(6), B256::repeat_byte(7)]);
}

#[test]
fn proposals_sharing_a_traded_pool_are_dropped() {
    let mut q = PendingQueue::default();
    q.merge(
        &event(),
        vec![proposal(1, &[1, 2], T0, Some(1)), proposal(2, &[2, 3], T0, Some(1)), proposal(3, &[4, 5], T0, Some(1))],
    );
    let traded = pools_of(&proposal(9, &[2, 9], T0, None));
    assert_eq!(q.drop_conflicting(&traded), 2);
    assert_eq!(q.len(), 1);
}

#[test]
fn the_deepest_the_queue_got_is_kept() {
    let mut q = PendingQueue::default();
    q.merge(&event(), vec![proposal(1, &[1], T0, Some(1)), proposal(2, &[2], T0, Some(1))]);
    let _ = q.pop_best(UnixNanos(T0));
    let _ = q.pop_best(UnixNanos(T0));
    assert_eq!(q.max_depth(), 2);
    assert!(q.is_empty());
}
