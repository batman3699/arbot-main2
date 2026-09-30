//! **The one call a ticket makes** (§25, INV-06).
//!
//! The plane builds it once and hands the same value to the simulator and the
//! signer. What these tests pin is that there is no way to hand either of them
//! bytes that are not the encoding of a commitment-checked plan, aimed at the
//! deployment it was checked for.

use alloy_primitives::{address, Address, B256, U256};
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider, Op, PlanV2, Step};
use apex_exec::encode::{start_v2_calldata, START_V2_SELECTOR};
use apex_exec::sign::SignedPlan;

const EXECUTOR: Address = address!("DbFB219b4F1CE08fA61C5cD3c08C1307760cAec6");
const BASE: u64 = 8453;

fn plan() -> PlanV2 {
    PlanV2 {
        loans: vec![Loan {
            token: address!("4200000000000000000000000000000000000006"),
            amount: U256::from(100_000_000_000_000_000u128),
            provider: LoanProvider::Balancer,
            provider_addr: address!("BA12222222228d8Ba445958a75a0704d566BF2C8"),
        }],
        cycle_slippage_bps: 30,
        steps: vec![Step { op: Op::UniV3, data: vec![0x01, 0x02, 0x03] }],
        min_profit: U256::from(1u64),
        declared_residue: U256::ZERO,
        chain_id: BASE,
        deadline: 1_781_049_614,
    }
}

fn checked() -> SignedPlan {
    let p = plan();
    let declared = plan_commitment(&p, BASE, EXECUTOR);
    SignedPlan::check(p, declared, BASE, EXECUTOR).expect("the commitment matches")
}

/// The call is the checked plan's own encoding, aimed at the deployment the
/// commitment was recomputed for.
#[test]
fn a_call_is_the_encoding_of_the_plan_it_was_checked_against() {
    let signed = checked();
    let commitment = signed.commitment();
    let call = ExecutorCall::from_checked(signed);

    assert_eq!(call.to(), EXECUTOR);
    assert_eq!(call.chain_id(), BASE);
    assert_eq!(call.commitment(), commitment);
    assert_eq!(call.data(), start_v2_calldata(&plan(), commitment).as_slice());
    assert_eq!(&call.data()[..4], &START_V2_SELECTOR);
    assert_eq!(call.plan(), &plan());
}

/// The commitment in the calldata is the one the contract will recompute: it
/// sits in the tuple head's sixth word, which is where `abi.decode` reads it.
#[test]
fn the_calldata_carries_the_checked_commitment_where_the_contract_reads_it() {
    let call = ExecutorCall::from_checked(checked());
    // selector | offset word | 5 head words before `commitment`
    let at = 4 + 32 + 5 * 32;
    assert_eq!(B256::from_slice(&call.data()[at..at + 32]), call.commitment());
}

/// A plan that does not hash to its declared commitment never becomes a
/// `SignedPlan`, and `from_checked` takes nothing else — so it never becomes a
/// call either.
#[test]
fn a_mismatched_commitment_never_becomes_a_call() {
    let p = plan();
    let wrong = plan_commitment(&p, BASE, address!("0000000000000000000000000000000000000001"));
    assert!(SignedPlan::check(p, wrong, BASE, EXECUTOR).is_err());
}
