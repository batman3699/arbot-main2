//! Task 8.5 R40.1: the third lean executor's plan (`apex_exec::lean3`), held to
//! `fixtures/lean3_plans.json`, which `test/lean/Lean3PlanFixture.t.sol` holds
//! the contract's decoder to as well. Neither side writes it.

use alloy_primitives::{address, keccak256, Address, B256};
use apex_exec::call::{CallPlan, ExecutorCall};
use apex_exec::lean::RUN_SELECTOR;
use apex_exec::lean2::{Lean2Lender, StartToken, USDC};
use apex_exec::lean3::{
    encode_plan3, lean3_commitment, run3_calldata, Hop3Kind, Lean3Error, Lean3Hop, Lean3Plan, SignedLean3Plan,
    LEAN3_PLAN_LEN, LEAN3_PLAN_VERSION,
};
use serde_json::Value;

const EXECUTOR: Address = address!("5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f");
const AXIS: Address = address!("5be75e3B0aFF5d1a042A5363Ae36acB1d6D755Ea");
const HOOK: Address = address!("b0b24b89DB0DafbE43c5B40226B63a179F592880");
const AXIS_V3: Address = address!("e1bfc5ada1125718b53b2affc638ba9981f6070f");

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/lean3_plans.json")).expect("the fixture parses")
}

fn number(v: &Value) -> u128 {
    v.as_str().expect("numbers are decimal strings").parse().expect("a u128")
}

fn small<T: TryFrom<u64>>(v: &Value) -> T {
    T::try_from(v.as_u64().expect("a number")).ok().expect("in range")
}

fn plan_of(c: &Value) -> Lean3Plan {
    let lender = match c["lender"].as_u64() {
        Some(0) => Lean2Lender::Balancer,
        Some(1) => Lean2Lender::Morpho,
        Some(2) => Lean2Lender::PoolManager,
        other => panic!("lender {other:?}"),
    };
    let start = match c["start"].as_u64() {
        Some(0) => StartToken::Weth,
        Some(1) => StartToken::Usdc,
        other => panic!("start {other:?}"),
    };
    let hop = |h: &Value| Lean3Hop {
        kind: match h["kind"].as_u64() {
            Some(0) => Hop3Kind::UniswapV3,
            Some(1) => Hop3Kind::PancakeV3,
            Some(2) => Hop3Kind::AerodromeV2,
            Some(3) => Hop3Kind::UniswapV4,
            Some(4) => Hop3Kind::Slipstream,
            other => panic!("kind {other:?}"),
        },
        pool: h["pool"].as_str().unwrap().parse().unwrap(),
        token_out: h["token_out"].as_str().unwrap().parse().unwrap(),
        fee: small(&h["fee"]),
        tick_spacing: i32::try_from(h["tick_spacing"].as_i64().unwrap()).unwrap(),
        cross_gas: small(&h["cross_gas"]),
    };
    let hops = c["hops"].as_array().unwrap();
    Lean3Plan {
        lender,
        start,
        cap: number(&c["cap"]),
        deadline: u64::try_from(number(&c["deadline"])).unwrap(),
        rate: number(&c["rate"]),
        gas_overhead: small(&c["gas_overhead"]),
        swap_gas: small(&c["swap_gas"]),
        l1_bound: u64::try_from(number(&c["l1_bound"])).unwrap(),
        min_net: u64::try_from(number(&c["min_net"])).unwrap(),
        max_steps: small(&c["max_steps"]),
        hops: [hop(&hops[0]), hop(&hops[1])],
    }
}

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", alloy_primitives::hex::encode(bytes))
}

/// USDC lent by PoolManager, sold into AXIS's hooked v4 pool, bought back on
/// Uniswap v3.
fn axis_v4_then_v3() -> Lean3Plan {
    Lean3Plan {
        lender: Lean2Lender::PoolManager,
        start: StartToken::Usdc,
        cap: 12_000_000_000,
        deadline: 1_793_900_020,
        rate: 3_150_000_000,
        gas_overhead: 62_000,
        swap_gas: 330_000,
        l1_bound: 2_500_000_000,
        min_net: 1,
        max_steps: 8,
        hops: [
            Lean3Hop { kind: Hop3Kind::UniswapV4, pool: HOOK, token_out: AXIS, fee: 100, tick_spacing: 1, cross_gas: 25_000 },
            Lean3Hop { kind: Hop3Kind::UniswapV3, pool: AXIS_V3, token_out: USDC, fee: 500, tick_spacing: 10, cross_gas: 30_000 },
        ],
    }
}

