//! Task 6.3 — §18.2's seven hard requirements, one test each, plus the nonce
//! manager's adapted behaviour.
//!
//! The loom model for the seventh (`no_cross_lane_nonce_reuse`) lives in
//! `tests/nonce_loom.rs`, because it only builds under `--cfg loom`.

use apex_capture::signer::{
    ExecutorAuth, LaneConfig, LaneHealth, LaneRequirements, NoLane, NonceError, NonceLane,
    SignerPool,
};
use apex_types::ids::{ChainId, SignerLaneId};
use apex_types::time::UnixNanos;

const EXECUTOR: [u8; 20] = [0xEE; 20];
const VERSION: [u8; 32] = [0x01; 32];
const GWEI: u128 = 1_000_000_000;

fn auth() -> ExecutorAuth {
    ExecutorAuth { chain: ChainId::BASE, executor: EXECUTOR, executor_version: VERSION }
}

fn lane(id: u16, reserve_gwei: u128) -> LaneConfig {
    LaneConfig { id: SignerLaneId(id), address: [id as u8; 20], gas_reserve_wei: reserve_gwei * GWEI }
}

fn need(min_reserve_gwei: u128) -> LaneRequirements {
    LaneRequirements {
        chain: ChainId::BASE,
        executor: EXECUTOR,
        executor_version: VERSION,
        min_gas_reserve_wei: min_reserve_gwei * GWEI,
    }
}

/// §18.4's initial Base sizing.
fn pool_of_four() -> SignerPool {
    SignerPool::new(auth(), vec![lane(0, 100), lane(1, 100), lane(2, 100), lane(3, 100)])
}

fn now() -> UnixNanos {
    UnixNanos(1_700_000_000_000_000_000)
}

// ----------------------------------------------- 1. independent nonce streams

#[test]
fn lanes_have_independent_nonce_streams() {
    let pool = pool_of_four();
    let a = pool.assign(&need(1)).unwrap();
    let b = pool.assign(&need(1)).unwrap();
    assert_ne!(a.lane(), b.lane());

    // Two lanes, two different chain-pending values, because two addresses have
    // two transaction counts. A shared stream would make the second answer
    // depend on the first.
    let na = a.reserve_nonce(7, now());
    let nb = b.reserve_nonce(41, now());
    assert_eq!((na.get(), nb.get()), (7, 41));

    let na2 = a.reserve_nonce(7, now());
    assert_eq!(na2.get(), 8, "lane A advanced");
    let nb2 = b.reserve_nonce(41, now());
    assert_eq!(nb2.get(), 42, "and lane B is unaffected by it");
}

/// A reservation carries its lane, so handing one to another lane is refused
/// rather than silently accepted. A nonce is only meaningful against one
/// address; a bare `u64` would make that mistake invisible.
#[test]
fn a_nonce_cannot_be_used_on_another_lane() {
    let pool = pool_of_four();
    let a = pool.assign(&need(1)).unwrap();
    let b = pool.assign(&need(1)).unwrap();
    let na = a.reserve_nonce(1, now());
    assert_eq!(
        b.mark_submitted(na),
        Err(NonceError::WrongLane { expected: b.lane(), got: a.lane() })
    );
}

// --------------------------------------- 2. independent pending-state tracking

#[test]
fn lane_pending_state_is_isolated() {
    let pool = pool_of_four();
    let (la, lb) = {
        let a = pool.assign(&need(1)).unwrap();
        let b = pool.assign(&need(1)).unwrap();
        a.reserve_nonce(10, now());
        a.reserve_nonce(10, now());
        a.reserve_nonce(10, now());
        b.reserve_nonce(10, now());
        (a.lane(), b.lane())
    };
    assert_eq!(pool.with_nonce_lane(la, NonceLane::in_flight_count), Some(3));
    assert_eq!(pool.with_nonce_lane(lb, NonceLane::in_flight_count), Some(1));
}

// --------------------------------------------------- 3. pre-funded gas reserve

#[test]
fn lane_without_reserve_is_not_assigned() {
    let pool = SignerPool::new(auth(), vec![lane(0, 1), lane(1, 500)]);

    // Needs 100 gwei: only lane 1 qualifies, whatever its health.
    let a = pool.assign(&need(100)).unwrap();
    assert_eq!(a.lane(), SignerLaneId(1));

    // And with lane 1 busy, the underfunded one is not a fallback -- a signer
    // that runs out mid-flight yields a ticket that can never be dispatched.
    assert_eq!(pool.assign(&need(100)), Err(NoLane::NoneFunded));
}

