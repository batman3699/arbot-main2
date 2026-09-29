//! **BP-045, BP-060.** Engine A: the weight, the cycles, and the new
//! requirement — that edges carry their state version and a stale one cannot
//! produce a candidate.

use alloy_primitives::{Address, B256};
use apex_search::engine_a::{
    to_proposal, Edge, GraphSnapshot, NoCycle, SearchLimits, Weight, WEIGHT_SCALE,
};
use apex_search::frontier::ProposalOrigin;
use apex_types::ids::{ChainId, PoolId, TokenId, VenueId};
use apex_types::miss::{ExplainsMiss, MissReason};
use apex_types::route::{ComplexityCost, RouteCommitment};
use apex_types::state::StateFingerprint;
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

const BASE: ChainId = ChainId(8453);

fn token(n: u8) -> TokenId {
    TokenId { chain: BASE, address: Address::repeat_byte(n) }
}

fn pool(n: u8) -> PoolId {
    PoolId { chain: BASE, address: Address::repeat_byte(n) }
}

fn edge(from: u8, to: u8, venue: u16, pool_n: u8, rate: f64, version: u64) -> Edge {
    Edge {
        from: token(from),
        to: token(to),
        venue: VenueId(venue),
        pool: pool(pool_n),
        weight: Weight::from_rate(rate, 1.0),
        state_version: version,
    }
}

fn live(versions: &[(u16, u64)]) -> StateFingerprint {
    StateFingerprint {
        chain_id: BASE,
        parent_block_hash: B256::repeat_byte(0xaa),
        confirmed_block_number: 47_079_437,
        preconf_sequence: None,
        flashblock_index: None,
        state_root_or_equivalent: None,
        block_hash_if_available: None,
        state_delta_hash: B256::repeat_byte(0xbb),
        venue_state_version: versions.iter().map(|(v, n)| (VenueId(*v), *n)).collect(),
        external_dependency_fingerprint: None,
    }
}

/// A round trip whose rates multiply above 1: a negative cycle.
fn profitable_pair() -> Vec<Edge> {
    vec![
        edge(0x01, 0x02, 1, 0x33, 1.02, 5),
        edge(0x02, 0x01, 2, 0x44, 1.01, 5),
    ]
}

/// `−ln(rate) × WEIGHT_SCALE`: a profitable hop carries a negative weight, which
/// is what makes an arbitrage a *negative* cycle.
#[test]
fn a_profitable_hop_carries_a_negative_weight() {
    assert!(Weight::from_rate(1.02, 1.0).0 < 0, "rate > 1 is negative weight");
    assert!(Weight::from_rate(0.98, 1.0).0 > 0, "rate < 1 is positive weight");
    assert_eq!(Weight::from_rate(1.0, 1.0).0, 0, "parity is exactly zero");

    // The scale is the legacy one, so a weight read from either side means the
    // same thing. ln(2) ≈ 0.6931.
    let two = Weight::from_rate(2.0, 1.0).0;
    #[allow(clippy::cast_precision_loss)]
    let expected = (-std::f64::consts::LN_2 * WEIGHT_SCALE as f64).round() as i64;
    assert_eq!(two, expected);
}