#[test]
fn every_fixture_plan_encodes_to_its_bytes() {
    let f = fixture();
    let names = f["names"].as_array().unwrap();
    assert_eq!(names.len(), 6);
    for n in names {
        let c = &f["cases"][n.as_str().unwrap()];
        let bytes = encode_plan3(&plan_of(c)).unwrap_or_else(|e| panic!("{n}: {e}"));
        assert_eq!(bytes.len(), LEAN3_PLAN_LEN, "{n}");
        assert_eq!(hex(&bytes), c["encoded"].as_str().unwrap(), "{n}");
    }
}

#[test]
fn the_hand_built_plan_is_the_fixtures() {
    let f = fixture();
    let want = f["cases"]["usdc_pool_manager_v4_v3"]["encoded"].as_str().unwrap().to_string();
    assert_eq!(hex(&encode_plan3(&axis_v4_then_v3()).unwrap()), want);
}

#[test]
fn a_plan_must_end_in_its_start_token() {
    let mut p = axis_v4_then_v3();
    p.start = StartToken::Weth;
    assert_eq!(encode_plan3(&p), Err(Lean3Error::LastHopNotStart));
}

#[test]
fn a_v4_hop_needs_the_pool_manager_lender() {
    let mut p = axis_v4_then_v3();
    p.lender = Lean2Lender::Morpho;
    assert_eq!(encode_plan3(&p), Err(Lean3Error::V4NeedsPoolManager));
}

#[test]
fn the_deadline_must_fit_five_bytes() {
    let mut p = axis_v4_then_v3();
    p.deadline = 1 << 40;
    assert_eq!(encode_plan3(&p), Err(Lean3Error::DeadlineTooLarge(1 << 40)));
}

#[test]
fn a_fee_or_spacing_past_its_bytes_is_refused() {
    let mut p = axis_v4_then_v3();
    p.hops[1].fee = 1 << 24;
    assert_eq!(encode_plan3(&p), Err(Lean3Error::HopOutOfRange { hop: 1 }));
    let mut p = axis_v4_then_v3();
    p.hops[0].tick_spacing = 1 << 23;
    assert_eq!(encode_plan3(&p), Err(Lean3Error::HopOutOfRange { hop: 0 }));
    p.hops[0].tick_spacing = -(1 << 23) - 1;
    assert_eq!(encode_plan3(&p), Err(Lean3Error::HopOutOfRange { hop: 0 }));
}

#[test]
fn max_steps_is_between_one_and_sixteen() {
    for bad in [0, 17] {
        let mut p = axis_v4_then_v3();
        p.max_steps = bad;
        assert_eq!(encode_plan3(&p), Err(Lean3Error::MaxSteps(bad)));
    }
}

#[test]
fn run_calldata_is_the_lean_selector_over_the_plan() {
    let p = axis_v4_then_v3();
    let data = run3_calldata(&p).unwrap();
    assert_eq!(data[..4], RUN_SELECTOR);
    assert_eq!(data[4..36], B256::left_padding_from(&[0x20]).0);
    assert_eq!(data[36..68], B256::left_padding_from(&[LEAN3_PLAN_LEN as u8]).0);
    assert_eq!(data[68..68 + LEAN3_PLAN_LEN], encode_plan3(&p).unwrap()[..]);
    assert_eq!(data.len(), 68 + 192, "padded to a whole word");
    assert!(data[68 + LEAN3_PLAN_LEN..].iter().all(|b| *b == 0));
}

#[test]
fn the_commitment_is_version_five_over_the_bytes() {
    let p = axis_v4_then_v3();
    let mut buf = Vec::new();
    buf.extend_from_slice(&B256::left_padding_from(&8453u64.to_be_bytes()).0);
    buf.extend_from_slice(&B256::left_padding_from(EXECUTOR.as_slice()).0);
    buf.extend_from_slice(&B256::left_padding_from(&[5]).0);
    buf.extend_from_slice(&keccak256(encode_plan3(&p).unwrap()).0);
    assert_eq!(LEAN3_PLAN_VERSION, 5);
    assert_eq!(lean3_commitment(&p, 8453, EXECUTOR).unwrap(), keccak256(&buf));
}

#[test]
fn a_sealed_plan_refuses_a_wrong_commitment_and_builds_the_call() {
    let p = axis_v4_then_v3();
    let right = lean3_commitment(&p, 8453, EXECUTOR).unwrap();
    let wrong = SignedLean3Plan::check(p.clone(), B256::ZERO, 8453, EXECUTOR).unwrap_err();
    assert_eq!(wrong, Lean3Error::CommitmentMismatch { declared: B256::ZERO, recomputed: right });
    let call = ExecutorCall::from_lean3(SignedLean3Plan::check(p.clone(), right, 8453, EXECUTOR).unwrap());
    assert_eq!(call.to(), EXECUTOR);
    assert_eq!(call.plan(), &CallPlan::Lean3(p.clone()));
    assert_eq!(call.data(), &run3_calldata(&p).unwrap()[..]);
}