// ------------------------------------- 4. shared immutable executor auth

#[test]
fn all_lanes_share_executor_auth() {
    let pool = pool_of_four();
    let a = pool.assign(&need(1)).unwrap();
    let b = pool.assign(&need(1)).unwrap();
    assert_eq!(a.auth(), b.auth());
    assert_eq!(a.auth(), pool.auth());
    // The same object, not merely an equal one: two copies can diverge, and a
    // ticket signed against a stale executor version reverts at best.
    assert!(std::ptr::eq(a.auth(), b.auth()));
}

#[test]
fn a_mismatched_executor_is_refused_before_any_lane_is_considered() {
    let pool = pool_of_four();
    for wrong in [
        LaneRequirements { chain: ChainId(1), ..need(1) },
        LaneRequirements { executor: [0xAB; 20], ..need(1) },
        LaneRequirements { executor_version: [0x02; 32], ..need(1) },
    ] {
        assert_eq!(pool.assign(&wrong), Err(NoLane::NotAuthorized));
    }
}

// --------------------------------------------------- 5. per-lane health score

#[test]
fn unhealthy_lane_leaves_hot_pool() {
    let pool = pool_of_four();
    assert_eq!(pool.hot_lanes().len(), 4);

    // Two failures halve the score twice: 1.0 -> 0.25, below HOT_THRESHOLD.
    pool.record_outcome(SignerLaneId(2), false);
    assert_eq!(pool.hot_lanes().len(), 4, "one failure is not a verdict");
    pool.record_outcome(SignerLaneId(2), false);

    let hot = pool.hot_lanes();
    assert_eq!(hot.len(), 3);
    assert!(!hot.contains(&SignerLaneId(2)));

    // And it comes back, but not instantly: a lane that just failed twice must
    // not be the healthiest candidate again after a single success.
    pool.record_outcome(SignerLaneId(2), true);
    assert!(pool.hot_lanes().contains(&SignerLaneId(2)));
    let h = pool.health();
    assert!(h[&SignerLaneId(2)].score() < h[&SignerLaneId(0)].score());
}

/// The healthiest free lane wins, which is the whole assignment rule.
#[test]
fn assignment_prefers_the_healthiest_free_lane() {
    let pool = pool_of_four();
    for id in [0u16, 1, 3] {
        pool.record_outcome(SignerLaneId(id), false);
    }
    assert_eq!(pool.assign(&need(1)).unwrap().lane(), SignerLaneId(2));
}

// ----------------------------------------------- 6. per-lane circuit breaker

#[test]
fn lane_breaker_does_not_stop_chain() {
    let pool = pool_of_four();
    for _ in 0..LaneHealth::BREAKER_TRIPS_AT {
        pool.record_outcome(SignerLaneId(0), false);
    }
    assert!(pool.health()[&SignerLaneId(0)].breaker_is_open());

    // The other three keep signing. That is the requirement: a slow or
    // conflicted lane leaves the hot pool WITHOUT stopping the chain.
    let mut assigned = Vec::new();
    for _ in 0..3 {
        assigned.push(pool.assign(&need(1)).unwrap());
    }
    assert_eq!(assigned.len(), 3);
    assert!(!assigned.iter().any(|a| a.lane() == SignerLaneId(0)));

    // Only when they are all taken does the pool run out, and it says why.
    assert_eq!(pool.assign(&need(1)), Err(NoLane::AllBusy));
}

/// Every lane broken is not the same as every lane busy, and conflating them
/// would send an operator to wait for capacity that is never coming back.
#[test]
fn a_pool_with_every_breaker_open_says_so() {
    let pool = pool_of_four();
    for id in 0..4u16 {
        for _ in 0..LaneHealth::BREAKER_TRIPS_AT {
            pool.record_outcome(SignerLaneId(id), false);
        }
    }
    assert_eq!(pool.assign(&need(1)), Err(NoLane::AllOutOfTheHotPool));
    assert!(pool.hot_lanes().is_empty());
}

// ------------------------------------------------------------- exclusivity

#[test]
fn a_lane_is_assigned_to_one_holder_at_a_time() {
    let pool = SignerPool::new(auth(), vec![lane(0, 100)]);
    let held = pool.assign(&need(1)).unwrap();
    assert_eq!(pool.assign(&need(1)), Err(NoLane::AllBusy));
    drop(held);
    assert!(pool.assign(&need(1)).is_ok(), "the lane must come back");
}

// ------------------------------------------- the adapted nonce manager itself