/// **`i64::MAX` means "do not traverse", not "very expensive".**
///
/// Carried from `util::compute_edge_weight`, whose comment gives the reason:
/// *"Overflow implies an absurd rate. Exclude rather than guess a sign —
/// guessing negative would fabricate an arbitrage out of a broken quote."*
#[test]
fn an_unusable_rate_is_impassable_rather_than_signed() {
    for (num, den) in [
        (0.0, 1.0),
        (-1.0, 1.0),
        (1.0, 0.0),
        (1.0, -1.0),
        (f64::NAN, 1.0),
        (f64::INFINITY, 1.0),
        (1.0, f64::NAN),
        // Both negative gives a POSITIVE rate: -1/-1 is 1.0, finite and
        // positive. Without the sign checks this reads as parity rather than as
        // the broken quote it is, and it is the one case the `rate <= 0.0` check
        // below cannot catch.
        (-1.0, -1.0),
        (-2.0, -1.0),
    ] {
        let w = Weight::from_rate(num, den);
        assert_eq!(w, Weight::IMPASSABLE, "rate {num}/{den} must be impassable");
        assert!(!w.is_passable());
    }

    // An impassable edge is not traversed, so it cannot appear in a cycle --
    // which is the whole point of the sentinel, and the case a "very expensive"
    // reading would get wrong.
    let mut edges = profitable_pair();
    edges[0].weight = Weight::IMPASSABLE;
    let (cycles, dropped) = GraphSnapshot::from_edges(edges)
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());
    assert!(cycles.is_empty(), "a broken quote must not become an arbitrage");

    // **Reported, not silently pruned.** A broken quote means an opportunity may
    // exist that this search cannot see, which is what §2.7's coverage auditor
    // is looking for. A mutation removing the prune changed nothing before this
    // assertion existed, because `i64::MAX` also makes every path sum positive --
    // correctness was resting on the sentinel's magnitude rather than on the
    // check.
    assert_eq!(dropped, vec![NoCycle::Unpriceable { venue: VenueId(1) }]);
    assert_eq!(dropped[0].miss_reason(), MissReason::SimFail, "a failure to evaluate");
}

/// The search finds the round trip.
#[test]
fn a_profitable_round_trip_is_a_negative_cycle() {
    let (cycles, dropped) = GraphSnapshot::from_edges(profitable_pair())
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());

    assert_eq!(cycles.len(), 1, "{cycles:?}");
    assert!(dropped.is_empty());
    let c = &cycles[0];
    assert_eq!(c.hops(), 2);
    assert!(c.weight < 0, "the rates multiply above 1");
    assert_eq!(c.pools, vec![pool(0x33), pool(0x44)]);
}

/// ...and does not find one that is not there. Without this, "finds the cycle"
/// is satisfied by a search that returns every walk.
#[test]
fn an_unprofitable_round_trip_is_not_a_cycle() {
    let edges = vec![
        edge(0x01, 0x02, 1, 0x33, 0.99, 5),
        edge(0x02, 0x01, 2, 0x44, 1.00, 5),
    ];
    let (cycles, _) = GraphSnapshot::from_edges(edges)
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());
    assert!(cycles.is_empty());
}

/// **The new requirement.** An edge read at a venue state version the live
/// fingerprint has passed cannot produce a candidate.
///
/// §4's audit row: *"edges must carry `state_version`, and search must consume
/// immutable snapshots."* Without it, a cycle through a moved pool is priced
/// against state that is gone — and it prices, simulates and reaches the sizing
/// path looking exactly like a live one.
#[test]
fn a_stale_edge_cannot_produce_a_candidate() {
    let snapshot = GraphSnapshot::from_edges(profitable_pair());

    // Venue 2 has moved since the edge was read.
    let (cycles, dropped) =
        snapshot.negative_cycles(&live(&[(1, 5), (2, 6)]), SearchLimits::default());

    assert!(cycles.is_empty(), "the cycle traverses a pool that has moved");
    assert_eq!(
        dropped,
        vec![NoCycle::StaleEdge { venue: VenueId(2), read_at: 5, live: 6 }],
        "and it says which venue and by how much"
    );

    // INV-40: a dropped cycle is a missed opportunity with a bucket.
    assert_eq!(dropped[0].miss_reason(), MissReason::StaleState);
}

/// The complement, so the pair discriminates: an edge read at a version the live
/// fingerprint has *not* passed is usable. Without this, "stale is refused" is
/// satisfied by refusing everything.
#[test]
fn an_edge_at_the_live_version_is_usable() {
    let snapshot = GraphSnapshot::from_edges(profitable_pair());
    let (cycles, dropped) =
        snapshot.negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());
    assert_eq!(cycles.len(), 1);
    assert!(dropped.is_empty());

    // An edge read *ahead* of the live version is also usable -- the fingerprint
    // is what the caller last saw, and a newer read is not stale.
    let ahead = vec![
        edge(0x01, 0x02, 1, 0x33, 1.02, 9),
        edge(0x02, 0x01, 2, 0x44, 1.01, 9),
    ];
    let (cycles, _) = GraphSnapshot::from_edges(ahead)
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());
    assert_eq!(cycles.len(), 1, "a newer read is not a stale one");
}

