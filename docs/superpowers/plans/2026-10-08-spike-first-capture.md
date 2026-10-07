# Spike-first capture (Task 8.5 R21) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The shadow's capture path handles a spike's proposals best-first from one queue, drops proposals too old to simulate, skips pools a dispatched ticket just traded, and sizes within the lender's holding.

**Architecture:** Engine C's proposals carry the net they were found at. The plane exposes `propose` and `handle_proposal` separately (`on_event` composes them, unchanged). A new `shadow::queue::PendingQueue` sits in the shadow's capture loop: every waiting event is proposed and merged before each pick, and the best entry is handled. `LivePricer` caps every cycle's input at a watched lender holding, which the head loop refreshes from the reader's view.

**Tech Stack:** Rust (tokio, alloy/ethers U256), cargo tests, the repository's CI gates.

Spec: `docs/superpowers/specs/2026-10-08-spike-first-capture-design.md`.

## Global Constraints

- `MAX_PROPOSAL_AGE` is 1 s (half the 2 s dispatch deadline), and an entry is stale only when strictly older.
- Ordering: highest `net_hint` first, `None` lowest; ties go to the newest `found_at`, then the lowest route hash.
- An unread lender holding (`None`) sizes nothing; a pricer with no lender handle keeps today's behaviour.
- Parallel Tier 2 is out of scope (operator decision, 2026-10-08).
- Never `cargo fmt`. Use `-j 3` for every cargo command. Stage named paths only, then check `git diff --stat -- <paths>` is empty. Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Never print `.env` secrets. Do not touch the running shadow until the rollout step.

---

### Task 1: Proposals carry the search's net

**Files:**
- Modify: `crates/apex-search/src/frontier.rs` (`RouteProposal`, and its construction in this file)
- Modify: `crates/apex-search/src/engine_c.rs` (the `proposals.push`)
- Modify (add `net_hint: None`): `crates/apex-search/src/engine_a.rs`, `crates/apex-search/src/lib.rs`, `crates/apex-runtime/src/plane.rs`, `crates/apex-runtime/tests/support/mod.rs` (2 sites), `crates/apex-runtime/tests/live_econ.rs`, `crates/apex-runtime/tests/live_pricing.rs`
- Test: `crates/apex-search/tests/finite_size_search.rs`

**Interfaces:**
- Produces: `RouteProposal::net_hint: Option<alloy_primitives::U256>`, set by Engine C to the `Surplus::Gain` it found, `None` elsewhere.

- [ ] **Step 1: Write the failing test** (append to `finite_size_search.rs`; add `Surplus` to its `apex_math::finite_size` import and `use apex_types::compat::u256_to_alloy;` if absent)

```rust
/// R21: a proposal carries the net its size was found at, so the capture loop
/// can take a spike's most valuable proposal first.
#[test]
fn a_proposal_carries_the_net_its_size_was_found_at() {
    let route = route_at(200, GAS);
    let Surplus::Gain(expected) = best_size(&route, SearchBudget::default()).expect("pays").net else {
        panic!("a proposal is a gain");
    };
    let mut frontier = Frontier::new();
    frontier.insert(template(1, 3_000)).expect("consistent");
    let (hits, permit) = frontier.revalue(&swap());
    let pricer = Fixture { routes: BTreeMap::from([(RouteId(1), route)]) };
    let out = FiniteSizeEngine::measured().propose(&hits, &permit, &swap(), &frontier, &pricer);
    assert_eq!(out.proposals[0].net_hint, Some(u256_to_alloy(expected)));
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -j 3 -p apex-search --test finite_size_search a_proposal_carries_the_net`
Expected: compile error, no field `net_hint` on `RouteProposal`.

- [ ] **Step 3: Add the field** after `size_hint` in `frontier.rs`:

```rust
    /// Engine C's net at `size_hint`, in the start token's wei: what the shadow's
    /// capture loop orders a spike's proposals by (R21). An ordering input only,
    /// exactly as `size_hint` is: the economics re-evaluates every proposal
    /// against the book as it then stands. `None` from an engine that does not
    /// size.
    pub net_hint: Option<U256>,
```

In `engine_c.rs`, import `Surplus` alongside `NoSize, SearchBudget, SizedOpportunity`, and set it in the `proposals.push`:

```rust
                        size_hint: Some(u256_to_alloy(best.amount_in)),
                        // `Ok` is a gain (see above), so a loss arm cannot
                        // happen; `None` keeps the type honest if it ever did.
                        net_hint: match best.net {
                            Surplus::Gain(g) => Some(u256_to_alloy(g)),
                            Surplus::Loss(_) => None,
                        },
```

