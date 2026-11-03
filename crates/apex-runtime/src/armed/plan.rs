//! An armed backrun's executor call (R26, R29). Floors only stop a trade that
//! cannot repay its loan: once included, the fee is spent either way.

use crate::live::book::PoolSnapshot;
use crate::live::calls::{lean2_hop, lean3_hop, lean_hop};
use crate::live::gas::SettlementGas;
use crate::live::frontier::Cycle;
use crate::live::lenders::LenderChoice;
use crate::live::pricing::LiveCycle;
use alloy_primitives::{Address, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::lean::{lean_commitment, LeanHop, LeanPlan, SignedLeanPlan};
use apex_exec::lean2::{lean2_commitment, Lean2Hop, Lean2Lender, Lean2Plan, SignedLean2Plan, StartToken};
use apex_exec::lean3::{lean3_commitment, Lean3Plan, SignedLean3Plan};
use apex_math::walk::{quote, walk, to_start, WalkLeg, WalkPrice, Walked};
use apex_types::compat::u256_to_alloy;
use ethers_core::types::U256 as EthersU256;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The cycle at `size`. Hop 0 must return `mid`, its output at the threshold:
/// less, and the cycle was not the one armed. Hop 1 sells all hop 0 returns,
/// and must return the loan plus one, and the plan a profit of at least one.
/// An Aerodrome v2 hop is asked for its armed quote instead (`mid`, or the
/// cycle's `output`): its pool pays exactly that.
#[allow(clippy::too_many_arguments)]
pub fn armed_call(
    cycle: &Cycle,
    size: EthersU256,
    mid: EthersU256,
    output: EthersU256,
    executor: Address,
    chain_id: u64,
    deadline: u64,
    lender: &LenderChoice,
) -> Result<ExecutorCall, String> {
    let hops = armed_hops(cycle, size, mid, output)?;
    let size = u256_to_alloy(size);
    let plan = LeanPlan {
        lender: lender.lender,
        amount: u128::try_from(size).map_err(|_| format!("the size {size} does not fit 128 bits"))?,
        deadline,
        min_profit: 1,
        hops: hops.to_vec(),
    };
    let declared = lean_commitment(&plan, chain_id, executor).map_err(|e| e.to_string())?;
    let checked = SignedLeanPlan::check(plan, declared, chain_id, executor).map_err(|e| e.to_string())?;
    Ok(ExecutorCall::from_lean(checked))
}

/// The armed call for `LeanExecutor2` (R37): `armed_call`'s floors, in the
/// cycle's start token. A route with a v4 hop borrows from PoolManager
/// whatever `lender` says: its swap runs only inside PoolManager's unlock.
#[allow(clippy::too_many_arguments)]
pub fn armed_call2(
    cycle: &Cycle,
    size: EthersU256,
    mid: EthersU256,
    output: EthersU256,
    executor: Address,
    chain_id: u64,
    deadline: u64,
    lender: Lean2Lender,
) -> Result<ExecutorCall, String> {
    let start = match cycle.start {
        s if s == crate::live::frontier::WETH => StartToken::Weth,
        s if s == apex_exec::lean2::USDC => StartToken::Usdc,
        s => return Err(format!("{s:#x} is not a start token LeanExecutor2 takes")),
    };
    let v4 = cycle.legs.iter().any(|l| l.venue == crate::live::inventory::Venue::UniswapV4);
    let hops = armed_hops2(cycle, size, mid, output)?;
    let size = u256_to_alloy(size);
    let plan = Lean2Plan {
        lender: if v4 { Lean2Lender::PoolManager } else { lender },
        start,
        amount: u128::try_from(size).map_err(|_| format!("the size {size} does not fit 128 bits"))?,
        deadline,
        min_profit: 1,
        hops: hops.to_vec(),
    };
    let declared = lean2_commitment(&plan, chain_id, executor).map_err(|e| e.to_string())?;
    let checked = SignedLean2Plan::check(plan, declared, chain_id, executor).map_err(|e| e.to_string())?;
    Ok(ExecutorCall::from_lean2(checked))
}

/// R40.1: what a sized plan carries beside its hops, and what predicting the
/// executor's choice needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizedTerms {
    /// The most the executor may size to: the size cap, or the lender's
    /// holding when that is less.
    pub cap: EthersU256,
    /// The book's USDC units a wei (`start_per_eth`); None for WETH.
    pub rate: Option<(EthersU256, EthersU256)>,
    pub overhead: u32,
    pub swap_gas: u32,
    pub l1_bound: u64,
    pub min_net: u64,
    pub max_steps: u8,
    /// The executor's reads and walk, as the gas model expects them: for
    /// predicting its early exit, and not in the plan.
    pub read_gas: u64,
}

