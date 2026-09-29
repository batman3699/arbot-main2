//! **BP-176.** The precomputed route frontier: eight carried attributes, and
//! events revalue known routes before anything discovers.

use alloy_primitives::{Address, B256};
use apex_search::frontier::{
    FeeVariant, Frontier, GasClass, Revalued, RouteId, RouteTemplate, TickNeighborhood,
};
use apex_state::feed::event::{EventKind, StateEvent};
use apex_state::Ordinal;
use apex_types::ids::{ChainId, FlashProviderId, PoolId, TokenId, VenueId};
use apex_types::state::StateFingerprint;
use apex_types::time::{DurationNanos, UnixNanos};
use std::collections::BTreeMap;

const BASE: ChainId = ChainId(8453);
const NOW: UnixNanos = UnixNanos(1_781_049_614_240_000_000);

fn pool(n: u8) -> PoolId {
    PoolId { chain: BASE, address: Address::repeat_byte(n) }
}

fn token(n: u8) -> TokenId {
    TokenId { chain: BASE, address: Address::repeat_byte(n) }
}

/// A two-hop template across two venues at the given fees — the shape the census
/// found dominant at every percentile.
fn template(id: u64, pools: &[(u8, u16, u32)]) -> RouteTemplate {
    RouteTemplate {
        id: RouteId(id),
        chain: BASE,
        topology: vec![token(0x01), token(0x02), token(0x01)],
        venue_sequence: pools.iter().map(|(_, v, _)| VenueId(*v)).collect(),
        fee_variants: pools
            .iter()
            .map(|(p, v, fee)| FeeVariant { venue: VenueId(*v), pool: pool(*p), fee_ppm: *fee })
            .collect(),
        tick_neighborhood: BTreeMap::new(),
        hook_fingerprint: None,
        flash_source: FlashProviderId(1),
        expected_gas_class: GasClass::TwoHopConstantProduct,
        last_profitable: None,
    }
}

fn swap_on(pools: &[u8], at: UnixNanos) -> StateEvent {
    StateEvent {
        chain: BASE,
        at: Ordinal::confirmed(47_079_437, 12, 0),
        observed_at: at,
        fingerprint: StateFingerprint {
            chain_id: BASE,
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
        kind: EventKind::PendingSwap {
            target: B256::repeat_byte(0x7f),
            pools: pools.iter().copied().map(pool).collect(),
            notional_usd: Some(7_500.0),
        },
    }
}

/// A broad graph search, standing in for Engine A. It **cannot be called**
/// without a [`Revalued`], and that is BP-176's claim expressed as a signature.
///
/// The `compile_fail` doctest on `Revalued` is the other half: the token cannot
/// be forged, so there is no path to this function that skipped the frontier.
fn broad_discovery(_after: &Revalued, graph_consulted: &mut bool) -> Vec<RouteId> {
    *graph_consulted = true;
    Vec::new()
}

/// **BP-176's test.** An event on a resident template's pool yields that template,
/// and the graph is not consulted to produce it.
///
/// The assertion is structural rather than temporal. "The frontier was consulted
/// first" measured with a stopwatch is a flaky proxy; here `revalue` takes no
/// graph, no registry and no network — it *cannot* consult one — and
/// `broad_discovery` cannot run without the token `revalue` returns.
#[test]
fn frontier_revalues_first() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, &[(0x33, 1, 500), (0x44, 2, 300)])).expect("a consistent template");
    frontier.insert(template(2, &[(0x55, 1, 100), (0x66, 2, 100)])).expect("a consistent template");

    let mut graph_consulted = false;
    let (hits, permit) = frontier.revalue(&swap_on(&[0x33], NOW));

    assert_eq!(hits, vec![RouteId(1)], "the event's pool is in template 1 and not template 2");
    assert!(!graph_consulted, "revaluation produced a route without any discovery");
    assert_eq!(permit.hits(), 1);
    assert_eq!(permit.event_ordinal(), swap_on(&[0x33], NOW).at, "the permit names its event");

    // Only now may broad discovery run, and it needs the token to do so.
    let _ = broad_discovery(&permit, &mut graph_consulted);
    assert!(graph_consulted);
}

/// An event on no resident pool produces no hits — and still a permit, because
/// the frontier *was* consulted and came up empty. That is the case broad
/// discovery exists for, and `hits() == 0` is the signal that it is doing real
/// work rather than duplicating the frontier.
#[test]
fn an_empty_revaluation_still_permits_discovery() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, &[(0x33, 1, 500), (0x44, 2, 300)])).expect("a consistent template");

    let (hits, permit) = frontier.revalue(&swap_on(&[0x99], NOW));
    assert!(hits.is_empty());
    assert_eq!(permit.hits(), 0);
}

