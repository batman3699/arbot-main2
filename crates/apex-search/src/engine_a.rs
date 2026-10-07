//! **Engine A: incremental negative-cycle search (§12.1, BP-045, BP-060).**
//!
//! # Last, deliberately
//!
//! §12.2 lists this first and §52 says not to pay for candidate count, and the
//! measurements settle it: **4× the cycles produced the same 96%
//! `no_profitable_size`.** Opportunity density is not this system's constraint.
//! So Engine A is built after the frontier, Engine D and Engine C, it runs on
//! §2.6's slow lane, and it is the fallback rather than the product.
//!
//! # The graph layer is allowed to be approximate, because it only proposes
//!
//! §12.2 says so in as many words, and it is what makes the arithmetic here
//! acceptable. The weight is `−ln(rate) × WEIGHT_SCALE` in `f64`, rounded to a
//! fixed-point `i64` — which can differ in the last digit from the legacy
//! `rust_decimal` implementation, and on a marginal cycle that is enough to flip
//! a verdict.
//!
//! **That is not a defect to be engineered away here.** A cycle this search
//! proposes is a `RouteProposal`, and the exact question — does any integer size
//! pay for the transaction — is Engine C's, in integer arithmetic, against the
//! real AMM curves. The band where a one-ulp weight difference matters is
//! precisely the 60–65 bps band where Engine A proposes and Engine C refuses,
//! and the refusal is the answer either way. Making this layer exact would buy
//! nothing and would cost the log transform that makes the search possible at
//! all.
//!
//! # `i64::MAX` means "do not traverse", not "very expensive"
//!
//! Carried over from `util::compute_edge_weight`, whose comment states the
//! reason: *"Overflow implies an absurd rate. Exclude rather than guess a sign —
//! guessing negative would fabricate an arbitrage out of a broken quote."* A
//! broken quote that guessed negative would be a negative cycle made of nothing.
//!
//! # What is not migrated
//!
//! `graph.rs` is 4,805 lines. Hub-anchored cycle search, incremental adjacency
//! refresh, the two-hop probe and cycle input capacity are **not** here. They are
//! search topologies and capacity arithmetic over the same edge set, they are
//! §52's "candidate count", and the module they live in carries upward
//! dependencies on `metrics` and `util` — so the crate-split discipline says
//! rebuild rather than move, and rebuilding 4,400 lines to raise a number the
//! measurements say does not bind would be the wrong dollar.

use crate::frontier::{ProposalOrigin, RouteProposal};
use apex_types::ids::{ChainId, PoolId, TokenId, VenueId};
use apex_types::miss::{ExplainsMiss, MissReason};
use apex_types::route::RouteCommitment;
use apex_types::state::StateFingerprint;
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

/// Fixed-point scale for edge weights, matching `util::WEIGHT_SCALE`.
pub const WEIGHT_SCALE: i64 = 1_000_000_000;

/// `−ln(rate) × WEIGHT_SCALE`. A profitable hop (rate > 1) carries a **negative**
/// weight, which is what makes an arbitrage a negative cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Weight(pub i64);

impl Weight {
    /// The sentinel: this edge may not be traversed.
    pub const IMPASSABLE: Self = Self(i64::MAX);

    /// From a rate expressed as a ratio, which is how a venue quote arrives.
    ///
    /// Refuses rather than guesses on anything it cannot express. A zero or
    /// non-finite rate, a zero denominator, an overflow — each returns
    /// [`Self::IMPASSABLE`], because the alternative is a weight whose sign was
    /// guessed, and a guessed negative sign is an arbitrage fabricated out of a
    /// broken quote.
    pub fn from_rate(num: f64, den: f64) -> Self {
        // The sign checks are not redundant with the `rate <= 0.0` check below,
        // and one case is why: **both negative gives a positive rate**. `-1 / -1`
        // is `1.0`, finite and positive, and would be read as parity rather than
        // refused. A quote is never negative, so a negative pair is a broken
        // quote and refusing it is the whole point of the sentinel.
        if !num.is_finite() || !den.is_finite() || den <= 0.0 || num <= 0.0 {
            return Self::IMPASSABLE;
        }
        let rate = num / den;
        if !rate.is_finite() || rate <= 0.0 {
            return Self::IMPASSABLE;
        }
        #[allow(clippy::cast_precision_loss)]
        let scaled = -rate.ln() * WEIGHT_SCALE as f64;
        if !scaled.is_finite() {
            return Self::IMPASSABLE;
        }
        let rounded = scaled.round();
        #[allow(clippy::cast_precision_loss)]
        if rounded >= i64::MAX as f64 || rounded <= i64::MIN as f64 {
            return Self::IMPASSABLE;
        }
        #[allow(clippy::cast_possible_truncation)]
        Self(rounded as i64)
    }

