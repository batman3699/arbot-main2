//! The event feed (§46.3), and §2.6's rule about the two paths.
//!
//! # The claim
//!
//! §2.6: both paths are mandatory and **the slow path never delays the fast
//! one** (BP-022). The legacy architecture had no such separation — §4.3 records
//! `scan_once` / `wait_for_scan_cadence` as a *polling* driver, and the measured
//! consequence was a 4.20 s median end-to-end against a 200 ms flashblock.
//!
//! # The mechanism, and the cost it imposes
//!
//! Each subscriber gets its own bounded queue and [`EventBus::publish`] uses a
//! non-blocking send. A subscriber that falls behind therefore **loses events**
//! rather than applying backpressure to the publisher. That is the only way to
//! honour §2.6: a shared queue means the slowest consumer sets the pace for
//! every other one, and a blocking send means a stalled coverage auditor can
//! stall the capture path.
//!
//! So the same mechanism that protects the fast path is the one that drops
//! slow-path work, and the drop must therefore be **counted and attributed**.
//! The two lanes mean different things by a drop:
//!
//! - **Slow lane.** Expected, and budgeted. §29.2 says to shed research first;
//!   a lagging coverage auditor is that shedding happening.
//! - **Fast lane.** A lost opportunity, and §16.6 puts unexplained pre-dispatch
//!   loss on the zero-tolerance list. [`EventBus::fast_lane_is_lossless`] is the
//!   predicate a health check reads, and it is separate from the slow-lane figure
//!   on purpose: one number covering both would let expected shedding mask a
//!   capture failure.
//!
//! # Subscribers are frozen once the bus is shared
//!
//! [`EventBus::subscribe`] takes `&mut self`. Once the bus is behind an `Arc` —
//! which is how workers hold it — `&mut` is unobtainable, so the subscriber set
//! cannot change while events are flowing. That is deliberate and it is not
//! merely tidiness: a lock around the subscriber list would put a lock on the
//! publish path, where §2.4 says a reader must never wait behind a writer.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// §2.6's two paths. Which lane a subscriber is on decides what a dropped event
/// *means*, which is the only reason the bus needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Lane {
    /// The capture path: search, pricing, sizing, simulation, dispatch.
    Fast,
    /// Broad search, coverage auditing, research. §29.2 sheds this first.
    Slow,
}

/// §12.4's event vocabulary, defined in `apex-state` and re-exported here.
///
/// It lived in this module until Task 2b.1, which was wrong in a way that only
/// showed up when `apex-search` came to exist: §6.1 runs `apex-state → … →
/// apex-search → … → apex-runtime`, so a `CandidateSource` implemented four
/// tiers above the runtime cannot take a type defined in it. `apex_types::ack`
/// had already made the same argument for `LifecycleStage`.
///
/// The re-export is for callers who think of an event as something that arrives
/// on this bus, which is most of them. `scripts/ci/crate_dependency_direction.sh`
/// keeps the definition where it belongs — cargo will not, because one
/// wrong-direction edge is not a cycle.
pub use apex_state::feed::event::{EventClass, EventKind, StateEvent};

struct Subscriber {
    name: String,
    lane: Lane,
    tx: mpsc::Sender<Arc<StateEvent>>,
    dropped: AtomicU64,
}

/// A subscriber's end of the feed.
pub struct Subscription {
    rx: mpsc::Receiver<Arc<StateEvent>>,
    sub: Arc<Subscriber>,
}

impl Subscription {
    pub fn name(&self) -> &str {
        &self.sub.name
    }

    pub fn lane(&self) -> Lane {
        self.sub.lane
    }

    /// How many events this subscriber was sent but could not hold.
    pub fn dropped(&self) -> u64 {
        self.sub.dropped.load(Ordering::SeqCst)
    }

    pub async fn recv(&mut self) -> Option<Arc<StateEvent>> {
        self.rx.recv().await
    }

    /// Non-blocking, for a worker that is polling several sources.
    pub fn try_recv(&mut self) -> Option<Arc<StateEvent>> {
        self.rx.try_recv().ok()
    }
}

#[derive(Debug)]
pub struct EventBus {
    subscribers: Vec<Arc<Subscriber>>,
    capacity: usize,
    published: AtomicU64,
}

impl std::fmt::Debug for Subscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriber")
            .field("name", &self.name)
            .field("lane", &self.lane)
            .field("dropped", &self.dropped.load(Ordering::SeqCst))
            .finish()
    }
}

impl EventBus {
    /// `capacity` is the per-subscriber queue depth, not a shared budget.
    pub const fn with_capacity(capacity: usize) -> Self {
        Self { subscribers: Vec::new(), capacity, published: AtomicU64::new(0) }
    }

    /// Takes `&mut self`: see the module header. Subscribe during wiring.
    pub fn subscribe(&mut self, name: &str, lane: Lane) -> Subscription {
        let (tx, rx) = mpsc::channel(self.capacity.max(1));
        let sub = Arc::new(Subscriber {
            name: name.to_string(),
            lane,
            tx,
            dropped: AtomicU64::new(0),
        });
        self.subscribers.push(Arc::clone(&sub));
        Subscription { rx, sub }
    }

    /// Fan out. **Never awaits**, which is the whole property: a publisher that
    /// could await is a publisher a stalled subscriber can stop.
    pub fn publish(&self, event: StateEvent) {
        let event = Arc::new(event);
        self.published.fetch_add(1, Ordering::SeqCst);
        for sub in &self.subscribers {
            // `try_send` rather than `send`. A `Full` queue is a drop we count;
            // a `Closed` one is a subscriber that has gone away, which is also a
            // drop from the bus's point of view -- the event reached nobody.
            if sub.tx.try_send(Arc::clone(&event)).is_err() {
                sub.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::SeqCst)
    }

    /// Events lost on one lane. Summed across that lane's subscribers, because
    /// a lane is a *class* of work and two coverage auditors both lagging is one
    /// fact about the slow path.
    pub fn dropped(&self, lane: Lane) -> u64 {
        self.subscribers
            .iter()
            .filter(|s| s.lane == lane)
            .map(|s| s.dropped.load(Ordering::SeqCst))
            .sum()
    }

    /// §16.6's zero-tolerance question, asked of the feed. Kept separate from
    /// [`Self::dropped`] so that expected slow-path shedding cannot mask it.
    pub fn fast_lane_is_lossless(&self) -> bool {
        self.dropped(Lane::Fast) == 0
    }

    /// Per-subscriber loss, for the operator who has to decide which worker is
    /// behind. A lane total says the slow path is losing work; this says which
    /// of the four slow workers it is.
    pub fn loss_by_subscriber(&self) -> Vec<(String, Lane, u64)> {
        self.subscribers
            .iter()
            .map(|s| (s.name.clone(), s.lane, s.dropped.load(Ordering::SeqCst)))
            .collect()
    }
}
