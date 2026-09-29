//! The precomputed route frontier (§12.1, §46.3). **BP-176.**
//!
//! > The system keeps a resident frontier of executable route templates for the
//! > most active pool neighbourhoods. A market event triggers **revaluation of
//! > known routes first**, then broader discovery. This — not another graph
//! > algorithm — is the primary mechanism for reducing capture latency.
//!
//! # "Revalue first" is a type, not a convention
//!
//! [`discovery_permit`] is the only way to obtain a [`Revalued`], and it is
//! returned *by* [`Frontier::revalue`]. Broad discovery takes one. So a search
//! path that skipped the frontier is not a code-review finding; it does not
//! compile. Same idiom as `Revalidated → SigningAuthorization` in
//! `apex-capture`, and for the same reason: an ordering constraint that matters
//! is one somebody will eventually get wrong.
//!
//! This is also why the BP-176 test does not measure latency. "The frontier was
//! consulted first" asserted with a stopwatch is a flaky proxy for a claim the
//! type system can make exactly.
//!
//! # The ranking is fee-first, and that is a measurement
//!
//! This repository's ranking used to be fee-blind, and fee-aware pool selection
//! alone cut the arbitrage hurdle from **−247 bps to −10 bps median** — a ~24×
//! reduction with no change to the search. So the frontier orders by total fee
//! ascending before anything else.
//!
//! It ranks on what it can compute **exactly and cheaply**: fees are known from
//! the pool registry, and EV is not the frontier's to estimate — `apex-econ`
//! owns that, one tier up. The ranking's job is to order work for a downstream
//! that will price it properly, not to pre-judge it.
//!
//! Recency is the tiebreak, carrying `hot_path.rs`'s signal: a linear decay from
//! the last time a route was profitable. It is a **tiebreak** and not the
//! primary key because recency describes where opportunity *was*, and the fee
//! describes what a route costs to take now.

use alloy_primitives::{B256, U256};
use apex_state::feed::event::StateEvent;
use apex_types::ids::{ChainId, FlashProviderId, PoolId, TokenId, VenueId};
use apex_types::route::RouteCommitment;
use apex_types::state::StateFingerprint;
use apex_types::time::{DurationNanos, UnixNanos};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Identity of a resident template.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RouteId(pub u64);

/// One hop's fee, and the pool that offers it (§12.1's `fee_variants`).
///
/// A template's topology can often be executed at several fee tiers — WETH/USDC
/// exists at 100, 500 and 3000 bps on the same venue — and which tier is cheapest
/// is the single largest term in whether the route clears. The variants are
/// carried rather than resolved so the choice is made against live state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FeeVariant {
    pub venue: VenueId,
    pub pool: PoolId,
    /// Parts per million. 500 ppm = 5 bps.
    pub fee_ppm: u32,
}

/// The tick range a concentrated-liquidity template's pricing is valid within
/// (§12.1's `tick_neighborhood`).
///
/// **This field exists because its absence was measured.** The fast path carried
/// no tick ladder, so every CL quote was single-tick plus a 50 bps haircut
/// regardless of configuration — the mechanism behind a ~140 bps gap between
/// local pricing and the quoter. A template that does not say which ticks its
/// price assumed cannot be revalued correctly when the pool crosses one.
///
/// `None` for constant-product pools, which have no ticks. Not a zero range:
/// "this pool has no ticks" and "this pool's valid range is empty" are opposite
/// facts, and the second would make every route look expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TickNeighborhood {
    pub lower: i32,
    pub upper: i32,
}

impl TickNeighborhood {
    pub const fn contains(&self, tick: i32) -> bool {
        tick >= self.lower && tick <= self.upper
    }
}

/// A coarse gas bucket (§12.1's `expected_gas_class`).
///
/// **Deliberately coarse, and not a gas estimate.** §23's `TotalExecutionCost` is
/// the cost model and it produces a distribution; this is a bucket the frontier
/// can rank by without calling it. Giving this type a `u64` would invite somebody
/// to price against it, and a route priced against a bucket is a route priced
/// against a guess.
///
/// The buckets are the shapes this repository has actually measured: a 2-hop
/// constant-product route, a 2-hop route touching concentrated liquidity, and
/// anything deeper — which the census found strictly worse at every percentile
/// and which is therefore one bucket rather than several.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GasClass {
    TwoHopConstantProduct,
    TwoHopConcentrated,
    Deeper,
}

