//! Precomputed cycle set, indexed by hop.
//!
//! Bellman-Ford re-runs per scan at O(V·E) per relaxation and rediscovers graph
//! *structure* every time. But structure — which tokens connect to which — is
//! nearly static, while *state* (reserves, `sqrtPrice`, liquidity) changes every
//! block. This module separates them:
//!
//! - **Offline / rarely:** enumerate the candidate cycle set from the token
//!   graph once ([`CycleIndex::build`]). Refresh only when the pool universe
//!   changes, detected via [`structure_digest`].
//! - **Per scan:** re-price precomputed cycles against updated state. Cost is
//!   O(cycles touched), not O(V·E).
//! - **Indexed by hop:** a state update re-prices only the cycles running
//!   through the token pairs that actually moved ([`CycleIndex::cycles_touching`]),
//!   typically a handful per block.
//!
//! A cycle here is **structure only** — an ordered list of token nodes. The
//! pools realising each hop are chosen per-scan from live state, because that
//! choice depends on rates that move every block while adjacency does not. One
//! `TokenCycle` therefore stands for every pool combination along that token
//! path, which is also what lets a downstream optimiser split a hop across
//! parallel pools rather than committing to one.
//!
//! This also removes the failure mode where Bellman-Ford returned
//! `found_total=1` in quiet markets: cycle *supply* becomes a property of the
//! precomputed set, not of whether a search happened to converge on that scan.

use crate::graph::Graph;
use ethers::types::Address;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Identifier of a cycle within a [`CycleIndex`].
pub type CycleId = u32;

/// Bounds on enumeration.
///
/// Measured on the real Base inventory (584 pools / 515 tokens / 1,168 directed
/// edges, 4 flash-loanable starts) via `cargo run --bin cycle_index_stats`:
///
/// | max_hops | cycles | build |
/// |----------|--------|-------|
/// | 2        |    536 |  <1ms |
/// | 4        |    860 |   1ms |
/// | 6        |  1,344 |  12ms |
///
/// Full depth is 1,344 cycles in 12ms, so the defaults track the configured
/// `MAX_HOPS` rather than trading coverage for a cost that turned out to be
/// negligible. `max_cycles` remains as a guard for denser graphs than Base.
#[derive(Clone, Copy, Debug)]
pub struct CycleIndexLimits {
    /// Maximum hops in a cycle (edges, not nodes). A 2-hop cycle is A→B→A.
    pub max_hops: usize,
    /// Minimum hops. Below 2 a "cycle" is a self-loop and not tradable.
    pub min_hops: usize,
    /// Hard ceiling on enumerated cycles. Enumeration proceeds by increasing
    /// length, so hitting this keeps the SHORT cycles and drops long ones —
    /// which is the right bias: long cycles pay more gas and more fee legs.
    pub max_cycles: usize,
}

impl Default for CycleIndexLimits {
    fn default() -> Self {
        Self {
            // Matches MAX_HOPS_CAP; measured at 1,344 cycles / 12ms on Base.
            max_hops: 6,
            min_hops: 2,
            max_cycles: 20_000,
        }
    }
}

/// The structural input: which pools exist and which token pair each serves.
///
/// This is deliberately NOT the graph's realised edge set. An edge only exists
/// once a pool has quoted successfully, so a pool whose quote times out drops
/// its token pair from the graph for that scan — and keying structure on the
/// edge set therefore reported a topology change every time a quote flaked.
/// Measured: after fixing index-vs-address keying, 8 of 16 scans still rebuilt,
/// with edge counts oscillating over just three distinct values (747/754/756).
///
/// Pool membership is what actually defines the cycle set, and it changes only
/// when the hot-pool inventory changes.
#[derive(Clone, Debug, Default)]
pub struct PoolUniverse {
    /// pool address -> the unordered token pair it serves.
    pools: HashMap<Address, (Address, Address)>,
    /// Distinct unordered token pairs, sorted. The cycle set depends on THIS,
    /// not on pool count: adding a fourth WETH/USDC fee tier is a new pool but
    /// not a new edge in the token graph, and must not invalidate the index.
    pairs: Vec<(Address, Address)>,
    /// Canonical unordered pair -> every pool serving it.
    ///
    /// The inverse of [`PoolUniverse::hops_for_pools`]. Deliberately NOT part
    /// of `digest`: the digest keys cycle-index staleness on adjacency, and
    /// this field is derived from the same `pools` map, so including it would
    /// add nothing but a rebuild trigger.
    by_pair: HashMap<(Address, Address), Vec<Address>>,
    digest: u64,
}