    pub const fn is_passable(self) -> bool {
        self.0 != i64::MAX
    }
}

/// One directed exchange, at the state version it was read at.
///
/// **`state_version` is the new field**, and it is what §4's audit row asks for:
/// *"edges must carry `state_version`, and search must consume immutable
/// snapshots."* Without it a snapshot cannot say which of its edges have since
/// moved, and a cycle through a moved pool is priced against state that is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: TokenId,
    pub to: TokenId,
    pub venue: VenueId,
    pub pool: PoolId,
    pub weight: Weight,
    /// The venue state version this rate was read at.
    pub state_version: u64,
}

/// Why a cycle was not proposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoCycle {
    /// An edge on the cycle was read at a venue state version the live
    /// fingerprint has since passed.
    StaleEdge { venue: VenueId, read_at: u64, live: u64 },
    /// The live fingerprint says nothing about a venue this cycle traverses, so
    /// there is no way to tell whether the edge is current.
    ///
    /// Distinct from `StaleEdge` on purpose: one is a pool that moved, the other
    /// is a pool nobody is watching, and the second is an ingestion gap rather
    /// than a pricing one.
    UnknownVenue { venue: VenueId },
    /// The search hit its §29.3 budget before the graph settled.
    BudgetExhausted { relaxations: u32 },
    /// The edge's rate could not be expressed as a weight — a zero, negative,
    /// non-finite or overflowing quote.
    ///
    /// **Reported rather than silently pruned.** A broken quote means an
    /// opportunity may exist that this search cannot see, which is precisely
    /// what §2.7's coverage auditor is looking for. A mutation removing the
    /// prune changed nothing, because `i64::MAX` also makes every path sum
    /// positive — correctness was being carried by the sentinel's *magnitude*,
    /// which would break silently if the constant ever changed. Reporting it
    /// makes the prune observable and the arithmetic no longer load-bearing.
    Unpriceable { venue: VenueId },
}

impl std::fmt::Display for NoCycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleEdge { venue, read_at, live } => {
                write!(f, "venue {} was read at v{read_at} and is now v{live}", venue.0)
            }
            Self::UnknownVenue { venue } => {
                write!(f, "the live fingerprint says nothing about venue {}", venue.0)
            }
            Self::BudgetExhausted { relaxations } => {
                write!(f, "the search stopped after {relaxations} relaxations")
            }
            Self::Unpriceable { venue } => {
                write!(f, "venue {} quoted a rate that is not a weight", venue.0)
            }
        }
    }
}

impl std::error::Error for NoCycle {}

/// INV-40. A cycle dropped for staleness is a missed opportunity, and it says so.
impl ExplainsMiss for NoCycle {
    fn miss_reason(&self) -> MissReason {
        match self {
            // The state moved under the quote. This is the bucket §33 has for it.
            Self::StaleEdge { .. } | Self::UnknownVenue { .. } => MissReason::StaleState,
            // A quote that cannot be priced is a failure to *evaluate*, not an
            // economic verdict -- the same line `apex-math`'s `NoSize` draws
            // between `Unpriceable` and `NoProfitableSize`.
            Self::Unpriceable { .. } => MissReason::SimFail,
            // The work did not finish in time. Same rule as `apex-runtime`'s
            // `Decline`: a refusal by our machinery is `RiskFail`, work that did
            // not happen in time is `TooSlow`.
            Self::BudgetExhausted { .. } => MissReason::TooSlow,
        }
    }
}

/// §29.3's budget, expressed where the search can obey it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchLimits {
    /// Ceiling on edge relaxations. Bellman-Ford is `O(V·E)` and a graph that
    /// has not settled by then is one this search will not finish on the slow
    /// lane's budget either.
    pub max_relaxations: u32,
    /// Longest cycle to extract. The census found 2-hop dominant at every
    /// percentile and 3/4-hop strictly worse, so a default above 4 spends the
    /// budget on shapes that were measured to lose.
    pub max_hops: usize,
    /// Top-K.
    pub k: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self { max_relaxations: 100_000, max_hops: 4, k: 8 }
    }
}