/// **The ranking is fee-first, and this is the measurement that says so.**
///
/// Fee-aware pool selection alone cut this repository's arbitrage hurdle from
/// −247 bps to −10 bps median — a ~24× reduction with no change to the search.
/// A frontier that ranked by recency first would hand the expensive route to a
/// downstream that then has to reject it, which is the fee-blind ranking the
/// measurement replaced.
#[test]
fn the_ranking_is_fee_first() {
    let mut frontier = Frontier::new();

    // Expensive but profitable moments ago.
    let mut hot_expensive = template(1, &[(0x33, 1, 3_000), (0x44, 2, 3_000)]);
    hot_expensive.last_profitable = Some(NOW);
    frontier.insert(hot_expensive).expect("a consistent template");

    // Cheap and never yet profitable.
    frontier.insert(template(2, &[(0x33, 3, 100), (0x44, 4, 100)])).expect("a consistent template");

    let (hits, _) = frontier.revalue(&swap_on(&[0x33], NOW));
    assert_eq!(
        hits,
        vec![RouteId(2), RouteId(1)],
        "6,000 ppm of fee is not outranked by having worked recently"
    );
}

/// ...and recency is the tiebreak, so it still decides between equal-fee routes.
/// Without this, "fee first" is satisfied by ignoring recency entirely.
#[test]
fn recency_breaks_ties_between_equally_cheap_routes() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, &[(0x33, 1, 100), (0x44, 2, 100)])).expect("a consistent template");
    let mut recent = template(2, &[(0x33, 3, 100), (0x44, 4, 100)]);
    recent.last_profitable = Some(NOW);
    frontier.insert(recent).expect("a consistent template");

    let (hits, _) = frontier.revalue(&swap_on(&[0x33], NOW));
    assert_eq!(hits, vec![RouteId(2), RouteId(1)], "equal fee, so recency decides");
}

/// `hot_path.rs`'s decay, carried forward unchanged.
#[test]
fn the_recency_signal_decays_linearly_and_clamps() {
    let window = DurationNanos(30_000_000_000);
    let mut t = template(1, &[(0x33, 1, 100)]);

    assert!(
        (t.recency_score(NOW, window) - 0.0).abs() < f64::EPSILON,
        "a route that has never been profitable scores 0, not 1"
    );

    t.last_profitable = Some(NOW);
    assert!((t.recency_score(NOW, window) - 1.0).abs() < f64::EPSILON);

    let half = UnixNanos(NOW.0 + 15_000_000_000);
    assert!((t.recency_score(half, window) - 0.5).abs() < 1e-9);

    let past = UnixNanos(NOW.0 + 60_000_000_000);
    assert!((t.recency_score(past, window) - 0.0).abs() < f64::EPSILON, "clamped, not negative");

    // A zero window disables the signal rather than dividing by zero.
    assert!((t.recency_score(NOW, DurationNanos(0)) - 0.0).abs() < f64::EPSILON);
}

/// Re-inserting a template whose pools changed must not leave it indexed under
/// the old ones. A stale index entry is a route revalued for an event that no
/// longer touches it — which reaches the sizing path as a real proposal and is
/// rejected there, so it costs compute on the hot path and shows up nowhere.
#[test]
fn reinserting_a_template_reindexes_its_pools() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, &[(0x33, 1, 100), (0x44, 2, 100)])).expect("a consistent template");

    let (before, _) = frontier.revalue(&swap_on(&[0x33], NOW));
    assert_eq!(before, vec![RouteId(1)]);

    // Same id, different pools.
    frontier.insert(template(1, &[(0x55, 1, 100), (0x66, 2, 100)])).expect("a consistent template");

    let (stale, _) = frontier.revalue(&swap_on(&[0x33], NOW));
    assert!(stale.is_empty(), "the old pool must no longer reach this template");

    let (fresh, _) = frontier.revalue(&swap_on(&[0x55], NOW));
    assert_eq!(fresh, vec![RouteId(1)]);
    assert_eq!(frontier.len(), 1, "re-insertion is a replacement, not a second copy");
}