Add `net_hint: None,` after `size_hint` at every other construction site listed under Files (`grep -rn "size_hint:" crates --include=*.rs` lists them).

- [ ] **Step 4: Run the tests**

Run: `cargo test -j 3 -p apex-search --all-targets && cargo test -j 3 -p apex-runtime --test end_to_end --test live_econ --test live_pricing`
Expected: all pass.

(Commits happen in Task 6, after the gates.)

---

### Task 2: The plane proposes and handles separately

**Files:**
- Modify: `crates/apex-runtime/src/plane.rs` (`on_event`, `handle`)
- Test: `crates/apex-runtime/tests/end_to_end.rs`

**Interfaces:**
- Produces:
  - `pub async fn propose(&self, event: &StateEvent) -> Result<Vec<RouteProposal>, Handled>`
  - `pub async fn handle_proposal(&self, event: &StateEvent, proposal: &RouteProposal) -> Handled` (today's private `handle`, renamed)
  - `pub fn decline_stale(&self, proposal: &RouteProposal, age: DurationNanos) -> Handled`

- [ ] **Step 1: Write the failing tests** (append to `end_to_end.rs`; import `apex_types::miss::MissReason` and `apex_types::time::DurationNanos` if absent)

```rust
/// R21: proposing and handling are separable, and together they are `on_event`.
#[tokio::test]
async fn proposing_then_handling_is_what_on_event_does() {
    let plane = landing_plane();
    plane.boot(&NoChain, BOOT).expect("boot");
    let stream = recorded_stream();
    let event = stream.first().expect("an event");

    let proposals = plane.propose(event).await.expect("a fresh observation proposes");
    assert_eq!(proposals.len(), 1);
    let handled = plane.handle_proposal(event, &proposals[0]).await;
    assert!(matches!(handled, Handled::Closed { .. }), "{handled:?}");

    match plane.propose(event).await {
        Err(Handled::Redelivered { .. }) => {}
        other => panic!("the same observation again is a redelivery: {other:?}"),
    }
}

/// R21: a proposal too old to simulate is filed as a stale-state miss.
#[tokio::test]
async fn a_stale_proposal_is_filed_as_a_miss() {
    let plane = landing_plane();
    plane.boot(&NoChain, BOOT).expect("boot");
    let event = recorded_stream().remove(0);
    let proposals = plane.propose(&event).await.expect("proposes");

    let handled = plane.decline_stale(&proposals[0], DurationNanos(1_500_000_000));
    assert!(matches!(handled, Handled::Declined(Decline::StaleState { .. })), "{handled:?}");
    let misses = plane.drain_misses();
    assert_eq!(misses.len(), 1);
    assert_eq!(misses.misses()[0].record.reason, MissReason::StaleState);
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -j 3 -p apex-runtime --test end_to_end proposing_then_handling a_stale_proposal`
Expected: compile errors, no method `propose` / `handle_proposal` / `decline_stale` on `Plane`.

- [ ] **Step 3: Split `on_event`** (`plane.rs` ~916–950):

```rust
    /// One event, end to end, for every proposal it produces.
    pub async fn on_event(&self, event: &StateEvent) -> Vec<Handled> {
        let proposals = match self.propose(event).await {
            Ok(p) => p,
            Err(handled) => return vec![handled],
        };
        let mut out = Vec::with_capacity(proposals.len());
        for proposal in &proposals {
            out.push(self.handle_proposal(event, proposal).await);
        }
        out
    }

    /// What `event` proposes, or why it proposes nothing: a redelivery, or a
    /// candidate-generation budget that is spent. The shadow's capture loop
    /// queues these and handles the best first (R21); `on_event` handles them
    /// in order.
    pub async fn propose(&self, event: &StateEvent) -> Result<Vec<RouteProposal>, Handled> {
        // The cheapest check first, and before any budget is spent: has this
        // exact observation already been handled? See `SeenEvents`.
        if !self.seen.take(event.chain, event.at) {
            return Err(Handled::Redelivered { chain: event.chain, at: event.at });
        }
        // §29.3: candidate generation is a bounded class. Reserved before the
        // search runs, so an overloaded process refuses work loudly instead of
        // queuing it.
        let Some(_permit) = self.budgets.reserve(ResourceClass::CandidateGeneration) else {
            let d = Decline::NoBudget(ResourceClass::CandidateGeneration);
            return Err(Handled::Declined(self.file(None, &d)));
        };
        Ok(self.ports.search.propose(event).await)
    }

    /// A proposal too old to be worth a simulation, filed as the stale-state
    /// miss it is (R21): it would reach Tier 2 past its deadline.
    pub fn decline_stale(&self, proposal: &RouteProposal, age: DurationNanos) -> Handled {
        Handled::Declined(self.file_proposal(proposal, &Decline::StaleState { age }))
    }
```

Rename `async fn handle(&self, event: &StateEvent, proposal: &RouteProposal) -> Handled` to `pub async fn handle_proposal(...)`, keeping its body and doc comment. Import `DurationNanos` from `apex_types::time` if `plane.rs` does not already.

- [ ] **Step 4: Run the plane's tests**

Run: `cargo test -j 3 -p apex-runtime --test end_to_end --test shadow --test real_search --test real_commitments`
Expected: all pass, the two new tests included.

---

### Task 3: The pending queue

**Files:**
- Create: `crates/apex-runtime/src/shadow/queue.rs`
- Modify: `crates/apex-runtime/src/shadow/mod.rs` (`pub mod queue;`)
- Test: `crates/apex-runtime/tests/shadow_queue.rs`

**Interfaces:**
- Consumes: `RouteProposal::net_hint` (Task 1).
- Produces: `MAX_PROPOSAL_AGE: DurationNanos`; `Pending { event: Arc<StateEvent>, proposal: RouteProposal }`; `Popped { stale: Vec<(Pending, DurationNanos)>, best: Option<Pending> }`; `PendingQueue` with `merge(&mut self, &Arc<StateEvent>, Vec<RouteProposal>)`, `pop_best(&mut self, UnixNanos) -> Popped`, `drop_conflicting(&mut self, &[Address]) -> usize`, `clear(&mut self) -> usize`, `len`, `is_empty`, `max_depth`; `pools_of(&RouteProposal) -> Vec<Address>`.

- [ ] **Step 1: Write the failing tests** in `shadow_queue.rs`:

```rust
//! R21's queue: best first, newest replaces, old drops, our own pools skip.

use alloy_primitives::{Address, B256, U256};
use apex_runtime::shadow::queue::{pools_of, PendingQueue, MAX_PROPOSAL_AGE};
use apex_search::frontier::{ProposalOrigin, RouteProposal};
use apex_state::feed::event::StateEvent;
use apex_types::time::UnixNanos;
use std::sync::Arc;

const T0: u64 = 1_000_000_000_000;

fn event() -> Arc<StateEvent> {
    Arc::new(apex_search::frontier::doc_event())
}

/// A proposal for route `id` over pools `pools`, found at `found_ns`, worth `net`.
fn proposal(id: u8, pools: &[u8], found_ns: u64, net: Option<u64>) -> RouteProposal {
    let mut p = apex_runtime::shadow::queue::test_proposal(B256::repeat_byte(id), &pools.iter().map(|b| Address::repeat_byte(*b)).collect::<Vec<_>>());
    p.found_at = UnixNanos(found_ns);
    p.net_hint = net.map(U256::from);
    p.origin = ProposalOrigin::FiniteSize;
    p
}

#[test]
fn the_most_valuable_proposal_is_handled_first() {
    let mut q = PendingQueue::default();
    q.merge(&event(), vec![proposal(1, &[1], T0, Some(5)), proposal(2, &[2], T0, Some(1_798)), proposal(3, &[3], T0, None)]);
    let order: Vec<B256> = std::iter::from_fn(|| q.pop_best(UnixNanos(T0)).best).map(|p| p.proposal.route.route_hash).collect();
    assert_eq!(order, vec![B256::repeat_byte(2), B256::repeat_byte(1), B256::repeat_byte(3)]);
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
    q.merge(&event(), vec![proposal(7, &[1], T0, Some(4)), proposal(5, &[2], T0 + 1, Some(4)), proposal(6, &[3], T0 + 1, Some(4))]);
    let order: Vec<B256> = std::iter::from_fn(|| q.pop_best(UnixNanos(T0 + 1)).best).map(|p| p.proposal.route.route_hash).collect();
    assert_eq!(order, vec![B256::repeat_byte(5), B256::repeat_byte(6), B256::repeat_byte(7)]);
}

#[test]
fn proposals_sharing_a_traded_pool_are_dropped() {
    let mut q = PendingQueue::default();
    q.merge(&event(), vec![proposal(1, &[1, 2], T0, Some(1)), proposal(2, &[2, 3], T0, Some(1)), proposal(3, &[4, 5], T0, Some(1))]);
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
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -j 3 -p apex-runtime --test shadow_queue`
Expected: compile error, no module `queue` in `shadow`.

- [ ] **Step 3: Write the module** `crates/apex-runtime/src/shadow/queue.rs`:

```rust
//! The capture loop's queue of proposals (Task 8.5 R21).
//!
//! A spike produces dozens of proposals at once. Handled one event at a time,
//! in the search's fee order, the most valuable waited behind the cheapest:
//! at 10:07 UTC on 2026-10-07 a $17.98 candidate was simulated fourth, 1.2 s
//! late, and later ones past their deadline. So the loop keeps one queue, merges
//! every waiting event's proposals into it before each pick, and handles the
//! best.

use alloy_primitives::{Address, B256};
use apex_search::frontier::RouteProposal;
use apex_state::feed::event::StateEvent;
use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::BTreeMap;
use std::sync::Arc;

/// How old a proposal may be and still be simulated: half its 2 s dispatch
/// deadline. Older, it would reach Tier 2 too late to be sent in time. A route
/// that still pays is proposed again by the next event that moves its pools.
pub const MAX_PROPOSAL_AGE: DurationNanos = DurationNanos(1_000_000_000);

/// A proposal and the event that produced it.
#[derive(Clone, Debug)]
pub struct Pending {
    pub event: Arc<StateEvent>,
    pub proposal: RouteProposal,
}

/// What one pick took out of the queue.
#[derive(Debug, Default)]
pub struct Popped {
    /// Entries too old to simulate, each with its age: misses to file.
    pub stale: Vec<(Pending, DurationNanos)>,
    /// The entry to handle now.
    pub best: Option<Pending>,
}

/// One entry per route, the newest proposal for it.
#[derive(Debug, Default)]
pub struct PendingQueue {
    by_route: BTreeMap<B256, Pending>,
    max_depth: usize,
}

impl PendingQueue {
    /// Add an event's proposals. A route already queued takes the newer
    /// proposal: it was priced against newer state.
    pub fn merge(&mut self, event: &Arc<StateEvent>, proposals: Vec<RouteProposal>) {
        for proposal in proposals {
            let key = proposal.route.route_hash;
            let newer = self.by_route.get(&key).is_none_or(|q| proposal.found_at.0 >= q.proposal.found_at.0);
            if newer {
                self.by_route.insert(key, Pending { event: Arc::clone(event), proposal });
            }
        }
        self.max_depth = self.max_depth.max(self.by_route.len());
    }

    /// Remove every entry older than [`MAX_PROPOSAL_AGE`] at `now`, then the
    /// best of the rest: the highest `net_hint` (`None` lowest), then the
    /// newest, then the lowest route hash.
    pub fn pop_best(&mut self, now: UnixNanos) -> Popped {
        let age = |p: &Pending| now.0.saturating_sub(p.proposal.found_at.0);
        let old: Vec<B256> =
            self.by_route.iter().filter(|(_, p)| age(p) > MAX_PROPOSAL_AGE.0).map(|(k, _)| *k).collect();
        let stale = old
            .into_iter()
            .filter_map(|k| self.by_route.remove(&k))
            .map(|p| {
                let a = DurationNanos(age(&p));
                (p, a)
            })
            .collect();
        let best = self
            .by_route
            .iter()
            .max_by(|(ka, a), (kb, b)| {
                (a.proposal.net_hint, a.proposal.found_at.0)
                    .cmp(&(b.proposal.net_hint, b.proposal.found_at.0))
                    .then_with(|| kb.cmp(ka))
            })
            .map(|(k, _)| *k)
            .and_then(|k| self.by_route.remove(&k));
        Popped { stale, best }
    }

    /// Remove every entry whose route shares a pool with `traded`: a ticket
    /// just dispatched there, and these would race it. Returns how many.
    pub fn drop_conflicting(&mut self, traded: &[Address]) -> usize {
        let before = self.by_route.len();
        self.by_route.retain(|_, p| !pools_of(&p.proposal).iter().any(|a| traded.contains(a)));
        before - self.by_route.len()
    }

    /// Empty the queue; returns how many entries it held.
    pub fn clear(&mut self) -> usize {
        let n = self.by_route.len();
        self.by_route.clear();
        n
    }

    pub fn len(&self) -> usize {
        self.by_route.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_route.is_empty()
    }

    /// The most entries the queue has held at once.
    pub const fn max_depth(&self) -> usize {
        self.max_depth
    }
}

/// The pools a proposal's route trades through.
pub fn pools_of(p: &RouteProposal) -> Vec<Address> {
    p.route.hops.iter().map(|h| h.pool.address).collect()
}
```

Add the queue tests' helper at the end of `queue.rs` (`RouteHop { venue, pool, token_in, token_out, fee_ppm }` and `PoolId`/`TokenId { chain, address }` are `apex_types`'):