/// A cycle whose weights sum below zero: the rates multiply to more than 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegativeCycle {
    pub tokens: Vec<TokenId>,
    pub venues: Vec<VenueId>,
    pub pools: Vec<PoolId>,
    /// Sum of `−ln(rate)` over the cycle, scaled. More negative is a larger
    /// marginal gross.
    pub weight: i64,
}

impl NegativeCycle {
    pub fn hops(&self) -> usize {
        self.venues.len()
    }
}

/// An immutable edge set, at a point in time.
///
/// Immutable is the point: §4's audit row asks that "search must consume
/// immutable snapshots", and a search over a mutating graph can traverse two
/// different states in one walk and report a cycle that never existed in either.
#[derive(Clone, Debug, Default)]
pub struct GraphSnapshot {
    edges: Vec<Edge>,
}

impl GraphSnapshot {
    /// No precomputed adjacency index, and clippy is what pointed that out: the
    /// first draft carried a `by_token` map that nothing read. It could not be
    /// read, because [`Self::walk`] needs adjacency over the **usable** subset —
    /// the edges left after staleness and impassability are removed — and an
    /// index over all edges is the wrong set. Fourth piece of dead code this
    /// phase, and the only one a linter caught rather than a mutation.
    pub fn from_edges(edges: Vec<Edge>) -> Self {
        Self { edges }
    }