/// The cushion on the USDC rate the plan carries: 5%, for the window it waits.
pub const RATE_CUSHION_PERCENT: u64 = 5;

impl SizedTerms {
    /// The plan's rate: start units per 1e18 wei, the book's plus
    /// [`RATE_CUSHION_PERCENT`], rounded up. None for WETH, which the plan
    /// carries as 0.
    pub fn plan_rate(&self) -> Option<EthersU256> {
        let (num, den) = self.rate?;
        let scaled = ethers_core::types::U512::from(num) * ethers_core::types::U512::from(10u64).pow(18.into())
            * ethers_core::types::U512::from(100 + RATE_CUSHION_PERCENT);
        let d = ethers_core::types::U512::from(den) * ethers_core::types::U512::from(100u64);
        EthersU256::try_from((scaled + d - 1) / d).ok()
    }
}

/// R40.1: what `LeanExecutor3` would do on `book`, as the mirror computes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizedOutcome {
    /// It trades `size` for `gross` over the loan, in the start token. What a
    /// walk with no cap, and one of 64 steps, would add to the gross is what
    /// each limit left.
    Trade { size: EthersU256, gross: EthersU256, walked: Walked, beyond_cap: EthersU256, beyond_steps: EthersU256 },
    /// Its `NoTrade` reason: 0 no gap, 1 below the floor, 2 unsupported.
    NoTrade(u8),
}

fn walk_leg<'a>(leg: &crate::live::frontier::Leg, p: &'a PoolSnapshot, gas: &SettlementGas) -> WalkLeg<'a> {
    match &p.reserves {
        Some(r) => {
            let (reserve_in, reserve_out) = if leg.zero_for_one { (r.reserve0, r.reserve1) } else { (r.reserve1, r.reserve0) };
            WalkLeg::Reserves { reserve_in, reserve_out, fee_ppm: p.state.fee_ppm }
        }
        None => WalkLeg::Cl { state: &p.state, ladder: &p.ladder, zero_for_one: leg.zero_for_one, cross_gas: cross_gas(leg, gas) },
    }
}

/// One tick crossed on `leg`: the gas model's mean in its direction.
pub fn cross_gas(leg: &crate::live::frontier::Leg, gas: &SettlementGas) -> u64 {
    if leg.zero_for_one { gas.crossing_down.of(leg.venue) } else { gas.crossing_up.of(leg.venue) }
}

/// The executor's choice at `book` and `wei_per_gas`: the walk, its exact
/// quote, and its early exit predicted with `terms.read_gas`.
pub fn sized_outcome(
    cycle: &Cycle,
    book: &BTreeMap<Address, Arc<PoolSnapshot>>,
    terms: &SizedTerms,
    wei_per_gas: EthersU256,
    gas: &SettlementGas,
) -> SizedOutcome {
    let (Some(a), Some(b)) = (book.get(&cycle.legs[0].pool), book.get(&cycle.legs[1].pool)) else { return SizedOutcome::NoTrade(2) };
    let legs = [walk_leg(&cycle.legs[0], a, gas), walk_leg(&cycle.legs[1], b, gas)];
    let price = WalkPrice { wei_per_gas, rate: terms.plan_rate() };
    let gross_at = |cap: EthersU256, steps: u32| -> Option<(Walked, EthersU256)> {
        let w = walk(legs, cap, price, steps)?;
        let [_, out] = quote(legs, w.size)?;
        Some((w, out.saturating_sub(w.size)))
    };
    let Some((walked, gross)) = gross_at(terms.cap, u32::from(terms.max_steps)) else { return SizedOutcome::NoTrade(0) };
    if walked.size.is_zero() {
        return SizedOutcome::NoTrade(0);
    }
    let ahead = terms.read_gas + u64::from(terms.swap_gas) + u64::from(terms.overhead) + walked.cross_gas;
    let cost = EthersU256::from(ahead).saturating_mul(wei_per_gas).saturating_add(EthersU256::from(terms.l1_bound));
    let floor = to_start(cost, price.rate).unwrap_or(EthersU256::MAX).saturating_add(EthersU256::from(terms.min_net));
    if gross < floor {
        return SizedOutcome::NoTrade(1);
    }
    let more = |cap, steps| gross_at(cap, steps).map_or(EthersU256::zero(), |(_, g)| g.saturating_sub(gross));
    SizedOutcome::Trade {
        size: walked.size,
        gross,
        walked,
        beyond_cap: more(EthersU256::MAX, u32::from(terms.max_steps)),
        beyond_steps: more(terms.cap, 64),
    }
}

