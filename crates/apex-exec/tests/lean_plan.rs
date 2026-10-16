//! Task 8.5 R29: the lean executor's plan (`apex_exec::lean`), held to
//! `fixtures/lean_plans.json`, which `test/lean/LeanPlanFixture.t.sol` holds
//! the contract's decoder to as well. Neither side writes it.

use alloy_primitives::{address, keccak256, Address, B256};
use apex_exec::call::{CallPlan, ExecutorCall};
use apex_exec::lean::{
    encode_plan, lean_commitment, run_calldata, HopKind, LeanError, LeanHop, LeanLender, LeanPlan, SignedLeanPlan,
    LEAN_PLAN_VERSION, RUN_SELECTOR, WETH,
};
use serde_json::Value;

const EXECUTOR: Address = address!("5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f");
const CBBTC: Address = address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf");

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/lean_plans.json")).expect("the fixture parses")
}

fn number(v: &Value) -> u128 {
    v.as_str().expect("numbers are decimal strings").parse().expect("a u128")
}

fn plan_of(c: &Value) -> LeanPlan {
    let lender = match c["lender"].as_u64() {
        Some(0) => LeanLender::Balancer,
        Some(1) => LeanLender::Morpho,
        other => panic!("lender {other:?}"),
    };
    let hops = c["hops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| LeanHop {
            kind: match h["kind"].as_u64() {
                Some(0) => HopKind::UniswapV3Style,
                Some(1) => HopKind::PancakeV3,
                Some(2) => HopKind::AerodromeV2,
                other => panic!("kind {other:?}"),
            },
            pool: h["pool"].as_str().unwrap().parse().unwrap(),
            token_out: h["token_out"].as_str().unwrap().parse().unwrap(),
            min_out: number(&h["min_out"]),
        })
        .collect();
    LeanPlan {
        lender,
        amount: number(&c["amount"]),
        deadline: u64::try_from(number(&c["deadline"])).unwrap(),
        min_profit: number(&c["min_profit"]),
        hops,
    }
}

fn two_hops() -> LeanPlan {
    plan_of(&fixture()["cases"]["balancer_two_v3"])
}

#[test]
fn every_fixture_plan_encodes_to_its_bytes() {
    let f = fixture();
    for name in f["names"].as_array().unwrap() {
        let c = &f["cases"][name.as_str().unwrap()];
        let expected = alloy_primitives::hex::decode(c["encoded"].as_str().unwrap()).unwrap();
        assert_eq!(encode_plan(&plan_of(c)).unwrap(), expected, "{name}");
    }
}

#[test]
fn run_calldata_wraps_the_plan_as_abi_bytes() {
    let plan = two_hops();
    let bytes = encode_plan(&plan).unwrap();
    let data = run_calldata(&plan).unwrap();
    assert_eq!(RUN_SELECTOR, keccak256(b"run(bytes)")[..4]);
    assert_eq!(&data[..4], &RUN_SELECTOR);
    assert_eq!(data[4..36], B256::left_padding_from(&[0x20]).0, "the offset");
    assert_eq!(data[36..68], B256::left_padding_from(&(bytes.len() as u64).to_be_bytes()).0, "the length");
    assert_eq!(&data[68..68 + bytes.len()], &bytes[..]);
    assert_eq!(data.len(), 68 + bytes.len().div_ceil(32) * 32, "padded to a word");
    assert!(data[68 + bytes.len()..].iter().all(|b| *b == 0));
}

#[test]
fn the_encoder_refuses_what_the_contract_would() {
    let mut none = two_hops();
    none.hops.clear();
    assert_eq!(encode_plan(&none), Err(LeanError::NoHops));

    let mut late = two_hops();
    late.deadline = 1 << 40;
    assert_eq!(encode_plan(&late), Err(LeanError::DeadlineTooLarge(1 << 40)));
    late.deadline = (1 << 40) - 1;
    assert!(encode_plan(&late).is_ok(), "the largest deadline five bytes hold");

    let mut stray = two_hops();
    stray.hops[1].token_out = CBBTC;
    assert_eq!(encode_plan(&stray), Err(LeanError::LastHopNotWeth));
}

#[test]
fn the_commitment_is_the_chain_the_executor_the_version_and_the_bytes() {
    let plan = two_hops();
    let mut buf = Vec::new();
    buf.extend_from_slice(&B256::left_padding_from(&8453u64.to_be_bytes()).0);
    buf.extend_from_slice(&B256::left_padding_from(EXECUTOR.as_slice()).0);
    buf.extend_from_slice(&B256::left_padding_from(&[u8::try_from(LEAN_PLAN_VERSION).unwrap()]).0);
    buf.extend_from_slice(&keccak256(encode_plan(&plan).unwrap()).0);
    assert_eq!(lean_commitment(&plan, 8453, EXECUTOR).unwrap(), keccak256(&buf));
}

#[test]
fn the_commitment_moves_with_every_field() {
    let base = two_hops();
    let c = |p: &LeanPlan, chain: u64, at: Address| lean_commitment(p, chain, at).unwrap();
    let start = c(&base, 8453, EXECUTOR);
    assert_ne!(c(&base, 1, EXECUTOR), start, "the chain");
    assert_ne!(c(&base, 8453, WETH), start, "the executor");
    let mut p = base.clone();
    p.lender = LeanLender::Morpho;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "the lender");
    let mut p = base.clone();
    p.amount += 1;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "the amount");
    let mut p = base.clone();
    p.deadline += 1;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "the deadline");
    let mut p = base.clone();
    p.min_profit += 1;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "the floor");
    let mut p = base.clone();
    p.hops[0].kind = HopKind::PancakeV3;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "a hop's kind");
    let mut p = base.clone();
    p.hops[0].pool = WETH;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "a hop's pool");
    let mut p = base.clone();
    p.hops[0].token_out = WETH;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "a hop's token");
    let mut p = base;
    p.hops[1].min_out += 1;
    assert_ne!(c(&p, 8453, EXECUTOR), start, "a hop's minimum");
}

#[test]
fn a_signed_plan_with_another_commitment_is_refused() {
    let plan = two_hops();
    let declared = lean_commitment(&plan, 8453, EXECUTOR).unwrap();
    let recomputed = declared;
    let wrong = B256::repeat_byte(7);
    assert_eq!(
        SignedLeanPlan::check(plan.clone(), wrong, 8453, EXECUTOR).unwrap_err(),
        LeanError::CommitmentMismatch { declared: wrong, recomputed }
    );
    assert_eq!(SignedLeanPlan::check(plan, declared, 8453, EXECUTOR).unwrap().commitment(), declared);
}

#[test]
fn an_executor_call_from_a_lean_plan_carries_its_calldata() {
    let plan = two_hops();
    let declared = lean_commitment(&plan, 8453, EXECUTOR).unwrap();
    let call = ExecutorCall::from_lean(SignedLeanPlan::check(plan.clone(), declared, 8453, EXECUTOR).unwrap());
    assert_eq!(call.to(), EXECUTOR);
    assert_eq!(call.chain_id(), 8453);
    assert_eq!(call.data(), &run_calldata(&plan).unwrap()[..]);
    assert_eq!(call.commitment(), declared);
    assert_eq!(call.plan(), &CallPlan::Lean(plan));
}
