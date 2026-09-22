//! The `ExactPricingEngine` contract (PLAN.md §11.1, blueprint §11).
//!
//! One trait, six methods, no default implementations. Every venue this
//! repository will ever price answers all six or it does not ship: a venue that
//! cannot say how it rounds, what makes it revert, or what state it read is a
//! venue whose quotes cannot be audited against the chain.
//!
//! # `next_state_exact` is the reason this trait exists
//!
//! Quoting is the easy half, and the legacy code already did it well. What it
//! could not do is say what the pool looks like *after* the swap — and without
//! that, a route cannot cross the same pool twice, a speculative branch (§5.4)
//! cannot be extended, and joint allocation over shared pools (§13) has nothing
//! to allocate against. Writing this method is what surfaced two real gaps:
//! `cl_math` had no inverse for `get_sqrt_ratio_at_tick`, and the multi-tick
//! loop's break convention leaves the post-swap state undetermined when a swap
//! comes to rest exactly on an initialized tick.
//!
//! Both are handled by refusing, never by guessing. `NotRepresentable` is a
//! first-class answer here.

use crate::cl_state::ClPoolState;
use crate::cl_swap::{quote_exact_input_multi_tick, TickLadder};
use crate::{cl_math, quote_common, quote_solidly};
use ethers_core::types::{Address, U256};

/// A trade request, independent of venue.
///
/// Direction is carried as the input TOKEN rather than a `zero_for_one` bool,
/// because only concentrated liquidity thinks in terms of token ordering and a
/// bool at this level means every caller has to know each venue's convention.
/// Each engine's state resolves the token to its own direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    pub token_in: Address,
    pub amount_in: U256,
}

impl Order {
    pub const fn new(token_in: Address, amount_in: U256) -> Self {
        Self { token_in, amount_in }
    }
}

/// What a quote answers. Every field is a measurement, never an estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactQuote {
    pub amount_out: U256,
    /// Fee paid in the INPUT token, in wei. Not a rate: rates cannot be
    /// summed across hops without reintroducing rounding error.
    pub fee_in: U256,
    /// Price impact against the pre-trade marginal price, in basis points.
    pub price_impact_bps: u32,
    /// Initialized ticks crossed. Always 0 for non-CL venues, which is a fact
    /// about those venues rather than a missing value.
    pub ticks_crossed: u32,
}

/// How an engine breaks ties against the chain's own arithmetic.
///
/// This is not decoration. A model that rounds the other way from the contract
/// it models is wrong by one wei per operation in the direction that invents
/// profit, and one wei per hop is enough to select a phantom edge when the
/// ranking maximises gross.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundingMode {
    /// Output rounds down, fee rounds up — the pool never pays out more than
    /// exact and never collects less. Every AMM this repository prices.
    OutputDownFeeUp,
}

/// A condition under which the venue's own contract reverts this trade.
///
/// Enumerated, not a string: the simulator classifies reverts by matching on
/// these, and a free-text reason cannot be matched on without a parser that
/// silently fails to recognise a new message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevertCondition {
    /// Input is zero, or rounds to zero after the fee.
    ZeroAmount,
    /// The pool holds no liquidity at the current price.
    ZeroLiquidity,
    /// The swap would move price past the venue's published bound — the pool
    /// cannot pay out that much of the output token at any price.
    PriceLimitReached,
    /// The requested input token is not in this pool.
    TokenNotInPool,
}

/// The message `NotRepresentable` carries when a CL quote runs out of proven
/// tick data. Named so callers can match on the cause rather than the prose.
///
/// This is deliberately NOT a [`RevertCondition`]. The pool would very likely
/// fill this trade; we simply cannot see far enough to say by how much.
/// Recording it as a revert would tell the simulator the chain refused a trade
/// the chain never saw, and every revert statistic built on that is then wrong.
pub const LADDER_EXHAUSTED: &str = "ladder does not prove the ticks this swap needs";

