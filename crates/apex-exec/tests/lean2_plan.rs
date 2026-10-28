//! Task 8.5 R37: the second lean executor's plan (`apex_exec::lean2`), held to
//! `fixtures/lean2_plans.json`, which `test/lean/Lean2PlanFixture.t.sol` holds
//! the contract's decoder to as well. Neither side writes it.

use alloy_primitives::{address, Address};
use apex_exec::call::{CallPlan, ExecutorCall};
use apex_exec::lean::{lean_commitment, HopKind, LeanHop, LeanLender, LeanPlan, WETH};
use apex_exec::lean2::{
    encode_plan2, lean2_commitment, run2_calldata, Hop2Kind, Lean2Error, Lean2Hop, Lean2Lender, Lean2Plan,
    SignedLean2Plan, StartToken, V4HopKey, LEAN2_PLAN_VERSION, USDC,
};
use serde_json::Value;

const EXECUTOR: Address = address!("5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f");
const AXIS: Address = address!("5be75e3B0aFF5d1a042A5363Ae36acB1d6D755Ea");
const HOOK: Address = address!("b0b24b89DB0DafbE43c5B40226B63a179F592880");
const AXIS_V3: Address = address!("e1bfc5ada1125718b53b2affc638ba9981f6070f");

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/lean2_plans.json")).expect("the fixture parses")
}

fn number(v: &Value) -> u128 {
    v.as_str().expect("numbers are decimal strings").parse().expect("a u128")
}

fn plan_of(c: &Value) -> Lean2Plan {
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
    let hops = c["hops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let kind = match h["kind"].as_u64() {
                Some(0) => Hop2Kind::UniswapV3Style,
                Some(1) => Hop2Kind::PancakeV3,
                Some(2) => Hop2Kind::AerodromeV2,
                Some(3) => Hop2Kind::UniswapV4,
                other => panic!("kind {other:?}"),
            };
            let v4 = (kind == Hop2Kind::UniswapV4).then(|| V4HopKey {
                fee: u32::try_from(h["fee"].as_u64().unwrap()).unwrap(),
                tick_spacing: i32::try_from(h["tick_spacing"].as_i64().unwrap()).unwrap(),
                hooks: h["hooks"].as_str().unwrap().parse().unwrap(),
            });
            Lean2Hop {
                kind,
                pool: h["pool"].as_str().unwrap().parse().unwrap(),
                token_out: h["token_out"].as_str().unwrap().parse().unwrap(),
                min_out: number(&h["min_out"]),
                v4,
            }
        })
        .collect();
    Lean2Plan {
        lender,
        start,
        amount: number(&c["amount"]),
        deadline: u64::try_from(number(&c["deadline"])).unwrap(),
        min_profit: number(&c["min_profit"]),
        hops,
    }
}

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", alloy_primitives::hex::encode(bytes))
}

