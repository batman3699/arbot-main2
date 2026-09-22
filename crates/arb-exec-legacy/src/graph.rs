use crate::{metrics::Metrics, util::u256_to_f64, util::WEIGHT_SCALE};
use dashmap::DashMap;
use ethers::types::{Address, U256, U512, U64};
use rayon::prelude::*;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

#[derive(Clone, Debug)]
pub enum VenueEdge {
    UniV3 {
        path: Vec<(Address, Option<u32>)>,
        pool: Address,
        fee: u32,
        /// Pool state captured when the edge was built, so the size search can
        /// re-quote this hop locally instead of paying an `eth_call` per probe.
        /// `None` falls back to the RPC quoter (state unavailable, or
        /// `ARBOT_LOCAL_CL_QUOTES=0`).
        state: Option<crate::cl_sim::ClPoolState>,
    },
    /// Aerodrome Slipstream CL pools (tick spacing stored in `fee` field of path hops).
    Slipstream {
        path: Vec<(Address, Option<u32>)>,
        pool: Address,
        tick_spacing: u32,
        router: Address,
        /// See `UniV3::state`.
        state: Option<crate::cl_sim::ClPoolState>,
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

impl VenueEdge {
    /// The pool this edge trades through, when it has a single one.
    ///
    /// `None` for venues that cannot be identified by a pool address -- a
    /// Balancer edge is keyed by `pool_id`, and a UniV4 `pool_manager` is not
    /// the pool. Those hops cannot be matched by pool identity, and saying so
    /// is better than returning a plausible wrong address.
    pub fn pool_address(&self) -> Option<Address> {
        match self {
            VenueEdge::UniV3 { pool, .. }
            | VenueEdge::Slipstream { pool, .. }
            | VenueEdge::Curve { pool, .. } => Some(*pool),
            VenueEdge::UniV2 { pair, .. } | VenueEdge::SolidlyV2 { pair, .. } => Some(*pair),
            _ => None,
        }
    }
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
    /// Ladder for the CL pool on this edge, when one was built. `None` keeps
    /// the single-tick path with its crossing buffer.
    pub tick_ladder: Option<std::sync::Arc<crate::cl_swap::TickLadder>>,
}

/// `a * b / d`, evaluated in 512 bits so the intermediate product cannot wrap.
/// Saturates instead of panicking; a zero divisor yields zero.
/// Per-hop capacity projected to start-token units with no intermediate flooring.
///
/// Returns `None` only if the running rate products would overflow U512, or a
/// rate is degenerate (zero numerator/denominator), in which case the caller
/// falls back to the sequential form.
fn cycle_input_capacity_exact(edges: &[Edge]) -> Option<U256> {
    let mut num = U512::one(); // PROD rate_num over hops already traversed
    let mut den = U512::one(); // PROD rate_den over hops already traversed
    let mut capacity: Option<U512> = None;

    for edge in edges {
        if edge.rate_num.is_zero() || edge.rate_den.is_zero() {
            return None;
        }
        // capacity_i = max_input_i * den / num, exact until this single divide.
        let scaled = U512::from(edge.max_input).checked_mul(den)?;
        let hop_capacity = scaled / num;
        capacity = Some(match capacity {
            Some(current) => current.min(hop_capacity),
            None => hop_capacity,
        });

        num = num.checked_mul(U512::from(edge.rate_num))?;
        den = den.checked_mul(U512::from(edge.rate_den))?;
    }

    let capacity = capacity?;
    if capacity > U512::from(U256::MAX) {
        return Some(U256::MAX);
    }
    let mut buf = [0u8; 64];
    capacity.to_little_endian(&mut buf);
    Some(U256::from_little_endian(&buf[..32]))
}

fn mul_div_floor(a: U256, b: U256, d: U256) -> U256 {
    if d.is_zero() {
        return U256::zero();
    }
    let wide = U512::from(a) * U512::from(b) / U512::from(d);
    if wide > U512::from(U256::MAX) {
        return U256::MAX;
    }
    let mut buf = [0u8; 64];
    wide.to_little_endian(&mut buf);
    U256::from_little_endian(&buf[..32])
}

/// Largest cycle input, in the START token's raw units, that respects every
/// hop's `max_input` capacity.
///
/// `Edge::max_input` is denominated in that edge's own `from` token, so the
/// per-hop caps are not comparable to each other. The scanner used to fold them
/// with a plain `min()`, which mixed WETH wei with USDC's 6-decimal units and
/// with AERO, then handed the winner to the sizer as a start-token ceiling: a
/// 19 USDC cap arrived as 1.9e-11 WETH and every candidate died with
/// `upper_cap < min_amount`.
///
/// Each cap is instead pulled back to the start token along the cycle's own
/// quoted rates. If `probe` units of the start token reach hop `i` as `amt_i`,
/// that hop binds at `probe * cap_i / amt_i` start-token units. The tightest
/// such bound is the cycle capacity.
///
/// `probe` must be the notional the edge rates were observed at: `rate_num /
/// rate_den` is an average price at that size, so the projection is exact at
/// `probe` and approximate away from it. That is sufficient here — this only
/// bounds the sizer's search range, and the sizer re-quotes for real at every
/// candidate size it evaluates.
pub fn cycle_input_capacity(edges: &[Edge], probe: U256) -> U256 {
    if edges.is_empty() || probe.is_zero() {
        return U256::zero();
    }
    // Exact-fraction projection in U512.
    //
    // The sequential form re-floored `amount` at every hop
    // (`amount = mul_div_floor(amount, rate_num, rate_den)`), so truncation
    // compounded along the cycle and the final divide could floor a real
    // capacity to zero. Measured on Base: 18/18 zero-capacity events were this,
    // at hops=4 and hops=5, on WETH/USDC/cbBTC/cbETH — the deepest pairs on the
    // chain. Those cycles were never dead; the arithmetic said zero.
    //
    // Algebraically the probe cancels out:
    //   amount_i    = probe * PROD_{j<i}(rate_num_j / rate_den_j)
    //   capacity_i  = probe * max_input_i / amount_i
    //               = max_input_i * PROD_{j<i}(rate_den_j) / PROD_{j<i}(rate_num_j)
    // so carrying the running rate as an exact num/den pair in U512 and dividing
    // ONCE per hop removes every intermediate floor. `mul_div_floor` was already
    // U512 internally, so widening it was never the fix — the fold was.
    //
    // Falls back to the sequential form only if the running products overflow
    // U512, which needs an implausible rate stack; behaviour is then no worse
    // than before.
    if let Some(exact) = cycle_input_capacity_exact(edges) {
        return exact;
    }

    let mut capacity = U256::MAX;
    let mut amount = probe;
    for (hop, edge) in edges.iter().enumerate() {
        // A hop that receives nothing cannot be scaled into; the cycle is dead.
        //
        // Two very different causes both land on zero here, and the caller
        // reports both as `no_liquidity`:
        //   1. `amount` floored to zero projecting through this hop's rate --
        //      an ARITHMETIC artifact of a small probe crossing a decimals gap
        //      (e.g. 18dp -> 8dp), not an absence of liquidity.
        //   2. `edge.max_input == 0` -- a genuinely dead edge.
        // Naming which one fired is the difference between "there is no trade"
        // and "we computed zero"; `no_liquidity` on WETH/USDC/cbBTC, which are
        // the deepest pairs on the chain, is the signature of (1).
        if amount.is_zero() {
            tracing::debug!(
                target: "capacity",
                hop,
                hops = edges.len(),
                %probe,
                "cycle capacity zero: amount floored to zero projecting through \
                 this hop's rate (decimals/probe artifact, NOT dead liquidity)"
            );
            return U256::zero();
        }
        if edge.max_input.is_zero() {
            tracing::debug!(
                target: "capacity",
                hop,
                hops = edges.len(),
                "cycle capacity zero: this hop's edge has max_input = 0 (dead edge)"
            );
            return U256::zero();
        }
        capacity = capacity.min(mul_div_floor(probe, edge.max_input, amount));
        amount = mul_div_floor(amount, edge.rate_num, edge.rate_den);
    }
    if capacity.is_zero() {
        tracing::debug!(
            target: "capacity",
            hops = edges.len(),
            %probe,
            "cycle capacity floored to zero across hops: probe too small to \
             project a non-zero capacity (arithmetic, not liquidity)"
        );
    }
    capacity
}

/// Bounds for [`Graph::hub_anchored_cycles`].
#[derive(Clone, Copy, Debug)]
pub struct HubSearchLimits {
    pub min_hops: usize,
    pub max_hops: usize,
    pub max_cycles: usize,
    pub timeout: Duration,
    /// Parallel edges retained per `(from, to)` pair after dominance pruning.
    /// 1 keeps only the best-rate pool, which loses the deeper-but-slightly-worse
    /// pool that often sizes better; 3 is a reasonable default.
    pub parallel_edges_per_pair: usize,
}

impl HubSearchLimits {
    pub fn sanitized(self) -> Self {
        let max_hops = self.max_hops.max(2);
        Self {
            min_hops: self.min_hops.max(2).min(max_hops),
            max_hops,
            max_cycles: self.max_cycles.max(1),
            timeout: if self.timeout.is_zero() {
                Duration::from_millis(1)
            } else {
                self.timeout
            },
            parallel_edges_per_pair: self.parallel_edges_per_pair.clamp(1, 8),
        }
    }
}

/// Natural-log of an edge's post-haircut exchange rate, or `None` if the edge
/// is unusable. This is the quantity cycles accumulate: a cycle is profitable
/// exactly when the sum over its hops is positive.
fn edge_log_rate(edge: &Edge) -> Option<f64> {
    if !edge.active || edge.rate_den.is_zero() {
        return None;
    }
    let protected_num =
        crate::util::apply_slippage(edge.rate_num, crate::util::detection_haircut_bps());
    if protected_num.is_zero() {
        return None;
    }
    let rate = u256_to_f64(protected_num) / u256_to_f64(edge.rate_den);
    if rate <= 0.0 {
        return None;
    }
    let log_rate = rate.ln();
    log_rate.is_finite().then_some(log_rate)
}

impl Graph {
    /// Enumerate profitable cycles that start and end at a hub token.
    ///
    /// Bellman-Ford is built to find negative cycles of unknown length anywhere
    /// in a large graph. That is not this problem. Every executable cycle must
    /// start and end at a flash-loan asset, `max_hops` is 2-3, and the token
    /// universe is small — so the candidate set can be enumerated exhaustively
    /// and exactly, for less work than BF's relaxation sweeps, with none of its
    /// costs: no fixed-point log-weight precision loss, no relaxation budget
    /// silently truncating the search, no rotated duplicates to canonicalise
    /// afterwards, and no need to bolt "must start at a hub" on as a priority
    /// heuristic.
    ///
    /// Three things keep it cheap:
    ///   * **Dominance pruning** — among parallel pools on the same token pair
    ///     only the best few rates can start a winning cycle, so the rest are
    ///     dropped before the walk (`parallel_edges_per_pair`).
    ///   * **Branch and bound** — a partial path is abandoned as soon as even
    ///     the most optimistic completion cannot turn a profit. The bound is
    ///     admissible: it credits every remaining hop the best log-rate any
    ///     edge in the graph offers, and the closing hop the best rate back to
    ///     this specific hub, so it can never prune a genuinely winning cycle.
    ///   * **Canonical keys** — cycles are keyed by their ordered pool sequence
    ///     anchored at the hub, so the same economic path is only ever emitted
    ///     once.
    ///
    /// Returns candidates sorted by estimated profit, best first.
    pub fn hub_anchored_cycles(
        &self,
        hubs: &[Address],
        limits: &HubSearchLimits,
        k: usize,
    ) -> Vec<CycleCandidate> {
        let limits = limits.sanitized();
        if self.nodes.is_empty() || hubs.is_empty() || k == 0 {
            return Vec::new();
        }
        let started = Instant::now();

        // Dominance pruning, done once for the whole walk.
        let pruned = self.dominant_edges_by_source(limits.parallel_edges_per_pair);

        // Admissible bound inputs. `global_max_log` is the most any single hop
        // can contribute; `best_return_log` is the most the closing hop into
        // this hub can contribute from a given token.
        let global_max_log = pruned
            .values()
            .flatten()
            .map(|(_, _, log_rate)| *log_rate)
            .fold(f64::NEG_INFINITY, f64::max);
        if !global_max_log.is_finite() {
            return Vec::new();
        }

        let mut out: Vec<CycleCandidate> = Vec::new();
        let mut seen: HashSet<Vec<[u8; 32]>> = HashSet::new();

        for &hub in hubs {
            if !self.ix.contains_key(&hub) {
                continue;
            }
            let reach = self.best_reach_hub_log(&pruned, hub, limits.max_hops);
            let mut path_edges: Vec<usize> = Vec::with_capacity(limits.max_hops);
            let mut visited: HashSet<Address> = HashSet::new();
            visited.insert(hub);
            self.walk_from(
                hub,
                hub,
                0.0,
                &pruned,
                &reach,
                &limits,
                started,
                &mut visited,
                &mut path_edges,
                &mut seen,
                &mut out,
            );
            if out.len() >= limits.max_cycles || started.elapsed() >= limits.timeout {
                break;
            }
        }

        out.sort_by_key(|c| Reverse(c.estimated_profit_bps));
        out.truncate(k);
        out
    }

    /// Outgoing edges per token, keeping only the top-rate few per destination.
    /// Values are `(edge_index, to, log_rate)`, sorted by log-rate descending so
    /// the walk explores the most promising branch first and the bound bites
    /// sooner.
    fn dominant_edges_by_source(
        &self,
        per_pair: usize,
    ) -> HashMap<Address, Vec<(usize, Address, f64)>> {
        let mut by_pair: HashMap<(Address, Address), Vec<(usize, f64)>> = HashMap::new();
        for (idx, edge) in self.edges.iter().enumerate() {
            if let Some(log_rate) = edge_log_rate(edge) {
                by_pair
                    .entry((edge.from, edge.to))
                    .or_default()
                    .push((idx, log_rate));
            }
        }
        let mut by_source: HashMap<Address, Vec<(usize, Address, f64)>> = HashMap::new();
        for ((from, to), mut candidates) in by_pair {
            candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
            candidates.truncate(per_pair);
            let bucket = by_source.entry(from).or_default();
            for (idx, log_rate) in candidates {
                bucket.push((idx, to, log_rate));
            }
        }
        for bucket in by_source.values_mut() {
            bucket.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));
        }
        by_source
    }