/// Exactly what state this quote read, so a caller can bind the answer to a
/// `StateVersion` and detect when it has gone stale.
///
/// `slot0`, `liquidity` and the tick set are separate fields rather than one
/// opaque hash because they invalidate independently: a swap in another pool
/// changes nothing here, a swap in THIS pool changes slot0 and possibly
/// liquidity, and a mint/burn changes the tick set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDeps {
    pub pool: Address,
    /// Reads the pool's current price / reserves.
    pub reads_spot: bool,
    /// Reads the pool's active liquidity.
    pub reads_liquidity: bool,
    /// Initialized ticks whose liquidity_net this quote depended on. Empty for
    /// venues without ticks.
    pub reads_ticks: Vec<i32>,
}

/// Why an engine could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PricingError {
    /// The venue's contract would revert. This is an ANSWER: the trade is
    /// known to fail, and the caller should record the condition.
    Reverts(RevertCondition),
    /// The model cannot represent this trade exactly, so it declines to
    /// approximate one. Distinct from `Reverts` — the trade might well
    /// succeed on chain; we simply cannot say by how much.
    ///
    /// Treating this as "no profit" is correct. Treating it as "zero output"
    /// and continuing is how a model manufactures an edge.
    NotRepresentable(&'static str),
}

pub type PricingResult<T> = Result<T, PricingError>;

/// The six-method contract. **No method has a default implementation**, by
/// design: a venue that inherits `rounding_exact` or `classify`-style defaults
/// is a venue nobody checked.
pub trait ExactPricingEngine {
    /// The state this engine needs. Owned by the engine, because what a CL
    /// pool must know and what a constant-product pair must know have nothing
    /// in common beyond both being "the pool".
    type State: Clone;

    /// Exact output for an exact input, at the state given.
    fn quote_exact(&self, s: &Self::State, o: &Order) -> PricingResult<ExactQuote>;

    /// The pool state this trade leaves behind.
    ///
    /// Must be exact or refuse. An approximate next state compounds across a
    /// route and there is no later stage that can detect it.
    fn next_state_exact(&self, s: &Self::State, o: &Order) -> PricingResult<Self::State>;

    /// Fee paid, in the input token, in wei.
    fn fee_exact(&self, s: &Self::State, o: &Order) -> PricingResult<U256>;

    /// How this engine rounds, relative to the contract it models.
    fn rounding_exact(&self) -> RoundingMode;

    /// Every way this trade can revert on chain, at this state.
    fn revert_conditions(&self, s: &Self::State, o: &Order) -> Vec<RevertCondition>;

    /// Exactly what this engine read.
    fn state_dependencies(&self, s: &Self::State) -> StateDeps;
}

// ─────────────────────────── concentrated liquidity ───────────────────────────

/// A CL pool as the engine needs it: the fetched state, the ladder proving the
/// ticks around it, and the token ordering the pool itself does not carry.
#[derive(Clone, Debug)]
pub struct ClEdgeState {
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    pub state: ClPoolState,
    pub ladder: TickLadder,
    /// Ceiling on ticks crossed, from configuration. Held in the STATE rather
    /// than read from the environment, because `apex-math` does not read the
    /// environment (PLAN.md §5.2) and because a quote that depends on a
    /// process-global is not reproducible.
    pub max_ticks: u32,
}

impl ClEdgeState {
    fn direction(&self, token_in: Address) -> Option<bool> {
        if token_in == self.token0 {
            Some(true)
        } else if token_in == self.token1 {
            Some(false)
        } else {
            None
        }
    }
}

/// The Uniswap V3 family swap maths.
///
/// Uniswap V3, PancakeSwap V3 and Aerodrome Slipstream run **the same swap
/// loop**. They differ in how a pool is discovered and in what their pool key
/// means — a fee tier on V3 and Pancake, a tick spacing on Slipstream — and
/// that difference is resolved by the loader, before `ClPoolState` exists. By
/// the time an engine sees the state, `fee_ppm` and `tick_spacing` are both
/// resolved and there is nothing venue-specific left.
///
/// The three named engines below therefore delegate here rather than each
/// carrying a copy. Three copies of one algorithm is three places for a fix to
/// land in two of them.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClEngine;