/// The legacy comment's claim, preserved: the chain's PENDING nonce is the
/// authoritative starting point. Using the confirmed count was the historical
/// bug -- it hands out a nonce still occupied by an unconfirmed transaction.
#[test]
fn the_chain_pending_nonce_is_the_authoritative_floor() {
    let mut l = NonceLane::new(SignerLaneId(0));
    assert_eq!(l.reserve(100, now()).get(), 100);
    // The node's pending view jumped: somebody else used this key, or a bundle
    // landed. The allocator follows it up rather than colliding.
    assert_eq!(l.reserve(140, now()).get(), 140);
}

/// And the local high-water mark guards the other direction: an RPC pending
/// view that lags privately-submitted bundles must not walk the nonce back.
#[test]
fn the_local_floor_guards_against_a_lagging_rpc_view() {
    let mut l = NonceLane::new(SignerLaneId(0));
    assert_eq!(l.reserve(100, now()).get(), 100);
    assert_eq!(l.reserve(100, now()).get(), 101);
    // The relay has our 100 and 101; this node's mempool does not.
    assert_eq!(l.reserve(100, now()).get(), 102, "the local floor was ignored");
}

/// Gap recovery, from the legacy comment: a failed highest nonce is reclaimed
/// so the next dispatch reuses it rather than leaving a permanent mempool gap.
#[test]
fn a_failed_highest_nonce_is_reclaimed() {
    let mut l = NonceLane::new(SignerLaneId(0));
    let a = l.reserve(10, now());
    let b = l.reserve(10, now());
    assert_eq!((a.get(), b.get()), (10, 11));

    l.release(b, false).unwrap();
    assert_eq!(l.reserved_nonce(), Some(11), "11 is free again");
    assert_eq!(l.reserve(10, now()).get(), 11);
}

/// But NOT when something higher is still in flight -- reclaiming there would
/// hand out a nonce below an outstanding transaction and start a replacement
/// war with ourselves.
#[test]
fn a_failed_nonce_below_an_in_flight_one_is_not_reclaimed() {
    let mut l = NonceLane::new(SignerLaneId(0));
    let a = l.reserve(10, now());
    let b = l.reserve(10, now());
    l.release(a, false).unwrap();
    assert_eq!(l.reserved_nonce(), Some(12), "10 must not be reclaimed under {b:?}");
    assert_eq!(l.reserve(10, now()).get(), 12);
}

/// A landed transaction never gives its nonce back, whatever the local view
/// said. The legacy comment: "we never reuse a nonce that confirmed".
#[test]
fn a_landed_nonce_is_never_reclaimed() {
    let mut l = NonceLane::new(SignerLaneId(0));
    let a = l.reserve(10, now());
    l.release(a, true).unwrap();
    assert_eq!(l.confirmed_nonce(), Some(10));
    assert_eq!(l.reserved_nonce(), Some(11));
    assert_eq!(l.reserve(10, now()).get(), 11);
}

#[test]
fn a_reservation_cannot_be_released_twice() {
    let mut l = NonceLane::new(SignerLaneId(0));
    let a = l.reserve(10, now());
    l.release(a, true).unwrap();
    assert_eq!(l.release(a, true), Err(NonceError::NotInFlight(10)));
}

/// Stale markers are dropped beyond any realistic inclusion window, so a lane
/// whose ticket vanished does not keep an in-flight entry forever and block its
/// own gap recovery.
#[test]
fn stale_in_flight_markers_expire() {
    let mut l = NonceLane::new(SignerLaneId(0));
    l.reserve(10, UnixNanos(0));
    assert_eq!(l.in_flight_count(), 1);
    let later = UnixNanos(apex_capture::signer::STALE_RESERVATION.0 + 1);
    l.reserve(10, later);
    assert_eq!(l.in_flight_count(), 1, "the first marker should have expired");
}

/// §27.4: a replacement reuses its nonce by definition, so recording one must
/// not allocate a new one.
#[test]
fn a_replacement_does_not_consume_a_new_nonce() {
    let mut l = NonceLane::new(SignerLaneId(0));
    let a = l.reserve(10, now());
    l.mark_submitted(a).unwrap();
    l.mark_replacement(a).unwrap();
    assert_eq!(l.reserved_nonce(), Some(11), "a replacement allocated a nonce");
    assert_eq!(l.replacements().len(), 1);

    l.release(a, true).unwrap();
    assert!(l.replacements().is_empty(), "a closed ticket leaves no replacement behind");
    assert!(l.submitted().is_empty());
}