impl GasClass {
    pub const fn label(self) -> &'static str {
        match self {
            Self::TwoHopConstantProduct => "two_hop_cp",
            Self::TwoHopConcentrated => "two_hop_cl",
            Self::Deeper => "deeper",
        }
    }
}

/// §12.1's route template, with all eight attributes.
///
/// `cycle_index.rs` supplies `topology`; `hot_path.rs` supplies
/// `last_profitable`; the other six are new, and §7's "what exists" table records
/// that the legacy pair carried two of eight.
///
/// **`last_profitable` is a `UnixNanos`, not an `Instant`.** §12.1's sketch says
/// `Option<Instant>`, which is a monotonic reading with no fixed epoch: it cannot
/// be journalled, compared across processes, or set by a test clock. The same
/// argument `apex_capture::clock` makes — "a deadline test would have to sleep,
/// and a sleeping test is a slow test that still races".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteTemplate {
    pub id: RouteId,
    pub chain: ChainId,
    /// 1. The token cycle.
    pub topology: Vec<TokenId>,
    /// 2. Which venue each hop executes on.
    pub venue_sequence: Vec<VenueId>,
    /// 3. The fee tiers this topology is available at.
    pub fee_variants: Vec<FeeVariant>,
    /// 4. The tick range the pricing assumed, per CL pool.
    pub tick_neighborhood: BTreeMap<PoolId, TickNeighborhood>,
    /// 5. `None` means no hooks, on every pool. §10.4: an unmodelled hook forces
    ///    `Exactness::Approximate`, which INV-17 then blocks from live dispatch.
    pub hook_fingerprint: Option<B256>,
    /// 6. Where the capital comes from.
    pub flash_source: FlashProviderId,
    /// 7. The gas bucket, for ranking only.
    pub expected_gas_class: GasClass,
    /// 8. `hot_path.rs`'s recency signal.
    pub last_profitable: Option<UnixNanos>,
}

impl RouteTemplate {
    /// The pools this template touches. The frontier's index key.
    pub fn pools(&self) -> Vec<PoolId> {
        let mut pools: Vec<PoolId> = self.fee_variants.iter().map(|v| v.pool).collect();
        pools.sort_unstable();
        pools.dedup();
        pools
    }

    /// Total fee across the cheapest variant of each hop, in ppm.
    ///
    /// Saturating rather than wrapping: a template whose fees overflow a `u32` is
    /// not a cheap route that wrapped around to zero, and ranking it first is
    /// exactly the failure this figure exists to prevent.
    pub fn cheapest_total_fee_ppm(&self) -> u32 {
        let mut by_venue: BTreeMap<VenueId, u32> = BTreeMap::new();
        for v in &self.fee_variants {
            by_venue
                .entry(v.venue)
                .and_modify(|f| *f = (*f).min(v.fee_ppm))
                .or_insert(v.fee_ppm);
        }
        by_venue.values().fold(0u32, |acc, f| acc.saturating_add(*f))
    }

    /// `hot_path.rs`'s decay, carried forward unchanged: 1.0 while a route is
    /// inside its window, falling linearly to 0 at the edge.
    pub fn recency_score(&self, now: UnixNanos, window: DurationNanos) -> f64 {
        let (Some(last), true) = (self.last_profitable, window.0 > 0) else {
            return 0.0;
        };
        let elapsed = now.0.saturating_sub(last.0);
        #[allow(clippy::cast_precision_loss)]
        let ratio = elapsed as f64 / window.0 as f64;
        (1.0 - ratio).clamp(0.0, 1.0)
    }
}

/// **Proof that the frontier was consulted for an event.**
///
/// Unforgeable outside this module, and the only producer is
/// [`Frontier::revalue`]. Broad discovery takes one, so §12.1's "revaluation of
/// known routes first, then broader discovery" is a fact about what compiles.
///
/// It cannot be written down:
///
/// ```compile_fail
/// use apex_search::frontier::Revalued;
/// let forged = Revalued { _sealed: () };
/// ```
///
/// The twin, differing only in going through the frontier:
///
/// ```
/// use apex_search::frontier::Frontier;
/// let frontier = Frontier::new();
/// let event = apex_search::frontier::doc_event();
/// let (hits, permit) = frontier.revalue(&event);
/// assert!(hits.is_empty(), "an empty frontier has nothing resident");
/// // ...and only now may broad discovery run.
/// assert_eq!(permit.event_ordinal(), event.at);
/// ```
#[derive(Debug)]
pub struct Revalued {
    /// Which event was revalued. Carried so a caller cannot revalue one event
    /// and then discover against another — the permit names its subject.
    ordinal: apex_state::Ordinal,
    hits: usize,
    _sealed: (),
}