```rust
/// A proposal over `pools` with route hash `hash`, everything else empty: for
/// the queue's tests, which judge only hashes, pools, times and nets.
#[doc(hidden)]
pub fn test_proposal(hash: B256, pools: &[Address]) -> RouteProposal {
    let mut p = RouteProposal {
        chain: apex_types::ids::ChainId(8453),
        route: apex_types::route::RouteCommitment {
            hops: Vec::new(),
            complexity_cost: apex_types::route::ComplexityCost {
                hops: 2,
                external_calls: 2,
                calldata_bytes: 0,
                state_deps: 0,
                tick_crossings: 0,
                hooks: 0,
                gas_estimate: 0,
                failure_surface: 0.0,
            },
            route_hash: hash,
        },
        venue_set: Vec::new(),
        state_fingerprint: apex_search::frontier::doc_event().fingerprint,
        found_at: UnixNanos(0),
        origin: apex_search::frontier::ProposalOrigin::FiniteSize,
        flash_source: None,
        size_hint: None,
        net_hint: None,
    };
    let chain = apex_types::ids::ChainId(8453);
    for a in pools {
        // One hop per pool: the queue reads only its address.
        p.route.hops.push(apex_types::route::RouteHop {
            venue: apex_types::ids::VenueId(1),
            pool: apex_types::ids::PoolId { chain, address: *a },
            token_in: apex_types::ids::TokenId { chain, address: Address::ZERO },
            token_out: apex_types::ids::TokenId { chain, address: Address::ZERO },
            fee_ppm: 0,
        });
    }
    p
}
```

