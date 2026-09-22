//! Convex-relaxation sizing over an active subgraph.
//!
//! The 1-D ternary search in `optimize_trade_size` optimises a single scalar
//! over one-pool-per-hop. It cannot express a SPLIT, so when a token pair exists
//! at several fee tiers simultaneously — WETH/USDC at 100/500/3000/10000 on Base
//! — it picks one and eats the full price impact of routing everything through
//! it.
//!
//! Maximum-profit arbitrage across a set of CFMMs is a single convex program
//! (Angeris/Chitra/Evans/Boyd, arXiv:2204.05238) solving routing, sizing and
//! splitting simultaneously. This module implements the tractable core of that
//! for a cycle plus its parallel pools.
//!
//! # Why no solver
//!
//! On a small active subgraph — a cycle plus parallel pools, roughly 5-20 edges
//! — the problem is small and highly structured. Each pool's exact-input curve
//! is concave and monotone in its input, so allocating a fixed hop input across
//! parallel pools to maximise total output is concave maximisation over a
//! simplex. Its optimum is characterised by MARGINAL RATE EQUALISATION: at the
//! optimum no unit of input can be moved between pools for a gain.
//!
//! Greedy incremental allocation reaches exactly that: hand each chunk to
//! whichever pool currently offers the best marginal output. For concave
//! per-pool curves this is optimal up to chunk granularity, needs no CVXPY-class
//! solver, and costs `chunks x pools` evaluations of local math — microseconds
//! at the sizes here.
//!
//! # Gas makes it mixed-integer, and that is not solved here
//!
//! Including per-pool gas turns optimal routing into a mixed-integer convex
//! problem — each pool touched is a fixed charge, which introduces
//! indicator/cardinality constraints and breaks convexity. The source is
//! explicit that this needs global optimisation or convex-based heuristics.
//!
//! So: solve the convex relaxation (fast, globally optimal ignoring gas), then
//! apply a cardinality prune, then re-solve on the chosen support. The exact
//! MICP is deliberately NOT attempted in-block.

// main.rs compiles its own copy of this module. Until the sizer calls into it,
// every item reads as dead there; the library and tests exercise them.
#![allow(dead_code)]

use ethers::types::U256;

/// Granularity of the greedy allocation.
///
/// The allocation is optimal up to one chunk, so error is bounded by the
/// marginal rate spread across `total_in / CHUNKS`. 64 puts that far below the
/// bps-scale edges this is deciding, while keeping evaluations at
/// `64 x pools` — trivial against a 320ms quote budget.
const CHUNKS: u32 = 64;

/// One pool available to serve a hop.
#[derive(Clone, Copy, Debug)]
pub struct HopPool {
    /// Index into the caller's own pool/edge list; this module never resolves it.
    pub id: usize,
    /// Fixed cost of touching this pool, in the same units as
    /// [`Allocation::gross_out`]. This is the term that breaks convexity.
    pub gas_cost: U256,
}

/// How a hop's input was split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allocation {
    /// `(pool id, amount_in)`, only for pools receiving a non-zero amount.
    pub parts: Vec<(usize, U256)>,
    /// Total output before gas.
    pub gross_out: U256,
    /// Gas for the pools actually used.
    pub gas_cost: U256,
}

impl Allocation {
    /// Output net of the gas of the pools touched. Saturates rather than
    /// wrapping: a split that cannot pay for its own gas is worth zero, not a
    /// huge number.
    pub fn net_out(&self) -> U256 {
        self.gross_out.saturating_sub(self.gas_cost)
    }

    /// Number of pools touched — the cardinality the prune operates on.
    pub fn support(&self) -> usize {
        self.parts.len()
    }
}

