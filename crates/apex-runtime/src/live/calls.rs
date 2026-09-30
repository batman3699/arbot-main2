//! The production `CallBuilder` (Task 8.5 R6): a live cycle as the one
//! `startV2(PlanV2)` the plane simulates and signs.
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
//! # Exact amounts, no dust
//!
//! The executor's settlement check covers only the borrowed token, so a surplus
//! of the middle token is dust left in the contract rather than a revert. A
//! per-hop slippage allowance on the first hop would buy tolerance with dust
//! worth the same basis points in the one world the trade is priced for — and
//! at edges of 1–10 bps that is most of the profit. So:
//!
//! - **hop 1** spends the input and must return at least its quote;
//! - **hop 2** spends exactly that quote, and must return at least the larger
//!   of its slippage bound and what the cycle owes: the loan plus the
//!   commitment's minimum profit.
//!
//! In the priced state hop 1 returns its quote to the wei (the local pricer
//! matches the venues' quoters exactly), so nothing is left over. In any other
//! state the plan either still clears its floor or reverts; it cannot land at
//! less.
//!
//! # Two venues and one lender, and anything else is refused
//!
//! - **Uniswap v3**: the `UNIV3` op, a one-hop path through the executor's
//!   SwapRouter02. Tokens and fee identify the pool.
//! - **Slipstream**: the `GENERIC` op through [`SLIPSTREAM_ADAPTER`], calling
//!   the router's `exactInputSingle`. Tokens and **tick spacing** identify the
//!   pool, and the swap pays the executor.
//! - **The loan**: Balancer V2, the only lender wired. The executor takes
//!   exactly one loan, so a commitment naming any other source — or none — is
//!   refused rather than built into a plan that reverts.

use crate::live::book::PoolBook;
use crate::live::frontier::{Cycle, Leg, BALANCER_FLASH, BALANCER_VAULT, SLIPSTREAM_ADAPTER};
use crate::live::inventory::Venue;
use crate::live::pricing::LiveCycle;
use crate::plane::{CallBuilder, Decline};
use alloy_primitives::{B256, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider, Op, Step};
use apex_exec::encode::{
    apply_slippage, generic_step, slipstream_exact_input_single, univ3_path, univ3_step, PathHop,
    PlanEncoder, SlipstreamSwap,
};
use apex_exec::sign::SignedPlan;
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

impl CallBuilder for LiveCallBuilder {
    fn build(&self, c: &Candidate, k: &ExecutionCommitment) -> Result<ExecutorCall, Decline> {
        if k.route_hash != c.route.route_hash {
            return Err(refuse("the commitment is for another route"));
        }
        let cycle = self.cycles.get(&k.route_hash).ok_or_else(|| refuse("no live cycle for this route"))?;
        if k.flash_source != BALANCER_FLASH {
            return Err(refuse(format!(
                "flash source {} is not wired: the executor takes one loan, and only Balancer's is",
                k.flash_source.0
            )));
        }
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
        let live = LiveCycle::new(cycle, &snapshot, 0).ok_or_else(stale)?;
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

        let steps = vec![
            self.step(&cycle.legs[0], input, mid, k)?,
            self.step(&cycle.legs[1], mid, floor.max(bound), k)?,
        ];
        let loan = Loan {
            token: cycle.start,
            amount: input,
            provider: LoanProvider::Balancer,
            provider_addr: BALANCER_VAULT,
        };
        // Zero: the hops' own bounds are the slippage policy, and the executor
        // refuses a cycle-level figure above its configured maximum.
        let plan = PlanEncoder::build(loan, steps, 0, k.min_profit, k.chain_id.0, k.deadline);
        let declared = plan_commitment(&plan, k.chain_id.0, k.executor_address);
        let checked = SignedPlan::check(plan, declared, k.chain_id.0, k.executor_address)
            .map_err(|e| refuse(e.to_string()))?;
        Ok(ExecutorCall::from_checked(checked))
    }
}

impl LiveCallBuilder {
    /// One hop as the executor's step for its venue.
    fn step(&self, leg: &Leg, amount_in: U256, min_out: U256, k: &ExecutionCommitment) -> Result<Step, Decline> {
        let pool = self.book.get(leg.pool).ok_or_else(stale)?;
        match leg.venue {
            Venue::UniswapV3 => {
                // The chain's fee, read with the pool's state: the router finds
                // the pool by tokens and fee, and the inventory's figure is only
                // what the pool was admitted under.
                let hop = PathHop { token_in: leg.token_in, fee: pool.state.fee_ppm };
                let path = univ3_path(&[hop], leg.token_out).map_err(|e| refuse(e.to_string()))?;
                let data = univ3_step(&path, amount_in, min_out).map_err(|e| refuse(e.to_string()))?;
                Ok(Step { op: Op::UniV3, data })
            }
            Venue::Slipstream => {
                let call = slipstream_exact_input_single(&SlipstreamSwap {
                    token_in: leg.token_in,
                    token_out: leg.token_out,
                    tick_spacing: pool.state.tick_spacing,
                    recipient: k.executor_address,
                    deadline: k.deadline,
                    amount_in,
                    min_out,
                })
                .map_err(|e| refuse(e.to_string()))?;
                Ok(Step {
                    op: Op::Generic,
                    data: generic_step(SLIPSTREAM_ADAPTER, leg.token_in, amount_in, &call),
                })
            }
        }
    }
}