impl Revalued {
    pub const fn event_ordinal(&self) -> apex_state::Ordinal {
        self.ordinal
    }

    /// How many resident templates matched. `0` is the signal that broad
    /// discovery is doing real work rather than duplicating the frontier.
    pub const fn hits(&self) -> usize {
        self.hits
    }
}

/// The resident set (§12.1).
///
/// `Default` is hand-written rather than derived, and the compiler is the one
/// that pointed it out: `DurationNanos` has no `Default`, so a derived one would
/// have needed a zero window — and a zero recency window makes every
/// `recency_score` exactly 0, silently disabling the tiebreak. The absent impl
/// upstream is doing its job.
#[derive(Debug)]
pub struct Frontier {
    templates: BTreeMap<RouteId, RouteTemplate>,
    /// pool → templates touching it. The whole point: an event names pools, and
    /// this answers "which known routes does that move" without a graph.
    by_pool: BTreeMap<PoolId, Vec<RouteId>>,
    /// §12.1's recency window, injected rather than constant — `hot_path.rs` made
    /// it a parameter and it is one here.
    recency_window: DurationNanos,
}

impl Default for Frontier {
    fn default() -> Self {
        Self::new()
    }
}

impl Frontier {
    /// 30 seconds. `hot_path.rs`'s default, and this repository measured edges
    /// persisting ~35 s, so a window shorter than that would discard routes that
    /// were still live.
    pub const DEFAULT_RECENCY_WINDOW: DurationNanos = DurationNanos(30_000_000_000);

    pub fn new() -> Self {
        Self::with_recency_window(Self::DEFAULT_RECENCY_WINDOW)
    }

    pub fn with_recency_window(recency_window: DurationNanos) -> Self {
        Self { templates: BTreeMap::new(), by_pool: BTreeMap::new(), recency_window }
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    pub fn get(&self, id: RouteId) -> Option<&RouteTemplate> {
        self.templates.get(&id)
    }

    /// Make a template resident. Replaces any template with the same id, index
    /// included — a template whose pools changed must not stay indexed under the
    /// old ones, which is how a route gets revalued for an event that no longer
    /// touches it.
    ///
    /// Refuses a template whose `chain` disagrees with its pools'. That is the
    /// one place the disagreement can be introduced — a template assembled
    /// against the wrong registry — and once it is resident there is no later
    /// check that can distinguish it from a correct one.
    pub fn insert(&mut self, template: RouteTemplate) -> Result<(), InconsistentTemplate> {
        if let Some(stray) = template.pools().into_iter().find(|p| p.chain != template.chain) {
            return Err(InconsistentTemplate { id: template.id, chain: template.chain, stray });
        }
        let id = template.id;
        if self.templates.contains_key(&id) {
            self.remove(id);
        }
        for pool in template.pools() {
            self.by_pool.entry(pool).or_default().push(id);
        }
        self.templates.insert(id, template);
        Ok(())
    }

    pub fn remove(&mut self, id: RouteId) -> Option<RouteTemplate> {
        let template = self.templates.remove(&id)?;
        for pool in template.pools() {
            if let Some(ids) = self.by_pool.get_mut(&pool) {
                ids.retain(|x| *x != id);
                if ids.is_empty() {
                    self.by_pool.remove(&pool);
                }
            }
        }
        Some(template)
    }

    /// Record that a template just made money. `hot_path.rs`'s write path.
    pub fn mark_profitable(&mut self, id: RouteId, at: UnixNanos) -> bool {
        match self.templates.get_mut(&id) {
            Some(t) => {
                t.last_profitable = Some(at);
                true
            }
            None => false,
        }
    }

    /// **§12.1's first step.** The resident templates this event moves, ranked,
    /// plus the permit broad discovery requires.
    ///
    /// Takes no graph, no registry and no network, and that is the design: a
    /// frontier that *could* consult a graph is one that eventually would.
    pub fn revalue(&self, event: &StateEvent) -> (Vec<RouteId>, Revalued) {
        let mut hits: Vec<RouteId> = Vec::new();
        for pool in event.touched_pools() {
            if let Some(ids) = self.by_pool.get(pool) {
                hits.extend(ids.iter().copied());
            }
        }
        hits.sort_unstable();
        hits.dedup();
        // No chain filter here, and that is deliberate. The first version had
        // one; a mutation removing it broke nothing, because `PoolId` carries its
        // chain, so `by_pool`'s keys are already chain-qualified and a lookup can
        // never reach another chain's template. It was dead code with a comment
        // rationalising it -- the same shape as the two dead guards Task 7.2
        // removed.
        //
        // The case it was reaching for is real, but it happens at INSERTION: a
        // template whose `chain` disagrees with its pools' is malformed, and
        // `insert` refuses it. Checking there turns a filter that can never fire
        // into an invariant that can.
        self.rank(&mut hits, event.observed_at);

        let permit = Revalued { ordinal: event.at, hits: hits.len(), _sealed: () };
        (hits, permit)
    }

    /// Fee ascending, then recency descending, then id.
    ///
    /// The id tiebreak exists only so the order is total: two otherwise identical
    /// templates must not compare equal, or the ranking becomes an implementation
    /// detail nothing can test. Same reasoning as `Work`'s in the scheduler.
    fn rank(&self, ids: &mut [RouteId], now: UnixNanos) {
        ids.sort_by(|a, b| {
            let (ta, tb) = (self.templates.get(a), self.templates.get(b));
            let (Some(ta), Some(tb)) = (ta, tb) else { return a.cmp(b) };
            ta.cheapest_total_fee_ppm()
                .cmp(&tb.cheapest_total_fee_ppm())
                .then_with(|| {
                    tb.recency_score(now, self.recency_window)
                        .partial_cmp(&ta.recency_score(now, self.recency_window))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.cmp(b))
        });
    }
}

/// A template that claims one chain and names another's pools.
///
/// **Not a candidate rejection** — nothing was declined, and no opportunity went
/// untaken. It is a malformed input, so it does not implement `ExplainsMiss` and
/// is named so that `scripts/ci/every_rejection_explains.sh` does not ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InconsistentTemplate {
    pub id: RouteId,
    pub chain: ChainId,
    pub stray: PoolId,
}

impl std::fmt::Display for InconsistentTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "template {} claims chain {} but names a pool on chain {}",
            self.id.0, self.chain.0, self.stray.chain.0
        )
    }
}