// main.rs compiles its own copy of this module; items used only by the
// library, tests or helper bins read as dead there.
#[allow(dead_code)]
impl PoolUniverse {
    /// Build from `(pool, token_a, token_b)` triples across every venue.
    pub fn from_pools(pools: impl IntoIterator<Item = (Address, Address, Address)>) -> Self {
        let mut map: HashMap<Address, (Address, Address)> = HashMap::new();
        for (pool, a, b) in pools {
            if a == b {
                continue;
            }
            map.insert(pool, (a, b));
        }

        let mut pairs: Vec<(Address, Address)> = map
            .values()
            .map(|(a, b)| if a <= b { (*a, *b) } else { (*b, *a) })
            .collect();
        pairs.sort_unstable();
        pairs.dedup();

        let mut by_pair: HashMap<(Address, Address), Vec<Address>> = HashMap::new();
        for (pool, (a, b)) in map.iter() {
            let key = if a <= b { (*a, *b) } else { (*b, *a) };
            by_pair.entry(key).or_default().push(*pool);
        }
        for pools in by_pair.values_mut() {
            pools.sort_unstable();
            pools.dedup();
        }

        let mut hasher = DefaultHasher::new();
        pairs.len().hash(&mut hasher);
        for (a, b) in &pairs {
            a.0.hash(&mut hasher);
            b.0.hash(&mut hasher);
        }

        Self {
            pools: map,
            digest: hasher.finish(),
            pairs,
            by_pair,
        }
    }

    pub fn digest(&self) -> u64 {
        self.digest
    }

    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    pub fn pair_count(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Ordered token hops served by any pool in `changed`. A pool serves both
    /// directions of its pair.
    pub fn hops_for_pools(
        &self,
        changed: &HashSet<Address>,
    ) -> HashSet<(Address, Address)> {
        let mut hops = HashSet::new();
        for pool in changed {
            if let Some((a, b)) = self.pools.get(pool) {
                hops.insert((*a, *b));
                hops.insert((*b, *a));
            }
        }
        hops
    }

    /// Every pool serving the token hop `from -> to`.
    ///
    /// A pool trades both directions, so the lookup is direction-insensitive.
    /// Returns all parallel pools: a hint on WETH/USDC must dirty every fee
    /// tier, not whichever one happened to be found first.
    pub fn pools_for_hop(&self, from: Address, to: Address) -> &[Address] {
        let key = if from <= to { (from, to) } else { (to, from) };
        self.by_pair.get(&key).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Directed adjacency over tokens: every pair, both ways.
    fn adjacency(&self) -> HashMap<Address, Vec<Address>> {
        let mut adj: HashMap<Address, HashSet<Address>> = HashMap::new();
        for (a, b) in &self.pairs {
            adj.entry(*a).or_default().insert(*b);
            adj.entry(*b).or_default().insert(*a);
        }
        adj.into_iter()
            .map(|(k, v)| {
                let mut t: Vec<Address> = v.into_iter().collect();
                t.sort_unstable();
                (k, t)
            })
            .collect()
    }
}

/// A cycle in the token graph, structure only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenCycle {
    /// Tokens in traversal order, **open** form: the closing hop back to
    /// `tokens[0]` is implied, not stored. Canonicalised so the numerically
    /// smallest address leads, making rotations of the same loop compare equal.
    ///
    /// Stored as ADDRESSES, not node indices. `Graph::add_node` assigns indices
    /// in first-seen order and the graph is rebuilt every scan, so an
    /// index-keyed cycle silently refers to different tokens after a rebuild.
    pub tokens: Vec<Address>,
}

// main.rs compiles its own copy of this module; items used only by the
// library, tests or helper bins read as dead there.
#[allow(dead_code)]
impl TokenCycle {
    /// Hop count — equal to token count, since the closing hop is implied.
    pub fn hops(&self) -> usize {
        self.tokens.len()
    }