/// A venue the live fingerprint says nothing about is a **different** refusal
/// from one that moved: an ingestion gap rather than a pricing one, and they
/// call for different responses.
#[test]
fn an_unwatched_venue_is_distinguished_from_a_moved_one() {
    let (cycles, dropped) = GraphSnapshot::from_edges(profitable_pair())
        .negative_cycles(&live(&[(1, 5)]), SearchLimits::default());

    assert!(cycles.is_empty());
    assert_eq!(dropped, vec![NoCycle::UnknownVenue { venue: VenueId(2) }]);
    assert_ne!(dropped[0], NoCycle::StaleEdge { venue: VenueId(2), read_at: 5, live: 5 });
    assert_eq!(dropped[0].miss_reason(), MissReason::StaleState);
}

/// Stale edges are removed **before** the search, not filtered after.
///
/// A relaxation through a stale edge can shift a path that a later cycle is
/// extracted from, so a cycle with no stale edge of its own can still be a
/// consequence of one. This fixture has exactly that shape: a clean 2-hop cycle
/// exists, and a stale edge offers a shortcut into it.
#[test]
fn a_stale_edge_does_not_contribute_to_a_clean_cycle() {
    let edges = vec![
        edge(0x01, 0x02, 1, 0x33, 1.02, 5),
        edge(0x02, 0x01, 2, 0x44, 1.01, 5),
        // Stale, and it would otherwise make a second, larger cycle.
        edge(0x02, 0x03, 3, 0x55, 1.50, 5),
        edge(0x03, 0x01, 4, 0x66, 1.50, 5),
    ];
    let (cycles, dropped) = GraphSnapshot::from_edges(edges)
        .negative_cycles(&live(&[(1, 5), (2, 5), (3, 9), (4, 5)]), SearchLimits::default());

    assert_eq!(cycles.len(), 1, "only the clean cycle survives: {cycles:?}");
    assert_eq!(cycles[0].pools, vec![pool(0x33), pool(0x44)]);
    assert_eq!(dropped.len(), 1);
}

/// §29.3: the search carries a budget and says when it hit it, rather than
/// running to convergence on the slow lane.
#[test]
fn the_search_is_bounded_and_says_when_it_stopped() {
    let mut edges = Vec::new();
    let mut versions = Vec::new();
    // A dense graph: every token to every other.
    for i in 1..=8u8 {
        for j in 1..=8u8 {
            if i != j {
                edges.push(edge(i, j, u16::from(i) * 10 + u16::from(j), i * 16 + j, 1.02, 5));
                versions.push((u16::from(i) * 10 + u16::from(j), 5u64));
            }
        }
    }
    let snapshot = GraphSnapshot::from_edges(edges);
    let (_, dropped) = snapshot.negative_cycles(
        &live(&versions),
        SearchLimits { max_relaxations: 50, max_hops: 4, k: 8 },
    );
    assert!(
        dropped.iter().any(|d| matches!(d, NoCycle::BudgetExhausted { .. })),
        "the budget must be reported, not silently absorbed: {dropped:?}"
    );
    assert_eq!(
        dropped
            .iter()
            .find(|d| matches!(d, NoCycle::BudgetExhausted { .. }))
            .map(ExplainsMiss::miss_reason),
        Some(MissReason::TooSlow)
    );
}