impl ExactPricingEngine for ClEngine {
    type State = ClEdgeState;

    fn quote_exact(&self, s: &Self::State, o: &Order) -> PricingResult<ExactQuote> {
        let zero_for_one = s
            .direction(o.token_in)
            .ok_or(PricingError::Reverts(RevertCondition::TokenNotInPool))?;
        if o.amount_in.is_zero() {
            return Err(PricingError::Reverts(RevertCondition::ZeroAmount));
        }
        if s.state.liquidity == 0 || s.state.sqrt_price_x96.is_zero() {
            return Err(PricingError::Reverts(RevertCondition::ZeroLiquidity));
        }

        let q = quote_exact_input_multi_tick(
            &s.state,
            &s.ladder,
            o.amount_in,
            zero_for_one,
            s.max_ticks,
        )
        .ok_or(PricingError::Reverts(RevertCondition::ZeroAmount))?;

        if q.exhausted {
            return Err(PricingError::NotRepresentable(LADDER_EXHAUSTED));
        }
        if q.is_unfillable(o.amount_in) {
            return Err(PricingError::Reverts(RevertCondition::PriceLimitReached));
        }

        Ok(ExactQuote {
            amount_out: q.amount_out,
            fee_in: self.fee_exact(s, o)?,
            price_impact_bps: sqrt_price_impact_bps(s.state.sqrt_price_x96, q.sqrt_price_after),
            ticks_crossed: q.ticks_crossed,
        })
    }

    fn next_state_exact(&self, s: &Self::State, o: &Order) -> PricingResult<Self::State> {
        let zero_for_one = s
            .direction(o.token_in)
            .ok_or(PricingError::Reverts(RevertCondition::TokenNotInPool))?;
        let q = quote_exact_input_multi_tick(
            &s.state,
            &s.ladder,
            o.amount_in,
            zero_for_one,
            s.max_ticks,
        )
        .ok_or(PricingError::Reverts(RevertCondition::ZeroAmount))?;

        if q.exhausted {
            return Err(PricingError::NotRepresentable(LADDER_EXHAUSTED));
        }
        if q.ended_on_tick_boundary {
            // v3-core crosses eagerly here and this loop does not, so the two
            // disagree about liquidity on the far side of a tick the swap
            // exactly reached. Refusing costs one route; publishing the
            // pre-crossing liquidity would price the NEXT swap against
            // depth that is not there.
            return Err(PricingError::NotRepresentable(
                "swap came to rest exactly on an initialized tick",
            ));
        }

        let tick = cl_math::get_tick_at_sqrt_ratio(q.sqrt_price_after).ok_or(
            PricingError::NotRepresentable("post-swap price is outside the published tick range"),
        )?;

        let mut next = s.clone();
        next.state.sqrt_price_x96 = q.sqrt_price_after;
        next.state.liquidity = q.liquidity_after;
        next.state.tick = tick;
        // Balances move by exactly the amounts swapped. A `None` balance stays
        // `None`: it means the read failed, and a failed read plus a delta is
        // still not a balance.
        if zero_for_one {
            next.state.balance0 = next.state.balance0.map(|b| b.saturating_add(q.amount_in_consumed));
            next.state.balance1 = next.state.balance1.map(|b| b.saturating_sub(q.amount_out));
        } else {
            next.state.balance1 = next.state.balance1.map(|b| b.saturating_add(q.amount_in_consumed));
            next.state.balance0 = next.state.balance0.map(|b| b.saturating_sub(q.amount_out));
        }
        Ok(next)
    }

    fn fee_exact(&self, s: &Self::State, o: &Order) -> PricingResult<U256> {
        if s.direction(o.token_in).is_none() {
            return Err(PricingError::Reverts(RevertCondition::TokenNotInPool));
        }
        // Uniswap V3 takes the fee off the INPUT, rounded UP, in hundredths of
        // a bip. `mul_div_rounding_up` is the same helper the swap step uses,
        // so the fee reported here is the fee the quote actually charged.
        cl_math::mul_div_rounding_up(
            o.amount_in,
            U256::from(s.state.fee_ppm),
            U256::from(1_000_000u64),
        )
        .ok_or(PricingError::NotRepresentable("fee overflows 256 bits"))
    }