    /// Ordered `(from, to)` hops, including the implied closing hop.
    pub fn hop_pairs(&self) -> impl Iterator<Item = (Address, Address)> + '_ {
        let n = self.tokens.len();
        (0..n).map(move |i| (self.tokens[i], self.tokens[(i + 1) % n]))
    }

    /// Rotate so the smallest address leads. Direction is preserved: A→B→C and
    /// A→C→B are genuinely different trades and must not collapse together.
    fn canonicalise(mut tokens: Vec<Address>) -> Self {
        if tokens.is_empty() {
            return Self { tokens };
        }
        let min_at = tokens
            .iter()
            .enumerate()
            .min_by_key(|(_, t)| **t)
            .map(|(i, _)| i)
            .unwrap_or(0);
        tokens.rotate_left(min_at);
        Self { tokens }
    }
}

/// Enumerated cycles plus a hop → cycle reverse index.
#[derive(Clone, Debug, Default)]
pub struct CycleIndex {
    cycles: Vec<TokenCycle>,
    by_hop: HashMap<(Address, Address), Vec<CycleId>>,
    structure_digest: u64,
    /// True when [`CycleIndexLimits::max_cycles`] stopped enumeration early, so
    /// callers can report that coverage is partial rather than silently
    /// treating a truncated set as exhaustive.
    pub truncated: bool,
}

// main.rs compiles its own copy of this module; items used only by the
// library, tests or helper bins read as dead there.
#[allow(dead_code)]
impl CycleIndex {
    pub fn len(&self) -> usize {
        self.cycles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cycles.is_empty()
    }

    pub fn cycle(&self, id: CycleId) -> Option<&TokenCycle> {
        self.cycles.get(id as usize)
    }

    pub fn cycles(&self) -> &[TokenCycle] {
        &self.cycles
    }

    pub fn digest(&self) -> u64 {
        self.structure_digest
    }

    /// True when the pool universe no longer matches what this index was built
    /// from. Cheap enough to call per scan.
    pub fn is_stale(&self, universe: &PoolUniverse) -> bool {
        self.structure_digest != universe.digest()
    }

