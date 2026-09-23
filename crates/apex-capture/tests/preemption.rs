//! Task 6.5 — INV-09, §16.4, §57.1.2.
//!
//! Acceptance criterion 4: "Under 10x overload, `Authorized` tickets are never
//! preempted and `U_capture` stays >= 0.99."

use apex_capture::scheduler::{Scheduler, Work, WorkClass};
use apex_types::ids::TicketId;
use apex_types::ticket::TicketStatus;
use apex_types::time::UnixNanos;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use std::collections::VecDeque;

fn work(id: u64, class: WorkClass, deadline: u64, ev: i128) -> Work {
    Work { id: TicketId(id), class, deadline: UnixNanos(deadline), expected_net_ev: ev }
}

/// One signer lane: one unit of work served per round.
const CAPACITY: usize = 1;
/// 10x candidate rate against that one lane.
const CANDIDATES_PER_ROUND: usize = 10;
const ROUNDS: u64 = 200;
/// A live ticket arrives every fourth round and must be served within three.
const TICKET_EVERY: u64 = 4;
const TICKET_BUDGET: u64 = 3;
/// The queue the scheduler is allowed to keep between rounds.
const QUEUE_BOUND: usize = 16;

struct Outcome {
    tickets_admitted: u64,
    tickets_in_time: u64,
    authorized_shed: u64,
}

/// The §16.4 scheduler under the overload.
fn run_priority() -> Outcome {
    let mut sched = Scheduler::new();
    let mut id = 0u64;
    let mut admitted = 0u64;
    let mut in_time = 0u64;
    let mut authorized_shed = 0u64;
    let mut deadlines = std::collections::BTreeMap::new();

    for round in 0..ROUNDS {
        for _ in 0..CANDIDATES_PER_ROUND {
            id += 1;
            // Research and discovery, which is what overload actually looks
            // like: cheap work arriving far faster than capacity.
            let class =
                if id.is_multiple_of(3) { WorkClass::Research } else { WorkClass::RouteDiscovery };
            sched.admit(work(id, class, round + 50, 1));
        }
        if round.is_multiple_of(TICKET_EVERY) {
            id += 1;
            admitted += 1;
            deadlines.insert(id, round + TICKET_BUDGET);
            sched.admit(work(id, WorkClass::AuthorizedLiveTicket, round + TICKET_BUDGET, 10_000));
        }

        for w in sched.shed(QUEUE_BOUND) {
            if w.class == WorkClass::AuthorizedLiveTicket {
                authorized_shed += 1;
            }
        }
        for _ in 0..CAPACITY {
            let Some(w) = sched.serve() else { break };
            if w.class == WorkClass::AuthorizedLiveTicket {
                if let Some(due) = deadlines.remove(&w.id.0) {
                    if round <= due {
                        in_time += 1;
                    }
                }
            }
        }
    }
    Outcome { tickets_admitted: admitted, tickets_in_time: in_time, authorized_shed }
}

/// The same workload served in arrival order, with an **unbounded** queue.
///
/// Unbounded on purpose. A bounded FIFO also fails, but for a second reason --
/// tail-drop discards the newest arrival, which is the live ticket -- and then
/// the comparison would be measuring the drop policy as much as the service
/// order. §46.1's claim is specifically about *order*, so FIFO is given the
/// most generous shape available: it never loses anything, it just serves in
/// the order things arrived.
fn run_fifo() -> Outcome {
    let mut queue: VecDeque<Work> = VecDeque::new();
    let mut id = 0u64;
    let mut admitted = 0u64;
    let mut in_time = 0u64;
    let authorized_shed = 0u64; // unbounded: nothing is ever shed
    let mut deadlines = std::collections::BTreeMap::new();

    for round in 0..ROUNDS {
        for _ in 0..CANDIDATES_PER_ROUND {
            id += 1;
            let class =
                if id.is_multiple_of(3) { WorkClass::Research } else { WorkClass::RouteDiscovery };
            queue.push_back(work(id, class, round + 50, 1));
        }
        if round.is_multiple_of(TICKET_EVERY) {
            id += 1;
            admitted += 1;
            deadlines.insert(id, round + TICKET_BUDGET);
            queue.push_back(work(id, WorkClass::AuthorizedLiveTicket, round + TICKET_BUDGET, 10_000));
        }
        for _ in 0..CAPACITY {
            let Some(w) = queue.pop_front() else { break };
            if w.class == WorkClass::AuthorizedLiveTicket {
                if let Some(due) = deadlines.remove(&w.id.0) {
                    if round <= due {
                        in_time += 1;
                    }
                }
            }
        }
    }
    Outcome { tickets_admitted: admitted, tickets_in_time: in_time, authorized_shed }
}

