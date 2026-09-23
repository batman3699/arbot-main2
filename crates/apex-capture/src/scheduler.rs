//! Priority scheduling and preemption (§16.4, §29.4, §57.1.2). **INV-09.**
//!
//! Two claims, and they are different:
//!
//! - **FIFO is forbidden on the final capture path** (§46.1). Under overload a
//!   queue that serves in arrival order spends its capacity on whatever showed
//!   up first, which at 10x candidate rate is candidates -- and the live ticket
//!   behind them misses its deadline. `fifo_loses_the_workload_the_scheduler_
//!   survives` in `tests/preemption.rs` runs the same load both ways, because
//!   "priority scheduling helps" is otherwise an untested adjective.
//!
//! - **An authorized live ticket is never preempted.** Not "rarely", not "only
//!   under configured pressure". [`Scheduler::shed`] cannot shed one: the
//!   capacity argument is a target it will miss rather than a licence.
//!
//! When shedding cannot reach the target, the queue stays over capacity and the
//! caller sees it. That is the honest signal, and it is what drives §16.5's
//! remediation ladder -- shed, expand lanes, fail over, raise admission
//! thresholds -- rather than the queue quietly dropping the one thing that was
//! about to make money.

use apex_types::ids::TicketId;
use apex_types::ticket::TicketStatus;
use apex_types::time::UnixNanos;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// §16.4's five classes, in **service** order: `AuthorizedLiveTicket` first.
///
/// The derived `Ord` is the priority, so variant order is load-bearing -- do
/// not reorder these to group them prettily. Preemption is the reverse order,
/// which [`WorkClass::is_preemptible`] states rather than re-deriving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkClass {
    /// 5. Slow research / coverage auditing. Shed first.
    Research,
    /// 4. Route discovery.
    RouteDiscovery,
    /// 3. Exact simulations likely to become live tickets.
    ExactSimulation,
    /// 2. High-EV candidates inside the capture window.
    HighEvCandidate,
    /// 1. Already-authorized live tickets. **Never preempted.**
    AuthorizedLiveTicket,
}

impl WorkClass {
    /// §57.1.2's order, reversed: research → slow search → low-confidence
    /// simulations → low-EV candidates → high-EV candidates → **authorized
    /// live tickets: never**.
    pub const fn is_preemptible(self) -> bool {
        !matches!(self, Self::AuthorizedLiveTicket)
    }

    /// The class a ticket at this status belongs to. `Authorized` and later hold
    /// reserved execution resources (`TicketStatus::is_authorized_or_later`),
    /// which is the same line INV-09 draws -- so the two cannot drift apart.
    pub const fn of_ticket(status: TicketStatus) -> Self {
        if status.is_authorized_or_later() {
            Self::AuthorizedLiveTicket
        } else {
            Self::HighEvCandidate
        }
    }
}

/// What the scheduler schedules. Not necessarily a ticket: classes 3-5 are
/// search and research work that competes for the same compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Work {
    pub id: TicketId,
    pub class: WorkClass,
    /// When this becomes worthless. Earlier is more urgent.
    pub deadline: UnixNanos,
    pub expected_net_ev: i128,
}

/// §16.4's key: `(status_rank, deadline, −expected_net_ev)`.
///
/// `Ord` here means "more urgent", so the `BinaryHeap` (a max-heap) pops the
/// most urgent. Class dominates, then the earlier deadline, then the larger EV.
/// The id is the final tiebreak and exists only so the order is total: two
/// otherwise identical entries must not compare `Equal`, or the heap's
/// behaviour becomes an implementation detail nothing can test.
impl Ord for Work {
    fn cmp(&self, other: &Self) -> Ordering {
        self.class
            .cmp(&other.class)
            .then_with(|| other.deadline.0.cmp(&self.deadline.0))
            .then_with(|| self.expected_net_ev.cmp(&other.expected_net_ev))
            .then_with(|| other.id.0.cmp(&self.id.0))
    }
}

impl PartialOrd for Work {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Default)]
pub struct Scheduler {
    queue: BinaryHeap<Work>,
    admitted: u64,
    served: u64,
    shed: u64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn admit(&mut self, work: Work) {
        self.queue.push(work);
        self.admitted += 1;
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    pub const fn admitted(&self) -> u64 {
        self.admitted
    }
    pub const fn served(&self) -> u64 {
        self.served
    }
    pub const fn shed_count(&self) -> u64 {
        self.shed
    }

    pub fn peek(&self) -> Option<&Work> {
        self.queue.peek()
    }

    /// Take the most urgent work. **Not FIFO** -- that is the point (§46.1).
    ///
    /// Named `serve` rather than `next` because a `next(&mut self) -> Option<T>`
    /// reads as an iterator, and this is not one: a scheduler you can `for`-loop
    /// over invites draining a queue that is still being filled.
    pub fn serve(&mut self) -> Option<Work> {
        let w = self.queue.pop();
        if w.is_some() {
            self.served += 1;
        }
        w
    }

    /// Drop the least urgent work until the queue fits `capacity`, in §57.1.2
    /// order, returning what was shed (least urgent first).
    ///
    /// **Never sheds an authorized live ticket**, even when that leaves the
    /// queue above `capacity`. `capacity` is a target this will miss rather
    /// than a licence: an authorized ticket holds reserved resources and a
    /// counterparty's money is already committed behind it.
    pub fn shed(&mut self, capacity: usize) -> Vec<Work> {
        if self.queue.len() <= capacity {
            return Vec::new();
        }
        // Sorted most-urgent-first; the tail is what goes.
        let mut all = self.queue.drain().collect::<Vec<_>>();
        all.sort_unstable_by(|a, b| b.cmp(a));

        let mut shed = Vec::new();
        while all.len() > capacity {
            match all.last() {
                // Everything left is unsheddable. Stop, and leave the queue
                // over capacity rather than preempting it.
                Some(w) if !w.class.is_preemptible() => break,
                Some(_) => {
                    if let Some(w) = all.pop() {
                        shed.push(w);
                    }
                }
                None => break,
            }
        }
        self.shed += shed.len() as u64;
        self.queue = all.into_iter().collect();
        shed
    }

    /// §16.5. Dispatched before deadline / admitted for live dispatch.
    ///
    /// Reported over live tickets only -- shedding a research task is the
    /// scheduler working, not a capture miss, and folding the two together
    /// would let a busy research queue paper over a lost trade.
    pub fn u_capture(dispatched_in_time: u64, admitted_live: u64) -> f64 {
        if admitted_live == 0 {
            return 1.0;
        }
        dispatched_in_time as f64 / admitted_live as f64
    }
}