Add `pub mod queue;` to `crates/apex-runtime/src/shadow/mod.rs` beside its other `mod` lines.

- [ ] **Step 4: Run the tests**

Run: `cargo test -j 3 -p apex-runtime --test shadow_queue`
Expected: 7 passed.

---

### Task 4: The capture loop runs through the queue

**Files:**
- Modify: `crates/apex-runtime/src/shadow/mod.rs` (`capture`, `Stats`, `Report`, report assembly)
- Modify: `scripts/shadow-status.sh`

**Interfaces:**
- Consumes: `Plane::{propose, handle_proposal, decline_stale}` (Task 2); `PendingQueue`, `pools_of` (Task 3); `BusSubscription::{recv, try_recv}` (`crates/apex-runtime/src/bus.rs`).
- Produces: `Report.queue: QueueReport { stale: u64, conflicts: u64, max_depth: u64 }`.

- [ ] **Step 1: Replace `capture`** (`shadow/mod.rs` ~594–604):

```rust
    /// Events into the plane, while the book can be priced from — through one
    /// queue, best first (R21; see `queue`).
    async fn capture(&self, sub: &mut BusSubscription) {
        let mut queue = PendingQueue::default();
        loop {
            // Wait only when there is nothing to do.
            if queue.is_empty() {
                let Some(ev) = sub.recv().await else { return };
                self.take(&mut queue, ev).await;
            }
            // Everything already waiting, before choosing.
            while let Some(ev) = sub.try_recv() {
                self.take(&mut queue, ev).await;
            }
            // Priced against a book being rebuilt: dropped, not handled. A route
            // that still pays is proposed again once the book is verified.
            if self.book.status() != ReconstructionStatus::Verified {
                queue.clear();
                continue;
            }
            let popped = queue.pop_best(SystemClock.now());
            let stale: Vec<Handled> =
                popped.stale.iter().map(|(p, age)| self.plane.decline_stale(&p.proposal, *age)).collect();
            self.stats.queue_stale.fetch_add(stale.len() as u64, Ordering::Relaxed);
            self.funnel.record(&stale);
            self.stats.queue_max_depth.fetch_max(queue.max_depth() as u64, Ordering::Relaxed);
            let Some(best) = popped.best else { continue };
            let handled = self.plane.handle_proposal(&best.event, &best.proposal).await;
            if dispatched(&handled) {
                let n = queue.drop_conflicting(&pools_of(&best.proposal));
                self.stats.queue_conflicts.fetch_add(n as u64, Ordering::Relaxed);
            }
            self.funnel.record(std::slice::from_ref(&handled));
        }
    }

    /// One event: counted, then proposed and queued while the book is verified.
    async fn take(&self, queue: &mut PendingQueue, ev: Arc<StateEvent>) {
        self.funnel.event();
        if self.book.status() != ReconstructionStatus::Verified {
            self.funnel.skipped_unverified();
            return;
        }
        match self.plane.propose(&ev).await {
            Ok(proposals) => queue.merge(&ev, proposals),
            Err(handled) => self.funnel.record(std::slice::from_ref(&handled)),
        }
    }
```

