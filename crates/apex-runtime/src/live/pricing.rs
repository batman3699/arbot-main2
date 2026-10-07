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
//! same costs — `LiveEconomics::route_costs` — so the search proposes exactly
//! the routes the economics will take.
//!
//! # Each size at its own cost (R12)
//!
//! A cycle's gas grows with every initialized tick its hops cross
//! (`live::gas`), so a larger trade costs more to settle. [`CostedCycle`]
//! charges each size what settling it uses — the quote that prices a size
//! counts its crossings — and the search sizes against that, never against a
//! cost every size shares.

use crate::econ::{Evaluated, GasEstimate, RouteCosts, RouteCurves, SettledRoute};
use crate::live::book::{PoolBook, PoolSnapshot};
use crate::live::frontier::Cycle;
use crate::live::gas::{HopSteps, SettlementGas};
use crate::live::near_miss::{NearMisses, LADDER_WEI};
use crate::plane::Decline;
use apex_math::cl_swap::quote_exact_input_multi_tick;
use apex_math::finite_size::{best_size, NoSize, Priced, SearchBudget, SizedOpportunity, SizedRoute};
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

/// A cycle, quoted against one consistent snapshot of its pools: the venue
/// arithmetic, with no costs. A plan's hop amounts come from here; sizing goes
/// through [`CostedCycle`].
pub struct LiveCycle {
    legs: [(Arc<PoolSnapshot>, bool); 2],
}

/// What both hops do at one size: what each returns, and the steps each takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CycleQuote {
    pub outputs: [U256; 2],
    pub hops: [HopSteps; 2],
}

impl LiveCycle {
    /// `None` if either pool is missing from the snapshot, or its price has left
    /// its ladder: there is nothing honest to price until it is reloaded.
    pub fn new(cycle: &Cycle, snapshot: &BTreeMap<alloy_primitives::Address, Arc<PoolSnapshot>>) -> Option<Self> {
        let pool = |i: usize| {
            snapshot
                .get(&cycle.legs[i].pool)
                .filter(|p| p.ladder_covers_price())
                .map(|p| (Arc::clone(p), cycle.legs[i].zero_for_one))
        };
        Some(Self { legs: [pool(0)?, pool(1)?] })
    }

    /// Both hops at `amount_in`. `None` if either cannot fill it whole.
    pub fn quote(&self, amount_in: U256) -> Option<CycleQuote> {
        let mut outputs = [U256::zero(); 2];
        let mut hops = [None; 2];
        let mut amount = amount_in;
        for (i, (pool, zero_for_one)) in self.legs.iter().enumerate() {
            let q =
                quote_exact_input_multi_tick(&pool.state, &pool.ladder, amount, *zero_for_one, MAX_TICKS)?;
            // A partial fill is not a price for the whole input.
            if q.exhausted || q.amount_in_consumed < amount {
                return None;
            }
            amount = q.amount_out;
            outputs[i] = amount;
            hops[i] = Some(HopSteps {
                venue: pool.spec.venue,
                zero_for_one: *zero_for_one,
                crossed: q.ticks_crossed,
                word_steps: q.word_steps,
            });
        }
        Some(CycleQuote { outputs, hops: [hops[0]?, hops[1]?] })
    }

    /// What each hop returns for `amount_in`, in order: the figures a plan's
    /// steps are built from, by the same arithmetic the cycle is priced by.
    /// `None` exactly when [`Self::output`] is.
    pub fn hop_outputs(&self, amount_in: U256) -> Option<[U256; 2]> {
        self.quote(amount_in).map(|q| q.outputs)
    }

    /// What the cycle returns for `amount_in`.
    pub fn output(&self, amount_in: U256) -> Option<U256> {
        self.hop_outputs(amount_in).map(|[_, last]| last)
    }