/// Split `total_in` across `pools` to maximise total output, ignoring gas.
///
/// `quote(pool_id, amount_in) -> Option<amount_out>` must be monotone
/// non-decreasing and concave in `amount_in` — true of every constant-product
/// and concentrated-liquidity exact-input curve. `None` means the pool cannot
/// serve that size and it is skipped.
///
/// This is the convex relaxation: globally optimal for the stated objective,
/// with no gas term.
pub fn split_hop<F>(total_in: U256, pools: &[HopPool], mut quote: F) -> Option<Allocation>
where
    F: FnMut(usize, U256) -> Option<U256>,
{
    if total_in.is_zero() || pools.is_empty() {
        return None;
    }

    let chunk = total_in / U256::from(CHUNKS);
    if chunk.is_zero() {
        // Too small to split meaningfully: give it all to the best single pool.
        return best_single(total_in, pools, &mut quote);
    }

    // Running input assigned to each pool, and the output at that input.
    let mut assigned: Vec<U256> = vec![U256::zero(); pools.len()];
    let mut out_at: Vec<U256> = vec![U256::zero(); pools.len()];
    let mut placed = U256::zero();

    for _ in 0..CHUNKS {
        // Marginal output of giving THIS chunk to each pool. Concavity means
        // the marginal is non-increasing, so the greedy choice is safe.
        let mut best: Option<(usize, U256, U256)> = None; // (idx, marginal, new_out)
        for (idx, pool) in pools.iter().enumerate() {
            let candidate_in = assigned[idx].saturating_add(chunk);
            let Some(new_out) = quote(pool.id, candidate_in) else {
                continue;
            };
            let marginal = new_out.saturating_sub(out_at[idx]);
            if marginal.is_zero() {
                continue;
            }
            let better = match &best {
                Some((_, best_marginal, _)) => marginal > *best_marginal,
                None => true,
            };
            if better {
                best = Some((idx, marginal, new_out));
            }
        }
        let Some((idx, _, new_out)) = best else {
            break;
        };
        assigned[idx] = assigned[idx].saturating_add(chunk);
        out_at[idx] = new_out;
        placed = placed.saturating_add(chunk);
    }

    if placed.is_zero() {
        return None;
    }

    // Integer division leaves a remainder; give it to whichever pool takes it
    // best so `parts` sums exactly to what was placed.
    let remainder = total_in.saturating_sub(placed);
    if !remainder.is_zero() {
        let mut best: Option<(usize, U256)> = None;
        for (idx, pool) in pools.iter().enumerate() {
            if assigned[idx].is_zero() {
                continue;
            }
            let candidate_in = assigned[idx].saturating_add(remainder);
            let Some(new_out) = quote(pool.id, candidate_in) else {
                continue;
            };
            let gain = new_out.saturating_sub(out_at[idx]);
            if best.as_ref().map(|(_, g)| gain > *g).unwrap_or(true) {
                best = Some((idx, gain));
            }
        }
        if let Some((idx, _)) = best {
            let candidate_in = assigned[idx].saturating_add(remainder);
            if let Some(new_out) = quote(pools[idx].id, candidate_in) {
                assigned[idx] = candidate_in;
                out_at[idx] = new_out;
            }
        }
    }

    let mut parts = Vec::new();
    let mut gross_out = U256::zero();
    let mut gas_cost = U256::zero();
    for (idx, pool) in pools.iter().enumerate() {
        if assigned[idx].is_zero() {
            continue;
        }
        parts.push((pool.id, assigned[idx]));
        gross_out = gross_out.saturating_add(out_at[idx]);
        gas_cost = gas_cost.saturating_add(pool.gas_cost);
    }
    if parts.is_empty() {
        return None;
    }
    Some(Allocation {
        parts,
        gross_out,
        gas_cost,
    })
}

/// Everything through whichever single pool pays best.
fn best_single<F>(total_in: U256, pools: &[HopPool], quote: &mut F) -> Option<Allocation>
where
    F: FnMut(usize, U256) -> Option<U256>,
{
    let mut best: Option<Allocation> = None;
    for pool in pools {
        let Some(out) = quote(pool.id, total_in) else {
            continue;
        };
        if out.is_zero() {
            continue;
        }
        let candidate = Allocation {
            parts: vec![(pool.id, total_in)],
            gross_out: out,
            gas_cost: pool.gas_cost,
        };
        if best
            .as_ref()
            .map(|b| candidate.net_out() > b.net_out())
            .unwrap_or(true)
        {
            best = Some(candidate);
        }
    }
    best
}