and, as a free function in the same file:

```rust
/// A ticket that went to a dispatcher: its pools are about to move.
fn dispatched(h: &Handled) -> bool {
    matches!(h, Handled::Closed { outcome, .. }
        if matches!(**outcome, TicketOutcome::Success { .. } | TicketOutcome::ShadowDispatched { .. }))
}
```

Imports: `crate::shadow::queue::{pools_of, PendingQueue}` (or `queue::...` from within the module), `TicketOutcome` from wherever `plane.rs` imports it (`grep -n "TicketOutcome" crates/apex-runtime/src/plane.rs | head -3`), `StateEvent` and `Arc` if absent.

- [ ] **Step 2: Counters and report.** In `Stats` add `queue_stale: AtomicU64, queue_conflicts: AtomicU64, queue_max_depth: AtomicU64` (it derives `Default`). After `NearMissReport`'s struct add:

```rust
/// R21's queue: what it dropped, and how deep it got.
#[derive(Clone, Debug, Serialize)]
pub struct QueueReport {
    /// Proposals older than `MAX_PROPOSAL_AGE` when picked, filed as misses.
    pub stale: u64,
    /// Proposals sharing a pool with a ticket just dispatched.
    pub conflicts: u64,
    pub max_depth: u64,
}
```

add `pub queue: QueueReport,` to `Report` after `near_miss`, and in the report assembly:

