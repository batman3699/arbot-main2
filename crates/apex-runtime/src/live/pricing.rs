//! The local pricer: a cycle as a finite-size curve over live pool state
//! (Task 8.5 R4).
//!
//! # Multi-tick, and a ladder's end is a refusal
//!
//! Each hop is `apex-math`'s `quote_exact_input_multi_tick` over the pool's
//! ladder. A quote that runs off the ladder, hits the tick limit or fills only
//! part of its input is **refused** — `SizedRoute::output` returns `None` —
//! rather than completed at the last known liquidity. That completion is the
//! constant-liquidity error that cost the legacy fast path ~140 bps against the
//! on-chain quoter, and the reason `TickLadder` records where its knowledge ends.
//!
//! **Checked against the venues' own quoters, 2026-09-30, block 51,988,099:**
//! twelve quotes over the four WETH pools in the book, at 0.01, 0.1 and 1 WETH,
//! against Uniswap's QuoterV2 and Slipstream's quoter at the same block. Every
//! one agreed to the wei but one, which differed by 2 units in 2.6 × 10⁹ after
//! crossing 8 ticks — 0.0000 bps. The ~140 bps the legacy fast path lost to the
//! quoter was the missing ladder; with one, there is no gap to report.
//!
//! # One pricer, two ports, one cost
//!
//! `LivePricer` serves Engine C (`TemplatePricer`, at search time) and
//! `LiveEconomics` (`RouteCurves`, at refinement), over the same curve and the
//! same fixed cost — `LiveEconomics::route_cost_wei` — so the search proposes
//! exactly the routes the economics will take.

use crate::econ::{Evaluated, RouteCurves};
use crate::live::book::{PoolBook, PoolSnapshot};
use crate::live::frontier::Cycle;
use crate::plane::Decline;
use apex_math::cl_swap::quote_exact_input_multi_tick;
use apex_math::finite_size::{best_size, NoSize, SearchBudget, SizedOpportunity, SizedRoute};
use apex_search::engine_c::TemplatePricer;
use apex_search::frontier::{RouteId, RouteProposal};
use apex_types::route::RouteCommitment;
use apex_types::state::StateFingerprint;
use ethers_core::types::U256;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Ticks a single hop may cross. Generous for the sizes the census found
/// profitable ($300–$10k against ≥ $100k pools); a quote needing more is one the
/// ladder would likely not cover anyway.
pub const MAX_TICKS: u32 = 64;

/// A cycle, priced against one consistent snapshot of its pools.
pub struct LiveCycle {
    legs: [(Arc<PoolSnapshot>, bool); 2],
    fixed_cost: U256,
}

impl LiveCycle {
    /// `None` if either pool is missing from the snapshot, or its price has left
    /// its ladder: there is nothing honest to price until it is reloaded.
    pub fn new(
        cycle: &Cycle,
        snapshot: &BTreeMap<alloy_primitives::Address, Arc<PoolSnapshot>>,
        fixed_cost_wei: u128,
    ) -> Option<Self> {
        let pool = |i: usize| {
            snapshot
                .get(&cycle.legs[i].pool)
                .filter(|p| p.ladder_covers_price())
                .map(|p| (Arc::clone(p), cycle.legs[i].zero_for_one))
        };
        Some(Self { legs: [pool(0)?, pool(1)?], fixed_cost: U256::from(fixed_cost_wei) })
    }
}

impl SizedRoute for LiveCycle {
    fn output(&self, amount_in: U256) -> Option<U256> {
        let mut amount = amount_in;
        for (pool, zero_for_one) in &self.legs {
            let q =
                quote_exact_input_multi_tick(&pool.state, &pool.ladder, amount, *zero_for_one, MAX_TICKS)?;
            // A partial fill is not a price for the whole input.
            if q.exhausted || q.amount_in_consumed < amount {
                return None;
            }
            amount = q.amount_out;
        }
        Some(amount)
    }

    fn fixed_cost(&self) -> U256 {
        self.fixed_cost
    }

    /// The second pool pays the start token out, so its real holding of it
    /// bounds what the cycle can return — and an arbitrage returns about what it
    /// took. From `balanceOf`, never from virtual reserves, which overstate a
    /// concentrated pool's holdings 16–56× on Base. An unread balance fails
    /// closed: zero, so no size is proposed.
    fn max_input(&self) -> U256 {
        let (pool, zero_for_one) = &self.legs[1];
        // Selling token0 means receiving token1, and the reverse.
        let held = if *zero_for_one { pool.state.balance1 } else { pool.state.balance0 };
        held.unwrap_or_default()
    }
}

/// Engine C's pricer and `LiveEconomics`' curves, over the live book.
pub struct LivePricer {
    book: Arc<PoolBook>,
    cycles: BTreeMap<RouteId, Cycle>,
    /// By route hash, so a proposal — which carries a commitment, not an id —
    /// finds its cycle.
    by_hash: BTreeMap<alloy_primitives::B256, RouteId>,
    fixed_cost_wei: u128,
}

impl LivePricer {
    pub fn new(book: Arc<PoolBook>, cycles: BTreeMap<RouteId, Cycle>, fixed_cost_wei: u128) -> Self {
        let by_hash = cycles.iter().map(|(id, c)| (c.commitment.route_hash, *id)).collect();
        Self { book, cycles, by_hash, fixed_cost_wei }
    }

    pub fn cycle(&self, id: RouteId) -> Option<&Cycle> {
        self.cycles.get(&id)
    }

    fn live(&self, id: RouteId) -> Option<LiveCycle> {
        LiveCycle::new(self.cycles.get(&id)?, &self.book.snapshot(), self.fixed_cost_wei)
    }
}

impl TemplatePricer for LivePricer {
    fn best_size(
        &self,
        id: RouteId,
        _at: &StateFingerprint,
        budget: SearchBudget,
    ) -> Result<SizedOpportunity, NoSize> {
        // A pool that has left its ladder cannot be priced; that is the pool's
        // state, not the route's economics, and `Unpriceable` says so.
        let Some(route) = self.live(id) else { return Err(NoSize::Unpriceable) };
        best_size(&route, budget)
    }

    fn commitment(&self, id: RouteId) -> Option<RouteCommitment> {
        self.cycles.get(&id).map(|c| c.commitment.clone())
    }
}

impl RouteCurves for LivePricer {
    fn evaluate(
        &self,
        p: &RouteProposal,
        f: &mut dyn FnMut(&dyn SizedRoute) -> Result<Evaluated, Decline>,
    ) -> Result<Evaluated, Decline> {
        let Some(id) = self.by_hash.get(&p.route.route_hash) else {
            return Err(Decline::Uncommittable { detail: "no live cycle for this route".into() });
        };
        // The state has moved since the search priced it, or a pool has left its
        // ladder: stale, and refused as such.
        let Some(route) = self.live(*id) else {
            return Err(Decline::StaleState { age: apex_types::time::DurationNanos(0) });
        };
        f(&route)
    }
}