/// Top-K, ranked most-negative first, with hop count breaking ties — because the
/// census found deeper routes strictly worse at every percentile.
#[test]
fn cycles_rank_by_weight_then_by_fewer_hops() {
    // The fixture is arranged so **enumeration order is not weight order**. The
    // walk pops a LIFO stack of the edges leaving the first token, so the
    // 0x55/0x66 pair is reached first — and here that is the *weaker* cycle, so a
    // search that did not sort would return it first. A mutation deleting the
    // sort passed against the earlier fixture, where the two orders happened to
    // agree.
    let edges = vec![
        // Reached second, and the stronger.
        edge(0x01, 0x02, 1, 0x33, 1.20, 5),
        edge(0x02, 0x01, 2, 0x44, 1.20, 5),
        // Reached first, and the weaker.
        edge(0x01, 0x03, 3, 0x55, 1.001, 5),
        edge(0x03, 0x01, 4, 0x66, 1.001, 5),
    ];
    let versions: Vec<(u16, u64)> = (1..=4).map(|v| (v, 5u64)).collect();
    let (cycles, _) = GraphSnapshot::from_edges(edges)
        .negative_cycles(&live(&versions), SearchLimits::default());

    assert_eq!(cycles.len(), 2, "{cycles:?}");
    assert!(cycles[0].weight <= cycles[1].weight, "most negative first");
    assert_eq!(
        cycles[0].pools,
        vec![pool(0x33), pool(0x44)],
        "the larger gross ranks first, even though the other was enumerated first"
    );

    // K bounds the output.
    let edges = vec![
        edge(0x01, 0x02, 1, 0x33, 1.02, 5),
        edge(0x02, 0x01, 2, 0x44, 1.02, 5),
        edge(0x01, 0x03, 3, 0x55, 1.02, 5),
        edge(0x03, 0x01, 4, 0x66, 1.02, 5),
    ];
    let (cycles, _) = GraphSnapshot::from_edges(edges)
        .negative_cycles(&live(&versions), SearchLimits { k: 1, ..SearchLimits::default() });
    assert_eq!(cycles.len(), 1, "K bounds the result");
}

/// A cycle may not traverse the same pool twice. Doing so would price one state
/// transition as two independent ones, which is INV-22's shared-pool coupling in
/// its smallest form.
#[test]
fn a_cycle_never_reuses_a_pool() {
    let edges = vec![
        edge(0x01, 0x02, 1, 0x33, 1.20, 5),
        // Same pool, back the other way. Buying and selling the same pool cannot
        // be an arbitrage against itself -- it is one state transition read
        // twice, and the second read is against state the first one moved.
        edge(0x02, 0x01, 1, 0x33, 1.20, 5),
    ];
    let (cycles, _) = GraphSnapshot::from_edges(edges)
        .negative_cycles(&live(&[(1, 5)]), SearchLimits::default());
    assert!(cycles.is_empty(), "round-tripping one pool is not an arbitrage: {cycles:?}");

    // The same shape across two pools IS one, so the refusal is about the pool
    // rather than about the shape.
    let two = vec![
        edge(0x01, 0x02, 1, 0x33, 1.20, 5),
        edge(0x02, 0x01, 2, 0x44, 1.20, 5),
    ];
    let (cycles, _) = GraphSnapshot::from_edges(two)
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());
    assert_eq!(cycles.len(), 1);
}

/// **A cycle may not revisit a token either, and that was a real defect.**
///
/// The first draft forbade only repeated pools. With a 1↔2 cycle and a 1↔3 cycle
/// in the same graph it reported `2→1→3→1→2` — which is simply both cycles in one
/// transaction. Its rate product is larger than either, so it **outranked the two
/// real cycles it was made of**, while costing more gas for the same edge.
///
/// §13's `ComplexityCost` and the census both say deeper routes cost more for no
/// gain, so a glued cycle is a strictly worse duplicate of proposals the search
/// already made.
#[test]
fn a_cycle_never_revisits_a_token() {
    let edges = vec![
        edge(0x01, 0x02, 1, 0x33, 1.001, 5),
        edge(0x02, 0x01, 2, 0x44, 1.001, 5),
        edge(0x01, 0x03, 3, 0x55, 1.20, 5),
        edge(0x03, 0x01, 4, 0x66, 1.20, 5),
    ];
    let versions: Vec<(u16, u64)> = (1..=4).map(|v| (v, 5u64)).collect();
    let (cycles, _) = GraphSnapshot::from_edges(edges)
        .negative_cycles(&live(&versions), SearchLimits::default());

    for c in &cycles {
        assert!(
            c.hops() <= 2,
            "a 4-hop cycle here is the 1<->2 and 1<->3 cycles glued at token 1: {c:?}"
        );
        // Every intermediate token appears once.
        let mut intermediates = c.tokens.clone();
        intermediates.pop();
        let unique: std::collections::BTreeSet<_> = intermediates.iter().collect();
        assert_eq!(unique.len(), intermediates.len(), "a token repeats: {c:?}");
    }
    assert_eq!(cycles.len(), 2, "both real cycles, and only those: {cycles:?}");
}

