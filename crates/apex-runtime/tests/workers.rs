//! §46.2 concurrency, §29's nine resource classes, §2.6's two paths.
//!
//! BP-175, BP-132 and BP-022. Three different claims about the same worker pool,
//! and each one fails in a different way when it is wrong.

mod support;

use apex_runtime::bus::{EventBus, Lane};
use apex_runtime::workers::{Budgets, ResourceClass};
use std::sync::Arc;
use std::time::Duration;
use support::*;

/// **BP-175.** On a state event, exact repricing / finite-size sizing /
/// competitor scenarios / cost refresh run **concurrently** and join.
///
/// # Why a barrier and not a stopwatch
///
/// The obvious test times four 50 ms stages and asserts the total is under
/// 200 ms. That is a measurement, so it is flaky under load, and it passes for
/// the wrong reason on a fast machine — a serial implementation with four 1 ms
/// stages also finishes quickly.
///
/// A `Barrier` of four turns the claim into a structural one: each stage waits
/// for the other three to arrive, so a serial implementation **deadlocks**. The
/// `timeout` then converts that deadlock into a deterministic failure. What is
/// asserted is not "it was fast" but "all four were in flight at once", which is
/// what §46.2 actually says.
#[tokio::test]
async fn independent_tasks_run_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let econ = Arc::new(BarrieredEconomics { barrier: Arc::clone(&barrier) });
    let c = candidate(1, 47_079_437, 320_000_000_000);

    let joined = tokio::time::timeout(
        Duration::from_secs(5),
        apex_runtime::workers::refine_concurrently(econ.as_ref(), &c),
    )
    .await;

    assert!(
        joined.is_ok(),
        "the four §46.2 stages did not overlap: a serial join cannot clear a 4-way barrier"
    );
    assert!(joined.expect("not timed out").is_ok(), "the stages themselves must succeed");
}

/// **BP-132.** Nine resource classes, each with its own budget. Saturating one
/// must not reduce another's capacity — a shared pool is how candidate
/// generation eats the permits submission needed.
#[tokio::test]
async fn resource_classes_isolated() {
    let budgets = Budgets::with_capacity(2);

    // Saturate candidate generation.
    let a = budgets.reserve(ResourceClass::CandidateGeneration).expect("first permit");
    let b = budgets.reserve(ResourceClass::CandidateGeneration).expect("second permit");
    assert!(
        budgets.reserve(ResourceClass::CandidateGeneration).is_none(),
        "a class must be bounded, or it can create the unbounded queue §29.3 forbids"
    );

    // Every other class is untouched.
    for class in ResourceClass::ALL {
        if class == ResourceClass::CandidateGeneration {
            continue;
        }
        assert!(
            budgets.reserve(class).is_some(),
            "{class:?} lost capacity to a saturated sibling"
        );
    }

    drop(a);
    assert!(budgets.reserve(ResourceClass::CandidateGeneration).is_some(), "a released permit returns");
    drop(b);
}

/// §29.2 names three classes to protect under overload. Shedding must refuse
/// them — the same rule as `Scheduler::shed` and an authorized live ticket: the
/// capacity argument is a target it will miss rather than a licence.
#[test]
fn a_protected_class_is_never_shed() {
    let protected: Vec<ResourceClass> =
        ResourceClass::ALL.into_iter().filter(|c| c.is_protected()).collect();

    assert_eq!(
        protected,
        vec![
            ResourceClass::StateIngestion,
            ResourceClass::Simulation,
            ResourceClass::Submission
        ],
        "§29.2 protects exactly these three"
    );

    for class in ResourceClass::ALL {
        assert_eq!(
            class.is_sheddable(),
            !class.is_protected(),
            "{class:?}: sheddable and protected must be exact complements"
        );
    }
}

/// ...and `shed()` has to obey the predicate.
///
/// The test above only exercises `is_protected`. A predicate nobody acts on is
/// decoration that happens to be true -- the lesson from Task 8.3's surviving
/// `sums_to` mutation -- so this one sheds for real and then asks the three
/// protected classes for permits.
#[test]
fn shedding_leaves_the_protected_classes_working() {
    let budgets = Budgets::with_capacity(1);
    let shed = budgets.shed();

    assert_eq!(shed.len(), 6, "six of nine are sheddable: {shed:?}");
    for class in ResourceClass::ALL {
        if class.is_protected() {
            assert!(!budgets.is_shed(class), "{class:?} was shed and must not have been");
            assert!(
                budgets.reserve(class).is_some(),
                "{class:?} is protected and must still admit work under overload"
            );
        } else {
            assert!(budgets.is_shed(class), "{class:?} is sheddable and was not shed");
            assert!(budgets.reserve(class).is_none(), "{class:?} kept admitting after shedding");
        }
    }
}

/// **BP-022 / §2.6.** Both paths are mandatory and the slow one never delays the
/// fast one.
///
/// The mechanism is per-subscriber buffering: a stalled subscriber lags rather
/// than applying backpressure to the publisher. The cost is that the slow path
/// *loses* events — so the test also asserts the loss is counted, because the
/// same mechanism that protects the fast path is the one that drops slow-path
/// work, and an uncounted drop is how a coverage audit silently stops covering.
#[tokio::test]
async fn fast_path_never_blocks_on_slow_path() {
    let mut bus = EventBus::with_capacity(4);
    let mut fast = bus.subscribe("search", Lane::Fast);
    let slow = bus.subscribe("coverage", Lane::Slow); // never read from

    let stream = recorded_stream();
    // More events than the buffer holds, so the slow subscriber must overflow.
    let published: usize = (0..3)
        .flat_map(|_| stream.iter())
        .map(|e| {
            bus.publish(e.clone());
            1
        })
        .sum();
    assert!(published > 4, "the test needs to exceed the buffer to mean anything");

    // The fast subscriber is drained as it goes in a real worker; here it is
    // read after the fact, so it too will have lagged. What must hold is that
    // `publish` never awaited anything: it returned for every event.
    assert_eq!(bus.published(), published as u64);

    // And the loss is attributed, not silent.
    let mut fast_seen = 0;
    while fast.try_recv().is_some() {
        fast_seen += 1;
    }
    assert!(fast_seen > 0, "the fast lane received something");
    assert_eq!(
        bus.dropped(Lane::Fast) + fast_seen as u64,
        published as u64,
        "every published event is either delivered or counted as dropped"
    );
    assert!(
        bus.dropped(Lane::Slow) > 0,
        "the stalled slow subscriber must show its loss"
    );
    drop(slow);
}

/// A fast-lane drop is not a budget decision. §16.6's zero-tolerance list makes
/// an unexplained pre-dispatch loss a halt condition, so the bus must be able to
/// say the two lanes apart rather than reporting one number.
#[test]
fn the_two_lanes_report_loss_separately() {
    let mut bus = EventBus::with_capacity(1);
    let _slow = bus.subscribe("coverage", Lane::Slow);
    let event = recorded_stream().first().cloned().expect("an event");
    for _ in 0..8 {
        bus.publish(event.clone());
    }
    assert!(bus.dropped(Lane::Slow) > 0);
    assert_eq!(bus.dropped(Lane::Fast), 0, "no fast subscriber, so no fast loss");
    assert!(bus.fast_lane_is_lossless(), "a fast lane that has lost nothing says so");
}