    /// The second pool pays the start token out, so its real holding of it
    /// bounds what the cycle can return — and an arbitrage returns about what it
    /// took. From `balanceOf`, never from virtual reserves, which overstate a
    /// concentrated pool's holdings 16–56× on Base. An unread balance fails
    /// closed: zero, so no size is proposed.
    pub fn max_input(&self) -> U256 {
        let (pool, zero_for_one) = &self.legs[1];
        // Selling token0 means receiving token1, and the reverse.
        let held = if *zero_for_one { pool.state.balance1 } else { pool.state.balance0 };
        held.unwrap_or_default()
    }
}

/// What the lender holds of the start token, as last read: the most any cycle
/// may borrow (R21). `None` until read, which sizes nothing.
pub type LenderHolding = tokio::sync::watch::Receiver<Option<U256>>;

/// A cycle and what settling it costs: what Engine C and the economics size.
pub struct CostedCycle {
    cycle: LiveCycle,
    costs: RouteCosts,
    gas: SettlementGas,
    /// A bound on the input beside the paying pool's holding: the lender's.
    cap: Option<U256>,
}

impl CostedCycle {
    pub const fn new(cycle: LiveCycle, costs: RouteCosts, gas: SettlementGas) -> Self {
        Self { cycle, costs, gas, cap: None }
    }

    /// Bound every size at `cap` as well as at the paying pool's holding: the
    /// lender's holding, for a cycle that borrows (R21).
    #[must_use]
    pub const fn capped(mut self, cap: Option<U256>) -> Self {
        self.cap = cap;
        self
    }

    fn cost_of(&self, gas: GasEstimate) -> U256 {
        U256::from(self.costs.with_gas(gas.expected))
    }

    /// The route's best net over [`LADDER_WEI`], each size at its own cost, in
    /// hundredths of a basis point of the size (R14). Sizes past what the paying
    /// pool holds are left out; `None` if no size prices.
    pub fn near_miss_centi_bps(&self) -> Option<i64> {
        let cap = self.max_input();
        LADDER_WEI
            .iter()
            .map(|x| U256::from(*x))
            .filter(|x| *x <= cap)
            .filter_map(|x| {
                let p = self.priced(x)?;
                let net = wei(p.output).saturating_sub(wei(x)).saturating_sub(wei(p.cost));
                // Clamped clear of `i64::MIN`, which `NearMisses` reads as "none yet".
                let centi = net.saturating_mul(1_000_000) / wei(x);
                Some(centi.clamp(i128::from(i64::MIN + 1), i128::from(i64::MAX)) as i64)
            })
            .max()
    }
}

/// A wei amount as a signed figure, saturating rather than wrapping.
fn wei(v: U256) -> i128 {
    if v > U256::from(i128::MAX as u128) {
        i128::MAX
    } else {
        v.as_u128() as i128
    }
}

impl SizedRoute for CostedCycle {
    fn output(&self, amount_in: U256) -> Option<U256> {
        self.cycle.output(amount_in)
    }

    /// A settlement that crosses nothing: the least any size costs.
    fn fixed_cost(&self) -> U256 {
        let hop = |h: &(Arc<PoolSnapshot>, bool)| HopSteps {
            venue: h.0.spec.venue,
            zero_for_one: h.1,
            crossed: 0,
            word_steps: 0,
        };
        self.cost_of(self.gas.estimate(&[hop(&self.cycle.legs[0]), hop(&self.cycle.legs[1])]))
    }

    fn max_input(&self) -> U256 {
        let held = self.cycle.max_input();
        self.cap.map_or(held, |c| held.min(c))
    }

    /// The output and its cost from one quote: the crossings that price the size
    /// are the ones its settlement pays for.
    fn priced(&self, amount_in: U256) -> Option<Priced> {
        let q = self.cycle.quote(amount_in)?;
        Some(Priced { output: q.outputs[1], cost: self.cost_of(self.gas.estimate(&q.hops)) })
    }
}

impl SettledRoute for CostedCycle {
    fn gas_at(&self, amount_in: U256) -> Option<GasEstimate> {
        self.cycle.quote(amount_in).map(|q| self.gas.estimate(&q.hops))
    }
}

