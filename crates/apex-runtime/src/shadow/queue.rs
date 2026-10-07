//! The capture loop's queue of proposals (Task 8.5 R21).
//!
//! A spike produces dozens of proposals at once. Handled one event at a time,
//! in the search's fee order, the most valuable waited behind the cheapest: at
//! 10:07 UTC on 2026-10-07 a $17.98 candidate was simulated fourth, 1.2 s late,
//! and later ones past their deadline. So the loop keeps one queue, merges every
//! waiting event's proposals into it before each pick, and handles the best.

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

/// One entry per route: the newest proposal for it.
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

/// A proposal over `pools` with route hash `hash`, everything else empty: for
/// the queue's tests, which judge only hashes, pools, times and nets.
#[doc(hidden)]
pub fn test_proposal(hash: B256, pools: &[Address]) -> RouteProposal {
    let chain = apex_types::ids::ChainId(8453);
    RouteProposal {
        chain,
        route: apex_types::route::RouteCommitment {
            hops: pools
                .iter()
                .map(|a| apex_types::route::RouteHop {
                    venue: apex_types::ids::VenueId(1),
                    pool: apex_types::ids::PoolId { chain, address: *a },
                    token_in: apex_types::ids::TokenId { chain, address: Address::ZERO },
                    token_out: apex_types::ids::TokenId { chain, address: Address::ZERO },
                    fee_ppm: 0,
                })
                .collect(),
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
    }
}
