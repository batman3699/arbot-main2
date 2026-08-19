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

/// Node index into [`Graph::nodes`]. `u32` keeps the index compact; the token
/// universe is capped far below `u32::MAX` by `TOKEN_WHITELIST_MAX`.
pub type NodeId = u32;

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

/// A cycle in the token graph, structure only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenCycle {
    /// Node indices in traversal order, **open** form: the closing hop back to
    /// `nodes[0]` is implied, not stored. Canonicalised so the numerically
    /// smallest node is first, making rotations of the same loop compare equal.
    pub nodes: Vec<NodeId>,
}

impl TokenCycle {
    /// Hop count — equal to node count, since the closing hop is implied.
    pub fn hops(&self) -> usize {
        self.nodes.len()
    }

    /// Ordered `(from, to)` hops, including the implied closing hop.
    pub fn hop_pairs(&self) -> impl Iterator<Item = (NodeId, NodeId)> + '_ {
        let n = self.nodes.len();
        (0..n).map(move |i| (self.nodes[i], self.nodes[(i + 1) % n]))
    }

    /// Rotate so the smallest node leads. Direction is preserved: A→B→C and
    /// A→C→B are genuinely different trades and must not collapse together.
    fn canonicalise(mut nodes: Vec<NodeId>) -> Self {
        if nodes.is_empty() {
            return Self { nodes };
        }
        let min_at = nodes
            .iter()
            .enumerate()
            .min_by_key(|(_, n)| **n)
            .map(|(i, _)| i)
            .unwrap_or(0);
        nodes.rotate_left(min_at);
        Self { nodes }
    }
}

/// Enumerated cycles plus a hop → cycle reverse index.
#[derive(Clone, Debug, Default)]
pub struct CycleIndex {
    cycles: Vec<TokenCycle>,
    by_hop: HashMap<(NodeId, NodeId), Vec<CycleId>>,
    structure_digest: u64,
    /// True when [`CycleIndexLimits::max_cycles`] stopped enumeration early, so
    /// callers can report that coverage is partial rather than silently
    /// treating a truncated set as exhaustive.
    pub truncated: bool,
}