    fn rounding_exact(&self) -> RoundingMode {
        RoundingMode::OutputDownFeeUp
    }

    fn revert_conditions(&self, s: &Self::State, o: &Order) -> Vec<RevertCondition> {
        let mut out = Vec::new();
        if s.direction(o.token_in).is_none() {
            out.push(RevertCondition::TokenNotInPool);
        }
        if o.amount_in.is_zero() {
            out.push(RevertCondition::ZeroAmount);
        }
        if s.state.liquidity == 0 || s.state.sqrt_price_x96.is_zero() {
            out.push(RevertCondition::ZeroLiquidity);
        }
        if let Some(zero_for_one) = s.direction(o.token_in) {
            match quote_exact_input_multi_tick(
                &s.state,
                &s.ladder,
                o.amount_in,
                zero_for_one,
                s.max_ticks,
            ) {
                // `exhausted` is not listed: it is a limit of our tick data,
                // not a condition under which the pool's contract reverts.
                Some(q) if !q.exhausted && q.is_unfillable(o.amount_in) => {
                    out.push(RevertCondition::PriceLimitReached)
                }
                _ => {}
            }
        }
        out
    }

    fn state_dependencies(&self, s: &Self::State) -> StateDeps {
        StateDeps {
            pool: s.pool,
            reads_spot: true,
            reads_liquidity: true,
            reads_ticks: s.ladder.ticks().iter().map(|(tick, _)| *tick).collect(),
        }
    }
}

/// Price impact from the sqrt price move, in basis points.
///
/// `sqrt` prices, so the price ratio is the square of the sqrt ratio; the
/// linearisation `2 * delta_sqrt / sqrt` is exact to first order and is what
/// the legacy ranking already used. Kept for continuity of the measurement,
/// not because it is the only defensible definition.
fn sqrt_price_impact_bps(before: U256, after: U256) -> u32 {
    if before.is_zero() {
        return 0;
    }
    let delta = if after > before { after - before } else { before - after };
    let bps = delta.saturating_mul(U256::from(20_000u64)) / before;
    bps.try_into().unwrap_or(u32::MAX)
}

macro_rules! cl_family_engine {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        ///
        /// Delegates to [`ClEngine`]. Named separately so a route, a metric or
        /// a circuit breaker can speak about this venue specifically, and so
        /// the contract test names it.
        #[derive(Clone, Copy, Debug, Default)]
        pub struct $name;

        impl ExactPricingEngine for $name {
            type State = ClEdgeState;

            fn quote_exact(&self, s: &Self::State, o: &Order) -> PricingResult<ExactQuote> {
                ClEngine.quote_exact(s, o)
            }
            fn next_state_exact(&self, s: &Self::State, o: &Order) -> PricingResult<Self::State> {
                ClEngine.next_state_exact(s, o)
            }
            fn fee_exact(&self, s: &Self::State, o: &Order) -> PricingResult<U256> {
                ClEngine.fee_exact(s, o)
            }
            fn rounding_exact(&self) -> RoundingMode {
                ClEngine.rounding_exact()
            }
            fn revert_conditions(&self, s: &Self::State, o: &Order) -> Vec<RevertCondition> {
                ClEngine.revert_conditions(s, o)
            }
            fn state_dependencies(&self, s: &Self::State) -> StateDeps {
                ClEngine.state_dependencies(s)
            }
        }
    };
}

cl_family_engine!(UniV3Engine, "Uniswap V3.");
cl_family_engine!(SlipstreamEngine, "Aerodrome Slipstream.");
cl_family_engine!(PancakeV3Engine, "PancakeSwap V3.");

// ───────────────────────────── constant product ─────────────────────────────