/// **INV-09 and acceptance criterion 4.**
#[test]
fn authorized_ticket_never_preempted_under_overload() {
    let o = run_priority();
    println!(
        "priority: {}/{} tickets in time, U_capture = {:.4}",
        o.tickets_in_time,
        o.tickets_admitted,
        Scheduler::u_capture(o.tickets_in_time, o.tickets_admitted)
    );
    assert_eq!(o.authorized_shed, 0, "an authorized live ticket was preempted");
    let u = Scheduler::u_capture(o.tickets_in_time, o.tickets_admitted);
    assert!(u >= 0.99, "U_capture was {u} over {} tickets", o.tickets_admitted);
}

/// §46.1: FIFO is forbidden on the final capture path. Run both ways, because
/// "priority scheduling helps" is otherwise an untested adjective -- and
/// because if FIFO also passed, this whole module would be ceremony.
#[test]
fn fifo_loses_the_workload_the_scheduler_survives() {
    let fair = run_priority();
    let fifo = run_fifo();
    assert_eq!(fair.tickets_admitted, fifo.tickets_admitted, "same workload");

    let u_fair = Scheduler::u_capture(fair.tickets_in_time, fair.tickets_admitted);
    let u_fifo = Scheduler::u_capture(fifo.tickets_in_time, fifo.tickets_admitted);
    println!("U_capture: priority {u_fair:.4}, FIFO {u_fifo:.4}");
    assert!(u_fair >= 0.99);
    assert!(
        u_fifo < 0.5,
        "FIFO kept U_capture at {u_fifo}; the overload is not actually loading the queue"
    );
}

/// `shed` misses its target rather than preempting. The queue stays over
/// capacity and the caller sees it -- that is what drives §16.5's remediation
/// ladder instead of the queue quietly dropping the trade.
#[test]
fn shedding_misses_its_target_rather_than_preempting_an_authorized_ticket() {
    let mut s = Scheduler::new();
    for i in 0..5 {
        s.admit(work(i, WorkClass::AuthorizedLiveTicket, 100 + i, 1));
    }
    for i in 5..15 {
        s.admit(work(i, WorkClass::Research, 100 + i, 1));
    }

    let shed = s.shed(2);
    assert_eq!(shed.len(), 10, "every sheddable item should have gone");
    assert!(shed.iter().all(|w| w.class == WorkClass::Research));
    assert_eq!(s.len(), 5, "the five authorized tickets must remain, over capacity");
}

/// §57.1.2's order: research goes before route discovery, which goes before
/// simulations, which go before candidates.
#[test]
fn shedding_follows_the_preemption_order() {
    let mut s = Scheduler::new();
    let classes = [
        WorkClass::HighEvCandidate,
        WorkClass::ExactSimulation,
        WorkClass::RouteDiscovery,
        WorkClass::Research,
    ];
    for (i, c) in classes.iter().enumerate() {
        s.admit(work(i as u64, *c, 100, 1));
    }
    // One at a time, so the order is observed rather than inferred from a set.
    for expected in [WorkClass::Research, WorkClass::RouteDiscovery, WorkClass::ExactSimulation] {
        let shed = s.shed(s.len() - 1);
        assert_eq!(shed.len(), 1);
        assert_eq!(shed[0].class, expected, "shed out of §57.1.2 order");
    }
    assert_eq!(s.peek().unwrap().class, WorkClass::HighEvCandidate);
}