/// Fingerprint of the graph's token adjacency — the thing the cycle set depends
/// on. Deliberately ignores rates, liquidity and `active`: those change every
/// block and must NOT invalidate the precomputed set, which is the entire point
/// of separating structure from state.
pub fn structure_digest(graph: &Graph) -> u64 {
    let mut pairs: Vec<(usize, usize)> = graph
        .edges
        .iter()
        .filter_map(|edge| {
            let from = *graph.ix.get(&edge.from)?;
            let to = *graph.ix.get(&edge.to)?;
            Some((from, to))
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup();

    let mut hasher = DefaultHasher::new();
    pairs.len().hash(&mut hasher);
    for pair in &pairs {
        pair.hash(&mut hasher);
    }
    hasher.finish()
}

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

    /// True when `graph`'s adjacency no longer matches what this index was built
    /// from. Cheap enough to call per scan.
    pub fn is_stale(&self, graph: &Graph) -> bool {
        self.structure_digest != structure_digest(graph)
    }

    /// Cycle ids traversing any of `hops`, deduplicated.
    ///
    /// This is the hot-path lookup: given the token pairs whose pools moved this
    /// block, return only the cycles that need re-pricing.
    pub fn cycles_touching(
        &self,
        hops: impl IntoIterator<Item = (NodeId, NodeId)>,
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

    /// Map changed pool addresses to the ordered hops they can serve, for
    /// feeding [`Self::cycles_touching`]. A pool serves both directions.
    pub fn hops_for_pools(
        graph: &Graph,
        pools: &HashSet<Address>,
    ) -> HashSet<(NodeId, NodeId)> {
        let mut hops = HashSet::new();
        for edge in &graph.edges {
            let Some(pool) = crate::venues::edge_pool_address(edge) else {
                continue;
            };
            if !pools.contains(&pool) {
                continue;
            }
            let (Some(from), Some(to)) = (graph.ix.get(&edge.from), graph.ix.get(&edge.to)) else {
                continue;
            };
            hops.insert((*from as NodeId, *to as NodeId));
        }
        hops
    }

    /// Enumerate cycles reachable from `starts`, shortest first.
    ///
    /// `starts` are the tokens a cycle may open and close on — in practice the
    /// flash-loanable set, since a cycle that cannot be funded cannot be traded.
    /// Restricting starts is what keeps this tractable: unrestricted enumeration
    /// over every token is combinatorial.
    pub fn build(graph: &Graph, starts: &[Address], limits: CycleIndexLimits) -> Self {
        let adjacency = structural_adjacency(graph);
        let start_ids: Vec<NodeId> = starts
            .iter()
            .filter_map(|addr| graph.ix.get(addr).map(|ix| *ix as NodeId))
            .collect();

        let max_hops = limits.max_hops.max(limits.min_hops);
        let mut seen: HashSet<Vec<NodeId>> = HashSet::new();
        let mut cycles: Vec<TokenCycle> = Vec::new();
        let mut truncated = false;

        // Iterative deepening: emit every cycle of length d before any of length
        // d+1, so a truncated set is the SHORTEST cycles rather than an
        // arbitrary prefix of a depth-first walk.
        'outer: for depth in limits.min_hops.max(2)..=max_hops {
            for start in &start_ids {
                let mut path = vec![*start];
                let mut on_path: HashSet<NodeId> = HashSet::from([*start]);
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

        let mut by_hop: HashMap<(NodeId, NodeId), Vec<CycleId>> = HashMap::new();
        for (id, cycle) in cycles.iter().enumerate() {
            for hop in cycle.hop_pairs() {
                by_hop.entry(hop).or_default().push(id as CycleId);
            }
        }

        Self {
            cycles,
            by_hop,
            structure_digest: structure_digest(graph),
            truncated,
        }
    }
}

/// Distinct directed token pairs, collapsing every pool between the same pair
/// into ONE structural hop. Parallel pools (WETH/USDC at 100/500/3000/10000)
/// are the same edge structurally; which one to use — or how to split across
/// them — is a per-scan pricing decision.
fn structural_adjacency(graph: &Graph) -> HashMap<NodeId, Vec<NodeId>> {
    let mut adjacency: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for edge in &graph.edges {
        let (Some(from), Some(to)) = (graph.ix.get(&edge.from), graph.ix.get(&edge.to)) else {
            continue;
        };
        if from == to {
            continue;
        }
        adjacency
            .entry(*from as NodeId)
            .or_default()
            .insert(*to as NodeId);
    }
    adjacency
        .into_iter()
        .map(|(k, v)| {
            let mut targets: Vec<NodeId> = v.into_iter().collect();
            targets.sort_unstable();
            (k, targets)
        })
        .collect()
}

/// DFS for simple cycles of exactly `remaining` more hops back to `start`.
/// Returns `false` once `max_cycles` is reached, so the caller can stop and
/// flag truncation rather than silently capping.
#[allow(clippy::too_many_arguments)]
fn enumerate_at_depth(
    adjacency: &HashMap<NodeId, Vec<NodeId>>,
    start: NodeId,
    remaining: usize,
    path: &mut Vec<NodeId>,
    on_path: &mut HashSet<NodeId>,
    seen: &mut HashSet<Vec<NodeId>>,
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
            if seen.insert(cycle.nodes.clone()) {
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

    fn addr(id: u64) -> Address {
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
        let idx = CycleIndex::build(&graph, &[addr(1)], CycleIndexLimits::default());
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
        let idx = CycleIndex::build(&graph, &[addr(1)], CycleIndexLimits::default());
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
            &graph,
            &[addr(1), addr(2), addr(3)],
            CycleIndexLimits::default(),
        );
        assert_eq!(idx.len(), 1, "one triangle, three possible entry points");
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
        let idx = CycleIndex::build(&graph, &[addr(1)], CycleIndexLimits::default());
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
        let idx = CycleIndex::build(&graph, &[addr(1)], CycleIndexLimits::default());
        assert_eq!(idx.len(), 2);

        let n1 = graph.ix[&addr(1)] as NodeId;
        let n2 = graph.ix[&addr(2)] as NodeId;
        let touched = idx.cycles_touching([(n1, n2)]);
        assert_eq!(touched.len(), 1, "only the 1<->2 loop re-prices");

        let cycle = idx.cycle(touched[0]).expect("touched cycle");
        assert!(
            cycle.nodes.contains(&n2),
            "the returned cycle must actually traverse the changed pair"
        );
    }

    #[test]
    fn pool_addresses_map_to_the_hops_they_serve() {
        let graph = two_hop_graph();
        let pools = HashSet::from([addr(100)]);
        let hops = CycleIndex::hops_for_pools(&graph, &pools);
        assert_eq!(hops.len(), 2, "one pool serves both directions of its pair");
    }

    #[test]
    fn structure_digest_ignores_state_but_tracks_topology() {
        let graph = two_hop_graph();
        let idx = CycleIndex::build(&graph, &[addr(1)], CycleIndexLimits::default());
        assert!(!idx.is_stale(&graph));

        // State churn must NOT invalidate: that is the whole point of splitting
        // structure from state.
        let mut restated = two_hop_graph();
        for e in restated.edges.iter_mut() {
            e.rate_num = U256::from(999u64);
            e.active = false;
            e.quote_block = Some(1234u64.into());
        }
        assert!(
            !idx.is_stale(&restated),
            "rates/liquidity/active must not invalidate the cycle set"
        );

        // A genuinely new token pair must invalidate.
        let mut grown = two_hop_graph();
        grown.add_edge(edge(addr(1), addr(9), addr(300)));
        assert!(idx.is_stale(&grown), "new adjacency must invalidate");
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
        let idx = CycleIndex::build(&graph, &[addr(1)], limits);
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
        let idx = CycleIndex::build(&graph, &[addr(42)], CycleIndexLimits::default());
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
        assert!(CycleIndex::build(&graph, &[addr(1)], tight).is_empty());

        let loose = CycleIndexLimits {
            max_hops: 4,
            ..CycleIndexLimits::default()
        };
        assert_eq!(CycleIndex::build(&graph, &[addr(1)], loose).len(), 1);
    }
}