/// A `x*y=k` pair: Uniswap V2, Aerodrome volatile, and every fork of either.
#[derive(Clone, Copy, Debug)]
pub struct CpmmState {
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    pub fee_bps: u32,
}

impl CpmmState {
    fn reserves_for(&self, token_in: Address) -> Option<(U256, U256)> {
        if token_in == self.token0 {
            Some((self.reserve0, self.reserve1))
        } else if token_in == self.token1 {
            Some((self.reserve1, self.reserve0))
        } else {
            None
        }
    }
}

/// Constant-product pricing, from `quote_common`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpmmEngine;

impl ExactPricingEngine for CpmmEngine {
    type State = CpmmState;

    fn quote_exact(&self, s: &Self::State, o: &Order) -> PricingResult<ExactQuote> {
        let (reserve_in, reserve_out) = s
            .reserves_for(o.token_in)
            .ok_or(PricingError::Reverts(RevertCondition::TokenNotInPool))?;
        if o.amount_in.is_zero() {
            return Err(PricingError::Reverts(RevertCondition::ZeroAmount));
        }
        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Err(PricingError::Reverts(RevertCondition::ZeroLiquidity));
        }
        let after_fee = quote_common::apply_swap_fee(o.amount_in, s.fee_bps)
            .map_err(|_| PricingError::Reverts(RevertCondition::ZeroAmount))?;
        if after_fee.is_zero() {
            return Err(PricingError::Reverts(RevertCondition::ZeroAmount));
        }
        let amount_out = quote_common::constant_product_out(after_fee, reserve_in, reserve_out)
            .ok_or(PricingError::NotRepresentable("constant product overflows"))?;
        if amount_out.is_zero() || amount_out >= reserve_out {
            return Err(PricingError::Reverts(RevertCondition::PriceLimitReached));
        }
        Ok(ExactQuote {
            amount_out,
            fee_in: o.amount_in.saturating_sub(after_fee),
            price_impact_bps: quote_common::constant_product_price_impact_bps(after_fee, reserve_in),
            ticks_crossed: 0,
        })
    }

    fn next_state_exact(&self, s: &Self::State, o: &Order) -> PricingResult<Self::State> {
        let quote = self.quote_exact(s, o)?;
        let mut next = *s;
        // The FULL input enters the pool, fee included — that is how a
        // constant-product pool accrues fees to its LPs. Adding only the
        // post-fee amount would leak the fee out of the reserves and price
        // the next swap against a pool that is poorer than it is.
        if o.token_in == s.token0 {
            next.reserve0 = s.reserve0.saturating_add(o.amount_in);
            next.reserve1 = s.reserve1.saturating_sub(quote.amount_out);
        } else {
            next.reserve1 = s.reserve1.saturating_add(o.amount_in);
            next.reserve0 = s.reserve0.saturating_sub(quote.amount_out);
        }
        Ok(next)
    }

    fn fee_exact(&self, s: &Self::State, o: &Order) -> PricingResult<U256> {
        if s.reserves_for(o.token_in).is_none() {
            return Err(PricingError::Reverts(RevertCondition::TokenNotInPool));
        }
        let after_fee = quote_common::apply_swap_fee(o.amount_in, s.fee_bps)
            .map_err(|_| PricingError::Reverts(RevertCondition::ZeroAmount))?;
        Ok(o.amount_in.saturating_sub(after_fee))
    }

    fn rounding_exact(&self) -> RoundingMode {
        RoundingMode::OutputDownFeeUp
    }

    fn revert_conditions(&self, s: &Self::State, o: &Order) -> Vec<RevertCondition> {
        let mut out = Vec::new();
        match s.reserves_for(o.token_in) {
            None => out.push(RevertCondition::TokenNotInPool),
            Some((reserve_in, reserve_out)) => {
                if reserve_in.is_zero() || reserve_out.is_zero() {
                    out.push(RevertCondition::ZeroLiquidity);
                }
            }
        }
        if o.amount_in.is_zero() {
            out.push(RevertCondition::ZeroAmount);
        }
        if matches!(
            self.quote_exact(s, o),
            Err(PricingError::Reverts(RevertCondition::PriceLimitReached))
        ) {
            out.push(RevertCondition::PriceLimitReached);
        }
        out
    }

    fn state_dependencies(&self, s: &Self::State) -> StateDeps {
        StateDeps {
            pool: s.pool,
            reads_spot: true,
            reads_liquidity: false,
            reads_ticks: Vec::new(),
        }
    }
}