/// Engine C's pricer and `LiveEconomics`' curves, over the live book.
pub struct LivePricer {
    book: Arc<PoolBook>,
    cycles: BTreeMap<RouteId, Cycle>,
    /// By route hash, so a proposal — which carries a commitment, not an id —
    /// finds its cycle.
    by_hash: BTreeMap<alloy_primitives::B256, RouteId>,
    /// `LiveEconomics::route_costs`, replaced when the economics' costs are.
    costs: apex_state::Versioned<RouteCosts>,
    gas: SettlementGas,
    /// How close each route Engine C prices comes to paying (R14).
    near_misses: Arc<NearMisses>,
    /// The lender's holding, when sizing is bounded by it (R21).
    lender: Option<LenderHolding>,
}

impl LivePricer {
    pub fn new(book: Arc<PoolBook>, cycles: BTreeMap<RouteId, Cycle>, costs: RouteCosts, gas: SettlementGas) -> Self {
        let by_hash = cycles.iter().map(|(id, c)| (c.commitment.route_hash, *id)).collect();
        let costs = apex_state::Versioned::new(costs, apex_types::state::ReconstructionStatus::Verified);
        Self { book, cycles, by_hash, costs, gas, near_misses: Arc::default(), lender: None }
    }

    /// Size every cycle within the lender's holding as `lender` reports it.
    /// Every live cycle borrows from Balancer (`frontier::BALANCER_FLASH`), so
    /// the cap applies to all of them. Without it, sizing is bounded only by the
    /// paying pool, and an optimum above the lender's holding is refused whole
    /// by the risk gate rather than traded smaller.
    #[must_use]
    pub fn with_lender(mut self, lender: LenderHolding) -> Self {
        self.lender = Some(lender);
        self
    }

    /// How close the routes Engine C has priced came to paying.
    pub fn near_misses(&self) -> Arc<NearMisses> {
        Arc::clone(&self.near_misses)
    }

    /// Size every later route at these costs — the economics' new
    /// `route_costs`, whenever their costs are replaced.
    pub fn set_costs(&self, costs: RouteCosts) {
        self.costs.store(costs, apex_types::state::ReconstructionStatus::Verified);
    }

    pub fn costs(&self) -> RouteCosts {
        *self.costs.load().value
    }

    pub const fn gas(&self) -> SettlementGas {
        self.gas
    }

    pub fn cycle(&self, id: RouteId) -> Option<&Cycle> {
        self.cycles.get(&id)
    }

    fn live(&self, id: RouteId) -> Option<CostedCycle> {
        let cycle = LiveCycle::new(self.cycles.get(&id)?, &self.book.snapshot())?;
        let cap = self.lender.as_ref().map(|l| (*l.borrow()).unwrap_or_default());
        Some(CostedCycle::new(cycle, self.costs(), self.gas).capped(cap))
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
        if let Some(near) = route.near_miss_centi_bps() {
            self.near_misses.record(near);
        }
        best_size(&route, budget)
    }

    fn commitment(&self, id: RouteId) -> Option<RouteCommitment> {
        self.cycles.get(&id).map(|c| c.commitment.clone())
    }

    /// The event's fingerprint, with the venue versions of **this cycle's own
    /// pools**, read now — Engine C asks before pricing. An unknown cycle reads
    /// as no venues, and is refused by `best_size` in any case.
    fn route_fingerprint(&self, id: RouteId, at: &StateFingerprint) -> StateFingerprint {
        let pools: Vec<alloy_primitives::Address> =
            self.cycles.get(&id).map(|c| c.legs.iter().map(|l| l.pool).collect()).unwrap_or_default();
        StateFingerprint { venue_state_version: self.book.versions_for(&pools), ..at.clone() }
    }
}

impl RouteCurves for LivePricer {
    fn evaluate(
        &self,
        p: &RouteProposal,
        f: &mut dyn FnMut(&dyn SettledRoute) -> Result<Evaluated, Decline>,
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
