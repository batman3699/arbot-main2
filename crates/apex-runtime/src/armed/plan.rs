//! An armed backrun's executor call (R26, R29). Floors only stop a trade that
//! cannot repay its loan: once included, the fee is spent either way.

use crate::live::book::PoolSnapshot;
use crate::live::calls::lean_hop;
use crate::live::frontier::Cycle;
use crate::live::lenders::LenderChoice;
use crate::live::pricing::LiveCycle;
use alloy_primitives::{Address, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::lean::{lean_commitment, LeanHop, LeanPlan, SignedLeanPlan};
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

/// The armed plan's two hops, each with its minimum.
fn armed_hops(cycle: &Cycle, size: EthersU256, mid: EthersU256, output: EthersU256) -> Result<[LeanHop; 2], String> {
    let (size, mid, output) = (u256_to_alloy(size), u256_to_alloy(mid), u256_to_alloy(output));
    let repay = size.checked_add(U256::from(1)).ok_or("the size overflows")?;
    Ok([lean_hop(&cycle.legs[0], mid, mid)?, lean_hop(&cycle.legs[1], output, repay)?])
}

/// The WETH gross of the armed plan on `book` (R36): its hops run as the lean
/// executor runs them, against the minimums [`armed_call`] encodes. An error
/// names why the plan would revert there.
pub fn armed_gross(
    cycle: &Cycle,
    book: &BTreeMap<Address, Arc<PoolSnapshot>>,
    size: EthersU256,
    mid: EthersU256,
    output: EthersU256,
) -> Result<EthersU256, String> {
    let [a, b] = armed_hops(cycle, size, mid, output)?;
    let live = LiveCycle::new(cycle, book).ok_or("a pool left its ladder")?;
    let q = live.quote_lean(size, [EthersU256::from(a.min_out), EthersU256::from(b.min_out)]).ok_or("a hop returns less than its minimum: it would revert")?;
    q.outputs[1].checked_sub(size).filter(|g| !g.is_zero()).ok_or_else(|| "below the loan plus one: it would revert".into())
}