/// **The proposal carries no size hint, and that is §12.2's "allowed to be
/// approximate because it only proposes".**
///
/// A rate-only search cannot express a quantity at all. Engine C is what finds
/// the size; an absent hint is what says this search did not.
#[test]
fn a_negative_cycle_proposes_without_a_size() {
    let (cycles, _) = GraphSnapshot::from_edges(profitable_pair())
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());

    let proposal = to_proposal(
        &cycles[0],
        BASE,
        RouteCommitment {
            hops: Vec::new(),
            complexity_cost: ComplexityCost {
                hops: 2,
                external_calls: 2,
                calldata_bytes: 420,
                state_deps: 2,
                tick_crossings: 0,
                hooks: 0,
                gas_estimate: 240_000,
                failure_surface: 0.01,
            },
            route_hash: B256::repeat_byte(0x44),
        },
        UnixNanos(1_000_000_000),
        live(&[(1, 5), (2, 5)]),
    );

    assert_eq!(proposal.origin, ProposalOrigin::NegativeCycle);
    assert!(
        proposal.size_hint.is_none(),
        "a rate-only search has no quantity to offer, and saying nothing is the honest form"
    );
    assert_eq!(proposal.venue_set, vec![VenueId(1), VenueId(2)]);
}

/// An empty graph produces nothing and refuses nothing — distinct from a graph
/// whose every cycle lost, which is the diagnosis the legacy
/// `bellman_ford_diagnostic` existed to separate.
#[test]
fn an_empty_graph_is_not_a_graph_whose_cycles_all_lost() {
    let (cycles, dropped) = GraphSnapshot::from_edges(Vec::new())
        .negative_cycles(&live(&[]), SearchLimits::default());
    assert!(cycles.is_empty());
    assert!(dropped.is_empty(), "nothing was refused, because nothing was considered");

    let unprofitable = vec![
        edge(0x01, 0x02, 1, 0x33, 0.99, 5),
        edge(0x02, 0x01, 2, 0x44, 0.99, 5),
    ];
    let (cycles, dropped) = GraphSnapshot::from_edges(unprofitable)
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());
    assert!(cycles.is_empty());
    assert!(dropped.is_empty(), "considered and lost is not refused either");
}

/// `max_hops` bounds the shapes searched. The census found 2-hop dominant and
/// 3/4-hop strictly worse, so a bound above 4 spends the budget on shapes that
/// were measured to lose.
#[test]
fn the_hop_bound_is_respected() {
    let edges = vec![
        edge(0x01, 0x02, 1, 0x33, 1.10, 5),
        edge(0x02, 0x03, 2, 0x44, 1.10, 5),
        edge(0x03, 0x01, 3, 0x55, 1.10, 5),
    ];
    let versions: Vec<(u16, u64)> = (1..=3).map(|v| (v, 5u64)).collect();
    let snapshot = GraphSnapshot::from_edges(edges);

    let (found, _) = snapshot
        .negative_cycles(&live(&versions), SearchLimits { max_hops: 3, ..SearchLimits::default() });
    assert_eq!(found.len(), 1, "a 3-hop cycle at a bound of 3");

    let (none, _) = snapshot
        .negative_cycles(&live(&versions), SearchLimits { max_hops: 2, ..SearchLimits::default() });
    assert!(none.is_empty(), "the same cycle is out of reach at a bound of 2");
}

/// Two-token maps are not confused across chains: a `TokenId` carries its chain,
/// so a token address shared between chains is two nodes.
#[test]
fn tokens_are_chain_qualified() {
    let mut other = profitable_pair();
    other[1].from = TokenId { chain: ChainId(1), address: Address::repeat_byte(0x02) };
    let (cycles, _) = GraphSnapshot::from_edges(other)
        .negative_cycles(&live(&[(1, 5), (2, 5)]), SearchLimits::default());
    assert!(cycles.is_empty(), "the return leg starts from a different chain's token");

    let _ = BTreeMap::<TokenId, u8>::new();
}