/// Removal takes the index with it, including collapsing an empty pool bucket —
/// otherwise the map grows without bound over a long run as templates rotate.
#[test]
fn removal_cleans_the_index() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, &[(0x33, 1, 100)])).expect("a consistent template");
    assert!(frontier.remove(RouteId(1)).is_some());
    assert!(frontier.is_empty());

    let (hits, _) = frontier.revalue(&swap_on(&[0x33], NOW));
    assert!(hits.is_empty());
    assert!(frontier.remove(RouteId(1)).is_none(), "removing twice is not an error and not a hit");
}

/// A block moves state without naming pools, so it revalues nothing and can only
/// reach broad discovery — the slow path, by construction (§2.6).
#[test]
fn a_block_event_revalues_nothing() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, &[(0x33, 1, 100)])).expect("a consistent template");

    let mut block = swap_on(&[0x33], NOW);
    block.kind = EventKind::Block;

    let (hits, permit) = frontier.revalue(&block);
    assert!(hits.is_empty(), "a block names no pools; answering 'all' would rebuild everything");
    assert_eq!(permit.hits(), 0);
}

/// `mark_profitable` is `hot_path.rs`'s write path and must reach the ranking.
#[test]
fn marking_a_route_profitable_moves_it_up_the_tiebreak() {
    let mut frontier = Frontier::new();
    frontier.insert(template(1, &[(0x33, 1, 100), (0x44, 2, 100)])).expect("a consistent template");
    frontier.insert(template(2, &[(0x33, 3, 100), (0x44, 4, 100)])).expect("a consistent template");

    let (before, _) = frontier.revalue(&swap_on(&[0x33], NOW));
    assert_eq!(before, vec![RouteId(1), RouteId(2)], "equal on everything, so id decides");

    assert!(frontier.mark_profitable(RouteId(2), NOW));
    let (after, _) = frontier.revalue(&swap_on(&[0x33], NOW));
    assert_eq!(after, vec![RouteId(2), RouteId(1)]);

    assert!(!frontier.mark_profitable(RouteId(99), NOW), "an unknown route is not silently added");
}

/// The cheapest variant of each hop is what counts. A template offering the same
/// venue at 100 and 3000 ppm costs 100 — carrying both is the point of
/// `fee_variants`, and summing them would make a template look worse the more
/// options it has.
#[test]
fn the_fee_total_takes_the_cheapest_variant_per_venue() {
    let t = template(
        1,
        &[(0x33, 1, 3_000), (0x34, 1, 100), (0x44, 2, 500), (0x45, 2, 300)],
    );
    assert_eq!(t.cheapest_total_fee_ppm(), 400, "100 + 300, not 3900");
}

/// A `TickNeighborhood` says which ticks a CL price assumed. Its absence is the
/// measured mechanism behind a ~140 bps local-vs-quoter gap: the fast path
/// carried no tick ladder, so every CL quote was single-tick plus a haircut.
#[test]
fn a_tick_neighborhood_bounds_where_a_cl_price_is_valid() {
    let n = TickNeighborhood { lower: -60, upper: 60 };
    assert!(n.contains(0));
    assert!(n.contains(-60), "inclusive at the lower edge");
    assert!(n.contains(60), "inclusive at the upper edge");
    assert!(!n.contains(61));

    // A constant-product pool has no entry at all -- which is not the same as an
    // empty range. An empty range would make every route look expired.
    let t = template(1, &[(0x33, 1, 100)]);
    assert!(t.tick_neighborhood.is_empty());
}

/// The check that replaced the dead chain filter in `revalue`.
///
/// A mutation deleting that filter broke nothing, because `PoolId` carries its
/// chain and `by_pool`'s keys are therefore already chain-qualified — the filter
/// could never fire. The disagreement it was reaching for is real, and it happens
/// at insertion: a template assembled against the wrong registry. Refused there,
/// it cannot become resident; allowed there, no later check can tell it from a
/// correct one.
#[test]
fn a_template_naming_another_chains_pool_is_refused() {
    let mut frontier = Frontier::new();
    let mut wrong = template(1, &[(0x33, 1, 100)]);
    wrong.fee_variants[0].pool = PoolId {
        chain: ChainId(1),
        address: Address::repeat_byte(0x33),
    };

    let err = frontier.insert(wrong).expect_err("a cross-chain pool must be refused");
    assert_eq!(err.id, RouteId(1));
    assert_eq!(err.chain, BASE);
    assert_eq!(err.stray.chain, ChainId(1));
    assert!(frontier.is_empty(), "and it must not have become resident");

    // The complement, so the pair discriminates: a consistent template goes in.
    assert!(frontier.insert(template(2, &[(0x33, 1, 100)])).is_ok());
    assert_eq!(frontier.len(), 1);
}