/// R40.1: the call for `LeanExecutor3`: no size, no hop minimums. A route with
/// a v4 hop borrows from PoolManager whatever `lender` says, as R37's does.
#[allow(clippy::too_many_arguments)]
pub fn sized_call(
    cycle: &Cycle,
    book: &BTreeMap<Address, Arc<PoolSnapshot>>,
    terms: &SizedTerms,
    executor3: Address,
    chain_id: u64,
    deadline: u64,
    lender: Lean2Lender,
    gas: &SettlementGas,
) -> Result<ExecutorCall, String> {
    let start = match cycle.start {
        s if s == crate::live::frontier::WETH => StartToken::Weth,
        s if s == apex_exec::lean2::USDC => StartToken::Usdc,
        s => return Err(format!("{s:#x} is not a start token LeanExecutor3 takes")),
    };
    let v4 = cycle.legs.iter().any(|l| l.venue == crate::live::inventory::Venue::UniswapV4);
    let hop = |i: usize| {
        let leg = &cycle.legs[i];
        let spacing = book.get(&leg.pool).map(|p| p.state.tick_spacing).ok_or_else(|| format!("{:#x} is not in the book", leg.pool))?;
        lean3_hop(leg, spacing, cross_gas(leg, gas))
    };
    let narrow = |v: EthersU256, what: &str| u128::try_from(u256_to_alloy(v)).map_err(|_| format!("the {what} {v} does not fit 128 bits"));
    let plan = Lean3Plan {
        lender: if v4 { Lean2Lender::PoolManager } else { lender },
        start,
        cap: narrow(terms.cap, "cap")?,
        deadline,
        rate: terms.plan_rate().map_or(Ok(0), |r| narrow(r, "rate"))?,
        gas_overhead: terms.overhead,
        swap_gas: terms.swap_gas,
        l1_bound: terms.l1_bound,
        min_net: terms.min_net,
        max_steps: terms.max_steps,
        hops: [hop(0)?, hop(1)?],
    };
    let declared = lean3_commitment(&plan, chain_id, executor3).map_err(|e| e.to_string())?;
    let checked = SignedLean3Plan::check(plan, declared, chain_id, executor3).map_err(|e| e.to_string())?;
    Ok(ExecutorCall::from_lean3(checked))
}

/// The armed plan's two hops, as `LeanExecutor2` runs them (R37).
fn armed_hops2(cycle: &Cycle, size: EthersU256, mid: EthersU256, output: EthersU256) -> Result<[Lean2Hop; 2], String> {
    let (size, mid, output) = (u256_to_alloy(size), u256_to_alloy(mid), u256_to_alloy(output));
    let repay = size.checked_add(U256::from(1)).ok_or("the size overflows")?;
    Ok([lean2_hop(&cycle.legs[0], mid, mid)?, lean2_hop(&cycle.legs[1], output, repay)?])
}

/// The armed plan's two hops, each with its minimum.
fn armed_hops(cycle: &Cycle, size: EthersU256, mid: EthersU256, output: EthersU256) -> Result<[LeanHop; 2], String> {
    let (size, mid, output) = (u256_to_alloy(size), u256_to_alloy(mid), u256_to_alloy(output));
    let repay = size.checked_add(U256::from(1)).ok_or("the size overflows")?;
    Ok([lean_hop(&cycle.legs[0], mid, mid)?, lean_hop(&cycle.legs[1], output, repay)?])
}

/// The gross of the armed plan on `book`, in its start token (R36, R37): its
/// hops run as the lean executors run them, against the minimums
/// [`armed_call`] and [`armed_call2`] encode (the same for every hop both
/// take; a v4 hop is exact input, as v3's). An error names why the plan would
/// revert there.
pub fn armed_gross(
    cycle: &Cycle,
    book: &BTreeMap<Address, Arc<PoolSnapshot>>,
    size: EthersU256,
    mid: EthersU256,
    output: EthersU256,
) -> Result<EthersU256, String> {
    let [a, b] = armed_hops2(cycle, size, mid, output)?;
    let live = LiveCycle::new(cycle, book).ok_or("a pool left its ladder")?;
    let q = live.quote_lean(size, [EthersU256::from(a.min_out), EthersU256::from(b.min_out)]).ok_or("a hop returns less than its minimum: it would revert")?;
    q.outputs[1].checked_sub(size).filter(|g| !g.is_zero()).ok_or_else(|| "below the loan plus one: it would revert".into())
}