    pub fn len(&self) -> usize {
        self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// **The staleness check.** An edge whose venue has moved since it was read.
    ///
    /// Per edge rather than per snapshot: one moved pool does not invalidate a
    /// graph, it invalidates the cycles through it. Refusing the whole snapshot
    /// would discard every cycle on every block, which on Base is every 2
    /// seconds.
    pub fn edge_staleness(&self, edge: &Edge, live: &StateFingerprint) -> Option<NoCycle> {
        match live.venue_state_version.get(&edge.venue) {
            None => Some(NoCycle::UnknownVenue { venue: edge.venue }),
            Some(&v) if v > edge.state_version => Some(NoCycle::StaleEdge {
                venue: edge.venue,
                read_at: edge.state_version,
                live: v,
            }),
            Some(_) => None,
        }
    }

    /// Negative cycles, Top-K by weight, over edges that are still current.
    ///
    /// Stale edges are removed **before** the search rather than filtered after:
    /// a relaxation through a stale edge can shift a path that a later cycle is
    /// extracted from, so a cycle with no stale edge of its own can still be a
    /// consequence of one. Filtering the result would leave that in.
    pub fn negative_cycles(
        &self,
        live: &StateFingerprint,
        limits: SearchLimits,
    ) -> (Vec<NegativeCycle>, Vec<NoCycle>) {
        let mut dropped: Vec<NoCycle> = Vec::new();
        let usable: Vec<&Edge> = self
            .edges
            .iter()
            .filter(|e| {
                if !e.weight.is_passable() {
                    dropped.push(NoCycle::Unpriceable { venue: e.venue });
                    return false;
                }
                match self.edge_staleness(e, live) {
                    Some(reason) => {
                        dropped.push(reason);
                        false
                    }
                    None => true,
                }
            })
            .collect();

        let mut cycles = self.walk(&usable, limits, &mut dropped);
        // Most negative first: a larger marginal gross ranks higher. Ties broken
        // by hop count, because the census found deeper routes strictly worse at
        // every percentile -- so between two cycles of equal weight the shallower
        // one is the better proposal.
        cycles.sort_by(|a, b| a.weight.cmp(&b.weight).then_with(|| a.hops().cmp(&b.hops())));
        cycles.truncate(limits.k);
        (cycles, dropped)
    }

    /// Bounded cycle enumeration.
    ///
    /// **Not Bellman-Ford**, and the reason is the bound rather than a preference
    /// for one algorithm. §12.2 wants Top-K cycles up to a small hop count;
    /// Bellman-Ford detects *that* a negative cycle exists and needs a separate
    /// walk to extract one, and extracting K distinct ones from it is where the
    /// legacy implementation's complexity comes from. With `max_hops` at 4 over a
    /// frontier-sized edge set, enumerating bounded walks from each token is
    /// simpler, gives every cycle rather than one per predecessor tree, and
    /// carries its own budget honestly.
    ///
    /// The budget is checked on every relaxation, so a graph that would not
    /// settle produces a `BudgetExhausted` rather than a stall on the slow lane.
    fn walk(
        &self,
        usable: &[&Edge],
        limits: SearchLimits,
        dropped: &mut Vec<NoCycle>,
    ) -> Vec<NegativeCycle> {
        let mut adjacency: BTreeMap<TokenId, Vec<&Edge>> = BTreeMap::new();
        for e in usable {
            adjacency.entry(e.from).or_default().push(e);
        }

        let mut relaxations = 0u32;
        let mut out: Vec<NegativeCycle> = Vec::new();
        let mut seen: std::collections::BTreeSet<Vec<PoolId>> = std::collections::BTreeSet::new();

        for start in adjacency.keys().copied() {
            let mut stack: Vec<(&Edge, i64, Vec<&Edge>)> = Vec::new();
            for e in adjacency.get(&start).into_iter().flatten() {
                stack.push((e, e.weight.0, vec![*e]));
            }

            while let Some((edge, weight, path)) = stack.pop() {
                relaxations = relaxations.saturating_add(1);
                if relaxations >= limits.max_relaxations {
                    dropped.push(NoCycle::BudgetExhausted { relaxations });
                    return out;
                }

                if edge.to == start {
                    if weight < 0 {
                        let mut pools: Vec<PoolId> = path.iter().map(|e| e.pool).collect();
                        let key = {
                            let mut k = pools.clone();
                            k.sort_unstable();
                            k
                        };
                        if seen.insert(key) {
                            out.push(NegativeCycle {
                                tokens: std::iter::once(start)
                                    .chain(path.iter().map(|e| e.to))
                                    .collect(),
                                venues: path.iter().map(|e| e.venue).collect(),
                                pools: std::mem::take(&mut pools),
                                weight,
                            });
                        }
                    }
                    continue;
                }

                if path.len() >= limits.max_hops {
                    continue;
                }
                for next in adjacency.get(&edge.to).into_iter().flatten() {
                    // **No pool twice.** Revisiting a pool would price one state
                    // transition as two independent ones, which is INV-22's
                    // shared-pool coupling in its smallest form.
                    if path.iter().any(|e| e.pool == next.pool) {
                        continue;
                    }
                    // **No token twice either**, and this was a real defect in
                    // the first draft. Without it the search glues cycles
                    // together: with a 1↔2 cycle and a 1↔3 cycle in the graph it
                    // reported 2→1→3→1→2, whose rate product is larger than
                    // either and which is simply both of them in one
                    // transaction. That is a *worse* proposal — §13's
                    // `ComplexityCost` and the census both say deeper routes cost
                    // more for no gain — and it outranked the two real cycles it
                    // was made of, because the ranking is by gross.
                    //
                    // A token visited twice means the walk returned somewhere it
                    // had already been, so everything after that point is a
                    // separate cycle. Closing back to `start` is the exception,
                    // and is handled above.
                    if next.to != start && path.iter().any(|e| e.to == next.to) {
                        continue;
                    }
                    let mut extended = path.clone();
                    extended.push(next);
                    stack.push((next, weight.saturating_add(next.weight.0), extended));
                }
            }
        }
        out
    }
}

/// A negative cycle as a proposal for the rest of the system.
///
/// The `size_hint` is **`None`**, always, and that is the point of §12.2's
/// "allowed to be approximate because it only proposes": a rate-only search
/// cannot express a quantity. Engine C is what finds the size, and the absent
/// hint is what says this search did not.
pub fn to_proposal(
    cycle: &NegativeCycle,
    chain: ChainId,
    route: RouteCommitment,
    found_at: UnixNanos,
    fingerprint: StateFingerprint,
) -> RouteProposal {
    let mut venue_set = cycle.venues.clone();
    venue_set.sort_unstable();
    venue_set.dedup();
    RouteProposal {
        chain,
        route,
        venue_set,
        state_fingerprint: fingerprint,
        found_at,
        origin: ProposalOrigin::NegativeCycle,
        flash_source: None,
        size_hint: None,
        net_hint: None,
    }
}