impl std::error::Error for InconsistentTemplate {}

/// What `apex-search` hands upward. **Not a `Candidate`** — see the crate docs.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteProposal {
    pub chain: ChainId,
    pub route: RouteCommitment,
    pub venue_set: Vec<VenueId>,
    pub state_fingerprint: StateFingerprint,
    pub found_at: UnixNanos,
    pub origin: ProposalOrigin,
    pub flash_source: Option<FlashProviderId>,
    /// Engine C searches *for* a finite size, so it has one in mind.
    ///
    /// A `U256` and **not** a `DiscreteSize`: INV-18 says the executed size comes
    /// from `apex-econ::sizing::discrete::refine` and nowhere else. This is an
    /// input to that search, never a substitute for it, and the type is what says
    /// so rather than a comment asking nicely.
    pub size_hint: Option<U256>,
}

/// Which engine proposed a route, and from where.
///
/// Carried because §2.7's coverage auditor compares what the fast path found
/// against a broader oracle, and "which engine found it" is the first question
/// asked of a gap. A proposal whose origin nobody recorded is one nobody can
/// attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProposalOrigin {
    /// §12.1: a resident template, revalued because the event touched it.
    Frontier(RouteId),
    /// Engine D (§12.4): an event template class matched.
    EventTemplate,
    /// Engine C (§12.3): finite-size search.
    FiniteSize,
    /// Engine A (§12.1): incremental negative-cycle search on the slow lane.
    NegativeCycle,
}

/// A fixture for the doctests above. Public because a doctest is compiled as an
/// external crate and cannot reach a private helper.
#[doc(hidden)]
pub fn doc_event() -> StateEvent {
    use apex_state::feed::event::EventKind;
    StateEvent {
        chain: ChainId::BASE,
        at: apex_state::Ordinal::confirmed(47_079_437, 12, 0),
        observed_at: UnixNanos(1_781_049_614_240_000_000),
        fingerprint: StateFingerprint {
            chain_id: ChainId::BASE,
            parent_block_hash: B256::repeat_byte(0xaa),
            confirmed_block_number: 47_079_437,
            preconf_sequence: None,
            flashblock_index: None,
            state_root_or_equivalent: None,
            block_hash_if_available: None,
            state_delta_hash: B256::repeat_byte(0xbb),
            venue_state_version: BTreeMap::new(),
            external_dependency_fingerprint: None,
        },
        kind: EventKind::Block,
    }
}