/// Service order within a class: the earlier deadline first, then the larger
/// EV. §16.4's key, in that order.
#[test]
fn within_a_class_the_earlier_deadline_wins_then_the_larger_ev() {
    let mut s = Scheduler::new();
    s.admit(work(1, WorkClass::HighEvCandidate, 200, 9_000));
    s.admit(work(2, WorkClass::HighEvCandidate, 100, 10));
    s.admit(work(3, WorkClass::HighEvCandidate, 100, 5_000));
    assert_eq!(s.serve().unwrap().id, TicketId(3), "earliest deadline, then highest EV");
    assert_eq!(s.serve().unwrap().id, TicketId(2));
    assert_eq!(s.serve().unwrap().id, TicketId(1));
}

/// A class always beats a better deadline or a better EV in a lower class. The
/// ordering is lexicographic, not a weighted score -- a weighted score is how
/// an enormous research batch out-votes a live ticket.
#[test]
fn class_dominates_deadline_and_ev() {
    let mut s = Scheduler::new();
    s.admit(work(1, WorkClass::Research, 1, i128::MAX));
    s.admit(work(2, WorkClass::AuthorizedLiveTicket, u64::MAX, i128::MIN));
    assert_eq!(s.serve().unwrap().id, TicketId(2));
}

/// The class of a ticket and the status that holds reserved resources are the
/// same line, so they cannot drift apart.
#[test]
fn the_class_boundary_is_the_authorized_status_boundary() {
    for s in [
        TicketStatus::Observed,
        TicketStatus::Reserved,
        TicketStatus::Exacting,
        TicketStatus::Simulated,
    ] {
        assert_eq!(WorkClass::of_ticket(s), WorkClass::HighEvCandidate, "{s:?}");
        assert!(WorkClass::of_ticket(s).is_preemptible());
    }
    for s in [
        TicketStatus::Authorized,
        TicketStatus::Signed,
        TicketStatus::Dispatching,
        TicketStatus::Acknowledged,
        TicketStatus::Preconfirmed,
        TicketStatus::Included,
        TicketStatus::Finalized,
        TicketStatus::Reconciled,
    ] {
        assert_eq!(WorkClass::of_ticket(s), WorkClass::AuthorizedLiveTicket, "{s:?}");
        assert!(!WorkClass::of_ticket(s).is_preemptible());
    }
}

#[test]
fn shedding_a_queue_already_within_capacity_does_nothing() {
    let mut s = Scheduler::new();
    s.admit(work(1, WorkClass::Research, 100, 1));
    assert!(s.shed(4).is_empty());
    assert_eq!(s.len(), 1);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 20_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/preemption.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// INV-09 over arbitrary mixes rather than the hand-built one: whatever
    /// arrives and whatever capacity is asked for, an authorized live ticket is
    /// never shed.
    #[test]
    fn no_authorized_ticket_is_ever_shed(
        items in prop::collection::vec((0usize..5, 0u64..1_000, -1_000i128..1_000), 0..40),
        capacity in 0usize..40,
    ) {
        let classes = [
            WorkClass::Research,
            WorkClass::RouteDiscovery,
            WorkClass::ExactSimulation,
            WorkClass::HighEvCandidate,
            WorkClass::AuthorizedLiveTicket,
        ];
        let mut s = Scheduler::new();
        let mut authorized = 0usize;
        for (i, (c, d, ev)) in items.iter().enumerate() {
            if classes[*c] == WorkClass::AuthorizedLiveTicket {
                authorized += 1;
            }
            s.admit(work(i as u64, classes[*c], *d, *ev));
        }
        let shed = s.shed(capacity);
        prop_assert!(
            shed.iter().all(|w| w.class != WorkClass::AuthorizedLiveTicket),
            "an authorized ticket was shed at capacity {}", capacity
        );
        prop_assert!(s.len() >= authorized, "authorized tickets went missing");
        // Shedding is monotone in priority: nothing shed outranks anything
        // kept. Without this the invariant above would still hold while the
        // scheduler shed high-EV candidates and kept research.
        let kept: Vec<Work> = std::iter::from_fn(|| s.serve()).collect();
        if let (Some(worst_kept), Some(best_shed)) = (kept.last(), shed.first()) {
            prop_assert!(
                best_shed <= worst_kept,
                "shed {:?} outranks the kept {:?}", best_shed, worst_kept
            );
        }
    }
}