```rust
            queue: QueueReport {
                stale: get(&self.stats.queue_stale),
                conflicts: get(&self.stats.queue_conflicts),
                max_depth: get(&self.stats.queue_max_depth),
            },
```

- [ ] **Step 3: Show it.** In `scripts/shadow-status.sh`, after the line that prints the feed's health, add (same indentation as its neighbours):

```python
    q = r.get("queue")
    if q:
        print(f"  queue       {q['stale']} dropped as stale, {q['conflicts']} as conflicts, deepest {q['max_depth']}")
```

- [ ] **Step 4: Build and run the shadow's tests**

Run: `cargo build -j 3 -p apex-runtime && cargo test -j 3 -p apex-runtime --test shadow --test shadow_parts --test shadow_queue`
Expected: builds; all pass. `bash -n scripts/shadow-status.sh` reports nothing.

---

### Task 5: Sizing within the lender's holding

**Files:**
- Modify: `crates/apex-runtime/src/live/pricing.rs` (`CostedCycle`, `LivePricer`)
- Modify: `crates/apex-runtime/src/shadow/mod.rs` (boot wiring, `Shadow`, `head_loop`)
- Test: `crates/apex-runtime/tests/live_pricing.rs`

**Interfaces:**
- Produces: `pub type LenderHolding = tokio::sync::watch::Receiver<Option<U256>>` (ethers `U256`); `LivePricer::with_lender(self, LenderHolding) -> Self`; `CostedCycle::capped(self, Option<U256>) -> Self`.

- [ ] **Step 1: Write the failing tests** (append to `live_pricing.rs`)