// ──────────────────────────────── solidly ────────────────────────────────

/// A Solidly pair plus the fee its factory reports.
///
/// The fee is not on `SolidlyPairState` because the pair contract does not
/// hold it — Aerodrome keeps it in the factory, per pool.
#[derive(Clone, Debug)]
pub struct SolidlyState {
    pub pool: Address,
    pub pair: quote_solidly::SolidlyPairState,
    pub fee_bps: u32,
}

/// Solidly stable and volatile curves, from `quote_solidly`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SolidlyEngine;

impl ExactPricingEngine for SolidlyEngine {
    type State = SolidlyState;

    fn quote_exact(&self, s: &Self::State, o: &Order) -> PricingResult<ExactQuote> {
        if o.amount_in.is_zero() {
            return Err(PricingError::Reverts(RevertCondition::ZeroAmount));
        }
        if s.pair.reserves_for(o.token_in).is_none() {
            return Err(PricingError::Reverts(RevertCondition::TokenNotInPool));
        }
        let quote = quote_solidly::quote_exact_input_from_state(
            &s.pair,
            o.token_in,
            o.amount_in,
            s.fee_bps,
        )
        .map_err(|_| PricingError::NotRepresentable("solidly curve did not converge"))?
        .ok_or(PricingError::Reverts(RevertCondition::ZeroLiquidity))?;

        Ok(ExactQuote {
            amount_out: quote.amount_out,
            fee_in: self.fee_exact(s, o)?,
            price_impact_bps: quote.price_impact_bps,
            ticks_crossed: 0,
        })
    }

    fn next_state_exact(&self, s: &Self::State, o: &Order) -> PricingResult<Self::State> {
        let quote = self.quote_exact(s, o)?;
        let mut next = s.clone();
        if o.token_in == s.pair.token0 {
            next.pair.reserve0 = s.pair.reserve0.saturating_add(o.amount_in);
            next.pair.reserve1 = s.pair.reserve1.saturating_sub(quote.amount_out);
        } else {
            next.pair.reserve1 = s.pair.reserve1.saturating_add(o.amount_in);
            next.pair.reserve0 = s.pair.reserve0.saturating_sub(quote.amount_out);
        }
        Ok(next)
    }

    fn fee_exact(&self, s: &Self::State, o: &Order) -> PricingResult<U256> {
        if s.pair.reserves_for(o.token_in).is_none() {
            return Err(PricingError::Reverts(RevertCondition::TokenNotInPool));
        }
        let after_fee = quote_common::apply_swap_fee(o.amount_in, s.fee_bps)
            .map_err(|_| PricingError::Reverts(RevertCondition::ZeroAmount))?;
        Ok(o.amount_in.saturating_sub(after_fee))
    }

    fn rounding_exact(&self) -> RoundingMode {
        RoundingMode::OutputDownFeeUp
    }

    fn revert_conditions(&self, s: &Self::State, o: &Order) -> Vec<RevertCondition> {
        let mut out = Vec::new();
        match s.pair.reserves_for(o.token_in) {
            None => out.push(RevertCondition::TokenNotInPool),
            Some((reserve_in, reserve_out)) => {
                if reserve_in.is_zero() || reserve_out.is_zero() {
                    out.push(RevertCondition::ZeroLiquidity);
                }
            }
        }
        if o.amount_in.is_zero() {
            out.push(RevertCondition::ZeroAmount);
        }
        out
    }

    fn state_dependencies(&self, s: &Self::State) -> StateDeps {
        StateDeps {
            pool: s.pool,
            reads_spot: true,
            reads_liquidity: false,
            reads_ticks: Vec::new(),
        }
    }
}
