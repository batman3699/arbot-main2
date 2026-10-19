//! An armed backrun's executor call (R26, R29). Floors only stop a trade that
//! cannot repay its loan: once included, the fee is spent either way.

use crate::live::calls::lean_hop;
use crate::live::frontier::Cycle;
use crate::live::lenders::LenderChoice;
use alloy_primitives::{Address, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::lean::{lean_commitment, LeanPlan, SignedLeanPlan};
use apex_types::compat::u256_to_alloy;

/// The cycle at `size`. Hop 0 must return `mid`, hop 1's input: less, and the
/// cycle was not the one armed. Hop 1 must return the loan plus one, and the
/// plan a profit of at least one. An Aerodrome v2 hop is asked for its armed
/// quote instead (`mid`, or the cycle's `output`): its pool pays exactly that.
#[allow(clippy::too_many_arguments)]
pub fn armed_call(
    cycle: &Cycle,
    size: ethers_core::types::U256,
    mid: ethers_core::types::U256,
    output: ethers_core::types::U256,
    executor: Address,
    chain_id: u64,
    deadline: u64,
    lender: &LenderChoice,
) -> Result<ExecutorCall, String> {
    let (size, mid, output, one) = (u256_to_alloy(size), u256_to_alloy(mid), u256_to_alloy(output), U256::from(1));
    let repay = size.checked_add(one).ok_or("the size overflows")?;
    let plan = LeanPlan {
        lender: lender.lender,
        amount: u128::try_from(size).map_err(|_| format!("the size {size} does not fit 128 bits"))?,
        deadline,
        min_profit: 1,
        hops: vec![lean_hop(&cycle.legs[0], mid, mid)?, lean_hop(&cycle.legs[1], output, repay)?],
    };
    let declared = lean_commitment(&plan, chain_id, executor).map_err(|e| e.to_string())?;
    let checked = SignedLeanPlan::check(plan, declared, chain_id, executor).map_err(|e| e.to_string())?;
    Ok(ExecutorCall::from_lean(checked))
}
