use std::str::FromStr;

use arb_exec::abi::ExecutorPlan;
use arb_exec::abi_fixture::{fixture_plan_v2, START_V2_SIGNATURE};
use ethers::abi::{AbiDecode, AbiEncode};
use ethers::types::H256;
use ethers::utils::keccak256;

const EXPECTED_SELECTOR: [u8; 4] = [0xf9, 0x62, 0x82, 0x44];
const EXPECTED_CALLDATA_HASH: &str =
    "0xae440720c6c478bb50ba47890e47a0b1502204f62fc3f0eec99a93633f201694";

#[test]
fn encodes_start_v2_plan_v2_roundtrip() {
    let plan = fixture_plan_v2();
    let selector = &keccak256(START_V2_SIGNATURE)[..4];
    let encoded_plan = plan.clone().encode();
    let mut calldata = Vec::with_capacity(selector.len() + encoded_plan.len());
    calldata.extend_from_slice(selector);
    calldata.extend_from_slice(&encoded_plan);

    let expected_selector = &keccak256(START_V2_SIGNATURE)[..4];
    assert_eq!(expected_selector, EXPECTED_SELECTOR.as_slice());
    assert_eq!(&calldata[..4], EXPECTED_SELECTOR.as_slice());

    let decoded = ExecutorPlan::decode(&calldata[4..]).expect("decode PlanV2");
    assert_eq!(decoded.loans.len(), plan.loans.len());
    let decoded_loan = &decoded.loans[0];
    let expected_loan = &plan.loans[0];
    assert_eq!(decoded_loan.token, expected_loan.token);
    assert_eq!(decoded_loan.amount, expected_loan.amount);
    assert_eq!(decoded_loan.provider, expected_loan.provider);
    assert_eq!(decoded_loan.provider_addr, expected_loan.provider_addr);
    assert_eq!(decoded.cycle_slippage_bps, plan.cycle_slippage_bps);
    assert_eq!(decoded.steps.len(), plan.steps.len());
    for (decoded_step, expected_step) in decoded.steps.iter().zip(plan.steps.iter()) {
        assert_eq!(decoded_step.op, expected_step.op);
        assert_eq!(decoded_step.data, expected_step.data);
    }
    assert_eq!(decoded.min_profit, plan.min_profit);

    let hash = H256::from(keccak256(&calldata));
    let expected = H256::from_str(EXPECTED_CALLDATA_HASH).expect("valid expected hash");
    assert_eq!(hash, expected);
}
