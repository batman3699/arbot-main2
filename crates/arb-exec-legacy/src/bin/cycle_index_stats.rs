//! Size the precomputed cycle set against a real pool inventory.
//!
//! `CycleIndex` trades memory and one-off enumeration cost for per-scan speed,
//! so the numbers that decide whether it is viable are: how many cycles a real
//! graph produces at each hop depth, and how selective the hop index is when a
//! handful of pools move. Guessing those from the algorithm is unreliable —
//! measure them.
//!
//! Reads structure only (token pairs) straight from the pool inventory, so it
//! needs no RPC and no quotes.
//!
//! Usage:
//!   cargo run --bin cycle_index_stats -- data/base/uniswap_v3/pools.jsonl [max_hops]

use anyhow::{Context, Result};
use arb_exec::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
use arb_exec::graph::{Edge, Graph, VenueEdge};
use ethers::types::{Address, U256};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

/// Structure-only edge: enumeration reads `from`/`to` and the pool identity,
/// never rates, so synthetic values here cannot affect the result.
fn structural_edge(from: Address, to: Address, pool: Address) -> Edge {
    Edge {
        from,
        to,
        rate_num: U256::one(),
        rate_den: U256::one(),
        venue: VenueEdge::UniV2 {
            pair: pool,
            token_out: to,
            token0: from,
            token1: to,
            reserve_in: U256::one(),
            reserve_out: U256::one(),
            fee_bps: 30,
        },
        estimated_gas: 0,
        weight: 0,
        max_input: U256::MAX,
        tolerance_bps: 0,
        observed_slippage_bps: 0,
        quote_block: None,
        active: true,
        // Structure-only edge: no CL ladder needed for adjacency enumeration.
        tick_ladder: None,
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .unwrap_or_else(|| "data/base/uniswap_v3/pools.jsonl".to_string());
    let max_hops: usize = args
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
        .clamp(2, 6);

    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    let mut graph = Graph::default();
    let mut pool_count = 0usize;
    let mut tokens: HashSet<Address> = HashSet::new();

    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let (Some(p), Some(t0), Some(t1)) = (
            v.get("pool").and_then(|x| x.as_str()),
            v.get("token0").and_then(|x| x.as_str()),
            v.get("token1").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        let (Ok(pool), Ok(a), Ok(b)) = (
            Address::from_str(p),
            Address::from_str(t0),
            Address::from_str(t1),
        ) else {
            continue;
        };
        pool_count += 1;
        tokens.insert(a);
        tokens.insert(b);
        graph.add_edge(structural_edge(a, b, pool));
        graph.add_edge(structural_edge(b, a, pool));
    }

    // Flash-loanable starts: a cycle that cannot be funded cannot be traded, and
    // restricting starts is what keeps enumeration tractable.
    let starts: Vec<Address> = [
        "0x4200000000000000000000000000000000000006", // WETH
        "0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913", // USDC
        "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf", // cbBTC
        "0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22", // cbETH
    ]
    .iter()
    .filter_map(|s| Address::from_str(s).ok())
    .filter(|a| graph.ix.contains_key(a))
    .collect();

    // Structure comes from the inventory itself, not from realised edges.
    let universe = PoolUniverse::from_pools(
        graph
            .edges
            .iter()
            .filter_map(|e| arb_exec::venues::edge_pool_address(e).map(|p| (p, e.from, e.to))),
    );

    println!("inventory : {path}");
    println!("pools     : {pool_count}");
    println!("tokens    : {}", tokens.len());
    println!("edges     : {} (both directions)", graph.edges.len());
    println!("pairs     : {} distinct token pairs", universe.pair_count());
    println!("starts    : {} flash-loanable\n", starts.len());

    for hops in 2..=max_hops {
        let limits = CycleIndexLimits {
            max_hops: hops,
            min_hops: 2,
            max_cycles: 250_000,
        };
        let began = std::time::Instant::now();
        let idx = CycleIndex::build(&universe, &starts, limits);
        let build_ms = began.elapsed().as_millis();

        let mut by_len: HashMap<usize, usize> = HashMap::new();
        for c in idx.cycles() {
            *by_len.entry(c.hops()).or_default() += 1;
        }
        let mut dist: Vec<_> = by_len.into_iter().collect();
        dist.sort_unstable();
        let dist_s: Vec<String> = dist.iter().map(|(h, n)| format!("{h}h={n}")).collect();
        println!(
            "max_hops={hops}: {:>7} cycles  build={:>5}ms  [{}]{}",
            idx.len(),
            build_ms,
            dist_s.join(" "),
            if idx.truncated { "  TRUNCATED" } else { "" }
        );

        // Selectivity: the actual win. If one pool moves, what fraction of the
        // set needs re-pricing? Full Bellman-Ford re-prices everything.
        if !idx.is_empty() {
            let mut samples: Vec<f64> = Vec::new();
            for edge in graph.edges.iter().take(200) {
                let Some(pool) = arb_exec::venues::edge_pool_address(edge) else {
                    continue;
                };
                let touched =
                    idx.cycles_touching(universe.hops_for_pools(&HashSet::from([pool])));
                samples.push(touched.len() as f64 / idx.len() as f64 * 100.0);
            }
            if !samples.is_empty() {
                samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let med = samples[samples.len() / 2];
                let p90 = samples[(samples.len() as f64 * 0.9) as usize % samples.len()];
                println!(
                    "            one pool moves -> re-price median {med:.1}% / p90 {p90:.1}% of the set"
                );
            }
        }
    }
    Ok(())
}