```rust
/// R21: a route sizes within what the lender holds. The gap of
/// `a_gap_wider_than_the_fees_has_a_profitable_size`, capped at half the size
/// it finds unbounded.
#[test]
fn a_route_sizes_within_the_lenders_holding() {
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), pool(SLIP, Venue::Slipstream, -197_300, 80, L)]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let free = LivePricer::new(b.clone(), cycles.clone(), flat(3_000_000_000_000), gas::MEASURED);
    let (id, unbounded) = cycles
        .keys()
        .find_map(|id| free.best_size(*id, &fp(), budget()).ok().map(|s| (*id, s.amount_in)))
        .expect("one direction pays");
    let cap = unbounded / 2;
    let (_tx, rx) = tokio::sync::watch::channel(Some(cap));
    let capped = LivePricer::new(b, cycles, flat(3_000_000_000_000), gas::MEASURED).with_lender(rx);
    let sized = capped.best_size(id, &fp(), budget()).expect("still pays at half the size");
    assert!(sized.amount_in <= cap, "{} > {}", sized.amount_in, cap);
}

/// R21: an unread lender holding sizes nothing, as an unread pool balance does.
#[test]
fn an_unread_lender_sizes_nothing() {
    let b = book(vec![pool(UNI, Venue::UniswapV3, -197_350, 500, L), pool(SLIP, Venue::Slipstream, -197_300, 80, L)]);
    let (_, cycles) = frontier::build(BASE, WETH, &b.snapshot());
    let (_tx, rx) = tokio::sync::watch::channel(None);
    let pricer = LivePricer::new(b, cycles.clone(), flat(3_000_000_000_000), gas::MEASURED).with_lender(rx);
    assert!(cycles.keys().all(|id| pricer.best_size(*id, &fp(), budget()).is_err()));
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -j 3 -p apex-runtime --test live_pricing lender`
Expected: compile error, no method `with_lender`.

- [ ] **Step 3: The cap.** In `pricing.rs`:

```rust
/// What the lender holds of the start token, as last read: the most any cycle
/// may borrow (R21). `None` until read, which sizes nothing.
pub type LenderHolding = tokio::sync::watch::Receiver<Option<U256>>;
```

`CostedCycle` gains `cap: Option<U256>` (initialised `None` in `new`) and:

```rust
    /// Bound every size at `cap` as well as at the paying pool's holding: the
    /// lender's holding, for a cycle that borrows (R21).
    #[must_use]
    pub const fn capped(mut self, cap: Option<U256>) -> Self {
        self.cap = cap;
        self
    }
```

and its `SizedRoute::max_input` becomes:

```rust
    fn max_input(&self) -> U256 {
        let held = self.cycle.max_input();
        self.cap.map_or(held, |c| held.min(c))
    }
```

`LivePricer` gains `lender: Option<LenderHolding>` (`None` in `new`), and:

```rust
    /// Size every cycle within the lender's holding as `lender` reports it.
    /// Every live cycle borrows from Balancer (`frontier::BALANCER_FLASH`), so
    /// the cap applies to all of them. Without this, sizing is bounded only by
    /// the paying pool, and an optimum above the lender's holding is refused
    /// whole by the risk gate rather than traded smaller.
    #[must_use]
    pub fn with_lender(mut self, lender: LenderHolding) -> Self {
        self.lender = Some(lender);
        self
    }
```

and in `live`:

```rust
        let cap = self.lender.as_ref().map(|l| (*l.borrow()).unwrap_or_default());
        Some(CostedCycle::new(cycle, self.costs(), self.gas).capped(cap))
```

- [ ] **Step 4: Run the pricing tests**

Run: `cargo test -j 3 -p apex-runtime --test live_pricing`
Expected: all pass, the two new tests included.

- [ ] **Step 5: Wire it in the shadow.** In `shadow/mod.rs`, after the boot read of `holding`:

```rust
    // R21: what the lender holds, for sizing. Seeded with the boot read; the
    // head loop replaces it after every view the reader takes.
    let (lender, lender_now) = watch::channel(Some(ethers_core::types::U256::from(holding)));
```

build the pricer with `.with_lender(lender_now)` before `Arc::new`, add `lender: watch::Sender<Option<ethers_core::types::U256>>,` to `Shadow` (initialised `lender,`), and in `head_loop` after the `view.note(... self.reader.refresh(...) ...)` line:

```rust
            if let Some(v) = self.reader.view() {
                let held = Some(ethers_core::types::U256::from(v.lender_holding));
                self.lender.send_if_modified(|now| {
                    let moved = *now != held;
                    *now = held;
                    moved
                });
            }
```

- [ ] **Step 6: Build and run the shadow's tests**

Run: `cargo build -j 3 -p apex-runtime && cargo test -j 3 -p apex-runtime --test shadow --test shadow_parts`
Expected: builds; all pass.

---

### Task 6: Mutation checks, gates, commits

**Files:**
- Modify: `PLAN.md` (R21 entry after R20)
- Create (scratchpad, not committed): `<scratchpad>/gates.sh`

