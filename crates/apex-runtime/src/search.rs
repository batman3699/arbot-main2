//! `RouteSource`, assembled from `apex-search` (Task 2b.5).
//!
//! # Why the implementation is here and not in `apex-search`
//!
//! [`RouteSource`](crate::plane::RouteSource) is the plane's port, so the trait
//! lives with the plane and `apex-search` — four tiers above the runtime in
//! §6.1's graph — cannot implement it without reversing the dependency. What
//! `apex-search` provides is the parts: a `Frontier`, `EventEngine`,
//! `FiniteSizeEngine` and a `TemplatePricer`. Composing them into the port is
//! wiring, and wiring is what a control plane is.
//!
//! # The frontier is shared truth, so it is `Versioned`
//!
//! Search workers read the frontier concurrently and events mutate it —
//! `mark_profitable` on a win, an `Invalidate` when a tick transition stales a
//! carried attribute. **INV-11 / §5.3: no global mutable state as shared truth
//! between search workers; workers receive immutable snapshots or versioned read
//! handles.** A `RwLock<Frontier>` would put a lock on the read path where §2.4
//! says a reader must never wait behind a writer, so this uses
//! `apex_state::Versioned` — wait-free reads, copy-on-write updates.
//!
//! Copy-on-write is affordable here for a measured reason rather than an
//! optimistic one: the tradeable set on Base is **8–11 cross-venue pairs**, so
//! the structure being cloned holds tens of entries, not thousands. If the
//! frontier ever grows past that, this is the line that has to change, and the
//! comment is here so it is found.
//!
//! # The pipeline, in §12.1's order
//!
//! ```text
//! event → Frontier::revalue  (known routes first, and the permit proves it)
//!       → EventEngine        (which of §12.4's classes, and what it means)
//!       → FiniteSizeEngine   (does any size pay, and which)
//!       → RouteProposal
//! ```
//!
//! Broad discovery — Engine A — is **not** in this path. §2.6 puts it on the slow
//! lane, and this repository measured that 4× the cycles changed nothing.

use crate::plane::RouteSource;
use apex_math::finite_size::SearchBudget;
use apex_search::engine_c::{FiniteSizeEngine, TemplatePricer};
use apex_search::engine_d::{EventEngine, StaleAttribute};
use apex_search::frontier::{Frontier, RouteId, RouteProposal, RouteTemplate};
use apex_state::feed::event::StateEvent;
use apex_state::Versioned;
use apex_types::state::ReconstructionStatus;
use apex_types::time::UnixNanos;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// What a search run did, beyond the proposals themselves.
///
/// Exposed because §28's coverage auditor asks "what did the fast path not look
/// at", and the answer lives in these counters rather than in a log line.
#[derive(Debug, Default)]
pub struct SearchCounters {
    /// Events that produced no work, by Engine D's reckoning.
    pub skipped: AtomicU64,
    /// Templates the §29.3 budget did not reach.
    pub unpriced: AtomicU64,
    /// Templates priced and refused — the `no_profitable_size` bucket.
    pub declined: AtomicU64,
    /// Carried attributes staled by an event (tick, hook, fee).
    pub invalidated: AtomicU64,
}

impl SearchCounters {
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.skipped.load(Ordering::Relaxed),
            self.unpriced.load(Ordering::Relaxed),
            self.declined.load(Ordering::Relaxed),
            self.invalidated.load(Ordering::Relaxed),
        )
    }
}

/// Engine C for a frontier whose events are whole flashblocks: every template
/// an event touches, up to the `resident` set.
///
/// [`FiniteSizeEngine::measured`] prices eight per event, sized when an event
/// named the one pool a swap moved — about six templates on the live universe.
/// Since R17 an event is a flashblock, and one can move several pools: two
/// WETH/USDC pools touch ten templates, and eight left the most expensive
/// unpriced, 114 in the first five minutes. The resident set bounds the work
/// instead, and pricing a flashblock's templates once costs no more than
/// pricing each of its swaps' did.
pub fn flashblock_engine(resident: usize) -> FiniteSizeEngine {
    FiniteSizeEngine::new(resident, SearchBudget::default())
}

/// The fast path's candidate generation.
pub struct FrontierSearch {
    frontier: Versioned<Frontier>,
    provenance: ReconstructionStatus,
    events: EventEngine,
    finite: FiniteSizeEngine,
    pricer: Arc<dyn TemplatePricer + Send + Sync>,
    counters: SearchCounters,
}