    /// Cycle ids traversing any of `hops`, deduplicated.
    ///
    /// This is the hot-path lookup: given the token pairs whose pools moved this
    /// block, return only the cycles that need re-pricing.
    pub fn cycles_touching(
        &self,
        hops: impl IntoIterator<Item = (Address, Address)>,
    ) -> Vec<CycleId> {
        let mut seen: HashSet<CycleId> = HashSet::new();
        let mut out = Vec::new();
        for hop in hops {
            if let Some(ids) = self.by_hop.get(&hop) {
                for id in ids {
                    if seen.insert(*id) {
                        out.push(*id);
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// Bounded hot-path lookup: the touched cycles worth pricing first, and how
    /// many were touched in total.
    ///
    /// The plan specified `cycles_touching(..).take(limit)`. That would be an
    /// ARBITRARY selection: `cycles_touching` sorts by `CycleId`, which is
    /// enumeration order, so `take` keeps whichever cycles happened to be built
    /// first. Calling the result "best" would be a claim the index cannot
    /// support — it holds no prices and therefore cannot rank by profit.
    ///
    /// What it CAN rank by is length, and shorter is genuinely better on the
    /// hot path: fewer legs is less gas, fewer swaps to fail, and less chance a
    /// leg moves between quote and execution. So the cap keeps the shortest
    /// cycles, breaking ties by id for determinism.
    ///
    /// Returns the total touched count as well, because a cap that silently
    /// drops candidates reads as "these are all of them". Ranking by expected
    /// net profit is the caller's job, once quotes exist.
    pub fn cycles_touching_limited(
        &self,
        hops: impl IntoIterator<Item = (Address, Address)>,
        limit: usize,
    ) -> (Vec<CycleId>, usize) {
        let mut ids = self.cycles_touching(hops);
        let total = ids.len();
        ids.sort_by_key(|id| {
            let len = self
                .cycle(*id)
                .map(|c| c.tokens.len())
                .unwrap_or(usize::MAX);
            (len, *id)
        });
        ids.truncate(limit);
        (ids, total)
    }

    /// Whether the token loop `tokens` (open or closed, any rotation) is in the
    /// index.
    pub fn contains_tokens(&self, tokens: &[Address]) -> bool {
        let mut open = tokens.to_vec();
        if open.len() > 1 && open.first() == open.last() {
            open.pop();
        }
        if open.is_empty() {
            return false;
        }
        let canonical = TokenCycle::canonicalise(open);
        self.cycles.iter().any(|c| c.tokens == canonical.tokens)
    }

    /// Membership for a cycle expressed as `graph`'s node indices.
    ///
    /// Resolves indices to tokens through the CALLER's graph, so a cached index
    /// stays valid across rebuilds that reshuffle index assignment. This is the
    /// cut-over safety check: before the index can replace the live search it
    /// must be a SUPERSET of what that search surfaces, and a miss means
    /// switching over would silently drop a profitable cycle.
    pub fn contains_nodes_in(&self, graph: &Graph, nodes: &[usize]) -> bool {
        let tokens: Vec<Address> = nodes
            .iter()
            .filter_map(|n| graph.nodes.get(*n).copied())
            .collect();
        if tokens.len() != nodes.len() {
            return false;
        }
        self.contains_tokens(&tokens)
    }

    /// Enumerate cycles reachable from `starts`, shortest first.
    ///
    /// `starts` are the tokens a cycle may open and close on — in practice the
    /// flash-loanable set, since a cycle that cannot be funded cannot be traded.
    /// Restricting starts is what keeps this tractable: unrestricted enumeration
    /// over every token is combinatorial.
    ///
    /// Takes the [`PoolUniverse`] rather than the graph, so a transient quote
    /// failure cannot look like a topology change.
    pub fn build(universe: &PoolUniverse, starts: &[Address], limits: CycleIndexLimits) -> Self {
        let adjacency = universe.adjacency();
        let max_hops = limits.max_hops.max(limits.min_hops);
        let mut seen: HashSet<Vec<Address>> = HashSet::new();
        let mut cycles: Vec<TokenCycle> = Vec::new();
        let mut truncated = false;

        // Iterative deepening: emit every cycle of length d before any of length
        // d+1, so a truncated set is the SHORTEST cycles rather than an
        // arbitrary prefix of a depth-first walk.
        'outer: for depth in limits.min_hops.max(2)..=max_hops {
            for start in starts {
                if !adjacency.contains_key(start) {
                    continue;
                }
                let mut path = vec![*start];
                let mut on_path: HashSet<Address> = HashSet::from([*start]);
                if !enumerate_at_depth(
                    &adjacency,
                    *start,
                    depth,
                    &mut path,
                    &mut on_path,
                    &mut seen,
                    &mut cycles,
                    limits.max_cycles,
                ) {
                    truncated = true;
                    break 'outer;
                }
            }
        }

        let mut by_hop: HashMap<(Address, Address), Vec<CycleId>> = HashMap::new();
        for (id, cycle) in cycles.iter().enumerate() {
            for hop in cycle.hop_pairs() {
                by_hop.entry(hop).or_default().push(id as CycleId);
            }
        }

        Self {
            cycles,
            by_hop,
            structure_digest: universe.digest(),
            truncated,
        }
    }
}

/// DFS for simple cycles of exactly `remaining` more hops back to `start`.
/// Returns `false` once `max_cycles` is reached, so the caller can stop and
/// flag truncation rather than silently capping.
#[allow(clippy::too_many_arguments)]
fn enumerate_at_depth(
    adjacency: &HashMap<Address, Vec<Address>>,
    start: Address,
    remaining: usize,
    path: &mut Vec<Address>,
    on_path: &mut HashSet<Address>,
    seen: &mut HashSet<Vec<Address>>,
    out: &mut Vec<TokenCycle>,
    max_cycles: usize,
) -> bool {
    let Some(current) = path.last().copied() else {
        return true;
    };
    let Some(neighbours) = adjacency.get(&current) else {
        return true;
    };

    for next in neighbours {
        if remaining == 1 {
            if *next != start {
                continue;
            }
            let cycle = TokenCycle::canonicalise(path.clone());
            if seen.insert(cycle.tokens.clone()) {
                out.push(cycle);
                if out.len() >= max_cycles {
                    return false;
                }
            }
            continue;
        }
        // Simple cycles only: revisiting an intermediate token means the path
        // contains a shorter cycle, which is enumerated at its own depth.
        if *next == start || on_path.contains(next) {
            continue;
        }
        path.push(*next);
        on_path.insert(*next);
        let ok = enumerate_at_depth(
            adjacency, start, remaining - 1, path, on_path, seen, out, max_cycles,
        );
        on_path.remove(next);
        path.pop();
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, VenueEdge};
    use ethers::types::U256;

    pub(crate) fn addr(id: u64) -> Address {
        Address::from_low_u64_be(id)
    }

    /// Structural edge: only `from`/`to` and a pool identity matter here, since
    /// enumeration reads adjacency and never rates.
    fn edge(from: Address, to: Address, pool: Address) -> Edge {
        Edge {
            from,
            to,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV2 {
                pair: pool,
                token_out: to,
                token0: from,
                token1: to,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::MAX,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        }
    }

    fn graph_from(edges: Vec<Edge>) -> Graph {
        let mut graph = Graph::default();
        for e in edges {
            graph.add_edge(e);
        }
        graph
    }

    /// Universe view of an existing Graph fixture, so both views stay in step.
    fn universe_of(graph: &Graph) -> PoolUniverse {
        PoolUniverse::from_pools(graph.edges.iter().filter_map(|e| {
            crate::venues::edge_pool_address(e).map(|p| (p, e.from, e.to))
        }))
    }

    /// Directed triangle 1→2→3→1.
    fn triangle_graph() -> Graph {
        graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(3), addr(101)),
            edge(addr(3), addr(1), addr(102)),
        ])
    }

    /// A→B→A, both directions present.
    fn two_hop_graph() -> Graph {
        graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(1), addr(100)),
        ])
    }

    #[test]
    fn enumerates_a_simple_two_hop_cycle() {
        let graph = two_hop_graph();
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        assert_eq!(idx.len(), 1, "A->B->A is one cycle");
        let cycle = idx.cycle(0).expect("cycle 0");
        assert_eq!(cycle.hops(), 2);
        let hops: Vec<_> = cycle.hop_pairs().collect();
        assert_eq!(hops.len(), 2, "closing hop is implied, not stored");
    }

    #[test]
    fn parallel_pools_collapse_to_one_structural_cycle() {
        // Same token pair via four fee tiers — structurally ONE hop. Which pool
        // (or split) to use is a per-scan pricing decision, not a new cycle.
        let graph = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(1), addr(2), addr(101)),
            edge(addr(1), addr(2), addr(102)),
            edge(addr(2), addr(1), addr(100)),
            edge(addr(2), addr(1), addr(101)),
        ]);
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        assert_eq!(idx.len(), 1, "5 edges over 1 token pair is still 1 cycle");
    }

    #[test]
    fn rotations_of_the_same_loop_are_deduplicated() {
        // Triangle reachable from two different starts; the same loop must not
        // be counted twice just because enumeration entered it elsewhere.
        let graph = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(3), addr(101)),
            edge(addr(3), addr(1), addr(102)),
        ]);
        let idx = CycleIndex::build(
            &universe_of(&graph),
            &[addr(1), addr(2), addr(3)],
            CycleIndexLimits::default(),
        );
        // A pool trades BOTH ways, so the universe is pair-keyed and yields the
        // triangle in each direction — but only once per direction, however many
        // entry points reach it.
        let triangles = idx.cycles().iter().filter(|c| c.hops() == 3).count();
        assert_eq!(triangles, 2, "one triangle per direction, not per entry point");
    }

    #[test]
    fn direction_is_preserved_when_deduplicating() {
        // A->B->C->A and A->C->B->A are different trades and must both survive.
        let graph = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(3), addr(101)),
            edge(addr(3), addr(1), addr(102)),
            edge(addr(1), addr(3), addr(102)),
            edge(addr(3), addr(2), addr(101)),
            edge(addr(2), addr(1), addr(100)),
        ]);
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        let triangles = idx.cycles().iter().filter(|c| c.hops() == 3).count();
        assert_eq!(triangles, 2, "both traversal directions are distinct trades");
    }

    #[test]
    fn hop_index_returns_only_cycles_through_changed_pairs() {
        // Two disjoint loops sharing node 1: 1->2->1 and 1->3->1.
        let graph = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(1), addr(100)),
            edge(addr(1), addr(3), addr(200)),
            edge(addr(3), addr(1), addr(200)),
        ]);
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        assert_eq!(idx.len(), 2);

        let touched = idx.cycles_touching([(addr(1), addr(2))]);
        assert_eq!(touched.len(), 1, "only the 1<->2 loop re-prices");

        let cycle = idx.cycle(touched[0]).expect("touched cycle");
        assert!(
            cycle.tokens.contains(&addr(2)),
            "the returned cycle must actually traverse the changed pair"
        );
    }

    #[test]
    fn pools_for_hop_resolves_both_directions_to_the_same_pools() {
        let graph = two_hop_graph();
        let universe = universe_of(&graph);
        let forward = universe.pools_for_hop(addr(1), addr(2));
        let reverse = universe.pools_for_hop(addr(2), addr(1));
        assert_eq!(forward, [addr(100)], "hop must resolve to its pool");
        assert_eq!(
            forward, reverse,
            "a pool trades both ways; direction must not change the answer"
        );
    }

    #[test]
    fn pools_for_hop_returns_every_parallel_pool() {
        // Same token pair across three fee tiers. A hint on this pair must
        // dirty ALL of them, not an arbitrary one.
        let graph = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(1), addr(2), addr(101)),
            edge(addr(1), addr(2), addr(102)),
        ]);
        let mut pools = universe_of(&graph).pools_for_hop(addr(1), addr(2)).to_vec();
        pools.sort_unstable();
        assert_eq!(pools, vec![addr(100), addr(101), addr(102)]);
    }

    #[test]
    fn pools_for_hop_is_empty_for_an_unknown_pair() {
        let universe = universe_of(&two_hop_graph());
        assert!(
            universe.pools_for_hop(addr(7), addr(8)).is_empty(),
            "an unknown pair must yield nothing, not a panic or a wrong pool"
        );
    }

    #[test]
    fn the_reverse_index_does_not_perturb_the_digest() {
        // digest() drives cycle-index staleness. If adding by_pair changed it,
        // every scan would rebuild.
        let graph = two_hop_graph();
        let universe = universe_of(&graph);
        let idx = CycleIndex::build(&universe, &[addr(1)], CycleIndexLimits::default());
        assert!(!idx.is_stale(&universe_of(&two_hop_graph())));
    }

    #[test]
    fn pool_addresses_map_to_the_hops_they_serve() {
        let graph = two_hop_graph();
        let pools = HashSet::from([addr(100)]);
        let hops = universe_of(&graph).hops_for_pools(&pools);
        assert_eq!(hops.len(), 2, "one pool serves both directions of its pair");
    }

    #[test]
    fn digest_is_stable_under_edge_insertion_order() {
        // Graph::add_node assigns indices in first-seen order and the graph is
        // rebuilt fresh every scan, while quoting is concurrent — so the SAME
        // adjacency arrives in a different order each scan. Keying the digest
        // on node indices made every scan look like a structure change.
        let forward = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(3), addr(101)),
            edge(addr(3), addr(1), addr(102)),
        ]);
        let shuffled = graph_from(vec![
            edge(addr(3), addr(1), addr(102)),
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(3), addr(101)),
        ]);

        // Same topology, different index assignment.
        assert_ne!(
            forward.ix[&addr(1)], shuffled.ix[&addr(1)],
            "precondition: insertion order really does shift indices"
        );
        assert_eq!(
            universe_of(&forward).digest(),
            universe_of(&shuffled).digest(),
            "identical adjacency must produce an identical digest"
        );

        let idx = CycleIndex::build(&universe_of(&forward), &[addr(1)], CycleIndexLimits::default());
        assert!(
            !idx.is_stale(&universe_of(&shuffled)),
            "a reordered rebuild of the same graph must not invalidate the set"
        );
    }

    #[test]
    fn membership_survives_reindexing() {
        // The cached cycle must still resolve to the same TOKENS after the
        // graph is rebuilt with different index assignment — otherwise
        // contains_nodes silently compares against the wrong tokens.
        let forward = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(3), addr(101)),
            edge(addr(3), addr(1), addr(102)),
        ]);
        let shuffled = graph_from(vec![
            edge(addr(2), addr(3), addr(101)),
            edge(addr(3), addr(1), addr(102)),
            edge(addr(1), addr(2), addr(100)),
        ]);
        let idx = CycleIndex::build(&universe_of(&forward), &[addr(1)], CycleIndexLimits::default());

        let cycle_in_shuffled = vec![
            shuffled.ix[&addr(1)],
            shuffled.ix[&addr(2)],
            shuffled.ix[&addr(3)],
        ];
        assert!(
            idx.contains_nodes_in(&shuffled, &cycle_in_shuffled),
            "the same token loop must be recognised under new indices"
        );
    }

    #[test]
    fn structure_digest_ignores_state_but_tracks_topology() {
        let graph = two_hop_graph();
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        assert!(!idx.is_stale(&universe_of(&graph)));

        // State churn must NOT invalidate: that is the whole point of splitting
        // structure from state.
        let mut restated = two_hop_graph();
        for e in restated.edges.iter_mut() {
            e.rate_num = U256::from(999u64);
            e.active = false;
            e.quote_block = Some(1234u64.into());
        }
        assert!(
            !idx.is_stale(&universe_of(&restated)),
            "rates/liquidity/active must not invalidate the cycle set"
        );

        // A genuinely new token pair must invalidate.
        let mut grown = two_hop_graph();
        grown.add_edge(edge(addr(1), addr(9), addr(300)));
        assert!(idx.is_stale(&universe_of(&grown)), "new adjacency must invalidate");
    }

    #[test]
    fn truncation_keeps_short_cycles_and_is_reported() {
        // Dense 6-clique: many cycles at every length.
        let mut edges = Vec::new();
        for a in 1..=6u64 {
            for b in 1..=6u64 {
                if a != b {
                    edges.push(edge(addr(a), addr(b), addr(1000 + a * 10 + b)));
                }
            }
        }
        let graph = graph_from(edges);
        let limits = CycleIndexLimits {
            max_hops: 5,
            min_hops: 2,
            max_cycles: 8,
        };
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], limits);
        assert!(idx.truncated, "hitting the cap must be reported, not silent");
        assert!(idx.len() <= 8);
        // Iterative deepening means the survivors are the SHORTEST cycles.
        let longest = idx.cycles().iter().map(|c| c.hops()).max().unwrap_or(0);
        let shortest = idx.cycles().iter().map(|c| c.hops()).min().unwrap_or(0);
        assert_eq!(shortest, 2, "2-hop cycles must be enumerated first");
        assert!(longest <= 3, "truncation must not keep long cycles over short");
    }

    #[test]
    fn unreachable_start_yields_no_cycles() {
        let graph = two_hop_graph();
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(42)], CycleIndexLimits::default());
        assert!(idx.is_empty());
        assert!(!idx.truncated);
    }

    #[test]
    fn respects_max_hops() {
        // 4-cycle only: 1->2->3->4->1.
        let graph = graph_from(vec![
            edge(addr(1), addr(2), addr(100)),
            edge(addr(2), addr(3), addr(101)),
            edge(addr(3), addr(4), addr(102)),
            edge(addr(4), addr(1), addr(103)),
        ]);
        let tight = CycleIndexLimits {
            max_hops: 3,
            ..CycleIndexLimits::default()
        };
        let tight_idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], tight);
        assert!(
            tight_idx.cycles().iter().all(|c| c.hops() <= 3),
            "max_hops must bind"
        );
        assert!(
            !tight_idx.cycles().iter().any(|c| c.hops() == 4),
            "the 4-cycle must be excluded below its length"
        );

        let loose = CycleIndexLimits {
            max_hops: 4,
            ..CycleIndexLimits::default()
        };
        let loose_idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], loose);
        assert!(
            loose_idx.cycles().iter().any(|c| c.hops() == 4),
            "the 4-cycle must appear once max_hops allows it"
        );
    }


    /// The cap must not be arbitrary. `cycles_touching` sorts by `CycleId`,
    /// which is enumeration order, so a plain `.take(limit)` keeps whichever
    /// cycles happened to be built first. Ranking by length is the only
    /// ordering the index can actually justify — and shorter is genuinely
    /// better on the hot path: less gas, fewer legs to fail.
    #[test]
    fn the_touched_cap_keeps_the_shortest_cycles() {
        let graph = triangle_graph();
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        let hop = (addr(1), addr(2));
        let (all, total) = idx.cycles_touching_limited([hop], usize::MAX);
        assert_eq!(all.len(), total, "an unbounded cap drops nothing");

        let mut lens: Vec<usize> = all
            .iter()
            .map(|id| idx.cycle(*id).expect("cycle").tokens.len())
            .collect();
        let sorted = {
            let mut v = lens.clone();
            v.sort_unstable();
            v
        };
        assert_eq!(lens, sorted, "selection order must be shortest-first");
        lens.dedup();

        if total > 1 {
            let (one, total_again) = idx.cycles_touching_limited([hop], 1);
            assert_eq!(one.len(), 1);
            assert_eq!(
                total_again, total,
                "the total must survive truncation, or a cap reads as \
                 'these are all of them'"
            );
            let shortest = idx.cycle(one[0]).expect("cycle").tokens.len();
            for id in &all {
                assert!(
                    idx.cycle(*id).expect("cycle").tokens.len() >= shortest,
                    "the kept cycle must be no longer than any dropped one"
                );
            }
        }
    }

    /// A hop nothing traverses yields nothing, and says so rather than
    /// reporting a truncated view of an empty set.
    #[test]
    fn an_untouched_hop_yields_no_cycles() {
        let graph = triangle_graph();
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        let (ids, total) = idx.cycles_touching_limited([(addr(90), addr(91))], 32);
        assert!(ids.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn contains_nodes_matches_closed_and_rotated_forms() {
        let graph = triangle_graph();
        let idx = CycleIndex::build(&universe_of(&graph), &[addr(1)], CycleIndexLimits::default());
        let n1 = graph.ix[&addr(1)];
        let n2 = graph.ix[&addr(2)];
        let n3 = graph.ix[&addr(3)];

        // BF emits closed form; the index stores open form.
        assert!(idx.contains_nodes_in(&graph, &[n1, n2, n3, n1]), "closed form");
        assert!(idx.contains_nodes_in(&graph, &[n1, n2, n3]), "open form");
        // Any rotation is the same loop.
        assert!(idx.contains_nodes_in(&graph, &[n2, n3, n1, n2]), "rotated closed");
        // Reversed traversal is a DIFFERENT trade, and since a pool trades both
        // ways the pair-keyed universe contains it too — as its own cycle, not
        // as a rotation of the forward one.
        assert!(
            idx.contains_nodes_in(&graph, &[n1, n3, n2, n1]),
            "reverse direction is its own tradable cycle"
        );
        assert!(!idx.contains_nodes_in(&graph, &[]), "empty is not a cycle");
    }

    #[test]
    fn pair_keying_makes_adjacency_bidirectional() {
        // Structural consequence of keying on pools rather than realised edges:
        // a pool is tradable in both directions regardless of which direction
        // happened to quote this scan. This newly surfaces the parallel-pool
        // 2-hop arb (buy on one fee tier, sell on another) that a one-directional
        // edge view could miss entirely.
        let one_way = graph_from(vec![edge(addr(1), addr(2), addr(100))]);
        let idx = CycleIndex::build(
            &universe_of(&one_way),
            &[addr(1)],
            CycleIndexLimits::default(),
        );
        assert_eq!(idx.len(), 1, "A<->B is one 2-hop cycle");
        assert!(idx.contains_tokens(&[addr(1), addr(2)]));
    }
}
