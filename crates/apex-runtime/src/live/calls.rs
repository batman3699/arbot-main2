//! The production `CallBuilder` (Task 8.5 R6, R29): a live cycle as the one
//! `run(plan)` the plane simulates and signs, for the lean executor.
//!
//! # A plan is built from the state it was priced against, or not at all
//!
//! Each hop is quoted again here, against the book, by the same arithmetic the
//! candidate was priced by ([`LiveCycle::hop_outputs`]), and the cycle's return
//! must be **exactly** the candidate's expected output. If the book has moved
//! since, the candidate's price is not the price of any plan that could be built
//! now, so it is declined as stale rather than built against a different state —
//! last-mile would refuse it in any case, because the route's pools moved.
//!
//! # Exact minimums
//!
//! The lean executor spends each hop's whole output on the next, so nothing is
//! left in the middle token, and each hop's minimum is a bound:
//!
//! - **hop 0** spends the loan and must return at least its quote;
//! - **hop 1** spends what hop 0 returned and must return at least the larger
//!   of its slippage bound and what the cycle owes: the loan plus the
//!   commitment's minimum profit.
//!
//! In the priced state each hop returns its quote to the wei (the local pricer
//! matches the venues exactly), so the plan returns the candidate's output. In
//! any other state it either still clears its floor or reverts.
//!
//! # Every venue, by the callback its pools make
//!
//! Uniswap v3 and both Slipstream factories pay through `uniswapV3SwapCallback`,
//! PancakeSwap v3 through `pancakeV3SwapCallback`, and Aerodrome v2 is paid
//! first and asked for exactly its quote. The lean executor supports all five
//! by its code; there is no registry to read.
//!
//! # The lender is the commitment's
//!
//! Morpho Blue or Balancer's vault ([`crate::live::lenders`]); a commitment
//! naming any other source — or none — is refused rather than built into a plan
//! that reverts.

use crate::live::book::PoolBook;
use crate::live::frontier::{Cycle, Leg};
use crate::live::inventory::Venue;
use crate::live::pricing::LiveCycle;
use crate::plane::{CallBuilder, Decline};
use alloy_primitives::{B256, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::encode::apply_slippage;
use apex_exec::lean::{lean_commitment, HopKind, LeanHop, LeanPlan, SignedLeanPlan};
use apex_types::candidate::Candidate;
use apex_types::commitment::ExecutionCommitment;
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::time::DurationNanos;
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct LiveCallBuilder {
    book: Arc<PoolBook>,
    /// By route hash: a candidate carries a commitment, not an id.
    cycles: BTreeMap<B256, Cycle>,
}

impl LiveCallBuilder {
    pub fn new(book: Arc<PoolBook>, cycles: impl IntoIterator<Item = Cycle>) -> Self {
        let cycles = cycles.into_iter().map(|c| (c.commitment.route_hash, c)).collect();
        Self { book, cycles }
    }
}

fn refuse(detail: impl Into<String>) -> Decline {
    Decline::Uncommittable { detail: detail.into() }
}

fn stale() -> Decline {
    Decline::StaleState { age: DurationNanos(0) }
}

fn u128_of(v: U256, what: &str) -> Result<u128, Decline> {
    u128::try_from(v).map_err(|_| refuse(format!("{what} {v} does not fit 128 bits")))
}

impl CallBuilder for LiveCallBuilder {
    fn build(&self, c: &Candidate, k: &ExecutionCommitment) -> Result<ExecutorCall, Decline> {
        if k.route_hash != c.route.route_hash {
            return Err(refuse("the commitment is for another route"));
        }
        let cycle = self.cycles.get(&k.route_hash).ok_or_else(|| refuse("no live cycle for this route"))?;
        let lender = crate::live::lenders::by_id(k.flash_source)
            .ok_or_else(|| refuse(format!("flash source {} is not wired", k.flash_source.0)))?;
        let [input] = k.exact_inputs[..] else {
            return Err(refuse(format!("a cycle has one exact input, not {}", k.exact_inputs.len())));
        };
        let [_, slippage_out] = k.slippage_constraints[..] else {
            return Err(refuse(format!(
                "a two-hop cycle has two slippage bounds, not {}",
                k.slippage_constraints.len()
            )));
        };

        let snapshot = self.book.snapshot();
        let live = LiveCycle::new(cycle, &snapshot).ok_or_else(stale)?;
        let [mid, out] = live.hop_outputs(u256_to_ethers(input)).ok_or_else(stale)?;
        let (mid, out) = (u256_to_alloy(mid), u256_to_alloy(out));
        if out != c.expected_output {
            return Err(stale());
        }
        let floor = input.saturating_add(k.min_profit);
        if out < floor {
            return Err(refuse(format!(
                "the cycle returns {out}, below the loan plus its minimum profit, {floor}"
            )));
        }
        let bound = apply_slippage(out, slippage_out).map_err(|e| refuse(e.to_string()))?;

        let plan = LeanPlan {
            lender: lender.lender,
            amount: u128_of(input, "the loan")?,
            deadline: k.deadline,
            min_profit: u128_of(k.min_profit, "the minimum profit")?,
            hops: vec![
                lean_hop(&cycle.legs[0], mid).map_err(refuse)?,
                lean_hop(&cycle.legs[1], floor.max(bound)).map_err(refuse)?,
            ],
        };
        let declared = lean_commitment(&plan, k.chain_id.0, k.executor_address).map_err(|e| refuse(e.to_string()))?;
        let checked = SignedLeanPlan::check(plan, declared, k.chain_id.0, k.executor_address)
            .map_err(|e| refuse(e.to_string()))?;
        Ok(ExecutorCall::from_lean(checked))
    }
}

/// One hop of a cycle as the lean executor's: shared by the reactive plan and
/// the armed one (R26).
pub fn lean_hop(leg: &Leg, min_out: U256) -> Result<LeanHop, String> {
    let kind = match leg.venue {
        Venue::UniswapV3 | Venue::Slipstream | Venue::SlipstreamV3 => HopKind::UniswapV3Style,
        Venue::PancakeV3 => HopKind::PancakeV3,
        Venue::AerodromeV2 => HopKind::AerodromeV2,
    };
    let min_out = u128::try_from(min_out).map_err(|_| format!("a hop's minimum {min_out} does not fit 128 bits"))?;
    Ok(LeanHop { kind, pool: leg.pool, token_out: leg.token_out, min_out })
}