impl FrontierSearch {
    /// `provenance` says where the templates came from, and `Versioned::new`
    /// demands it — *"callers must say which they mean"*, because defaulting to
    /// `Verified` authorizes trades on unverified state and defaulting to
    /// `Unsafe` makes the safe path the one you get by forgetting.
    ///
    /// For a frontier the honest answer depends on the **pool inventory** it was
    /// built from, and this repository has a specific reason to care:
    /// `base_venues_complete.yaml` was found to invent router and quoter
    /// addresses, and `scripts/ci/no_fabricated_venue_sources.sh` exists because
    /// of it. A frontier seeded from an unverified inventory is `Unsafe`.
    ///
    /// **Nothing consults this yet, and that is INV-08, which is open.** It is
    /// taken and exposed rather than omitted so the input exists when the gate is
    /// written; recording the gap is better than a half-enforcement that looks
    /// like one.
    pub fn new(
        frontier: Frontier,
        provenance: ReconstructionStatus,
        pricer: Arc<dyn TemplatePricer + Send + Sync>,
        events: EventEngine,
        finite: FiniteSizeEngine,
    ) -> Self {
        Self {
            frontier: Versioned::new(frontier, provenance),
            provenance,
            events,
            finite,
            pricer,
            counters: SearchCounters::default(),
        }
    }

    /// Where the resident templates came from. See [`Self::new`]: INV-08 is the
    /// gate that will consult this, and it is not yet written.
    pub const fn provenance(&self) -> ReconstructionStatus {
        self.provenance
    }

    /// Wiring-time only, before the search is shared. Returns `false` if the
    /// template is inconsistent, which is `Frontier::insert`'s refusal.
    pub fn seed(&self, template: RouteTemplate) -> bool {
        let mut next = (*self.frontier.load().value).clone();
        let ok = next.insert(template).is_ok();
        if ok {
            self.frontier.store(next, self.provenance);
        }
        ok
    }

    /// `hot_path.rs`'s write path: a route just made money.
    pub fn mark_profitable(&self, id: RouteId, at: UnixNanos) {
        let mut next = (*self.frontier.load().value).clone();
        if next.mark_profitable(id, at) {
            self.frontier.store(next, self.provenance);
        }
    }

    pub fn counters(&self) -> &SearchCounters {
        &self.counters
    }

    pub fn resident(&self) -> usize {
        self.frontier.load().value.len()
    }
}

#[async_trait::async_trait]
impl RouteSource for FrontierSearch {
    async fn propose(&self, event: &StateEvent) -> Vec<RouteProposal> {
        let snapshot = self.frontier.load();
        let frontier = &*snapshot.value;

        // §12.1: known routes first. The permit is the proof, and Engine C takes
        // it — so nothing here can reach pricing without having revalued.
        let response = self.events.respond(event, frontier);
        if response.skipped.is_some() {
            self.counters.skipped.fetch_add(1, Ordering::Relaxed);
        }

        // An invalidated template is not priced. Its carried attribute is stale,
        // so a price computed from it would be computed against the stale value --
        // which is the ~140 bps mechanism this repository already measured.
        if !response.invalidate.is_empty() {
            self.counters
                .invalidated
                .fetch_add(response.invalidate.len() as u64, Ordering::Relaxed);
            self.stale(&response.invalidate);
        }

        // No early return on an empty `revalue`. The first draft had one; a
        // mutation deleting it changed nothing, because `FiniteSizeEngine::propose`
        // over an empty hit list already produces an empty result. It was a
        // redundant shortcut reading like a guard, and a reader would have had to
        // work out which. Third one this phase.
        let out = self.finite.propose(
            &response.revalue,
            &response.permit,
            event,
            frontier,
            self.pricer.as_ref(),
        );
        self.counters.unpriced.fetch_add(out.unpriced.len() as u64, Ordering::Relaxed);
        self.counters.declined.fetch_add(out.declined.len() as u64, Ordering::Relaxed);
        out.proposals
    }
}

impl FrontierSearch {
    /// Drop staled templates from the resident set.
    ///
    /// **Removed rather than marked, for now, and that is a deviation worth
    /// stating.** Engine D's design note says an invalidated template should be
    /// *marked* so the recency signal a live run paid for is not lost. Doing that
    /// needs a `stale: Option<StaleAttribute>` field on `RouteTemplate`, which is
    /// a ninth attribute §12.1 does not list — so it waits for the task that
    /// refreshes templates, and until then the safe direction is to stop pricing
    /// them. Losing a recency score costs ranking; pricing against a stale tick
    /// costs money.
    fn stale(&self, invalidated: &[(RouteId, StaleAttribute)]) {
        let mut next = (*self.frontier.load().value).clone();
        for (id, _attr) in invalidated {
            next.remove(*id);
        }
        self.frontier.store(next, self.provenance);
    }
}