    /// `reach[h][token]` = best achievable log-rate getting from `token` back to
    /// `hub` in at most `h` hops.
    ///
    /// This is the branch-and-bound heuristic. Because it relaxes the
    /// simple-path constraint (it may reuse a token the real walk could not),
    /// it can only ever be optimistic — which is exactly what makes pruning on
    /// it safe. An earlier version used the best *single* return hop, which
    /// wrongly discarded every token that reaches the hub in two hops and made
    /// 3-hop cycles unfindable.
    fn best_reach_hub_log(
        &self,
        pruned: &HashMap<Address, Vec<(usize, Address, f64)>>,
        hub: Address,
        max_hops: usize,
    ) -> Vec<HashMap<Address, f64>> {
        // Index by hop budget; slot 0 is unusable (no hops, cannot reach).
        let mut reach: Vec<HashMap<Address, f64>> = vec![HashMap::new(); max_hops + 1];
        if max_hops == 0 {
            return reach;
        }
        // One hop: a direct edge into the hub.
        for (from, bucket) in pruned {
            for (_, to, log_rate) in bucket {
                if *to == hub {
                    let slot = reach[1].entry(*from).or_insert(f64::NEG_INFINITY);
                    if *log_rate > *slot {
                        *slot = *log_rate;
                    }
                }
            }
        }
        for h in 2..=max_hops {
            let prev = reach[h - 1].clone();
            let mut cur = prev.clone(); // "at most h" includes "at most h-1"
            for (from, bucket) in pruned {
                for (_, to, log_rate) in bucket {
                    if *to == hub {
                        continue; // already covered by the 1-hop seed
                    }
                    if let Some(rest) = prev.get(to) {
                        let total = log_rate + rest;
                        let slot = cur.entry(*from).or_insert(f64::NEG_INFINITY);
                        if total > *slot {
                            *slot = total;
                        }
                    }
                }
            }
            reach[h] = cur;
        }
        reach
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_from(
        &self,
        hub: Address,
        current: Address,
        acc_log: f64,
        pruned: &HashMap<Address, Vec<(usize, Address, f64)>>,
        reach: &[HashMap<Address, f64>],
        limits: &HubSearchLimits,
        started: Instant,
        visited: &mut HashSet<Address>,
        path_edges: &mut Vec<usize>,
        seen: &mut HashSet<Vec<[u8; 32]>>,
        out: &mut Vec<CycleCandidate>,
    ) {
        if out.len() >= limits.max_cycles || started.elapsed() >= limits.timeout {
            return;
        }
        let hops = path_edges.len();
        if hops >= limits.max_hops {
            return;
        }
        let Some(bucket) = pruned.get(&current) else {
            return;
        };

        for &(edge_idx, to, log_rate) in bucket {
            if out.len() >= limits.max_cycles || started.elapsed() >= limits.timeout {
                return;
            }
            let next_acc = acc_log + log_rate;
            let next_hops = hops + 1;

            if to == hub {
                // Closing the cycle.
                if next_hops >= limits.min_hops && next_acc > 0.0 {
                    path_edges.push(edge_idx);
                    self.emit_cycle(hub, path_edges, seen, out);
                    path_edges.pop();
                }
                continue;
            }

            // Simple cycles only: an intermediate token is visited at most once.
            if visited.contains(&to) || next_hops >= limits.max_hops {
                // `next_hops == max_hops` with `to != hub` cannot close in time.
                continue;
            }

            // Branch and bound: `to` must still get back to the hub within the
            // remaining budget. `reach` gives the most optimistic log-rate for
            // doing so, so if even that cannot clear zero, no completion of this
            // prefix can profit.
            let budget = limits.max_hops.saturating_sub(next_hops);
            let Some(best_rest) = reach.get(budget).and_then(|m| m.get(&to)).copied() else {
                continue; // `to` cannot reach the hub within the remaining hops
            };
            if next_acc + best_rest <= 0.0 {
                continue;
            }

            visited.insert(to);
            path_edges.push(edge_idx);
            self.walk_from(
                hub,
                to,
                next_acc,
                pruned,
                reach,
                limits,
                started,
                visited,
                path_edges,
                seen,
                out,
            );
            path_edges.pop();
            visited.remove(&to);
        }
    }

    /// Canonicalise and record a closed cycle.
    fn emit_cycle(
        &self,
        hub: Address,
        path_edges: &[usize],
        seen: &mut HashSet<Vec<[u8; 32]>>,
        out: &mut Vec<CycleCandidate>,
    ) {
        // A round trip through a single pool pays that pool's fee twice against
        // its own curve and can never profit. These used to surface as the
        // scanner's "best two-hop" and produced absurd headline spreads.
        let pool_keys: Option<Vec<[u8; 32]>> = path_edges
            .iter()
            .map(|&idx| self.edges.get(idx).and_then(edge_pool_key))
            .collect();
        let Some(pool_keys) = pool_keys else {
            return;
        };
        if pool_keys.len() >= 2 {
            let unique: HashSet<&[u8; 32]> = pool_keys.iter().collect();
            if unique.len() < pool_keys.len() {
                return;
            }
        }
        // Anchored at the hub, so the pool sequence is already canonical: the
        // same economic path can only be walked one way from one hub.
        if !seen.insert(pool_keys) {
            return;
        }

        let Some(estimated_profit_bps) = self.estimate_cycle_profit_bps_from_edges(path_edges)
        else {
            return;
        };
        let mut cycle: Vec<usize> = Vec::with_capacity(path_edges.len() + 1);
        let Some(&hub_ix) = self.ix.get(&hub) else {
            return;
        };
        cycle.push(hub_ix);
        for &idx in path_edges {
            let Some(edge) = self.edges.get(idx) else {
                return;
            };
            let Some(&to_ix) = self.ix.get(&edge.to) else {
                return;
            };
            cycle.push(to_ix);
        }
        let weight = path_edges
            .iter()
            .filter_map(|&idx| self.edges.get(idx))
            .fold(0i64, |acc, edge| acc.saturating_add(edge.weight));

        out.push(CycleCandidate {
            cycle,
            edge_indices: path_edges.to_vec(),
            weight,
            start: hub,
            estimated_profit_bps,
        });
    }
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
        let max_relaxations = self.max_relaxations.clamp(1, 256);
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
/// One negative cycle found by the parallel Bellman-Ford sweep, as
/// `(weight, cycle, edge_indices, start_priority, estimated_profit_bps)`.
type DiscoveredCycle = (EdgeWeight, Vec<NodeIx>, Vec<usize>, i128, i64);

/// Cloneable so the scan can publish an immutable snapshot for readers.
///
/// The flashblock fast path reads a snapshot; it never mutates the graph. That
/// is what makes sharing safe here where sharing `LiveState` was not: LiveState
/// has one global ordinal cursor and two writers broke it, while a graph
/// snapshot has exactly one writer and any number of readers.
#[derive(Clone)]
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

/// A dedup key naming the ROUTE, not just the tokens it visits.
///
/// Deduplicating on the token sequence alone collapses genuinely different
/// trades. WETH->USDC->WETH via UniV3+Aerodrome and the same loop via
/// UniV3+Pancake share every token, so the second was discarded as a repeat of
/// the first -- and parallel venues on one pair are precisely the dislocation
/// this bot exists to find. The cheapest route on paper is not always the one
/// that sizes, funds or fills, so throwing the alternatives away costs real
/// candidates.
///
/// Rotating the edges by the SAME offset the nodes were rotated by keeps the
/// key rotation-invariant: one loop entered at a different token is still one
/// route, while the same loop on different pools is not.
pub(crate) fn canonicalize_route(
    cycle: &[usize],
    edge_indices: &[usize],
) -> (Vec<usize>, Vec<usize>) {
    let nodes = canonicalize_cycle(cycle.to_vec());
    // `canonicalize_cycle` returns the loop CLOSED, so a 3-hop cycle comes back
    // as 4 entries. Hops is the open length.
    let closed = nodes.len() > 1 && nodes.first() == nodes.last();
    let hops = if closed { nodes.len() - 1 } else { nodes.len() };
    // Edges are only meaningful when there is exactly one per hop. Anything
    // else degrades to the node-only key rather than inventing an alignment.
    if hops == 0 || edge_indices.len() != hops {
        return (nodes, Vec::new());
    }
    let open: Vec<usize> = if cycle.first() == cycle.last() && cycle.len() > 1 {
        cycle[..cycle.len() - 1].to_vec()
    } else {
        cycle.to_vec()
    };
    let offset = open
        .iter()
        .position(|n| *n == nodes[0])
        .unwrap_or(0);
    let rotated = (0..hops)
        .map(|i| edge_indices[(offset + i) % hops])
        .collect();
    (nodes, rotated)
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
    /// Resolve a node path to concrete edge indices IN THIS GRAPH.
    ///
    /// Edge indices are positions in `self.edges` and are only meaningful for
    /// the graph that produced them. Anything that survives a graph rebuild
    /// (e.g. cross-scan cycle seeds) must carry the node path and re-resolve
    /// through here, never carry indices forward.
    pub(crate) fn best_edge_indices_for_node_path(&self, cycle: &[usize]) -> Option<Vec<usize>> {
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

    /// Translate a token loop into an `IndexedCycle` this graph can price.
    ///
    /// `CycleIndex` stores loops as ADDRESSES in OPEN form, deliberately: node
    /// indices are assigned in first-seen order and the graph is rebuilt every
    /// scan, so an index-keyed cycle silently refers to different tokens after a
    /// rebuild. This is the translation across that boundary, and it is where
    /// the flashblock fast path hands a cycle to the existing plan machinery.
    ///
    /// The loop is CLOSED here — `IndexedCycle.cycle` carries the return to
    /// `tokens[0]` and `hops()` is `len - 1`. Passing the open form would build
    /// a plan one hop short, which is a path that ends holding the wrong token,
    /// not a cycle.
    ///
    /// `None` when any token is absent from the graph or any hop has no edge.
    /// A partial translation is worse than none: it would produce a plan whose
    /// steps do not compose.
    // Test-only since the fast path moved to `indexed_cycle_for_pools`. Kept,
    // not deleted: the tests use it to prove the premise of that change -- that
    // left to itself the graph picks a DIFFERENT pool from the one a caller
    // priced. It must never become a fallback for a failed pool match, because
    // silently substituting a route is the bug.
    #[allow(dead_code)]
    pub fn indexed_cycle_for_tokens(&self, tokens: &[Address]) -> Option<IndexedCycle> {
        if tokens.len() < 2 {
            return None;
        }
        let mut cycle: Vec<usize> = Vec::with_capacity(tokens.len() + 1);
        for t in tokens {
            cycle.push(*self.ix.get(t)?);
        }
        cycle.push(cycle[0]);
        let edge_indices = self.best_edge_indices_for_node_path(&cycle)?;
        Some(IndexedCycle {
            cycle,
            edge_indices,
        })
    }

    /// Translate a token loop into an `IndexedCycle` that uses THESE pools.
    ///
    /// `indexed_cycle_for_tokens` asks the graph for its own best edge per hop,
    /// which is chosen from the scan's quotes. When the caller has already
    /// decided which pool it priced -- as the flashblock fast path has, from
    /// preconfirmed state the scan has never seen -- that is a silent
    /// substitution: the profitability claim describes one route and the plan
    /// executes another. Neither side reports anything wrong.
    ///
    /// `pools[i]` is the pool for the hop `tokens[i] -> tokens[i+1]`, with the
    /// closing hop last. `None` when any hop has no edge on the named pool,
    /// which is a real answer: the caller priced a pool this graph cannot
    /// execute through, and substituting a different one would hide that.
    pub fn indexed_cycle_for_pools(
        &self,
        tokens: &[Address],
        pools: &[Address],
    ) -> Option<IndexedCycle> {
        if tokens.len() < 2 || pools.len() != tokens.len() {
            return None;
        }
        let mut cycle: Vec<usize> = Vec::with_capacity(tokens.len() + 1);
        for t in tokens {
            cycle.push(*self.ix.get(t)?);
        }
        cycle.push(cycle[0]);

        let mut edge_indices = Vec::with_capacity(tokens.len());
        for (i, window) in cycle.windows(2).enumerate() {
            let &from = self.nodes.get(window[0])?;
            let &to = self.nodes.get(window[1])?;
            let want = pools[i];
            edge_indices.push(self.best_edge_index_on_pool(from, to, want)?);
        }
        Some(IndexedCycle { cycle, edge_indices })
    }

    /// Node index for a token, if the graph knows it.
    pub fn node_index(&self, token: Address) -> Option<usize> {
        self.ix.get(&token).copied()
    }

    /// Index of the active edge for this hop on `pool`, if there is one.
    pub fn best_edge_index_on_pool(
        &self,
        from: Address,
        to: Address,
        pool: Address,
    ) -> Option<usize> {
        self.edge_lookup.get(&(from, to))?.iter().copied().find(|&e| {
            self.edges
                .get(e)
                .filter(|edge| edge.active)
                .and_then(|edge| edge.venue.pool_address())
                == Some(pool)
        })
    }

    /// Whether any ACTIVE edge for this hop trades through `pool`.
    pub fn has_edge_on_pool(&self, from: Address, to: Address, pool: Address) -> bool {
        self.best_edge_index_on_pool(from, to, pool).is_some()
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
        self.bellman_ford_diagnostic(start_priorities, limits, k, metrics)
            .0
    }

    /// As [`Self::bellman_ford`], plus the best gross edge observed across all
    /// candidates INCLUDING rejected ones, as `ln(prod rate)` scaled by
    /// `WEIGHT_SCALE`. `None` when no cycle was even evaluated (an empty or
    /// disconnected graph), which is a different diagnosis from "every cycle
    /// lost to fees".
    pub fn bellman_ford_diagnostic(
        &self,
        start_priorities: &HashMap<Address, i128>,
        limits: &BellmanFordLimits,
        k: usize,
        metrics: Option<&Metrics>,
    ) -> (Vec<CycleCandidate>, Option<i64>) {
        let best_gross_scaled = AtomicI64::new(i64::MIN);
        let cycles =
            self.bellman_ford_inner(start_priorities, limits, k, metrics, &best_gross_scaled);
        let best = best_gross_scaled.load(AtomicOrdering::Relaxed);
        (cycles, (best != i64::MIN).then_some(best))
    }

    fn bellman_ford_inner(
        &self,
        start_priorities: &HashMap<Address, i128>,
        limits: &BellmanFordLimits,
        k: usize,
        metrics: Option<&Metrics>,
        best_gross_scaled: &AtomicI64,
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

        let discovered: Vec<DiscoveredCycle> = filtered_starts
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
                    best_gross_scaled,
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
        // Route key: (canonical nodes, canonical edges). Nodes alone would
        // collapse the same token loop through different pools.
        let mut seen: HashSet<(Vec<usize>, Vec<usize>)> = HashSet::new();

        for (weight, mut cycle, edge_indices, priority, estimated_profit_bps) in discovered {
            if cycle.len() < 2 {
                continue;
            }
            if cycle.first() != cycle.last() {
                if let Some(&first) = cycle.first() {
                    cycle.push(first);
                }
            }

            // Keyed on the route, not the token loop: the same tokens through
            // different pools are different trades.
            if !seen.insert(canonicalize_route(&cycle, &edge_indices)) {
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
    /// Best (largest) `ln(prod rate)` seen across every cycle CONSIDERED this
    /// search, scaled by `WEIGHT_SCALE`, including cycles that were rejected.
    ///
    /// Without this the engine reports only "no viable cycles", which cannot
    /// distinguish "we were 2 bps short" from "we were 500 bps short" — two
    /// situations demanding completely different responses. Recording the best
    /// REJECTED candidate turns a bare negative into a distance-to-profit.
    best_gross_scaled: &'a AtomicI64,
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
            best_gross_scaled,
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
        // Route key: (canonical nodes, canonical edges). Nodes alone would
        // collapse the same token loop through different pools.
        let mut seen: HashSet<(Vec<usize>, Vec<usize>)> = HashSet::new();

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

            if !seen.insert(canonicalize_route(&cycle, &edge_path)) {
                return;
            }

            let weight = if edge_path.len() == hops {
                self.cycle_weight_from_edge_indices(&edge_path)
            } else {
                self.cycle_weight(&cycle)
            };
            let Some(weight) = weight else {
                return;
            };

            // Stage-1 admission tests the SIZE-INDEPENDENT gross edge, never
            // profit. Profit is Stage 2's decision (spec §1: detection "never
            // computes final profit, never decides money").
            //
            // This used to reject on `weight >= 0`. That weight is
            // `sum(gas_ratio_i) - ln(prod rate_i)`, where each `gas_ratio_i` is
            // gas divided by ONE fixed probe notional. Gas is a fixed cost while
            // gross scales with size, so `gas_ratio` shrinks as size grows: a
            // cycle can be unprofitable at the probe notional and clearly
            // profitable at the size Stage 2 would actually choose. Rejecting on
            // it discarded real money at a size nobody had chosen yet — the unit
            // test covering this was even named
            // `gas_penalties_can_remove_profitable_cycles`.
            //
            // `prod rate_i <= 1` is the sound test: the cycle loses value before
            // gas is considered at all, so NO size can rescue it. That is a
            // statement about the rates alone and holds at every notional.
            //
            // `weight` is still carried and is still what cycles are RANKED by,
            // so the gas toll continues to order candidates cheapest-first
            // (spec §Phase 2 keeps the toll "baked in" to the weight) — it just
            // no longer decides admission.
            //
            // Fails closed when the rate product cannot be established: an
            // unquotable cycle is not a candidate.
            let Some(log_rate_sum) = self
                .cycle_log_rate_sum_from_edges(&edge_path)
                .or_else(|| self.cycle_log_rate_sum(&cycle))
            else {
                return;
            };
            // Track the best gross edge seen even when it loses, so the
            // operator learns how far from profitable the market actually was.
            let scaled = (log_rate_sum * WEIGHT_SCALE as f64).round();
            if scaled.is_finite() {
                best_gross_scaled.fetch_max(
                    scaled.clamp(i64::MIN as f64, i64::MAX as f64) as i64,
                    AtomicOrdering::Relaxed,
                );
            }

            if log_rate_sum <= 0.0 {
                return;
            }
            let estimated_profit_bps = log_rate_sum_to_bps(log_rate_sum).unwrap_or(0);

            store.push(DetectedCycle {
                weight,
                cycle,
                edge_indices: edge_path,
                estimated_profit_bps,
            });
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

    /// The exact edge per hop from the search's predecessor chain, or `None`.
    ///
    /// This used to fall back to the token-pair lookup and take the
    /// lowest-weight active edge whenever the predecessor edge did not match.
    /// That is a substitution: the caller asked which edges this cycle was
    /// found on and got a different route that happens to connect the same
    /// tokens. On any pair served by more than one venue -- the case this bot
    /// exists to exploit -- the substituted pool has its own price, fee,
    /// reserves and router, so everything computed downstream describes a trade
    /// the search never evaluated.
    ///
    /// It could also return FEWER indices than hops: when the lookup found no
    /// candidate the branch pushed nothing and the length mismatch travelled on
    /// silently. Both failures are now one `None`, and the caller drops the
    /// candidate. A route that cannot be named exactly is not a route.
    fn resolve_edge_path_for_cycle(
        &self,
        cycle: &[usize],
        pred_edge: &[Option<usize>],
    ) -> Option<Vec<usize>> {
        if cycle.len() < 2 {
            return None;
        }
        let hops = cycle.len().saturating_sub(1);
        let mut indices = Vec::with_capacity(hops);
        for window in cycle.windows(2) {
            let from_ix = window[0];
            let to_ix = window[1];
            let idx = pred_edge.get(to_ix).copied().flatten()?;
            if !self.edges.get(idx).is_some_and(|edge| {
                edge.active
                    && self.ix.get(&edge.from) == Some(&from_ix)
                    && self.ix.get(&edge.to) == Some(&to_ix)
            }) {
                return None;
            }
            indices.push(idx);
        }
        debug_assert_eq!(indices.len(), hops);
        Some(indices)
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
                self.resolve_edge_path_for_cycle(&rotated_nodes, pred_edge)?
            };

            Some((rotated_nodes, rotated_edges))
        } else {
            let resolved_edges = if edges.len() == hops {
                edges
            } else {
                self.resolve_edge_path_for_cycle(&cycle, pred_edge)?
            };
            Some((cycle, resolved_edges))
        }
    }

    #[allow(dead_code)] // consumed by the `arb-exec` binary target
    pub(crate) fn cycle_weight(&self, cycle: &[usize]) -> Option<i64> {
        let edge_indices = self.best_edge_indices_for_node_path(cycle)?;
        self.cycle_weight_from_edge_indices(&edge_indices)
    }

    /// `sum(ln(post-fee rate))` over a cycle's edges — i.e. `ln(prod rate_i)`.
    ///
    /// This is the SIZE-INDEPENDENT gross edge of the cycle: `> 0` means the
    /// rates alone compound to a gain before any cost is considered, and `<= 0`
    /// means no trade size can ever make the cycle profitable. Admission uses
    /// this raw value rather than the bps figure below, because rounding to
    /// whole basis points floors any gross edge under 0.5 bps to zero and would
    /// silently drop cycles the fixed-point weights were built to detect.
    fn cycle_log_rate_sum_from_edges(&self, edge_indices: &[usize]) -> Option<f64> {
        if edge_indices.is_empty() {
            return None;
        }
        let mut log_rate_sum = 0.0f64;
        for &idx in edge_indices {
            let edge = self.edges.get(idx)?;
            if !edge.active {
                return None;
            }
            let protected_num =
                crate::util::apply_slippage(edge.rate_num, crate::util::detection_haircut_bps());
            if protected_num.is_zero() || edge.rate_den.is_zero() {
                return None;
            }
            let rate = u256_to_f64(protected_num) / u256_to_f64(edge.rate_den);
            if rate <= 0.0 {
                return None;
            }
            log_rate_sum += rate.ln();
        }
        if !log_rate_sum.is_finite() {
            return None;
        }
        Some(log_rate_sum)
    }

    /// Node-path sibling of [`Self::cycle_log_rate_sum_from_edges`].
    fn cycle_log_rate_sum(&self, cycle: &[usize]) -> Option<f64> {
        let edge_indices = self.best_edge_indices_for_node_path(cycle)?;
        self.cycle_log_rate_sum_from_edges(&edge_indices)
    }

    fn estimate_cycle_profit_bps_from_edges(&self, edge_indices: &[usize]) -> Option<i64> {
        let log_rate_sum = self.cycle_log_rate_sum_from_edges(edge_indices)?;
        log_rate_sum_to_bps(log_rate_sum)
    }

    /// Node-path fallback for [`Self::estimate_cycle_profit_bps_from_edges`],
    /// used when the search did not carry explicit edge indices: resolve each
    /// hop to its best edge, then run the identical log-rate profit math.
    fn estimate_cycle_profit_bps(&self, cycle: &[usize]) -> Option<i64> {
        let edge_indices = self.best_edge_indices_for_node_path(cycle)?;
        self.estimate_cycle_profit_bps_from_edges(&edge_indices)
    }
}

/// Identity of the pool an edge trades through, for telling a same-pool
/// round trip apart from a genuine cross-venue one.
fn edge_pool_key(edge: &Edge) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    match &edge.venue {
        VenueEdge::UniV3 { pool, .. }
        | VenueEdge::Slipstream { pool, .. }
        | VenueEdge::Curve { pool, .. } => out[12..].copy_from_slice(pool.as_bytes()),
        VenueEdge::UniV2 { pair, .. } | VenueEdge::SolidlyV2 { pair, .. } => {
            out[12..].copy_from_slice(pair.as_bytes())
        }
        VenueEdge::Balancer { pool_id, .. } => out = *pool_id,
        VenueEdge::Univ4 { pool_manager, .. } => out[12..].copy_from_slice(pool_manager.as_bytes()),
        _ => return None,
    }
    Some(out)
}

/// Best closed two-hop round trip in the graph, measured regardless of sign.
#[derive(Clone, Copy, Debug)]
pub struct TwoHopProbe {
    /// `(prod rate - 1) * 10_000`, i.e. gross edge in basis points. Negative
    /// means the best available round trip still loses to fees.
    pub best_bps: f64,
    pub token_a: Address,
    pub token_b: Address,
    /// True when the two legs use DIFFERENT pools — a genuine cross-venue
    /// round trip. A same-pool best means the graph contains no pair quoted by
    /// two venues, which is a structural finding in its own right.
    pub cross_pool: bool,
}

impl Graph {
    /// Enumerate every closed two-hop route and return the best one BY SIGN-FREE
    /// gross edge.
    ///
    /// This exists because `bellman_ford_diagnostic` can only report on cycles
    /// the negative-cycle search actually surfaces — it reports `None` whenever
    /// no cycle clears `prod rate > 1`, which cannot distinguish "closed cycles
    /// exist but all lose to fees" from "the search is not surfacing cycles that
    /// exist". Those demand opposite responses, and 580 scans of "none" could
    /// not separate them.
    ///
    /// This pass needs no search: it groups active edges by ordered token pair,
    /// keeps the best rate per pair, and pairs `(A,B)` with `(B,A)`. O(E) in the
    /// edge count, so it is cheap enough to run every scan.
    ///
    /// Rates use the same post-fee, slippage-adjusted definition the detector
    /// uses, so the number is directly comparable to `best_gross_bps`.
    pub fn best_two_hop_roundtrip(&self) -> Option<TwoHopProbe> {
        // Best rate per ordered node pair (parallel edges collapse to the best).
        let mut best: HashMap<(usize, usize), (f64, usize)> = HashMap::new();
        for (idx, edge) in self.edges.iter().enumerate() {
            if !edge.active {
                continue;
            }
            let (Some(&from_ix), Some(&to_ix)) = (self.ix.get(&edge.from), self.ix.get(&edge.to))
            else {
                continue;
            };
            let protected =
                crate::util::apply_slippage(edge.rate_num, crate::util::detection_haircut_bps());
            if protected.is_zero() || edge.rate_den.is_zero() {
                continue;
            }
            let rate = u256_to_f64(protected) / u256_to_f64(edge.rate_den);
            if !rate.is_finite() || rate <= 0.0 {
                continue;
            }
            best.entry((from_ix, to_ix))
                .and_modify(|slot| {
                    if rate > slot.0 {
                        *slot = (rate, idx);
                    }
                })
                .or_insert((rate, idx));
        }

        let mut winner: Option<TwoHopProbe> = None;
        for (&(a, b), &(rate_ab, idx_ab)) in best.iter() {
            if a >= b {
                continue; // consider each unordered pair once
            }
            let Some(&(rate_ba, idx_ba)) = best.get(&(b, a)) else {
                continue;
            };
            let product = rate_ab * rate_ba;
            if !product.is_finite() {
                continue;
            }
            let bps = (product - 1.0) * 10_000.0;
            if winner.map(|w| bps > w.best_bps).unwrap_or(true) {
                let cross_pool = match (
                    self.edges.get(idx_ab).and_then(edge_pool_key),
                    self.edges.get(idx_ba).and_then(edge_pool_key),
                ) {
                    (Some(x), Some(y)) => x != y,
                    _ => false,
                };
                winner = Some(TwoHopProbe {
                    best_bps: bps,
                    token_a: self.nodes[a],
                    token_b: self.nodes[b],
                    cross_pool,
                });
            }
        }
        winner
    }
}

/// Convert a cycle's log-rate sum into whole basis points of gross edge.
/// Quantizing loses sub-0.5bps detail, so this is for reporting and ranking
/// only — never for admission (see [`Graph::cycle_log_rate_sum_from_edges`]).
fn log_rate_sum_to_bps(log_rate_sum: f64) -> Option<i64> {
    let profit_ratio = log_rate_sum.exp() - 1.0;
    if !profit_ratio.is_finite() {
        return None;
    }
    let scaled = (profit_ratio * 10_000.0).round();
    Some(scaled.clamp(i64::MIN as f64, i64::MAX as f64) as i64)
}

fn is_better(candidate: &Edge, current: &Edge) -> bool {
    if candidate.active != current.active {
        return candidate.active;
    }
    // Freshness outranks price. `edge_signature` keys on the pool (and fee tier
    // / tick spacing), so the two edges compared here are always the SAME pool
    // in the SAME direction — two quotes of one price, taken at different
    // blocks. Keeping the "better" one meant a stale quote could beat the
    // current one purely by being more favourable, which is the definition of a
    // phantom: a price that is not there any more. filter_stale_edges bounds
    // this to MAX_QUOTE_BLOCK_LAG (2 blocks, ~4s on Base), but 4 seconds is
    // ample to invent an edge on a pair whose real spread is 1-2 bps.
    match (candidate.quote_block, current.quote_block) {
        (Some(cand), Some(cur)) if cand != cur => return cand > cur,
        // An evidence-backed quote beats one with no block attribution.
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        _ => {}
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
    use crate::util::compute_edge_weight;
    use std::time::Duration;

    fn addr(id: u64) -> Address {
        Address::from_low_u64_be(id)
    }

    fn hop_edge(from: Address, to: Address) -> Edge {
        Edge {
            from,
            to,
            rate_num: U256::from(1u64),
            rate_den: U256::from(1u64),
            venue: VenueEdge::UniV3 {
                path: vec![(from, None), (to, Some(500))],
                pool: Address::zero(),
                fee: 500,
                state: None,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        }
    }

    /// A hop the search cannot name exactly is refused, never substituted.
    ///
    /// The old fallback searched the token-pair lookup and took the
    /// lowest-weight active edge whenever the predecessor edge did not match,
    /// so the caller asked "which edges was this cycle found on?" and received
    /// a different route connecting the same tokens. With parallel venues on a
    /// pair, that silently swaps the pool, fee and router underneath a priced
    /// candidate.
    #[test]
    fn edge_resolution_refuses_rather_than_substituting() {
        let (a, b) = (addr(1), addr(2));
        let mut g = Graph::default();
        // Two DISTINCT pools serve a->b, so a substitution has somewhere to go.
        // `add_edge` dedups on venue signature, so the second needs its own
        // pool or it is simply the first again.
        g.add_edge(hop_edge(a, b));
        let mut second = hop_edge(a, b);
        second.weight = 1;
        if let VenueEdge::UniV3 { pool, .. } = &mut second.venue {
            *pool = addr(77);
        }
        g.add_edge(second);
        g.add_edge(hop_edge(b, a));
        assert_eq!(g.edges.len(), 3, "test needs two parallel a->b edges");
        let (ai, bi) = (g.ix[&a], g.ix[&b]);

        // A predecessor chain that names nothing for the hop into `b`.
        let empty: Vec<Option<usize>> = vec![None; g.nodes.len()];
        assert!(
            g.resolve_edge_path_for_cycle(&[ai, bi, ai], &empty).is_none(),
            "an unnamed hop must refuse, not fall back to the lookup"
        );

        // A predecessor edge that exists but does not serve this hop.
        let mut wrong: Vec<Option<usize>> = vec![None; g.nodes.len()];
        wrong[bi] = Some(2); // the b->a edge, offered for the a->b hop
        wrong[ai] = Some(0);
        assert!(
            g.resolve_edge_path_for_cycle(&[ai, bi, ai], &wrong).is_none(),
            "a mismatched predecessor edge must refuse"
        );

        // Correctly named hops resolve, and to exactly those indices.
        let mut good: Vec<Option<usize>> = vec![None; g.nodes.len()];
        good[bi] = Some(1); // the SECOND a->b edge, not the graph's first
        good[ai] = Some(2);
        let got = g
            .resolve_edge_path_for_cycle(&[ai, bi, ai], &good)
            .expect("named hops resolve");
        assert_eq!(got, vec![1, 2], "must return the named edges verbatim");
    }

    fn closed_triangle() -> (Graph, Address, Address, Address) {
        let mut g = Graph::default();
        let (a, b, c) = (addr(1), addr(2), addr(3));
        for t in [a, b, c] {
            g.add_node(t);
        }
        for (f, t) in [(a, b), (b, c), (c, a)] {
            g.add_edge(hop_edge(f, t));
        }
        (g, a, b, c)
    }

    /// The substitution this exists to prevent.
    ///
    /// A caller that priced a cycle through specific pools -- as the fast path
    /// does, from preconfirmed state this graph has never seen -- must execute
    /// through THOSE pools. `indexed_cycle_for_tokens` picks the graph's own
    /// best edge per hop from the SCAN's quotes, so the plan would trade a
    /// different route from the one judged profitable, with nothing on either
    /// side reporting a problem.
    #[test]
    fn a_route_priced_on_one_pool_is_not_translated_onto_another() {
        let (a, b, c) = (addr(1), addr(2), addr(3));
        let (cheap, rich) = (addr(90), addr(91));
        let mut g = Graph::default();
        // Two pools serve a->b. The graph prefers `rich` on rate.
        let mut e = hop_edge(a, b);
        if let VenueEdge::UniV3 { ref mut pool, .. } = e.venue {
            *pool = cheap;
        }
        g.add_edge(e);
        let mut e = hop_edge(a, b);
        if let VenueEdge::UniV3 { ref mut pool, .. } = e.venue {
            *pool = rich;
        }
        e.rate_num = U256::from(100u64);
        g.add_edge(e);
        for (f, t) in [(b, c), (c, a)] {
            let mut e = hop_edge(f, t);
            if let VenueEdge::UniV3 { ref mut pool, .. } = e.venue {
                *pool = cheap;
            }
            g.add_edge(e);
        }

        // Asked for `cheap`, we must get the edge on `cheap` -- not the edge
        // the graph would have chosen for itself.
        let ic = g
            .indexed_cycle_for_pools(&[a, b, c], &[cheap, cheap, cheap])
            .expect("every hop has an edge on `cheap`");
        let chosen = g.edges[ic.edge_indices[0]].venue.pool_address();
        assert_eq!(chosen, Some(cheap), "the priced pool must be the executed pool");

        // And the graph's own preference really would have differed, or this
        // test proves nothing.
        let free = g
            .indexed_cycle_for_tokens(&[a, b, c])
            .expect("translatable");
        assert_eq!(
            g.edges[free.edge_indices[0]].venue.pool_address(),
            Some(rich),
            "premise: left to itself the graph picks the other pool"
        );
    }

    /// A pool the graph cannot execute through is a REFUSAL, not an invitation
    /// to substitute. The caller priced something this graph cannot trade.
    #[test]
    fn a_route_through_an_unknown_pool_is_refused() {
        let (g, a, b, c) = closed_triangle();
        assert!(
            g.indexed_cycle_for_pools(&[a, b, c], &[addr(777), addr(777), addr(777)])
                .is_none(),
            "no edge uses that pool, so there is no executable route"
        );
        assert!(
            g.indexed_cycle_for_pools(&[a, b, c], &[Address::zero()]).is_none(),
            "one pool for a three-hop loop is not a route"
        );
    }

    /// The boundary the flashblock fast path hands cycles across. `CycleIndex`
    /// stores ADDRESSES in OPEN form on purpose -- node indices are assigned in
    /// first-seen order and the graph is rebuilt every scan, so an index-keyed
    /// cycle silently repoints after a rebuild.
    ///
    /// The loop must be CLOSED here. An open path builds a plan one hop short,
    /// which ends holding the wrong token.
    #[test]
    fn a_token_loop_translates_to_a_closed_indexed_cycle() {
        let (g, a, b, c) = closed_triangle();
        let ic = g
            .indexed_cycle_for_tokens(&[a, b, c])
            .expect("triangle is fully connected");
        assert_eq!(ic.hops(), 3, "three hops: a->b, b->c, c->a");
        assert_eq!(ic.cycle.len(), 4, "closed form carries the return to start");
        assert_eq!(
            ic.cycle.first(),
            ic.cycle.last(),
            "the path must return to where it began"
        );
        assert_eq!(ic.edge_indices.len(), 3);
        assert!(ic.edge_indices_valid());
    }

    /// A partial translation is worse than none: a plan whose steps do not
    /// compose would be built and only fail at execution.
    #[test]
    fn a_missing_hop_or_token_refuses_to_translate() {
        let (mut g, a, b, c) = closed_triangle();
        // A token the graph has never seen.
        assert!(g.indexed_cycle_for_tokens(&[a, b, addr(99)]).is_none());
        // A hop with no edge: add an isolated node.
        let d = addr(4);
        g.add_node(d);
        assert!(
            g.indexed_cycle_for_tokens(&[a, b, c, d]).is_none(),
            "d has no edges; the loop cannot close"
        );
        assert!(g.indexed_cycle_for_tokens(&[a]).is_none(), "one token is not a loop");
    }

    /// The rescue path in `main.rs` re-anchors an unfundable cycle onto a
    /// fundable node in the same loop. It relies on rotation preserving the
    /// trade exactly — same hops, same edges, same order — so a rotated cycle
    /// must be the identical trade entered at a different point.
    #[test]
    fn rotation_preserves_the_trade_when_re_anchoring() {
        // closed 3-cycle 10 -> 11 -> 12 -> 10, edges [0,1,2]
        let cycle = vec![10usize, 11, 12, 10];
        let edges = vec![0usize, 1, 2];

        let at_11 = rotate_indexed_cycle(&cycle, &edges, 11).expect("rotate to 11");
        assert_eq!(at_11.cycle, vec![11, 12, 10, 11], "loop re-entered at 11");
        assert_eq!(at_11.edge_indices, vec![1, 2, 0], "edges follow the nodes");

        let at_12 = rotate_indexed_cycle(&cycle, &edges, 12).expect("rotate to 12");
        assert_eq!(at_12.cycle, vec![12, 10, 11, 12]);
        assert_eq!(at_12.edge_indices, vec![2, 0, 1]);

        // Hop count and edge multiset are invariant — it is the same trade.
        for rotated in [&at_11, &at_12] {
            assert_eq!(rotated.cycle.len(), cycle.len());
            let mut got = rotated.edge_indices.clone();
            got.sort();
            assert_eq!(got, edges, "rotation must not add or drop a hop");
        }
    }

    #[test]
    fn rotation_refuses_a_node_outside_the_cycle() {
        // Guards the rescue loop: a start that is not in the loop must yield
        // None rather than a silently malformed path.
        let cycle = vec![10usize, 11, 12, 10];
        let edges = vec![0usize, 1, 2];
        assert!(rotate_indexed_cycle(&cycle, &edges, 99).is_none());
        // Arity mismatch between nodes and edges is likewise unrecoverable.
        assert!(rotate_indexed_cycle(&cycle, &[0usize, 1], 11).is_none());
    }

    fn fp_weight(num: u64, den: u64) -> i64 {
        compute_edge_weight(U256::from(num), U256::from(den))
    }

    fn limits(max_hops: usize) -> BellmanFordLimits {
        BellmanFordLimits {
            min_hops: 2,
            max_hops,
            max_relaxations: max_hops,
            max_cycles: 16,
            timeout: Duration::from_secs(60),
        }
    }

    /// Edge carrying only what `cycle_input_capacity` reads: a rate and a cap.
    fn cap_edge(from: Address, to: Address, num: u64, den: u64, max_input: U256) -> Edge {
        Edge {
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
            max_input,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        }
    }

    /// The regression this whole function exists for. WETH(18dp) -> USDC(6dp) ->
    /// WETH: the USDC hop's cap is ~19.2 USDC (1.92e7 raw), which the old
    /// `min()` fold compared directly against a 1e15 wei probe and "won",
    /// yielding a 1.9e-11 WETH ceiling. Projected through the rate it is worth
    /// ~0.0096 WETH — nine orders of magnitude apart.
    #[test]
    fn cycle_capacity_projects_caps_across_token_decimals() {
        let weth = addr(1);
        let usdc = addr(2);
        let probe = U256::from(1_000_000_000_000_000u64); // 0.001 WETH

        // 0.001 WETH -> 2 USDC, so the pair trades at 2000 USDC/WETH.
        let hop0 = cap_edge(weth, usdc, 2_000_000, 1_000_000_000_000_000, U256::MAX);
        // Cap of 19.226910 USDC on the return leg.
        let hop1 = cap_edge(usdc, weth, 1_000_000_000_000_000, 2_000_000, U256::from(19_226_910u64));

        let capacity = cycle_input_capacity(&[hop0, hop1], probe);

        // 19.22691 USDC / 2000 USDC-per-WETH = 0.009613455 WETH.
        assert_eq!(capacity, U256::from(9_613_455_000_000_000u64));
        // The bug produced the raw USDC integer as if it were wei.
        assert_ne!(capacity, U256::from(19_226_910u64));
        // And it must clear the 0.001 WETH minimum that used to reject it.
        assert!(capacity > probe);
    }

    #[test]
    fn cycle_capacity_takes_the_tightest_projected_hop() {
        let a = addr(1);
        let b = addr(2);
        // Identity rates keep the projection 1:1 so the tightest cap wins outright.
        let loose = cap_edge(a, b, 1, 1, U256::from(900u64));
        let tight = cap_edge(b, a, 1, 1, U256::from(100u64));
        let probe = U256::from(50u64);

        assert_eq!(
            cycle_input_capacity(&[loose, tight], probe),
            U256::from(100u64)
        );
    }

    #[test]
    fn cycle_capacity_first_hop_cap_is_used_verbatim() {
        let a = addr(1);
        let b = addr(2);
        // Hop 0 is already in start-token units: no projection should occur,
        // regardless of how extreme the rate on that hop is.
        let hop0 = cap_edge(a, b, 1_000_000, 1, U256::from(7u64));
        let hop1 = cap_edge(b, a, 1, 1_000_000, U256::MAX);

        assert_eq!(cycle_input_capacity(&[hop0, hop1], U256::from(5u64)), U256::from(7u64));
    }

    #[test]
    fn cycle_capacity_degenerate_inputs_are_zero() {
        let a = addr(1);
        let b = addr(2);
        let edge = cap_edge(a, b, 1, 1, U256::from(10u64));

        assert!(cycle_input_capacity(&[], U256::from(5u64)).is_zero());
        assert!(cycle_input_capacity(std::slice::from_ref(&edge), U256::zero()).is_zero());

        // A hop that outputs nothing kills the cycle rather than dividing by zero.
        let dead = cap_edge(b, a, 0, 1, U256::MAX);
        assert!(cycle_input_capacity(&[edge, dead, cap_edge(a, b, 1, 1, U256::MAX)], U256::from(5u64)).is_zero());
    }

    /// A multi-hop cycle across a decimals gap must not have its capacity
    /// floored to zero by the fold's own truncation.
    ///
    /// Measured on Base: 18/18 zero-capacity events came from this path at
    /// hops=4 and hops=5, on WETH/USDC/cbBTC/cbETH — the four deepest pairs on
    /// the chain. `no_liquidity` was the largest rejection reason (401/490) and
    /// none of it was missing liquidity.
    ///
    /// Shape below: WETH(18dp) -> USDC(6dp) -> cbBTC(8dp) -> WETH, i.e. rates
    /// that swing across twelve orders of magnitude, which is exactly where the
    /// sequential `mul_div_floor` fold loses the value.
    #[test]
    fn cycle_capacity_survives_a_cross_decimal_multi_hop() {
        fn hop(from: Address, to: Address, num: u128, den: u128, max_input: u128) -> Edge {
            Edge {
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
                weight: -1,
                max_input: U256::from(max_input),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,
                active: true,
                tick_ladder: None,
            }
        }
        let weth = addr(1);
        let usdc = addr(2);
        let cbbtc = addr(3);

        // 1 WETH (1e18) -> 3600 USDC (3.6e9)  => 3.6e9 / 1e18
        // 1 USDC (1e6)  -> 1/95000 cbBTC      => 1.05e3 / 1e6  (8dp cbBTC)
        // 1 cbBTC (1e8) -> 26.4 WETH          => 2.64e19 / 1e8
        let edges = vec![
            hop(weth, usdc, 3_600_000_000, 1_000_000_000_000_000_000, 50_000_000_000),
            hop(usdc, cbbtc, 1_052, 1_000_000, 10_000_000_000),
            hop(cbbtc, weth, 26_400_000_000_000_000_000, 100_000_000, 500_000_000),
        ];

        let probe = U256::exp10(19); // 20 WETH, the probe seen in the live logs
        let capacity = cycle_input_capacity(&edges, probe);

        assert!(
            !capacity.is_zero(),
            "a cross-decimal multi-hop cycle with real per-hop depth must report \
             non-zero capacity; zero here is the arithmetic underflow, not liquidity"
        );

        // And the exact path must be the one answering — not the fallback.
        assert!(
            cycle_input_capacity_exact(&edges).is_some_and(|c| !c.is_zero()),
            "exact U512 projection must resolve this cycle without falling back"
        );
    }

    #[test]
    fn mul_div_floor_saturates_instead_of_wrapping() {
        assert_eq!(mul_div_floor(U256::MAX, U256::from(2u64), U256::one()), U256::MAX);
        assert_eq!(mul_div_floor(U256::from(10u64), U256::from(3u64), U256::from(4u64)), U256::from(7u64));
        assert!(mul_div_floor(U256::from(1u64), U256::from(1u64), U256::zero()).is_zero());
    }

    fn hub_limits(max_hops: usize) -> HubSearchLimits {
        HubSearchLimits {
            min_hops: 2,
            max_hops,
            max_cycles: 64,
            timeout: Duration::from_secs(5),
            parallel_edges_per_pair: 3,
        }
    }

    /// Edge on an identifiable pool, so `edge_pool_key` distinguishes parallel pools.
    fn pool_edge(from: Address, to: Address, num: u64, den: u64, pool: u64) -> Edge {
        Edge {
            from,
            to,
            rate_num: U256::from(num),
            rate_den: U256::from(den),
            venue: VenueEdge::UniV3 {
                path: vec![(from, None), (to, Some(500))],
                pool: addr(pool),
                fee: 500,
                state: None,
            },
            estimated_gas: 0,
            weight: fp_weight(num, den),
            max_input: U256::MAX,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        }
    }

    fn graph_with(edges: Vec<Edge>) -> Graph {
        let mut graph = Graph::default();
        for edge in edges {
            graph.add_node(edge.from);
            graph.add_node(edge.to);
            graph.add_edge(edge);
        }
        graph
    }

    #[test]
    fn hub_search_finds_a_profitable_two_hop_cycle() {
        let hub = addr(1);
        let mid = addr(2);
        // 1 hub -> 3 mid, then 1 mid -> 0.5 hub  =>  1 -> 1.5 hub. Profitable.
        let graph = graph_with(vec![
            pool_edge(hub, mid, 3, 1, 901),
            pool_edge(mid, hub, 1, 2, 902),
        ]);

        let found = graph.hub_anchored_cycles(&[hub], &hub_limits(3), 10);

        assert_eq!(found.len(), 1, "expected exactly one canonical cycle");
        assert_eq!(found[0].start, hub);
        assert_eq!(found[0].edge_indices.len(), 2);
        assert!(found[0].estimated_profit_bps > 0);
    }

    #[test]
    fn hub_search_rejects_a_same_pool_round_trip() {
        let hub = addr(1);
        let mid = addr(2);
        // Both legs on pool 901: a round trip through one pool cannot profit,
        // however good the quoted rates look.
        let graph = graph_with(vec![
            pool_edge(hub, mid, 3, 1, 901),
            pool_edge(mid, hub, 3, 1, 901),
        ]);

        assert!(graph
            .hub_anchored_cycles(&[hub], &hub_limits(3), 10)
            .is_empty());
    }

    #[test]
    fn hub_search_ignores_losing_cycles() {
        let hub = addr(1);
        let mid = addr(2);
        // 1 -> 0.5 -> 0.5: round trip loses.
        let graph = graph_with(vec![
            pool_edge(hub, mid, 1, 2, 901),
            pool_edge(mid, hub, 1, 1, 902),
        ]);

        assert!(graph
            .hub_anchored_cycles(&[hub], &hub_limits(3), 10)
            .is_empty());
    }

    #[test]
    fn hub_search_emits_each_pool_combination_once() {
        let hub = addr(1);
        let mid = addr(2);
        // Two parallel return pools => two distinct, legitimate cycles.
        let graph = graph_with(vec![
            pool_edge(hub, mid, 3, 1, 901),
            pool_edge(mid, hub, 1, 2, 902),
            pool_edge(mid, hub, 2, 3, 903),
        ]);

        let found = graph.hub_anchored_cycles(&[hub], &hub_limits(3), 10);

        assert_eq!(found.len(), 2);
        let mut keys: Vec<Vec<usize>> = found.iter().map(|c| c.edge_indices.clone()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 2, "cycles must be distinct, not duplicates");
        // Sorted best-first.
        assert!(found[0].estimated_profit_bps >= found[1].estimated_profit_bps);
    }

    #[test]
    fn hub_search_dominance_pruning_keeps_only_the_best_parallel_edges() {
        let hub = addr(1);
        let mid = addr(2);
        let mut edges = vec![pool_edge(hub, mid, 3, 1, 901)];
        // Five parallel return pools with descending rates.
        for i in 0..5u64 {
            edges.push(pool_edge(mid, hub, 10 - i, 20, 910 + i));
        }
        let graph = graph_with(edges);

        let mut limits = hub_limits(3);
        limits.parallel_edges_per_pair = 2;
        let found = graph.hub_anchored_cycles(&[hub], &limits, 10);

        assert_eq!(found.len(), 2, "only the top 2 parallel pools survive");
    }

    #[test]
    fn hub_search_finds_three_hop_cycles_and_respects_max_hops() {
        let hub = addr(1);
        let a = addr(2);
        let b = addr(3);
        // hub -> a -> b -> hub, each leg 2x: strongly profitable, 3 hops.
        let edges = vec![
            pool_edge(hub, a, 2, 1, 901),
            pool_edge(a, b, 2, 1, 902),
            pool_edge(b, hub, 2, 1, 903),
        ];
        let graph = graph_with(edges);

        assert_eq!(graph.hub_anchored_cycles(&[hub], &hub_limits(3), 10).len(), 1);
        // With max_hops = 2 the same cycle is out of reach.
        assert!(graph
            .hub_anchored_cycles(&[hub], &hub_limits(2), 10)
            .is_empty());
    }

    /// Branch-and-bound must never discard a genuinely winning cycle. Here the
    /// first hop is a heavy loss and only the final hop recovers it, which is
    /// exactly the shape a too-tight bound would prune.
    #[test]
    fn hub_search_bound_does_not_prune_a_late_winning_cycle() {
        let hub = addr(1);
        let a = addr(2);
        let b = addr(3);
        let graph = graph_with(vec![
            pool_edge(hub, a, 1, 10, 901), // 0.1x
            pool_edge(a, b, 1, 1, 902),    // 1.0x
            pool_edge(b, hub, 30, 1, 903), // 30x  => net 3x
        ]);

        let found = graph.hub_anchored_cycles(&[hub], &hub_limits(3), 10);
        assert_eq!(found.len(), 1, "profitable cycle must survive the bound");
        assert!(found[0].estimated_profit_bps > 0);
    }

    #[test]
    fn hub_search_walks_every_hub() {
        let hub_a = addr(1);
        let hub_b = addr(2);
        let mid = addr(3);
        let graph = graph_with(vec![
            pool_edge(hub_a, mid, 3, 1, 901),
            pool_edge(mid, hub_a, 1, 2, 902),
            pool_edge(hub_b, mid, 3, 1, 903),
            pool_edge(mid, hub_b, 1, 2, 904),
        ]);

        let found = graph.hub_anchored_cycles(&[hub_a, hub_b], &hub_limits(3), 10);
        let starts: HashSet<Address> = found.iter().map(|c| c.start).collect();
        assert_eq!(starts.len(), 2, "both hubs should yield a cycle");
    }

    #[test]
    fn hub_search_degenerate_inputs_are_empty() {
        let hub = addr(1);
        let mid = addr(2);
        let graph = graph_with(vec![
            pool_edge(hub, mid, 3, 1, 901),
            pool_edge(mid, hub, 1, 2, 902),
        ]);

        assert!(graph.hub_anchored_cycles(&[], &hub_limits(3), 10).is_empty());
        assert!(graph.hub_anchored_cycles(&[hub], &hub_limits(3), 0).is_empty());
        // A hub that is not a graph node yields nothing rather than panicking.
        assert!(graph
            .hub_anchored_cycles(&[addr(99)], &hub_limits(3), 10)
            .is_empty());
    }

    #[test]
    fn sanitized_limits_do_not_clamp_relaxations_to_hops() {
        let limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 3,
            max_relaxations: 8,
            max_cycles: 4,
            timeout: Duration::from_secs(60),
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
                tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
                tick_ladder: None,
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
                state: None,
            },
            estimated_gas: 0,
            weight: 0,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
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
                state: None,
            },
            estimated_gas: 0,
            weight: -1,
            max_input: U256::from(1_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
                tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
        });

        let priorities = HashMap::from([(source, 1)]);
        let constrained_limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 4,
            max_relaxations: 8,
            max_cycles: 2,
            timeout: Duration::from_secs(60),
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
        });

        let mut priorities = HashMap::new();
        priorities.insert(origin, 1);
        let cycles = graph.bellman_ford(&priorities, &limits(1), 2, None);
        assert!(cycles.is_empty());
    }