/// Convex relaxation, then a cardinality prune for gas, then re-solve.
///
/// Splitting always weakly improves GROSS output, but every extra pool touched
/// costs gas. The relaxation ignores that and will happily spread across all
/// available pools; on a thin edge the last pool can easily contribute less
/// output than it costs to touch.
///
/// The prune is the convex-based heuristic the source prescribes in place of an
/// exact mixed-integer solve: drop the lowest-allocation pool, re-solve on the
/// remaining support, and keep the change only if NET output improved. Repeat
/// while it helps. Monotone in support size, so it terminates in at most
/// `pools.len()` re-solves.
pub fn optimise_hop<F>(total_in: U256, pools: &[HopPool], mut quote: F) -> Option<Allocation>
where
    F: FnMut(usize, U256) -> Option<U256>,
{
    let mut support: Vec<HopPool> = pools.to_vec();
    let mut best = split_hop(total_in, &support, &mut quote)?;

    while support.len() > 1 {
        // Try removing EACH pool and keep whichever removal improves net most.
        //
        // Dropping the smallest allocation looks like the obvious heuristic and
        // is wrong: with near-equal allocations it can drop the CHEAP pool and
        // keep the expensive one, so a genuinely uneconomic pool survives. Only
        // the net figure knows which pool is not paying for itself, and with a
        // handful of parallel pools per pair evaluating all of them is cheap.
        let mut improvement: Option<(Vec<HopPool>, Allocation)> = None;
        for drop in support.iter().map(|p| p.id) {
            let pruned: Vec<HopPool> =
                support.iter().copied().filter(|p| p.id != drop).collect();
            if pruned.is_empty() {
                continue;
            }
            let Some(candidate) = split_hop(total_in, &pruned, &mut quote) else {
                continue;
            };
            if candidate.net_out() <= best.net_out() {
                continue;
            }
            let better = improvement
                .as_ref()
                .map(|(_, a)| candidate.net_out() > a.net_out())
                .unwrap_or(true);
            if better {
                improvement = Some((pruned, candidate));
            }
        }
        let Some((pruned, candidate)) = improvement else {
            break;
        };
        best = candidate;
        support = pruned;
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(id: usize, gas: u64) -> HopPool {
        HopPool {
            id,
            gas_cost: U256::from(gas),
        }
    }

    /// Constant-product exact-input: `out = r_out * x / (r_in + x)`. Concave and
    /// monotone, the shape `split_hop` requires.
    fn cpmm(reserve_in: u128, reserve_out: u128) -> impl Fn(U256) -> Option<U256> {
        move |x: U256| {
            if x.is_zero() {
                return Some(U256::zero());
            }
            let ri = U256::from(reserve_in);
            let ro = U256::from(reserve_out);
            Some(ro.saturating_mul(x) / (ri.saturating_add(x)))
        }
    }

    #[test]
    fn single_pool_takes_everything() {
        let f = cpmm(1_000_000, 1_000_000);
        let a = split_hop(U256::from(1_000u64), &[pool(0, 0)], |_, x| f(x)).expect("alloc");
        assert_eq!(a.parts, vec![(0, U256::from(1_000u64))]);
        assert_eq!(a.support(), 1);
    }

    #[test]
    fn identical_pools_split_evenly() {
        let f = cpmm(1_000_000, 1_000_000);
        let a = split_hop(U256::from(64_000u64), &[pool(0, 0), pool(1, 0)], |_, x| f(x))
            .expect("alloc");
        assert_eq!(a.support(), 2, "must use both pools");
        let (x0, x1) = (a.parts[0].1, a.parts[1].1);
        let diff = if x0 > x1 { x0 - x1 } else { x1 - x0 };
        assert!(
            diff <= U256::from(1_000u64),
            "identical curves must split ~evenly, got {x0} vs {x1}"
        );
    }

    #[test]
    fn deeper_pool_receives_more() {
        let shallow = cpmm(100_000, 100_000);
        let deep = cpmm(10_000_000, 10_000_000);
        let a = split_hop(U256::from(64_000u64), &[pool(0, 0), pool(1, 0)], |id, x| {
            if id == 0 { shallow(x) } else { deep(x) }
        })
        .expect("alloc");
        let got = |id: usize| {
            a.parts
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, v)| *v)
                .unwrap_or_default()
        };
        assert!(
            got(1) > got(0),
            "the deeper pool must absorb more: shallow={} deep={}",
            got(0),
            got(1)
        );
    }

    #[test]
    fn splitting_beats_routing_everything_through_one_pool() {
        // The whole point: parallel pools at the same pair. One-pool-per-hop
        // eats the full price impact.
        let f = cpmm(1_000_000, 1_000_000);
        let total = U256::from(200_000u64);
        let pools = [pool(0, 0), pool(1, 0)];

        let split = split_hop(total, &pools, |_, x| f(x)).expect("alloc");
        let single = f(total).expect("single-pool quote");

        assert!(
            split.gross_out > single,
            "splitting must beat one-pool-per-hop: {} vs {}",
            split.gross_out,
            single
        );
    }

    #[test]
    fn gas_prune_drops_a_pool_whose_gas_exceeds_the_split_gain() {
        // Two identical pools, so the relaxation genuinely splits across both.
        // A pool the relaxation would never touch anyway (dust, worse marginal
        // on every chunk) proves nothing about the prune — the relaxation
        // already excludes it.
        let f = cpmm(1_000_000, 1_000_000);
        let total = U256::from(200_000u64);
        let quote = |_: usize, x: U256| f(x);

        let relaxed = split_hop(total, &[pool(0, 0), pool(1, 0)], quote).expect("relaxed");
        assert_eq!(relaxed.support(), 2, "relaxation must use both");

        // Gain from splitting rather than routing it all through one pool.
        let single = f(total).expect("single quote");
        let gain = relaxed.gross_out.saturating_sub(single);
        assert!(!gain.is_zero(), "fixture must actually benefit from splitting");

        // Price the second pool's gas ABOVE that gain: touching it now costs
        // more than it returns, so the prune must collapse to one pool.
        let too_costly = gain.saturating_mul(U256::from(2u64)).as_u64();
        let pruned =
            optimise_hop(total, &[pool(0, 0), pool(1, too_costly)], quote).expect("pruned");
        assert_eq!(
            pruned.support(),
            1,
            "gas above the split gain must prune the pool away"
        );
        assert_eq!(pruned.parts[0].0, 0, "and keep the one with no gas charge");
    }

    #[test]
    fn gas_prune_keeps_a_pool_that_earns_its_gas() {
        // Two comparable pools and negligible gas: splitting genuinely wins, so
        // the prune must NOT collapse to one.
        let f = cpmm(1_000_000, 1_000_000);
        let a = optimise_hop(U256::from(200_000u64), &[pool(0, 1), pool(1, 1)], |_, x| f(x))
            .expect("alloc");
        assert_eq!(a.support(), 2, "a split that pays for its gas must survive");
    }

    #[test]
    fn allocation_sums_to_the_input() {
        let f = cpmm(1_000_000, 1_000_000);
        let total = U256::from(100_003u64); // deliberately not a multiple of CHUNKS
        let a = split_hop(total, &[pool(0, 0), pool(1, 0)], |_, x| f(x)).expect("alloc");
        let sum: U256 = a.parts.iter().fold(U256::zero(), |s, (_, v)| s + *v);
        assert_eq!(sum, total, "every unit of input must be allocated");
    }

    #[test]
    fn unquotable_pools_are_skipped_not_fatal() {
        let f = cpmm(1_000_000, 1_000_000);
        let a = split_hop(U256::from(64_000u64), &[pool(0, 0), pool(1, 0)], |id, x| {
            if id == 1 { None } else { f(x) }
        })
        .expect("alloc");
        assert_eq!(a.support(), 1);
        assert_eq!(a.parts[0].0, 0);
    }

    #[test]
    fn no_pool_can_serve_yields_none() {
        assert!(split_hop(U256::from(1_000u64), &[pool(0, 0)], |_, _| None).is_none());
        assert!(split_hop(U256::zero(), &[pool(0, 0)], |_, x| Some(x)).is_none());
        assert!(split_hop(U256::from(1_000u64), &[], |_, x| Some(x)).is_none());
    }

    #[test]
    fn net_out_saturates_when_gas_exceeds_output() {
        let a = Allocation {
            parts: vec![(0, U256::from(1u64))],
            gross_out: U256::from(10u64),
            gas_cost: U256::from(999u64),
        };
        assert_eq!(
            a.net_out(),
            U256::zero(),
            "a split that cannot pay its gas is worth zero, never a wrapped max"
        );
    }

    #[test]
    fn dust_input_falls_back_to_the_best_single_pool() {
        // Below CHUNKS the chunk size floors to zero; must not divide by zero
        // or return nothing.
        let shallow = cpmm(100, 100);
        let deep = cpmm(1_000_000, 1_000_000);
        let a = split_hop(U256::from(8u64), &[pool(0, 0), pool(1, 0)], |id, x| {
            if id == 0 { shallow(x) } else { deep(x) }
        })
        .expect("alloc");
        assert_eq!(a.support(), 1, "dust goes to one pool");
    }
}
