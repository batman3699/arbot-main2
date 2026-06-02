use arb_exec::abi_fixture::{fixture_plan_v2, START_V2_SIGNATURE};
use ethers::abi::AbiEncode;
use ethers::utils::keccak256;

fn main() {
    let plan = fixture_plan_v2();
    let selector = &keccak256(START_V2_SIGNATURE)[..4];
    let encoded_plan = plan.encode();
    let mut calldata = Vec::with_capacity(selector.len() + encoded_plan.len());
    calldata.extend_from_slice(selector);
    calldata.extend_from_slice(&encoded_plan);

    println!("selector=0x{}", hex::encode(selector));
    println!("calldata=0x{}", hex::encode(&calldata));
    println!("calldata_keccak256=0x{}", hex::encode(keccak256(&calldata)));
}