/// USDC lent by PoolManager, sold into AXIS's hooked v4 pool, bought back on
/// Uniswap v3.
fn axis_v4_then_v3() -> Lean2Plan {
    Lean2Plan {
        lender: Lean2Lender::PoolManager,
        start: StartToken::Usdc,
        amount: 1_000_000_000,
        deadline: 1_792_133_500,
        min_profit: 1,
        hops: vec![
            Lean2Hop {
                kind: Hop2Kind::UniswapV4,
                pool: Address::ZERO,
                token_out: AXIS,
                min_out: 3_990_000_000_000_000_000_000,
                v4: Some(V4HopKey { fee: 100, tick_spacing: 1, hooks: HOOK }),
            },
            Lean2Hop { kind: Hop2Kind::UniswapV3Style, pool: AXIS_V3, token_out: USDC, min_out: 1_000_000_001, v4: None },
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
        let bytes = encode_plan2(&plan_of(c)).unwrap_or_else(|e| panic!("{n}: {e}"));
        assert_eq!(hex(&bytes), c["encoded"].as_str().unwrap(), "{n}");
    }
}

#[test]
fn a_v4_hop_carries_its_hook_fee_and_spacing() {
    let bytes = encode_plan2(&axis_v4_then_v3()).unwrap();
    assert_eq!(bytes[0..2], [2, 1], "PoolManager lends USDC");
    assert_eq!(bytes[39], 3, "a v4 hop");
    assert_eq!(&bytes[40..60], HOOK.as_slice(), "the pool field holds the hook");
    assert_eq!(&bytes[60..80], AXIS.as_slice());
    assert_eq!(bytes[96..99], [0x00, 0x00, 0x64], "fee 100");
    assert_eq!(bytes[99..102], [0x00, 0x00, 0x01], "tick spacing 1");
    assert_eq!(bytes.len(), 39 + 63 + 57);
}

#[test]
fn a_plan_must_end_in_its_start_token() {
    let mut p = axis_v4_then_v3();
    p.start = StartToken::Weth;
    assert_eq!(encode_plan2(&p), Err(Lean2Error::LastHopNotStart));
}

#[test]
fn a_v4_hop_needs_the_pool_manager_lender() {
    let mut p = axis_v4_then_v3();
    p.lender = Lean2Lender::Morpho;
    assert_eq!(encode_plan2(&p), Err(Lean2Error::V4NeedsPoolManager));
}

#[test]
fn a_v4_key_must_match_its_hop_kind() {
    let mut p = axis_v4_then_v3();
    p.hops[0].v4 = None;
    assert_eq!(encode_plan2(&p), Err(Lean2Error::V4KeyMismatch { hop: 0 }));
    let mut p = axis_v4_then_v3();
    p.hops[1].v4 = Some(V4HopKey { fee: 100, tick_spacing: 1, hooks: HOOK });
    assert_eq!(encode_plan2(&p), Err(Lean2Error::V4KeyMismatch { hop: 1 }));
}

#[test]
fn a_v4_key_out_of_its_bytes_is_refused() {
    let mut p = axis_v4_then_v3();
    p.hops[0].v4 = Some(V4HopKey { fee: 1 << 24, tick_spacing: 1, hooks: HOOK });
    assert_eq!(encode_plan2(&p), Err(Lean2Error::V4KeyOutOfRange { hop: 0 }));
    let mut p = axis_v4_then_v3();
    p.hops[0].v4 = Some(V4HopKey { fee: 100, tick_spacing: 1 << 23, hooks: HOOK });
    assert_eq!(encode_plan2(&p), Err(Lean2Error::V4KeyOutOfRange { hop: 0 }));
}

#[test]
fn a_negative_tick_spacing_round_trips() {
    let mut p = axis_v4_then_v3();
    p.hops[0].v4 = Some(V4HopKey { fee: 100, tick_spacing: -1, hooks: HOOK });
    let bytes = encode_plan2(&p).unwrap();
    assert_eq!(bytes[99..102], [0xff, 0xff, 0xff]);
}

#[test]
fn the_commitment_names_version_four() {
    assert_eq!(LEAN2_PLAN_VERSION, 4);
    // The same fields as a v1 plan: WETH under Morpho, two v3 hops.
    let pool = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
    let v1 = LeanPlan {
        lender: LeanLender::Morpho,
        amount: 10u128.pow(18),
        deadline: 1_792_133_385,
        min_profit: 1,
        hops: vec![
            LeanHop { kind: HopKind::UniswapV3Style, pool, token_out: USDC, min_out: 1 },
            LeanHop { kind: HopKind::UniswapV3Style, pool, token_out: WETH, min_out: 2 },
        ],
    };
    let v2 = Lean2Plan {
        lender: Lean2Lender::Morpho,
        start: StartToken::Weth,
        amount: v1.amount,
        deadline: v1.deadline,
        min_profit: v1.min_profit,
        hops: v1
            .hops
            .iter()
            .map(|h| Lean2Hop { kind: Hop2Kind::UniswapV3Style, pool: h.pool, token_out: h.token_out, min_out: h.min_out, v4: None })
            .collect(),
    };
    assert_ne!(lean2_commitment(&v2, 8453, EXECUTOR).unwrap(), lean_commitment(&v1, 8453, EXECUTOR).unwrap());
}

#[test]
fn a_checked_plan_is_its_executor_call() {
    let p = axis_v4_then_v3();
    let declared = lean2_commitment(&p, 8453, EXECUTOR).unwrap();
    let call = ExecutorCall::from_lean2(SignedLean2Plan::check(p.clone(), declared, 8453, EXECUTOR).unwrap());
    assert_eq!((call.to(), call.chain_id(), call.commitment()), (EXECUTOR, 8453, declared));
    assert_eq!(call.data(), run2_calldata(&p).unwrap().as_slice());
    assert_eq!(call.data()[..4], apex_exec::lean::RUN_SELECTOR, "run(bytes), as v1");
    assert_eq!(call.plan(), &CallPlan::Lean2(p.clone()));
    let wrong = SignedLean2Plan::check(p, alloy_primitives::B256::ZERO, 8453, EXECUTOR).unwrap_err();
    assert!(matches!(wrong, Lean2Error::CommitmentMismatch { .. }));
}

#[test]
fn start_tokens_name_their_addresses() {
    assert_eq!(StartToken::Weth.address(), WETH);
    assert_eq!(StartToken::Usdc.address(), address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"));
}