    #[test]
    fn fresher_quote_wins_even_when_the_stale_one_looks_better() {
        // Same pool, same direction, two blocks. The older quote is strictly
        // more favourable — which is exactly the case that must NOT win, since
        // it prices a trade at a rate that no longer exists.
        let a = addr(1);
        let b = addr(2);
        let pool = addr(100);

        let mut stale = cap_edge(a, b, 3, 1, U256::from(1_000u64));
        stale.venue = VenueEdge::UniV3 {
            path: Default::default(),
            pool,
            fee: 3000,
            state: None,
        };
        stale.quote_block = Some(U64::from(100u64));
        stale.weight = -500;

        let mut fresh = stale.clone();
        fresh.quote_block = Some(U64::from(102u64));
        fresh.weight = -100; // worse price, but current

        assert!(
            is_better(&fresh, &stale),
            "the fresher quote must win despite the worse rate"
        );
        assert!(
            !is_better(&stale, &fresh),
            "and the stale one must never displace it"
        );
    }

    /// The same token loop through different pools is two routes, not one.
    ///
    /// Deduplicating on the token sequence alone discarded the second, and
    /// parallel venues on a pair are exactly the dislocation this bot exists to
    /// find -- so the old key threw away the candidates most worth having.
    #[test]
    fn route_dedup_separates_parallel_venues_but_folds_rotations() {
        let loop_nodes = vec![0usize, 1, 2, 0];
        let via_a = vec![10usize, 11, 12];
        let via_b = vec![20usize, 21, 22];

        assert_ne!(
            canonicalize_route(&loop_nodes, &via_a),
            canonicalize_route(&loop_nodes, &via_b),
            "same tokens, different pools must not collapse"
        );

        // The same route entered at a different token is still ONE route, so
        // the edges rotate with the nodes.
        let rotated_nodes = vec![1usize, 2, 0, 1];
        let rotated_edges = vec![11usize, 12, 10];
        assert_eq!(
            canonicalize_route(&loop_nodes, &via_a),
            canonicalize_route(&rotated_nodes, &rotated_edges),
            "a rotation of one route is the same route"
        );

        // Same pools in a different ORDER is a different route.
        assert_ne!(
            canonicalize_route(&loop_nodes, &via_a),
            canonicalize_route(&loop_nodes, &[12usize, 11, 10]),
        );

        // A mismatched edge count degrades to the node key rather than
        // inventing an alignment between hops and edges.
        let (nodes, edges) = canonicalize_route(&loop_nodes, &[10usize]);
        assert!(edges.is_empty());
        assert_eq!(nodes, canonicalize_cycle(loop_nodes));
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
                tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
        });

        let mut priorities = HashMap::new();
        priorities.insert(a, 1);

        let tight_limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 3,
            max_relaxations: 8,
            max_cycles: 4,
            timeout: Duration::from_secs(60),
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
                tick_ladder: None,
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
                tick_ladder: None,
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
                tick_ladder: None,
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
    fn cycles_are_found_by_rate_alone_regardless_of_gas() {
        // The regression this locks in. Previously the edge weight folded in
        // `gas_cost / base_amount_in`, so a cycle whose gas exceeded its gross
        // edge at one arbitrary probe notional produced all-positive weights —
        // no negative cycle existed and the search never proposed it at ANY
        // size. The test covering that was literally named
        // `gas_penalties_can_remove_profitable_cycles`.
        //
        // Weights are rate-only now, so discovery depends solely on
        // `prod rate_i > 1`, which is size-independent and true or false at
        // every notional. Gas is charged once, exactly, by Stage 2.
        let mut graph = Graph::default();
        let a = addr(100);
        let b = addr(101);
        let c = addr(102);

        for (from, to) in [(a, b), (b, c), (c, a)] {
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
                // A deliberately huge per-swap gas figure: it must have NO
                // bearing on whether the cycle is discoverable.
                estimated_gas: 5_000_000,
                weight: compute_edge_weight(
                    crate::util::apply_slippage(U256::from(105u64), 10),
                    U256::from(100u64),
                ),
                max_input: U256::from(2_000_000u64),
                tolerance_bps: 10,
                observed_slippage_bps: 10,
                quote_block: None,
                active: true,
                tick_ladder: None,
            });
        }

        let priorities = HashMap::new();
        let cycles = graph.bellman_ford(&priorities, &limits(3), 1, None);
        assert!(
            !cycles.is_empty(),
            "a cycle with a positive gross edge must be discoverable no matter \
             how large the gas estimate is"
        );
        assert!(
            cycles.iter().any(|c| c.weight < 0),
            "the gaining cycle must carry negative summed weight"
        );
        assert!(
            cycles.iter().all(|c| c.estimated_profit_bps > 0),
            "every admitted cycle must have a positive size-independent gross edge"
        );
    }

    #[test]
    fn diagnostic_reports_distance_to_profit_for_rejected_cycles() {
        // The point of the diagnostic: a losing cycle is REJECTED, but the
        // search still reports how far from profitable it was. Three hops at
        // 0.99 compound to ~-2.97%, i.e. roughly -297 bps of gross edge.
        let mut graph = Graph::default();
        let a = addr(300);
        let b = addr(301);
        let c = addr(302);
        for (from, to) in [(a, b), (b, c), (c, a)] {
            graph.add_edge(Edge {
                from,
                to,
                rate_num: U256::from(99u64),
                rate_den: U256::from(100u64),
                venue: VenueEdge::Balancer {
                    pool_id: [9u8; 32],
                    token_in: from,
                    token_out: to,
                },
                estimated_gas: 0,
                weight: compute_edge_weight(U256::from(99u64), U256::from(100u64)),
                max_input: U256::from(2_000_000u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,
                active: true,
                tick_ladder: None,
            });
        }

        let priorities = HashMap::new();
        let (cycles, best) = graph.bellman_ford_diagnostic(&priorities, &limits(3), 1, None);
        assert!(cycles.is_empty(), "a losing cycle must still be rejected");

        // A losing cycle may or may not be reached by the negative-cycle search;
        // when it is, the reported distance must be negative and sane.
        if let Some(scaled) = best {
            let bps = (((scaled as f64) / WEIGHT_SCALE as f64).exp() - 1.0) * 10_000.0;
            assert!(
                bps < 0.0,
                "a rejected cycle must report a negative gross edge, got {bps}"
            );
            assert!(
                bps > -10_000.0,
                "distance must be a sane bps figure, got {bps}"
            );
        }
    }

    #[test]
    fn diagnostic_reports_positive_distance_for_a_winning_cycle() {
        let mut graph = Graph::default();
        let a = addr(400);
        let b = addr(401);
        let c = addr(402);
        for (from, to) in [(a, b), (b, c), (c, a)] {
            graph.add_edge(Edge {
                from,
                to,
                rate_num: U256::from(105u64),
                rate_den: U256::from(100u64),
                venue: VenueEdge::Balancer {
                    pool_id: [4u8; 32],
                    token_in: from,
                    token_out: to,
                },
                estimated_gas: 0,
                weight: compute_edge_weight(U256::from(105u64), U256::from(100u64)),
                max_input: U256::from(2_000_000u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,
                active: true,
                tick_ladder: None,
            });
        }
        let priorities = HashMap::new();
        let (cycles, best) = graph.bellman_ford_diagnostic(&priorities, &limits(3), 1, None);
        assert!(!cycles.is_empty(), "a gaining cycle must be found");
        let scaled = best.expect("a considered cycle must report a gross edge");
        let bps = (((scaled as f64) / WEIGHT_SCALE as f64).exp() - 1.0) * 10_000.0;
        // 1.05^3 - 1 = 15.76% = ~1576 bps
        assert!(
            bps > 1_000.0,
            "1.05^3 should report ~1576 bps of gross edge, got {bps}"
        );
    }

    #[test]
    fn edge_indices_do_not_survive_a_graph_rebuild() {
        // The bug this guards: a cycle seed carried across scans kept its
        // edge_indices, but those are positions in a specific Graph::edges. The
        // graph is rebuilt every scan and repopulated in a different order, so
        // the stored index denoted a DIFFERENT edge — failing the
        // `edge.from != u` check in candidate prep and silently discarding a
        // profitable cycle. Seeds must re-resolve from the node path.
        fn build(order: &[(u64, u64)]) -> Graph {
            let mut g = Graph::default();
            for (from, to) in order {
                g.add_edge(Edge {
                    from: addr(*from),
                    to: addr(*to),
                    rate_num: U256::from(101u64),
                    rate_den: U256::from(100u64),
                    venue: VenueEdge::Balancer {
                        pool_id: [(*from as u8); 32],
                        token_in: addr(*from),
                        token_out: addr(*to),
                    },
                    estimated_gas: 0,
                    weight: compute_edge_weight(U256::from(101u64), U256::from(100u64)),
                    max_input: U256::from(1_000_000u64),
                    tolerance_bps: 0,
                    observed_slippage_bps: 0,
                    quote_block: None,
                    active: true,
                    tick_ladder: None,
                });
            }
            g
        }

        // Same three hops, inserted in two different orders — as concurrent
        // venue collectors genuinely do between scans.
        let g1 = build(&[(1, 2), (2, 3), (3, 1)]);
        let g2 = build(&[(3, 1), (1, 2), (2, 3)]);

        let path: Vec<usize> = [1u64, 2, 3, 1]
            .iter()
            .map(|t| g1.ix[&addr(*t)])
            .collect();

        let idx1 = g1
            .best_edge_indices_for_node_path(&path)
            .expect("resolvable in g1");
        let path2: Vec<usize> = [1u64, 2, 3, 1]
            .iter()
            .map(|t| g2.ix[&addr(*t)])
            .collect();
        let idx2 = g2
            .best_edge_indices_for_node_path(&path2)
            .expect("resolvable in g2");

        // The indices genuinely differ between graphs — this is what made
        // carrying them forward unsound.
        assert_ne!(
            idx1, idx2,
            "insertion order must change edge indices, else this test proves nothing"
        );

        // Re-resolved indices must match the hops they claim to represent.
        for (hop, window) in path2.windows(2).enumerate() {
            let edge = g2.edge_by_index(idx2[hop]).expect("edge exists");
            assert_eq!(edge.from, g2.nodes[window[0]], "hop {hop} from-token");
            assert_eq!(edge.to, g2.nodes[window[1]], "hop {hop} to-token");
        }

        // And the stale indices would NOT have matched — the actual failure.
        let mut stale_mismatch = false;
        for (hop, window) in path2.windows(2).enumerate() {
            if let Some(edge) = g2.edge_by_index(idx1[hop]) {
                if edge.from != g2.nodes[window[0]] || edge.to != g2.nodes[window[1]] {
                    stale_mismatch = true;
                }
            }
        }
        assert!(
            stale_mismatch,
            "carrying g1's indices into g2 must mismatch at least one hop"
        );
    }

    fn two_hop_edge(from: u64, to: u64, num: u64, den: u64, pool: u8) -> Edge {
        Edge {
            from: addr(from),
            to: addr(to),
            rate_num: U256::from(num),
            rate_den: U256::from(den),
            venue: VenueEdge::UniV3 {
                path: Vec::new(),
                pool: Address::from_low_u64_be(pool as u64),
                fee: 500,
                state: None,
            },
            estimated_gas: 0,
            weight: compute_edge_weight(U256::from(num), U256::from(den)),
            max_input: U256::from(1_000_000u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        }
    }

    #[test]
    fn two_hop_probe_reports_a_losing_round_trip() {
        // THE point of this diagnostic: report distance to profit even when the
        // negative-cycle search surfaces nothing. 0.999 * 0.999 = 0.998001,
        // i.e. -19.99 bps.
        let mut g = Graph::default();
        g.add_edge(two_hop_edge(1, 2, 999, 1000, 1));
        g.add_edge(two_hop_edge(2, 1, 999, 1000, 2));

        let probe = g.best_two_hop_roundtrip().expect("a closed 2-hop exists");
        assert!(
            (probe.best_bps - (-19.99)).abs() < 0.1,
            "expected ~-19.99 bps, got {}",
            probe.best_bps
        );
        assert!(probe.cross_pool, "different pools must read as cross-pool");

        // And the search itself surfaces nothing, which is exactly the blind
        // spot this probe covers.
        let (cycles, best) = g.bellman_ford_diagnostic(&HashMap::new(), &limits(2), 4, None);
        assert!(cycles.is_empty());
        assert!(best.is_none(), "search reports nothing; the probe must not");
    }

    #[test]
    fn two_hop_probe_reports_a_winning_round_trip_and_picks_the_best_pool() {
        // 1.01 * 1.00 = +100 bps. A worse parallel edge must not win.
        let mut g = Graph::default();
        g.add_edge(two_hop_edge(1, 2, 101, 100, 1));
        g.add_edge(two_hop_edge(1, 2, 90, 100, 3)); // worse parallel edge
        g.add_edge(two_hop_edge(2, 1, 100, 100, 2));

        let probe = g.best_two_hop_roundtrip().expect("closed 2-hop exists");
        assert!(
            (probe.best_bps - 100.0).abs() < 0.5,
            "expected ~+100 bps from the BEST parallel edge, got {}",
            probe.best_bps
        );
    }

    #[test]
    fn two_hop_probe_flags_a_same_pool_round_trip() {
        // Both legs on one pool is not a real arbitrage route; the caller needs
        // to know the "best" route was degenerate.
        let mut g = Graph::default();
        g.add_edge(two_hop_edge(1, 2, 999, 1000, 7));
        g.add_edge(two_hop_edge(2, 1, 999, 1000, 7));
        let probe = g.best_two_hop_roundtrip().expect("closed 2-hop exists");
        assert!(!probe.cross_pool, "same pool on both legs must be flagged");
    }

    #[test]
    fn two_hop_probe_returns_none_without_a_return_leg() {
        // One-way edges only: no closed route exists at all. Distinct from
        // "a route exists and loses".
        let mut g = Graph::default();
        g.add_edge(two_hop_edge(1, 2, 101, 100, 1));
        g.add_edge(two_hop_edge(2, 3, 101, 100, 2));
        assert!(g.best_two_hop_roundtrip().is_none());
    }

    #[test]
    fn two_hop_probe_ignores_inactive_edges() {
        let mut g = Graph::default();
        g.add_edge(two_hop_edge(1, 2, 101, 100, 1));
        let mut back = two_hop_edge(2, 1, 101, 100, 2);
        back.active = false;
        g.add_edge(back);
        assert!(
            g.best_two_hop_roundtrip().is_none(),
            "a deactivated return leg is not a tradable route"
        );
    }

    #[test]
    fn detection_haircut_is_independent_of_execution_tolerance() {
        // The separation this guards. `tolerance_bps` is the EXECUTION min_out
        // margin (plan.rs); it must no longer shrink the rate the DETECTOR sees.
        // Previously both read the same field, so raising revert protection
        // silently raised the bar detection had to clear — 30bps per leg became
        // a ~60bps bar on a 2-hop round trip, measured as -61.3bps of apparent
        // loss on a market that was really about -11bps.
        std::env::remove_var("DETECTION_HAIRCUT_BPS"); // default 0

        let mut a = Graph::default();
        a.add_edge(two_hop_edge(1, 2, 1000, 1000, 1));
        a.add_edge(two_hop_edge(2, 1, 1000, 1000, 2));

        // Same rates, but a large EXECUTION tolerance on every edge.
        let mut b = Graph::default();
        for (f, t, pool) in [(1u64, 2u64, 1u8), (2, 1, 2)] {
            let mut e = two_hop_edge(f, t, 1000, 1000, pool);
            e.tolerance_bps = 300; // 3% execution margin
            b.add_edge(e);
        }

        let pa = a.best_two_hop_roundtrip().expect("route exists");
        let pb = b.best_two_hop_roundtrip().expect("route exists");
        assert!(
            (pa.best_bps - pb.best_bps).abs() < 1e-6,
            "execution tolerance must not move the detection measurement: {} vs {}",
            pa.best_bps,
            pb.best_bps
        );
        assert!(
            pa.best_bps.abs() < 1e-6,
            "rate 1.0 both legs with a zero detection haircut is exactly break-even, got {}",
            pa.best_bps
        );
    }

    #[test]
    fn detection_haircut_still_applies_when_configured() {
        // Opting in must still work — it is a recall/cost dial, just no longer
        // welded to the execution margin.
        std::env::set_var("DETECTION_HAIRCUT_BPS", "25");
        // OnceLock means the value may already be fixed by another test in this
        // binary; only assert when this process actually observes 25.
        if crate::util::detection_haircut_bps() == 25 {
            let mut g = Graph::default();
            g.add_edge(two_hop_edge(1, 2, 1000, 1000, 1));
            g.add_edge(two_hop_edge(2, 1, 1000, 1000, 2));
            let p = g.best_two_hop_roundtrip().expect("route exists");
            // Two legs haircut 25bps each => ~-50bps.
            assert!(
                (p.best_bps - (-49.94)).abs() < 1.0,
                "expected ~-50bps from 2x25bps haircut, got {}",
                p.best_bps
            );
        }
        std::env::remove_var("DETECTION_HAIRCUT_BPS");
    }

    #[test]
    fn losing_cycles_are_still_rejected() {
        // The other half: admission must still reject `prod rate_i <= 1`, where
        // no trade size can ever help.
        let mut graph = Graph::default();
        let a = addr(200);
        let b = addr(201);
        let c = addr(202);

        for (from, to) in [(a, b), (b, c), (c, a)] {
            graph.add_edge(Edge {
                from,
                to,
                rate_num: U256::from(99u64),
                rate_den: U256::from(100u64),
                venue: VenueEdge::Balancer {
                    pool_id: [7u8; 32],
                    token_in: from,
                    token_out: to,
                },
                estimated_gas: 0,
                weight: compute_edge_weight(U256::from(99u64), U256::from(100u64)),
                max_input: U256::from(2_000_000u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,
                active: true,
                tick_ladder: None,
            });
        }

        let priorities = HashMap::new();
        let cycles = graph.bellman_ford(&priorities, &limits(3), 1, None);
        assert!(
            cycles.is_empty(),
            "a cycle that loses value on rates alone must never be admitted"
        );
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
                state: None,
            },
            estimated_gas: 0,
            weight: fp_weight(2, 1),
            max_input: U256::from(5u64),
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
            tick_ladder: None,
        };

        let mut better = worse.clone();
        better.venue = VenueEdge::UniV3 {
            path: vec![(a, None), (b, Some(3000))],
            pool: Address::from_low_u64_be(1),
            fee: 3000,
            state: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
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
            tick_ladder: None,
        });

        let priorities = HashMap::new();
        let limits = BellmanFordLimits {
            min_hops: 2,
            max_hops: 2,
            max_relaxations: 8,
            max_cycles: 4,
            timeout: Duration::from_secs(60),
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