- [ ] **Step 1: Mutation checks.** Apply each mutant alone, run the named test with `--no-fail-fast`, confirm it fails, restore the file and `touch` it (memory: restoring leaves stale builds):
  1. `pop_best`: `.max_by(` → `.min_by(` → `shadow_queue` fails.
  2. `pop_best`: `age(p) > MAX_PROPOSAL_AGE.0` → `>=` → `a_proposal_is_stale_only_past_one_second` fails.
  3. `merge`: `>=` → `<` in `newer` → `a_newer_proposal_for_a_route_replaces_the_older` fails.
  4. `merge`: `newer` → `true` → `an_older_proposal_does_not_replace_a_newer_one` fails.
  5. `drop_conflicting`: `!pools_of(...)…` → `true` → `proposals_sharing_a_traded_pool_are_dropped` fails.
  6. `pop_best` tie-break: `kb.cmp(ka)` → `ka.cmp(kb)` → `ties_go_to_the_newest_then_the_lowest_route` fails.
  7. `CostedCycle::max_input`: `held.min(c)` → `held` → `a_route_sizes_within_the_lenders_holding` fails.
  8. `LivePricer::live`: `.unwrap_or_default()` → `.unwrap_or(U256::MAX)` → `an_unread_lender_sizes_nothing` fails.
  9. `engine_c`: `Some(u256_to_alloy(g))` → `None` → `a_proposal_carries_the_net_its_size_was_found_at` fails.
  10. `Plane::decline_stale`: `Decline::StaleState { age }` → `Decline::NoProfitableSize` → `a_stale_proposal_is_filed_as_a_miss` fails.

  A mutant that survives means the code it guards is dead or the test is wrong: stop and decide which (memory: dead guards need mutation).

- [ ] **Step 2: Recreate `gates.sh`** in the scratchpad (the reboot wiped it):

```bash
#!/usr/bin/env bash
cd /home/scotty/arbot-main2/arbot-main-main || exit 1
for g in scripts/ci/*.sh; do printf '%-42s ' "$(basename $g)"; timeout 600 ./"$g" >/dev/null 2>&1 && echo ok || echo FAIL; done
crates=$(grep -oE -- '-p apex-[a-z]+' .github/workflows/ci.yml | sort -u | tr '\n' ' ')
cargo clippy -j 3 $crates --all-targets -- -D warnings 2>&1 | tail -3; echo "clippy exit ${PIPESTATUS[0]}"
cargo test -j 3 --workspace --all-targets 2>&1 | grep -E "^test result|FAILED|panicked" | sort | uniq -c | tail -8
cargo test -j 3 --workspace --doc 2>&1 | grep -E "^test result|FAILED" | sort | uniq -c | tail -4
forge test 2>&1 | tail -3
```

- [ ] **Step 3: PLAN.md R21 entry** after R20's: what the spec found (the 10:07 queue, the lender cap), what changed, the mutant count. No live numbers yet; those come with the rollout.

- [ ] **Step 4: Run the gates.** Expected: only `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh` FAIL; clippy exit 0; no test failures; forge passes.

- [ ] **Step 5: Commit R21 as one commit** by named paths, because `plane.rs` and `shadow/mod.rs` each carry two tasks' changes and hunk staging (`git add -p`) is interactive. Paths: `crates/apex-search/src/frontier.rs crates/apex-search/src/engine_c.rs crates/apex-search/src/engine_a.rs crates/apex-search/src/lib.rs crates/apex-search/tests/finite_size_search.rs crates/apex-runtime/src/plane.rs crates/apex-runtime/src/shadow/queue.rs crates/apex-runtime/src/shadow/mod.rs crates/apex-runtime/src/live/pricing.rs crates/apex-runtime/tests/support/mod.rs crates/apex-runtime/tests/live_econ.rs crates/apex-runtime/tests/live_pricing.rs crates/apex-runtime/tests/end_to_end.rs crates/apex-runtime/tests/shadow_queue.rs scripts/shadow-status.sh PLAN.md`. Message: `feat(shadow): a spike's proposals are handled best-first, within the lender's holding (Task 8.5 R21)`, covering the 10:07 evidence, the queue, the age rule, the conflict rule, the cap and the mutant count. Then `git diff --stat -- <paths>` must print nothing.

- [ ] **Step 6: Verify the commit** in a clean worktree with its own target: `git worktree add <scratchpad>/wt-r21 <sha>`, then from it `CARGO_TARGET_DIR=<scratchpad>/wt-target cargo test -j 3 --locked -p apex-search -p apex-runtime --all-targets`. Expected: all pass. Remove the worktree after (`git worktree remove`).

The release build and restart are in the R22 plan's rollout task, so R21 and R22 ship in one restart.
