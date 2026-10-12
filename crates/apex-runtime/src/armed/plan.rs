//! An armed backrun's executor call (R26). Floors only stop a trade that
//! cannot repay its loan: once included, the fee is spent either way.

use crate::live::book::PoolSnapshot;
use crate::live::calls::encode_step;
use crate::live::frontier::{Cycle, BALANCER_VAULT};
use alloy_primitives::{Address, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider};
use apex_exec::encode::PlanEncoder;
use apex_exec::sign::SignedPlan;
use apex_types::compat::u256_to_alloy;

/// The cycle at `size`. Hop 1 must return `mid`, hop 2's fixed input: less,
/// and hop 2 could not run anyway. Hop 2 must return the loan plus one, and
/// the plan a profit of at least one.
pub fn armed_call(
    cycle: &Cycle,
    pools: [&PoolSnapshot; 2],
    size: ethers_core::types::U256,
    mid: ethers_core::types::U256,
    executor: Address,
    chain_id: u64,
    deadline: u64,
) -> Result<ExecutorCall, String> {
    let (size, mid, one) = (u256_to_alloy(size), u256_to_alloy(mid), U256::from(1));
    let repay = size.checked_add(one).ok_or("the size overflows")?;
    let steps = vec![
        encode_step(&cycle.legs[0], pools[0], size, mid, executor, deadline)?,
        encode_step(&cycle.legs[1], pools[1], mid, repay, executor, deadline)?,
    ];
    let loan = Loan { token: cycle.start, amount: size, provider: LoanProvider::Balancer, provider_addr: BALANCER_VAULT };
    let plan = PlanEncoder::build(loan, steps, 0, one, chain_id, deadline);
    let declared = plan_commitment(&plan, chain_id, executor);
    let checked = SignedPlan::check(plan, declared, chain_id, executor).map_err(|e| e.to_string())?;
    Ok(ExecutorCall::from_checked(checked))
}
