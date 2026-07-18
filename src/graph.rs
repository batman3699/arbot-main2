use crate::{metrics::Metrics, util::u256_to_f64};
use dashmap::DashMap;
use ethers::types::{Address, U256, U512, U64};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

#[derive(Clone, Debug)]
pub enum VenueEdge {
    UniV3 {
        path: Vec<(Address, Option<u32>)>,
        pool: Address,
        fee: u32,
    },
    /// Aerodrome Slipstream CL pools (tick spacing stored in `fee` field of path hops).
    Slipstream {
        path: Vec<(Address, Option<u32>)>,
        pool: Address,
        tick_spacing: u32,
        router: Address,
    },
    Balancer {
        pool_id: [u8; 32],
        token_in: Address,
        token_out: Address,
    },
    Curve {
        pool: Address,
        selector: [u8; 4],
        i: i128,
        j: i128,
    },
    UniV2 {
        pair: Address,
        #[allow(dead_code)]
        token_out: Address,
        token0: Address,
        token1: Address,
        reserve_in: U256,
        reserve_out: U256,
        fee_bps: u32,
    },
    SolidlyV2 {
        pair: Address,
        #[allow(dead_code)]
        token_out: Address,
        token0: Address,
        token1: Address,
        stable: bool,
        reserve_in: U256,
        reserve_out: U256,
        fee_bps: u32,
        decimals0: u8,
        decimals1: u8,
    },
    Univ4 {
        pool_manager: Address,
        token0: Address,
        token1: Address,
        fee: u32,
        tick_spacing: i32,
        hooks: Address,
        sqrt_price_x96: U256,
    },
    Bridge {
        router: Address,
        token_in: Address,
        token_out: Address,
        dst_chain_id: u64,
        selector: [u8; 4],
        bridge_name: String,
        max_bridge_time_secs: u64,
        estimated_time_secs: u64,
        fee_bps: u32,
        liquidity_limit: U256,
    },
    Liquidation {
        adapter: Address,
        selector: [u8; 4],
        flash_loan_pool: Address,
        debt_token: Address,
        collateral_token: Address,
        user: Address,
        receive_atoken: bool,
        protocol: String,
    },
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: Address,
    pub to: Address,
    pub rate_num: U256,
    pub rate_den: U256,
    pub venue: VenueEdge,
    pub estimated_gas: u64,
    /// Fixed-point scaled weight (negative log exchange rate plus gas ratio) scaled by `WEIGHT_SCALE`.
    pub weight: i64,
    pub max_input: U256,
    pub tolerance_bps: u32,
    pub observed_slippage_bps: u32,
    pub quote_block: Option<U64>,
    pub active: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct BellmanFordLimits {
    pub min_hops: usize,
    pub max_hops: usize,
    pub max_relaxations: usize,
    pub max_cycles: usize,
    pub timeout: Duration,
}

impl BellmanFordLimits {
    pub fn sanitized(self) -> Self {
        let min_hops = self.min_hops.max(1);
        let max_hops = self.max_hops.max(1);
        let max_relaxations = self.max_relaxations.max(1).min(256);
        let max_cycles = self.max_cycles.max(1);
        let timeout = if self.timeout.is_zero() {
            Duration::from_millis(1)
        } else {
            self.timeout
        };

        Self {
            min_hops: min_hops.min(max_hops),
            max_hops,
            max_relaxations,
            max_cycles,
            timeout,
        }
    }
}
    
type NodeIx = usize;
type EdgeWeight = i64;
type AdjacentEdge = (NodeIx, EdgeWeight, usize);
type AdjacencyList = Arc<Vec<AdjacentEdge>>;
type AdjacencyMap = DashMap<NodeIx, AdjacencyList>;

pub struct Graph {
    pub nodes: Vec<Address>,
    pub ix: HashMap<Address, usize>,
    pub edges: Vec<Edge>,
    edges_from: HashMap<Address, Vec<usize>>,
    edge_lookup: HashMap<(Address, Address), Vec<usize>>,
    adjacency: Arc<AdjacencyMap>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            ix: HashMap::new(),
            edges: Vec::new(),
            edges_from: HashMap::new(),
            edge_lookup: HashMap::new(),
            adjacency: Arc::new(DashMap::new()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum EdgeSignature {
    UniV3 {
        pool: Address,
        fee: u32,
    },
    Slipstream {
        pool: Address,
        tick_spacing: u32,
    },
    Balancer {
        pool_id: [u8; 32],
    },
    Curve {
        pool: Address,
        selector: [u8; 4],
        i: i128,
        j: i128,
    },
    UniV2 {
        pair: Address,
    },
    SolidlyV2 {
        pair: Address,
    },
    Univ4 {
        pool_manager: Address,
        token0: Address,
        token1: Address,
        fee: u32,
        tick_spacing: i32,
        hooks: Address,
    },
    Bridge {
        router: Address,
        selector: [u8; 4],
        dst_chain_id: u64,
    },
    Liquidation {
        adapter: Address,
        selector: [u8; 4],
        user: Address,
    },
}

#[derive(Clone, Debug)]
struct DetectedCycle {
    weight: i64,
    cycle: Vec<usize>,
    edge_indices: Vec<usize>,
    estimated_profit_bps: i64,
}

pub(crate) fn canonicalize_cycle(mut cycle: Vec<usize>) -> Vec<usize> {
    if cycle.len() <= 1 {
        return cycle;
    }

    let closed = cycle.first() == cycle.last();
    if closed {
        cycle.pop();
    }

    if cycle.is_empty() {
        return if closed { Vec::new() } else { cycle };
    }

    let len = cycle.len();
    let mut best_start = 0usize;

    for i in 1..len {
        for offset in 0..len {
            let a = cycle[(best_start + offset) % len];
            let b = cycle[(i + offset) % len];
            if a == b {
                continue;
            }
            if b < a {
                best_start = i;
            }
            break;
        }
    }

    let mut normalized = Vec::with_capacity(len + if closed { 1 } else { 0 });
    for offset in 0..len {
        normalized.push(cycle[(best_start + offset) % len]);
    }
    if closed {
        let first = normalized[0];
        normalized.push(first);
    }

    normalized
}

impl Graph {
    fn edge_signature(edge: &Edge) -> EdgeSignature {
        match &edge.venue {
            VenueEdge::UniV3 { pool, fee, .. } => EdgeSignature::UniV3 {
                pool: *pool,
                fee: *fee,
            },
            VenueEdge::Slipstream {
                pool,
                tick_spacing,
                ..
            } => EdgeSignature::Slipstream {
                pool: *pool,
                tick_spacing: *tick_spacing,
            },
            VenueEdge::Balancer { pool_id, .. } => EdgeSignature::Balancer { pool_id: *pool_id },
            VenueEdge::Curve {
                pool,
                selector,
                i,
                j,
            } => EdgeSignature::Curve {
                pool: *pool,
                selector: *selector,
                i: *i,
                j: *j,
            },
            VenueEdge::UniV2 { pair, .. } => EdgeSignature::UniV2 { pair: *pair },
            VenueEdge::SolidlyV2 { pair, .. } => EdgeSignature::SolidlyV2 { pair: *pair },
            VenueEdge::Univ4 {
                pool_manager,
                token0,
                token1,
                fee,
                tick_spacing,
                hooks,
                ..
            } => EdgeSignature::Univ4 {
                pool_manager: *pool_manager,
                token0: *token0,
                token1: *token1,
                fee: *fee,
                tick_spacing: *tick_spacing,
                hooks: *hooks,
            },
            VenueEdge::Bridge {
                router,
                selector,
                dst_chain_id,
                ..
            } => EdgeSignature::Bridge {
                router: *router,
                selector: *selector,
                dst_chain_id: *dst_chain_id,
            },
            VenueEdge::Liquidation {
                adapter,
                selector,
                user,
                ..
            } => EdgeSignature::Liquidation {
                adapter: *adapter,
                selector: *selector,
                user: *user,
            },
        }
    }

    fn update_adjacency_for_from(&self, from: Address) {
        let Some(&from_idx) = self.ix.get(&from) else {
            return;
        };

        let mut slot = Vec::new();
        if let Some(edge_indices) = self.edges_from.get(&from) {
            slot.reserve(edge_indices.len());
            for &edge_idx in edge_indices {
                let Some(edge) = self.edges.get(edge_idx) else {
                    continue;
                };
                if !edge.active || matches!(&edge.venue, VenueEdge::Bridge { .. }) {
                    continue;
                }
                if let Some(&to_idx) = self.ix.get(&edge.to) {
                    slot.push((to_idx, edge.weight, edge_idx));
                }
            }
        }

        self.adjacency.insert(from_idx, Arc::new(slot));
    }

    fn ensure_adjacency_slot(&self, node_idx: usize) {
        self.adjacency
            .entry(node_idx)
            .or_insert_with(|| Arc::new(Vec::new()));
    }

    pub fn refresh_incremental_adjacency(&self) {
        self.refresh_incremental_adjacency_with_metrics(None, None);
    }

    pub fn refresh_incremental_adjacency_with_metrics(
        &self,
        metrics: Option<&Metrics>,
        chain: Option<&str>,
    ) {
        let started = Instant::now();
        let mut updated_edges = 0usize;

        for from in self.edges_from.keys().copied() {
            if let Some(edge_indices) = self.edges_from.get(&from) {
                updated_edges = updated_edges.saturating_add(edge_indices.len());
            }
            self.update_adjacency_for_from(from);
        }
        for node_idx in 0..self.nodes.len() {
            self.ensure_adjacency_slot(node_idx);
        }

        if let Some(metrics) = metrics {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            metrics.record_graph_update_ms(elapsed_ms);
            if let Some(chain) = chain {
                metrics.record_edges_updated(chain, updated_edges);
            }
        }
    }

    pub fn add_node(&mut self, token: Address) -> usize {
        if let Some(&idx) = self.ix.get(&token) {
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(token);
            self.ix.insert(token, idx);
            self.ensure_adjacency_slot(idx);
            idx
        }
    }

    pub fn add_edge(&mut self, edge: Edge) {
        let from = edge.from;
        let to = edge.to;
        self.add_node(from);
        self.add_node(to);

        let signature = Self::edge_signature(&edge);
        let entry = self.edge_lookup.entry((from, to)).or_default();

        if let Some(&existing_idx) = entry.iter().find(|&&idx| {
            self.edges
                .get(idx)
                .map(|existing| Self::edge_signature(existing) == signature)
                .unwrap_or(false)
        }) {
            let should_replace = {
                let existing = &self.edges[existing_idx];
                match (&edge.venue, &existing.venue) {
                    (VenueEdge::Bridge { .. }, VenueEdge::Bridge { .. }) => {
                        is_better(&edge, existing)
                    }
                    (VenueEdge::Bridge { .. }, _) => false,
                    (_, VenueEdge::Bridge { .. }) => true,
                    _ => is_better(&edge, existing),
                }
            };

            if should_replace {
                self.edges[existing_idx] = edge;
                self.update_adjacency_for_from(from);
            }
            return;
        }

        let idx = self.edges.len();
        self.edges.push(edge);
        self.edges_from.entry(from).or_default().push(idx);
        entry.push(idx);
        self.update_adjacency_for_from(from);
    }

    pub fn edge_by_index(&self, idx: usize) -> Option<&Edge> {
        self.edges.get(idx)
    }

    /// Index of the best active edge from `from` to `to` (by `is_better`:
    /// lowest weight, then higher max_input, then better rate). Single source of
    /// truth for per-hop edge selection — `edge_between` and the cycle
    /// weight/profit helpers all resolve through this so their tie-breaking
    /// cannot diverge.
    fn best_edge_index(&self, from: Address, to: Address) -> Option<usize> {
        let indices = self.edge_lookup.get(&(from, to))?;
        let mut best: Option<usize> = None;
        for &idx in indices {
            let Some(edge) = self.edges.get(idx) else {
                continue;
            };
            if !edge.active {
                continue;
            }
            match best {
                None => best = Some(idx),
                Some(current_idx) => {
                    if is_better(edge, &self.edges[current_idx]) {
                        best = Some(idx);
                    }
                }
            }
        }
        best
    }

    /// Resolve a node-index cycle to the best active edge index per hop. Returns
    /// `None` if the cycle has fewer than 2 nodes or any hop has no active edge.
    fn best_edge_indices_for_node_path(&self, cycle: &[usize]) -> Option<Vec<usize>> {
        if cycle.len() < 2 {
            return None;
        }
        let mut edge_indices = Vec::with_capacity(cycle.len().saturating_sub(1));
        for window in cycle.windows(2) {
            let &from_addr = self.nodes.get(window[0])?;
            let &to_addr = self.nodes.get(window[1])?;
            edge_indices.push(self.best_edge_index(from_addr, to_addr)?);
        }
        Some(edge_indices)
    }

    pub fn edge_between(&self, from: Address, to: Address) -> Option<&Edge> {
        self.best_edge_index(from, to).map(|idx| &self.edges[idx])
    }

    pub fn bellman_ford(
        &self,
        start_priorities: &HashMap<Address, i128>,
        limits: &BellmanFordLimits,
        k: usize,
        metrics: Option<&Metrics>,
    ) -> Vec<CycleCandidate> {
        let limits = limits.sanitized();
        if self.nodes.is_empty() || limits.max_hops == 0 || k == 0 {
            return Vec::new();
        }

        self.refresh_incremental_adjacency();
        let adjacency = self.build_adjacency();
        let mut ordered_starts: Vec<(i128, usize)> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(idx, address)| {
                let priority = start_priorities.get(address).copied().unwrap_or_default();
                (priority, idx)
            })
            .collect();

        ordered_starts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let has_positive_priority = ordered_starts.iter().any(|(priority, _)| *priority > 0);
        let filtered_starts: Vec<(i128, usize)> = ordered_starts
            .into_iter()
            .filter(|(priority, _)| !has_positive_priority || *priority > 0)
            .collect();

        let abort = AtomicBool::new(false);
        let timed_out = AtomicBool::new(false);
        let deadline = Instant::now().checked_add(limits.timeout);

        let max_priority = filtered_starts
            .iter()
            .map(|(priority, _)| *priority)
            .max()
            .unwrap_or_default();

        let discovered: Vec<(i64, Vec<usize>, Vec<usize>, i128, i64)> = filtered_starts
            .par_iter()
            .flat_map(|&(priority, start_idx)| {
                if abort.load(AtomicOrdering::Relaxed) || timed_out.load(AtomicOrdering::Relaxed) {
                    return Vec::new();
                }
                let allow_abort = priority == max_priority;
                let search_control = SearchControl {
                    abort: &abort,
                    timed_out: &timed_out,
                    deadline,
                    allow_abort,
                };
                self.bellman_ford_from(start_idx, &limits, &adjacency, search_control)
                    .into_iter()
                    .map(|(weight, cycle, edge_indices)| {
                        let estimated_profit_bps = self
                            .estimate_cycle_profit_bps_from_edges(&edge_indices)
                            .or_else(|| self.estimate_cycle_profit_bps(&cycle))
                            .unwrap_or(0);
                        (weight, cycle, edge_indices, priority, estimated_profit_bps)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        if timed_out.load(AtomicOrdering::Relaxed) {
            // A timeout means some start nodes were not fully explored, but every
            // cycle already in `discovered` is a fully-validated negative cycle
            // found before the deadline. Discarding them throws away real,
            // already-paid-for opportunities and guarantees a missed block.
            // Rank and return what we have; only yield nothing if nothing was
            // found. Start nodes are explored in descending priority order, so
            // the partial set is biased toward the highest-value cycles.
            if let Some(metrics) = metrics {
                metrics.record_cycle_search_timeout();
            }
            warn!(
                discovered = discovered.len(),
                "Cycle search timed out before completion; ranking partial results"
            );
        }

        let mut by_color: HashMap<u64, Vec<ScoredCycle>> = HashMap::new();
        let mut seen: HashSet<Vec<usize>> = HashSet::new();

        for (weight, mut cycle, edge_indices, priority, estimated_profit_bps) in discovered {
            if cycle.len() < 2 {
                continue;
            }
            if cycle.first() != cycle.last() {
                if let Some(&first) = cycle.first() {
                    cycle.push(first);
                }
            }

            let canonical = canonicalize_cycle(cycle.clone());
            if !seen.insert(canonical) {
                continue;
            }

            if !self.is_line_graph_compliant(&cycle) {
                continue;
            }

            let Some(&start) = cycle.first().and_then(|ix| self.nodes.get(*ix)) else {
                continue;
            };

            let color = self.cycle_color(&cycle);

            by_color.entry(color).or_default().push(ScoredCycle {
                weight,
                cycle,
                edge_indices,
                start,
                priority,
                estimated_profit_bps,
                color,
            });
        }

        for bucket in by_color.values_mut() {
            bucket.sort_by(|a, b| {
                a.weight
                    .cmp(&b.weight)
                    .then_with(|| a.cycle.len().cmp(&b.cycle.len()))
                    .then_with(|| b.priority.cmp(&a.priority))
                    .then_with(|| a.start.cmp(&b.start))
            });
        }

        let mut colors: Vec<u64> = by_color.keys().copied().collect();
        colors.sort_by(|a, b| {
            let a_best = by_color.get(a).and_then(|bucket| bucket.first());
            let b_best = by_color.get(b).and_then(|bucket| bucket.first());

            match (a_best, b_best) {
                (Some(a_best), Some(b_best)) => a_best.cmp(b_best).then_with(|| a.cmp(b)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => a.cmp(b),
            }
        });
        if colors.len() > k {
            colors.truncate(k);
        }
        let mut selected: Vec<ScoredCycle> = Vec::new();

        while selected.len() < k {
            let mut progressed = false;
            for color in &colors {
                let Some(bucket) = by_color.get_mut(color) else {
                    continue;
                };
                if let Some(entry) = bucket.first().cloned() {
                    bucket.remove(0);
                    selected.push(entry);
                    progressed = true;
                    if selected.len() >= k {
                        break;
                    }
                }
            }
            if !progressed {
                break;
            }
        }

        let mut heap: BinaryHeap<ScoredCycle> = BinaryHeap::new();
        for entry in selected {
            heap.push(entry);
        }

        let cycles: Vec<CycleCandidate> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|entry| CycleCandidate {
                cycle: entry.cycle,
                edge_indices: entry.edge_indices,
                weight: entry.weight,
                start: entry.start,
                estimated_profit_bps: entry.estimated_profit_bps,
            })
            .collect();

        cycles
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CycleCandidate {
    pub cycle: Vec<usize>,
    pub edge_indices: Vec<usize>,
    pub weight: i64,
    pub start: Address,
    pub estimated_profit_bps: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedCycle {
    pub cycle: Vec<usize>,
    pub edge_indices: Vec<usize>,
}

impl IndexedCycle {
    #[allow(dead_code)]
    pub fn from_nodes(cycle: Vec<usize>) -> Self {
        Self {
            cycle,
            edge_indices: Vec::new(),
        }
    }

    pub fn hops(&self) -> usize {
        self.cycle.len().saturating_sub(1)
    }

    pub fn edge_indices_valid(&self) -> bool {
        self.edge_indices.len() == self.hops()
    }
}

/// Rotate a closed cycle so it starts at `new_start_ix`, preserving hop edges.
pub fn rotate_indexed_cycle(
    cycle: &[usize],
    edge_indices: &[usize],
    new_start_ix: usize,
) -> Option<IndexedCycle> {
    if cycle.len() < 2 || edge_indices.len() != cycle.len().saturating_sub(1) {
        return None;
    }
    let closed = cycle.first() == cycle.last();
    let body: Vec<usize> = if closed {
        cycle[..cycle.len().saturating_sub(1)].to_vec()
    } else {
        cycle.to_vec()
    };
    let start_pos = body.iter().position(|&ix| ix == new_start_ix)?;
    let len = body.len();
    let mut rotated_nodes = Vec::with_capacity(len + if closed { 1 } else { 0 });
    for offset in 0..len {
        rotated_nodes.push(body[(start_pos + offset) % len]);
    }
    if closed {
        rotated_nodes.push(rotated_nodes[0]);
    }
    let mut rotated_edges = Vec::with_capacity(edge_indices.len());
    for hop in 0..edge_indices.len() {
        rotated_edges.push(edge_indices[(start_pos + hop) % len]);
    }
    Some(IndexedCycle {
        cycle: rotated_nodes,
        edge_indices: rotated_edges,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScoredCycle {
    weight: i64,
    cycle: Vec<usize>,
    edge_indices: Vec<usize>,
    start: Address,
    priority: i128,
    estimated_profit_bps: i64,
    color: u64,
}

#[derive(Copy, Clone)]
struct SearchControl<'a> {
    abort: &'a AtomicBool,
    timed_out: &'a AtomicBool,
    deadline: Option<Instant>,
    allow_abort: bool,
}

impl Ord for ScoredCycle {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.weight.cmp(&other.weight) {
            Ordering::Equal => match self.cycle.len().cmp(&other.cycle.len()) {
                Ordering::Equal => match self.priority.cmp(&other.priority).reverse() {
                    Ordering::Equal => match self.color.cmp(&other.color) {
                        Ordering::Equal => self.start.cmp(&other.start),
                        ord => ord,
                    },
                    ord => ord,
                },
                ord => ord,
            },
            ord => ord,
        }
    }
}

impl PartialOrd for ScoredCycle {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Graph {
    fn cycle_color(&self, cycle: &[usize]) -> u64 {
        let mut mask = 0u64;
        for window in cycle.windows(2) {
            let Some(&from_addr) = self.nodes.get(window[0]) else {
                continue;
            };
            let Some(&to_addr) = self.nodes.get(window[1]) else {
                continue;
            };
            let Some(edge) = self.edge_between(from_addr, to_addr) else {
                continue;
            };
            let bit = match &edge.venue {
                VenueEdge::UniV3 { .. } => 1u64 << 0,
                VenueEdge::Slipstream { .. } => 1u64 << 8,
                VenueEdge::Balancer { .. } => 1u64 << 1,
                VenueEdge::Curve { .. } => 1u64 << 2,
                VenueEdge::UniV2 { .. } => 1u64 << 3,
                VenueEdge::SolidlyV2 { .. } => 1u64 << 4,
                VenueEdge::Univ4 { .. } => 1u64 << 5,
                VenueEdge::Bridge { .. } => 1u64 << 6,
                VenueEdge::Liquidation { .. } => 1u64 << 7,
            };
            mask |= bit;
        }

        if mask == 0 {
            1u64 << 63
        } else {
            mask
        }
    }

    fn is_line_graph_compliant(&self, cycle: &[usize]) -> bool {
        if cycle.len() < 3 {
            return false;
        }
        let mut transitions: HashSet<(Address, Address)> = HashSet::new();
        for window in cycle.windows(2) {
            let Some(&from_addr) = self.nodes.get(window[0]) else {
                return false;
            };
            let Some(&to_addr) = self.nodes.get(window[1]) else {
                return false;
            };

            if from_addr == to_addr || !transitions.insert((from_addr, to_addr)) {
                return false;
            }
        }
        true
    }

    fn build_adjacency(&self) -> Vec<Vec<(usize, i64, usize)>> {
        let n = self.nodes.len();
        let mut adjacency: Vec<Vec<(usize, i64, usize)>> = vec![Vec::new(); n];

        for (from_idx, slot) in adjacency.iter_mut().enumerate() {
            if let Some(entry) = self.adjacency.get(&from_idx) {
                *slot = entry.value().as_ref().clone();
            }
        }

        adjacency
    }

    pub(crate) fn cycle_weight_from_edge_indices(&self, edge_indices: &[usize]) -> Option<i64> {
        let mut total: i64 = 0;
        for &idx in edge_indices {
            let edge = self.edges.get(idx)?;
            if !edge.active {
                return None;
            }
            total = total.saturating_add(edge.weight);
        }
        Some(total)
    }

    fn bellman_ford_from(
        &self,
        source_idx: usize,
        limits: &BellmanFordLimits,
        adjacency: &[Vec<(usize, i64, usize)>],
        search_control: SearchControl<'_>,
    ) -> Vec<(i64, Vec<usize>, Vec<usize>)> {
        let limits = limits.sanitized();
        let n = self.nodes.len();
        if n == 0 || limits.max_hops == 0 {
            return Vec::new();
        }

        let SearchControl {
            abort,
            timed_out,
            deadline,
            allow_abort,
        } = search_control;

        let deadline_exceeded =
            |deadline: Option<Instant>| -> bool { deadline.is_some_and(|d| Instant::now() >= d) };

        let expired = deadline_exceeded(deadline);
        if abort.load(AtomicOrdering::Relaxed) {
            return Vec::new();
        }
        if timed_out.load(AtomicOrdering::Relaxed) || expired {
            if expired {
                timed_out.store(true, AtomicOrdering::Relaxed);
            }
            return Vec::new();
        }

        let mut dist = vec![i128::MAX / 4; n];
        let mut pred: Vec<Option<usize>> = vec![None; n];
        let mut pred_edge: Vec<Option<usize>> = vec![None; n];
        dist[source_idx] = 0;

        // Preserve `max_relaxations` as an iteration budget (queue frontiers / passes),
        // and consume budget per frontier processed (not per scanned adjacency edge).
        let max_relax_iterations = limits.max_relaxations.max(1);
        let mut cycles: Vec<DetectedCycle> = Vec::new();
        let mut seen: HashSet<Vec<usize>> = HashSet::new();

        let mut record_cycle =
            |cycle: Vec<usize>, edge_path: Vec<usize>, store: &mut Vec<DetectedCycle>| {
            if store.len() >= limits.max_cycles {
                return;
            }
            let mut cycle = cycle;
            if cycle.len() < 2 {
                return;
            }

            if cycle.first() != cycle.last() {
                if let Some(&first) = cycle.first() {
                    cycle.push(first);
                }
            }

            let hops = cycle.len().saturating_sub(1);
            if hops < limits.min_hops || hops > limits.max_hops {
                return;
            }

            if !self.is_line_graph_compliant(&cycle) {
                return;
            }

            let canonical = canonicalize_cycle(cycle.clone());
            if !seen.insert(canonical) {
                return;
            }

            let weight = if edge_path.len() == hops {
                self.cycle_weight_from_edge_indices(&edge_path)
            } else {
                self.cycle_weight(&cycle)
            };
            if let Some(weight) = weight {
                if weight >= 0 {
                    return;
                }
                let estimated_profit_bps = self
                    .estimate_cycle_profit_bps_from_edges(&edge_path)
                    .or_else(|| self.estimate_cycle_profit_bps(&cycle))
                    .unwrap_or(0);
                store.push(DetectedCycle {
                    weight,
                    cycle,
                    edge_indices: edge_path,
                    estimated_profit_bps,
                });
            }
        };

        let mut in_queue = vec![false; n];
        let mut relax_count = vec![0usize; n];
        let mut queue = VecDeque::new();
        queue.push_back(source_idx);
        in_queue[source_idx] = true;

        let mut relax_iterations = 0usize;
        while !queue.is_empty() && relax_iterations < max_relax_iterations {
            relax_iterations = relax_iterations.saturating_add(1);
            let frontier_len = queue.len();

            for _ in 0..frontier_len {
                let Some(u_id) = queue.pop_front() else {
                    break;
                };

                if deadline_exceeded(deadline) {
                    timed_out.store(true, AtomicOrdering::Relaxed);
                    abort.store(true, AtomicOrdering::Relaxed);
                    return Vec::new();
                }

                in_queue[u_id] = false;
                if dist[u_id] == i128::MAX / 4 {
                    continue;
                }

                for &(v_id, weight, edge_idx) in &adjacency[u_id] {
                    let candidate = dist[u_id].saturating_add(weight as i128);
                    if candidate >= dist[v_id] {
                        continue;
                    }

                    dist[v_id] = candidate;
                    pred[v_id] = Some(u_id);
                    pred_edge[v_id] = Some(edge_idx);
                    relax_count[v_id] = relax_count[v_id].saturating_add(1);

                    if let Some((cycle, edges)) =
                        self.extract_cycle_with_edges(v_id, &pred, &pred_edge, source_idx)
                    {
                        record_cycle(cycle, edges, &mut cycles);
                        if cycles.len() >= limits.max_cycles {
                            break;
                        }
                    }

                    if relax_count[v_id] > n {
                        if let Some((cycle, edges)) =
                            self.extract_cycle_with_edges(v_id, &pred, &pred_edge, source_idx)
                        {
                            record_cycle(cycle, edges, &mut cycles);
                        }
                    }

                    if !in_queue[v_id] {
                        queue.push_back(v_id);
                        in_queue[v_id] = true;
                    }
                }

                if cycles.len() >= limits.max_cycles {
                    break;
                }
            }

            if cycles.len() >= limits.max_cycles {
                break;
            }
        }

        if allow_abort && !cycles.is_empty() {
            abort.store(true, AtomicOrdering::Relaxed);
        }

        cycles.sort_by(|a, b| {
            a.weight
                .cmp(&b.weight)
                .then_with(|| b.estimated_profit_bps.cmp(&a.estimated_profit_bps))
        });

        cycles
            .into_iter()
            .take(limits.max_cycles)
            .map(|cycle| (cycle.weight, cycle.cycle, cycle.edge_indices))
            .collect()
    }

    fn resolve_edge_path_for_cycle(
        &self,
        cycle: &[usize],
        pred_edge: &[Option<usize>],
    ) -> Vec<usize> {
        if cycle.len() < 2 {
            return Vec::new();
        }
        let hops = cycle.len().saturating_sub(1);
        let mut indices = Vec::with_capacity(hops);
        for window in cycle.windows(2) {
            let from_ix = window[0];
            let to_ix = window[1];
            let mut matched = false;
            if let Some(idx) = pred_edge.get(to_ix).copied().flatten() {
                if self.edges.get(idx).is_some_and(|edge| {
                    edge.active
                        && self.ix.get(&edge.from) == Some(&from_ix)
                        && self.ix.get(&edge.to) == Some(&to_ix)
                }) {
                    indices.push(idx);
                    matched = true;
                }
            }
            if !matched {
                let from_addr = self.nodes[from_ix];
                let to_addr = self.nodes[to_ix];
                let lookup = self.edge_lookup.get(&(from_addr, to_addr));
                if let Some(candidates) = lookup {
                    let mut best: Option<(i64, usize)> = None;
                    for &idx in candidates {
                        let Some(edge) = self.edges.get(idx) else {
                            continue;
                        };
                        if !edge.active {
                            continue;
                        }
                        match best {
                            None => best = Some((edge.weight, idx)),
                            Some((best_weight, _)) if edge.weight < best_weight => {
                                best = Some((edge.weight, idx));
                            }
                            _ => {}
                        }
                    }
                    if let Some((_, idx)) = best {
                        indices.push(idx);
                    }
                }
            }
        }
        indices
    }

    fn extract_cycle_with_edges(
        &self,
        mut v_id: usize,
        pred: &[Option<usize>],
        pred_edge: &[Option<usize>],
        source_idx: usize,
    ) -> Option<(Vec<usize>, Vec<usize>)> {
        let n = self.nodes.len();
        for _ in 0..n {
            v_id = pred[v_id]?;
        }

        let entry = v_id;
        let mut cycle = vec![v_id];
        let mut edges = Vec::new();
        let mut current = pred[v_id]?;
        let mut guard = 0usize;
        while guard <= n {
            if let Some(edge_idx) = pred_edge.get(v_id).copied().flatten() {
                edges.push(edge_idx);
            }
            cycle.push(current);
            if current == entry {
                break;
            }
            v_id = current;
            current = pred[current]?;
            guard += 1;
        }

        if *cycle.last()? != entry {
            return None;
        }

        cycle.reverse();
        edges.reverse();
        if cycle.len() < 2 {
            return None;
        }
        if cycle.first() != cycle.last() {
            cycle.push(cycle[0]);
        }

        let hops = cycle.len().saturating_sub(1);
        if let Some(pos) = cycle[..hops].iter().position(|&ix| ix == source_idx) {
            let mut rotated_nodes = Vec::with_capacity(hops + 1);
            for offset in 0..hops {
                rotated_nodes.push(cycle[(pos + offset) % hops]);
            }
            rotated_nodes.push(rotated_nodes[0]);

            let rotated_edges = if edges.len() == hops {
                (0..hops)
                    .map(|offset| edges[(pos + offset) % hops])
                    .collect()
            } else {
                self.resolve_edge_path_for_cycle(&rotated_nodes, pred_edge)
            };

            Some((rotated_nodes, rotated_edges))
        } else {
            let resolved_edges = if edges.len() == hops {
                edges
            } else {
                self.resolve_edge_path_for_cycle(&cycle, pred_edge)
            };
            Some((cycle, resolved_edges))
        }
    }

    #[allow(dead_code)] // consumed by the `arb-exec` binary target
    pub(crate) fn cycle_weight(&self, cycle: &[usize]) -> Option<i64> {
        let edge_indices = self.best_edge_indices_for_node_path(cycle)?;
        self.cycle_weight_from_edge_indices(&edge_indices)
    }

    fn estimate_cycle_profit_bps_from_edges(&self, edge_indices: &[usize]) -> Option<i64> {
        if edge_indices.is_empty() {
            return None;
        }
        let mut log_rate_sum = 0.0f64;
        for &idx in edge_indices {
            let edge = self.edges.get(idx)?;
            if !edge.active {
                return None;
            }
            let protected_num = crate::util::apply_slippage(edge.rate_num, edge.tolerance_bps);
            if protected_num.is_zero() || edge.rate_den.is_zero() {
                return None;
            }
            let rate = u256_to_f64(protected_num) / u256_to_f64(edge.rate_den);
            if rate <= 0.0 {
                return None;
            }
            log_rate_sum += rate.ln();
        }
        let profit_ratio = log_rate_sum.exp() - 1.0;
        if !profit_ratio.is_finite() {
            return None;
        }
        let scaled = (profit_ratio * 10_000.0).round();
        Some(scaled.clamp(i64::MIN as f64, i64::MAX as f64) as i64)
    }

    /// Node-path fallback for [`Self::estimate_cycle_profit_bps_from_edges`],
    /// used when the search did not carry explicit edge indices: resolve each
    /// hop to its best edge, then run the identical log-rate profit math.
    fn estimate_cycle_profit_bps(&self, cycle: &[usize]) -> Option<i64> {
        let edge_indices = self.best_edge_indices_for_node_path(cycle)?;
        self.estimate_cycle_profit_bps_from_edges(&edge_indices)
    }
}

fn is_better(candidate: &Edge, current: &Edge) -> bool {
    if candidate.active != current.active {
        return candidate.active;
    }
    if candidate.weight != current.weight {
        return candidate.weight < current.weight;
    }
    if candidate.max_input > current.max_input {
        return true;
    }
    if candidate.max_input < current.max_input {
        return false;
    }
    let left = U512::from(candidate.rate_num) * U512::from(current.rate_den);
    let right = U512::from(current.rate_num) * U512::from(candidate.rate_den);
    left > right
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{compute_edge_weight, NativePrice};
    use std::collections::HashSet;
    use std::time::Duration;

    fn addr(id: u64) -> Address {
        Address::from_low_u64_be(id)
    }

    fn fp_weight(num: u64, den: u64) -> i64 {
        compute_edge_weight(
            U256::from(num),
            U256::from(den),
            0,
            U256::zero(),
            U256::from(1u64),
            NativePrice::new(U256::exp10(18), U256::exp10(18), true),
        )
    }

    fn limits(max_hops: usize) -> BellmanFordLimits {
        BellmanFordLimits {
            min_hops: 2,
            max_hops,
            max_relaxations: max_hops,
            max_cycles: 16,
            timeout: Duration::from_millis(250),
        }
    }

    #[test]
    fn sanitized_limits_do_not_clamp_relaxations_to_hops() {
        let limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 3,
            max_relaxations: 8,
            max_cycles: 4,
            timeout: Duration::from_millis(250),
        }
        .sanitized();

        assert_eq!(limits.max_hops, 3);
        assert_eq!(limits.max_relaxations, 8);
    }

    #[test]
    fn bellman_ford_returns_closed_cycle() {
        let mut graph = Graph::default();

        let a = addr(1);
        let b = addr(2);
        let c = addr(3);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [0u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });
        graph.add_edge(Edge {
            from: b,
            to: c,
            rate_num: U256::from(3u64),
            rate_den: U256::from(2u64),
            venue: VenueEdge::Balancer {
                pool_id: [1u8; 32],
                token_in: b,
                token_out: c,
            },
            estimated_gas: 0,
            weight: fp_weight(3, 2),
            max_input: U256::from(2u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });
        graph.add_edge(Edge {
            from: c,
            to: a,
            rate_num: U256::from(4u64),
            rate_den: U256::from(3u64),
            venue: VenueEdge::Balancer {
                pool_id: [2u8; 32],
                token_in: c,
                token_out: a,
            },
            estimated_gas: 0,
            weight: fp_weight(4, 3),
            max_input: U256::from(3u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });

        let mut priorities = HashMap::new();
        priorities.insert(a, 10);
        priorities.insert(b, 1);
        priorities.insert(c, 1);

        let cycles = graph.bellman_ford(&priorities, &limits(5), 4, None);
        let cycle = cycles
            .first()
            .map(|c| c.cycle.clone())
            .expect("expected an arbitrage cycle");

        assert!(cycle.len() >= 2, "cycle must contain at least two vertices");
        let first = graph.nodes[cycle[0]];
        let last = graph.nodes[*cycle.last().unwrap()];
        assert_eq!(first, last, "cycle should return to its starting vertex");

        let unique_hops = cycle.windows(2).count();
        assert_eq!(unique_hops, cycle.len() - 1);
    }

    #[test]
    fn bellman_ford_detects_two_pool_arb() {
        // The most common real arb: one token pair priced differently across
        // two venues (e.g. WETH/USDC on Uniswap vs Aerodrome). This is a 2-hop
        // cycle a->b->a using two DISTINCT pools. It must be discoverable with
        // min_hops=2; the token graph resolves the best edge per direction, so
        // the forward leg uses the venue that overpays for `a` and the return
        // leg uses the venue that sells `a` cheaply.
        let mut graph = Graph::default();

        let a = addr(101);
        let b = addr(102);

        // Pool 1 (a->b): 1 a yields 2100 b (a is richly priced here).
        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(2100u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV2 {
                pair: addr(900),
                token_out: b,
                token0: a,
                token1: b,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(2_100_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: fp_weight(2100, 1),
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        // Pool 2 (b->a): 2000 b buys 1 a (a is cheap here) -> round trip nets +5%.
        graph.add_edge(Edge {
            from: b,
            to: a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(2000u64),
            venue: VenueEdge::UniV2 {
                pair: addr(901),
                token_out: a,
                token0: a,
                token1: b,
                reserve_in: U256::from(2_000_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 2000),
            max_input: U256::from(2_000_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let mut priorities = HashMap::new();
        priorities.insert(a, 10);
        priorities.insert(b, 1);

        let cycles = graph.bellman_ford(&priorities, &limits(6), 4, None);
        let cycle = cycles
            .iter()
            .find(|c| c.weight < 0)
            .expect("expected a profitable two-pool (2-hop) cycle");

        // 2-hop cycle = 3 node entries (start repeated), 2 edges.
        assert_eq!(
            cycle.cycle.len(),
            3,
            "two-pool arb must be a 2-hop cycle, got {:?}",
            cycle.cycle
        );
        let first = graph.nodes[cycle.cycle[0]];
        let last = graph.nodes[*cycle.cycle.last().unwrap()];
        assert_eq!(first, last, "cycle must return to its start");
    }

    #[test]
    fn fixed_point_detects_small_profit() {
        let mut graph = Graph::default();

        let a = addr(11);
        let b = addr(12);
        let c = addr(13);

        let tiny = 1_000_000_001u64;
        let base = 1_000_000_000u64;

        let edges = [
            (a, b, tiny, base),
            (b, c, tiny, base),
            (c, a, base.saturating_sub(1), base),
        ];

        for (from, to, num, den) in edges {
            graph.add_edge(Edge {
                from,
                to,
                rate_num: U256::from(num),
                rate_den: U256::from(den),
                venue: VenueEdge::Balancer {
                    pool_id: [0u8; 32],
                    token_in: from,
                    token_out: to,
                },
                estimated_gas: 0,
                weight: fp_weight(num, den),
                max_input: U256::from(10_000u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,

                active: true,
            });
        }

        let mut priorities = HashMap::new();
        priorities.insert(a, 5);
        priorities.insert(b, 4);
        priorities.insert(c, 3);

        let cycles = graph.bellman_ford(&priorities, &limits(4), 3, None);
        assert!(cycles.iter().any(|cycle| cycle.weight < 0));
    }

    #[test]
    fn bellman_ford_cycle_may_start_away_from_origin() {
        let mut graph = Graph::default();

        let origin = addr(0);
        let a = addr(1);
        let b = addr(2);
        let c = addr(3);

        graph.add_edge(Edge {
            from: origin,
            to: a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [9u8; 32],
                token_in: origin,
                token_out: a,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [0u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });
        graph.add_edge(Edge {
            from: b,
            to: c,
            rate_num: U256::from(3u64),
            rate_den: U256::from(2u64),
            venue: VenueEdge::Balancer {
                pool_id: [1u8; 32],
                token_in: b,
                token_out: c,
            },
            estimated_gas: 0,
            weight: fp_weight(3, 2),
            max_input: U256::from(2u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });
        graph.add_edge(Edge {
            from: c,
            to: a,
            rate_num: U256::from(4u64),
            rate_den: U256::from(3u64),
            venue: VenueEdge::Balancer {
                pool_id: [2u8; 32],
                token_in: c,
                token_out: a,
            },
            estimated_gas: 0,
            weight: fp_weight(4, 3),
            max_input: U256::from(3u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });

        let mut priorities = HashMap::new();
        priorities.insert(origin, 5);
        priorities.insert(a, 4);
        priorities.insert(b, 3);
        priorities.insert(c, 2);

        let cycles = graph.bellman_ford(&priorities, &limits(5), 4, None);
        let cycle = cycles
            .iter()
            .find(|cand| cand.start == a || cand.start == b || cand.start == c)
            .map(|cand| cand.cycle.clone())
            .expect("expected an arbitrage cycle reachable from origin");

        assert!(cycle.len() >= 2, "cycle must contain at least two vertices");
        let first = graph.nodes[cycle[0]];
        let last = graph.nodes[*cycle.last().unwrap()];
        assert_ne!(first, origin, "cycle should not begin at the origin");
        assert_eq!(first, last, "cycle should return to its starting vertex");
        assert!(
            cycle.iter().any(|&ix| graph.nodes[ix] == a),
            "cycle should include the first profitable hop"
        );
    }

    #[test]
    fn detects_cycle_on_final_iteration() {
        let mut graph = Graph::default();

        let a = addr(21);
        let b = addr(22);
        let c = addr(23);

        let edges = [(a, b, 5u64, 4u64), (b, c, 5u64, 4u64), (c, a, 4u64, 5u64)];

        for (from, to, num, den) in edges {
            graph.add_edge(Edge {
                from,
                to,
                rate_num: U256::from(num),
                rate_den: U256::from(den),
                venue: VenueEdge::Balancer {
                    pool_id: [7u8; 32],
                    token_in: from,
                    token_out: to,
                },
                estimated_gas: 0,
                weight: fp_weight(num, den),
                max_input: U256::from(1_000u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,

                active: true,
            });
        }

        let mut priorities = HashMap::new();
        priorities.insert(a, 3);
        priorities.insert(b, 2);
        priorities.insert(c, 1);

        let cycles = graph.bellman_ford(&priorities, &limits(3), 2, None);
        assert!(!cycles.is_empty());
    }

    #[test]
    fn color_round_robin_prioritizes_best_bucket_before_truncation() {
        let mut graph = Graph::default();

        let a = addr(31);
        let b = addr(32);
        let c = addr(33);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(1000u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::UniV3 {
                path: vec![(a, None), (b, None)],
                pool: addr(901),
                fee: 500,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: b,
            to: a,
            rate_num: U256::from(1001u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::UniV3 {
                path: vec![(b, None), (a, None)],
                pool: addr(902),
                fee: 500,
            },
            estimated_gas: 0,
            weight: -1,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        graph.add_edge(Edge {
            from: a,
            to: c,
            rate_num: U256::from(1000u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::Balancer {
                pool_id: [11u8; 32],
                token_in: a,
                token_out: c,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: c,
            to: a,
            rate_num: U256::from(1010u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::Balancer {
                pool_id: [12u8; 32],
                token_in: c,
                token_out: a,
            },
            estimated_gas: 0,
            weight: -10,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let priorities = HashMap::from([(a, 5), (b, 4), (c, 3)]);
        let cycles = graph.bellman_ford(&priorities, &limits(3), 1, None);

        assert_eq!(cycles.len(), 1, "k=1 should keep a single candidate");
        assert_eq!(
            cycles[0].weight, -10,
            "must pick the best cycle across colors"
        );
    }

    #[test]
    fn relax_budget_scales_with_edge_count_for_dense_vertices() {
        let mut graph = Graph::default();

        let source = addr(40);
        let mid = addr(41);

        for i in 0..8u64 {
            graph.add_edge(Edge {
                from: source,
                to: mid,
                rate_num: U256::from(1000u64),
                rate_den: U256::from(1000u64),
                venue: VenueEdge::Balancer {
                    pool_id: [i as u8; 32],
                    token_in: source,
                    token_out: mid,
                },
                estimated_gas: 0,
                weight: 0,
                max_input: U256::from(1_000u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,
                active: true,
            });
        }

        graph.add_edge(Edge {
            from: source,
            to: mid,
            rate_num: U256::from(1001u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::Balancer {
                pool_id: [200u8; 32],
                token_in: source,
                token_out: mid,
            },
            estimated_gas: 0,
            weight: -1,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        graph.add_edge(Edge {
            from: mid,
            to: source,
            rate_num: U256::from(1000u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::Balancer {
                pool_id: [201u8; 32],
                token_in: mid,
                token_out: source,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let priorities = HashMap::from([(source, 1), (mid, 1)]);
        let cycles = graph.bellman_ford(&priorities, &limits(2), 2, None);

        assert!(
            cycles.iter().any(|cycle| cycle.weight < 0),
            "negative cycle should be found even when an improving edge appears late in a dense adjacency list"
        );
    }

    #[test]
    fn relax_budget_preserves_cycles_reachable_after_non_cycle_hops() {
        let mut graph = Graph::default();

        let source = addr(70);
        let hop1 = addr(71);
        let cycle_a = addr(72);
        let cycle_b = addr(73);
        let cycle_c = addr(74);

        graph.add_edge(Edge {
            from: source,
            to: hop1,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [31u8; 32],
                token_in: source,
                token_out: hop1,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: hop1,
            to: cycle_a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [32u8; 32],
                token_in: hop1,
                token_out: cycle_a,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        graph.add_edge(Edge {
            from: cycle_a,
            to: cycle_b,
            rate_num: U256::from(1001u64),
            rate_den: U256::from(1000u64),
            venue: VenueEdge::Balancer {
                pool_id: [33u8; 32],
                token_in: cycle_a,
                token_out: cycle_b,
            },
            estimated_gas: 0,
            weight: -1,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: cycle_b,
            to: cycle_c,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [34u8; 32],
                token_in: cycle_b,
                token_out: cycle_c,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: cycle_c,
            to: cycle_a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [35u8; 32],
                token_in: cycle_c,
                token_out: cycle_a,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let priorities = HashMap::from([(source, 1)]);
        let constrained_limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 4,
            max_relaxations: 8,
            max_cycles: 2,
            timeout: Duration::from_millis(250),
        };

        let cycles = graph.bellman_ford(&priorities, &constrained_limits, 2, None);
        assert!(
            cycles.iter().any(|candidate| candidate.weight < 0),
            "negative cycle should still be discovered when it is a few hops away from the prioritized start"
        );
    }

    #[test]
    fn bellman_ford_skips_edges_with_missing_predecessors() {
        let mut graph = Graph::default();

        let origin = addr(0);
        let intermediate = addr(1);
        let relay = addr(2);
        let sink = addr(3);

        graph.add_node(origin);
        graph.add_node(intermediate);
        graph.add_node(relay);
        graph.add_node(sink);

        graph.add_edge(Edge {
            from: origin,
            to: relay,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [9u8; 32],
                token_in: origin,
                token_out: relay,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });

        graph.add_edge(Edge {
            from: relay,
            to: intermediate,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [10u8; 32],
                token_in: relay,
                token_out: intermediate,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });

        graph.add_edge(Edge {
            from: intermediate,
            to: sink,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [11u8; 32],
                token_in: intermediate,
                token_out: sink,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });

        let mut priorities = HashMap::new();
        priorities.insert(origin, 1);
        let cycles = graph.bellman_ford(&priorities, &limits(1), 2, None);
        assert!(cycles.is_empty());
    }

    #[test]
    fn canonicalize_cycle_removes_rotations() {
        let base = vec![0usize, 1, 2, 0];
        let rotated = vec![1usize, 2, 0, 1];

        assert_eq!(canonicalize_cycle(base), canonicalize_cycle(rotated));
    }

    #[test]
    fn canonicalize_cycle_preserves_direction() {
        let forward = vec![0usize, 1, 2, 0];
        let backward = vec![0usize, 2, 1, 0];

        assert_ne!(canonicalize_cycle(forward), canonicalize_cycle(backward));
    }

    #[test]
    fn high_out_degree_node_does_not_exhaust_relax_budget_mid_scan() {
        let mut graph = Graph::default();

        let a = addr(100);
        let b = addr(101);
        let c = addr(102);

        for i in 0..200u64 {
            let noise = addr(1_000 + i);
            graph.add_edge(Edge {
                from: a,
                to: noise,
                rate_num: U256::from(1u64),
                rate_den: U256::from(1u64),
                venue: VenueEdge::Balancer {
                    pool_id: [13u8; 32],
                    token_in: a,
                    token_out: noise,
                },
                estimated_gas: 0,
                weight: 1,
                max_input: U256::from(1u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,

                active: true,
            });
        }

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [14u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });
        graph.add_edge(Edge {
            from: b,
            to: c,
            rate_num: U256::from(2u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [15u8; 32],
                token_in: b,
                token_out: c,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });
        graph.add_edge(Edge {
            from: c,
            to: a,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [16u8; 32],
                token_in: c,
                token_out: a,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(1u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,

            active: true,
        });

        let mut priorities = HashMap::new();
        priorities.insert(a, 1);

        let tight_limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 3,
            max_relaxations: 8,
            max_cycles: 4,
            timeout: Duration::from_millis(250),
        };

        let cycles = graph.bellman_ford(&priorities, &tight_limits, 1, None);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].start, a);
    }

    #[test]
    fn honors_start_priorities_when_collecting_cycles() {
        let mut graph = Graph::default();

        let high = addr(30);
        let low = addr(40);

        let cycles = [(high, addr(31), addr(32)), (low, addr(41), addr(42))];

        for (start, mid, end) in cycles {
            graph.add_edge(Edge {
                from: start,
                to: mid,
                rate_num: U256::from(2u64),
                rate_den: U256::from(1u64),
                venue: VenueEdge::Balancer {
                    pool_id: [5u8; 32],
                    token_in: start,
                    token_out: mid,
                },
                estimated_gas: 0,
                weight: fp_weight(2, 1),
                max_input: U256::from(10u64),
                tolerance_bps: 0,
                observed_slippage_bps: 1,
                quote_block: None,

                active: true,
            });
            graph.add_edge(Edge {
                from: mid,
                to: end,
                rate_num: U256::from(2u64),
                rate_den: U256::from(1u64),
                venue: VenueEdge::Balancer {
                    pool_id: [6u8; 32],
                    token_in: mid,
                    token_out: end,
                },
                estimated_gas: 0,
                weight: fp_weight(2, 1),
                max_input: U256::from(10u64),
                tolerance_bps: 0,
                observed_slippage_bps: 1,
                quote_block: None,

                active: true,
            });
            graph.add_edge(Edge {
                from: end,
                to: start,
                rate_num: U256::from(1u64),
                rate_den: U256::from(1u64),
                venue: VenueEdge::Balancer {
                    pool_id: [7u8; 32],
                    token_in: end,
                    token_out: start,
                },
                estimated_gas: 0,
                weight: fp_weight(1, 1),
                max_input: U256::from(10u64),
                tolerance_bps: 0,
                observed_slippage_bps: 1,
                quote_block: None,

                active: true,
            });
        }

        let mut priorities = HashMap::new();
        priorities.insert(high, 1_000);
        priorities.insert(low, 1);

        let results = graph.bellman_ford(&priorities, &limits(3), 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].start, high);
    }

    #[test]
    fn gas_penalties_can_remove_profitable_cycles() {
        fn build_graph(gas_price: U256) -> Graph {
            let mut graph = Graph::default();
            let estimated_gas = 50_000u64;
            let base_amount = U256::from(100_000_000u64);
            let native_price = NativePrice::new(U256::exp10(18), U256::exp10(18), true);

            let a = addr(100);
            let b = addr(101);
            let c = addr(102);

            for (from, to) in [(a, b), (b, c), (c, a)] {
                let weight = compute_edge_weight(
                    U256::from(105u64),
                    U256::from(100u64),
                    estimated_gas,
                    gas_price,
                    base_amount,
                    native_price,
                );

                graph.add_edge(Edge {
                    from,
                    to,
                    rate_num: U256::from(105u64),
                    rate_den: U256::from(100u64),
                    venue: VenueEdge::Balancer {
                        pool_id: [3u8; 32],
                        token_in: from,
                        token_out: to,
                    },
                    estimated_gas,
                    weight,
                    max_input: U256::from(2_000_000u64),
                    tolerance_bps: 10,
                    observed_slippage_bps: 10,
                    quote_block: None,

                    active: true,
                });
            }

            graph
        }

        let cheap_graph = build_graph(U256::zero());
        let priorities = HashMap::new();
        let cheap_cycles = cheap_graph.bellman_ford(&priorities, &limits(3), 1, None);
        assert!(cheap_cycles.iter().any(|c| c.weight < 0));

        let expensive_graph = build_graph(U256::from(20_000u64));
        let expensive_cycles = expensive_graph.bellman_ford(&priorities, &limits(3), 1, None);
        assert!(expensive_cycles.is_empty());
    }

    #[test]
    fn incremental_adjacency_updates_only_changed_vertex() {
        let mut graph = Graph::default();

        let a = addr(1);
        let b = addr(2);
        let c = addr(3);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [41u8; 32],
                token_in: a,
                token_out: b,
            },
            estimated_gas: 0,
            weight: -10,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let before = graph.build_adjacency();
        assert_eq!(before[graph.ix[&a]].len(), 1);

        graph.add_edge(Edge {
            from: a,
            to: c,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::Balancer {
                pool_id: [42u8; 32],
                token_in: a,
                token_out: c,
            },
            estimated_gas: 0,
            weight: -8,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let after = graph.build_adjacency();
        let a_idx = graph.ix[&a];
        let b_idx = graph.ix[&b];
        assert_eq!(after[a_idx].len(), 2);
        assert!(after[b_idx].is_empty());
    }
    #[test]
    fn edge_between_returns_inserted_edge() {
        let mut graph = Graph::default();

        let a = addr(10);
        let b = addr(20);

        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV2 {
                pair: Address::zero(),
                token_out: b,
                token0: a,
                token1: b,
                reserve_in: U256::from(1_000u64),
                reserve_out: U256::from(1_000u64),
                fee_bps: 30,
            },
            estimated_gas: 0,
            weight: fp_weight(1, 1),
            max_input: U256::from(10u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        });

        let edge = graph
            .edge_between(a, b)
            .expect("edge should be retrievable");
        assert_eq!(edge.from, a);
        assert_eq!(edge.to, b);
        assert!(edge.active);
    }

    #[test]
    fn edge_between_prefers_best_duplicate_edges() {
        let mut graph = Graph::default();

        let a = addr(1);
        let b = addr(2);

        let worse = Edge {
            from: a,
            to: b,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(a, None), (b, Some(500))],
                pool: Address::zero(),
                fee: 500,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(5u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        };

        let mut better = worse.clone();
        better.venue = VenueEdge::UniV3 {
            path: vec![(a, None), (b, Some(3000))],
            pool: Address::from_low_u64_be(1),
            fee: 3000,
        };
        better.weight = fp_weight(1, 1);

        graph.add_edge(worse.clone());
        graph.add_edge(better.clone());

        let best = graph
            .edge_between(a, b)
            .expect("at least one edge should be returned");
        let expected = if is_better(&better, &worse) {
            better.weight
        } else {
            worse.weight
        };
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(best.weight, expected);
    }

    #[test]
    fn bellman_ford_preserves_parallel_edge_path() {
        let mut graph = Graph::default();
        let a = addr(1);
        let b = addr(2);

        // Two parallel A→B edges: worse (weight 0) and better (weight -5).
        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(99u64),
            rate_den: U256::from(100u64),
            venue: VenueEdge::UniV2 {
                pair: addr(100),
                token_out: b,
                token0: a,
                token1: b,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 100_000,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 30,
            observed_slippage_bps: 10,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: a,
            to: b,
            rate_num: U256::from(101u64),
            rate_den: U256::from(100u64),
            venue: VenueEdge::UniV2 {
                pair: addr(101),
                token_out: b,
                token0: a,
                token1: b,
                reserve_in: U256::from(2_000_000u64),
                reserve_out: U256::from(2_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 100_000,
            weight: -5,
            max_input: U256::from(1_000u64),
            tolerance_bps: 30,
            observed_slippage_bps: 10,
            quote_block: None,
            active: true,
        });
        graph.add_edge(Edge {
            from: b,
            to: a,
            rate_num: U256::from(102u64),
            rate_den: U256::from(100u64),
            venue: VenueEdge::UniV2 {
                pair: addr(102),
                token_out: a,
                token0: a,
                token1: b,
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(1_000_000u64),
                fee_bps: 30,
            },
            estimated_gas: 100_000,
            weight: -5,
            max_input: U256::from(1_000u64),
            tolerance_bps: 30,
            observed_slippage_bps: 10,
            quote_block: None,
            active: true,
        });

        let priorities = HashMap::new();
        let limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 2,
            max_relaxations: 8,
            max_cycles: 4,
            timeout: Duration::from_millis(200),
        };
        let cycles = graph.bellman_ford(&priorities, &limits, 4, None);
        assert!(!cycles.is_empty(), "should detect 2-hop arb");
        let candidate = cycles
            .iter()
            .find(|c| c.edge_indices.len() == 2)
            .or_else(|| cycles.first())
            .expect("cycle with edges");
        assert_eq!(candidate.edge_indices.len(), 2);
        // First hop must be the better parallel A→B edge (index 1), not edge_between pick.
        assert_eq!(candidate.edge_indices[0], 1);
        let edge = graph
            .edge_by_index(candidate.edge_indices[0])
            .expect("edge index must resolve");
        assert_eq!(edge.weight, -5);
    }
}
