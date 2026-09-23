//! Task 4.2 — `eth_simulateV1` is the capture-critical Base backend, and its
//! request says what it simulated against.

use apex_sim::backends::{
    base_simulate_v1, capture_critical_backend, eth_call, BackendKind, SimCall, SimContext,
};
use apex_types::ids::ChainId;
use serde_json::json;

fn ctx() -> SimContext {
    SimContext {
        block_number_hex: "0x1312d00".into(),
        state_overrides: json!({}),
        block_overrides: json!({}),
    }
}

fn call() -> SimCall {
    SimCall {
        from: "0x1111111111111111111111111111111111111111".into(),
        to: "0x2222222222222222222222222222222222222222".into(),
        data: "0xdeadbeef".into(),
        value: None,
        gas: None,
    }
}

#[test]
fn simulate_v1_is_preferred_over_eth_call_on_base() {
    assert_eq!(
        capture_critical_backend(ChainId::BASE),
        BackendKind::EthSimulateV1
    );
    assert!(BackendKind::EthSimulateV1.is_capture_critical());
    assert!(
        !BackendKind::EthCall.is_capture_critical(),
        "eth_call against pending may return a cached block context (§24.6), so \
         it cannot be what a live dispatch rests on"
    );
}

/// An unchecked chain gets the weaker backend, not an assumption.
///
/// `eth_simulateV1` is not universally available, and claiming it for a chain
/// nobody has checked would produce a backend that fails at runtime on the one
/// path that must not fail.
#[test]
fn an_unchecked_chain_falls_back_rather_than_assuming() {
    assert_eq!(capture_critical_backend(ChainId(1)), BackendKind::EthSimulateV1);
    for unchecked in [10u64, 42161, 56, 137, 999_999] {
        assert_eq!(
            capture_critical_backend(ChainId(unchecked)),
            BackendKind::EthCall,
            "chain {unchecked} claimed a backend nobody verified it has"
        );
    }
}

#[test]
fn simulate_v1_sends_explicit_block_and_state_context() {
    let req = base_simulate_v1::request(1, &[call()], &ctx());
    assert!(req["params"][0]["blockStateCalls"].is_array());
    assert_eq!(req["params"][0]["validation"], json!(true));
    assert_ne!(req["method"], json!("eth_call"));
    assert_eq!(req["method"], json!("eth_simulateV1"));
}

/// Both backends pin the same block, which is what makes the quorum check
/// mean anything. A verifier answering about a different state is a second
/// source of disagreement, not a second opinion.
#[test]
fn the_primary_and_the_verifier_pin_the_same_block() {
    let block = "0x1312d00";
    let primary = base_simulate_v1::request(1, &[call()], &ctx());
    let verifier = eth_call::request(2, &call(), block);
    assert_eq!(primary["params"][1], verifier["params"][1]);
    assert_eq!(primary["params"][1], json!(block));
}

/// Neither backend may address a tag. This is the §24.6 rule, checked on both
/// paths rather than only on the one that matters most — a fallback that
/// silently uses `pending` would make the quorum's agreement meaningless
/// exactly when the primary was right.
#[test]
fn neither_backend_addresses_a_block_tag() {
    let requests = [
        base_simulate_v1::request(1, &[call()], &ctx()),
        eth_call::request(2, &call(), "0x1312d00"),
    ];
    for req in requests {
        let block = req["params"][1].as_str().expect("a string block");
        assert!(block.starts_with("0x"), "{} used {block}", req["method"]);
        for tag in ["pending", "latest", "safe", "finalized", "earliest"] {
            assert_ne!(block, tag, "{} addressed {tag}", req["method"]);
        }
    }
}

/// The request is serialisable and stable: the same inputs produce the same
/// bytes, so a recorded request can be compared against a replayed one when a
/// divergence has to be attributed after the fact.
#[test]
fn the_request_is_stable_for_the_same_inputs() {
    let a = serde_json::to_string(&base_simulate_v1::request(1, &[call()], &ctx())).expect("json");
    let b = serde_json::to_string(&base_simulate_v1::request(1, &[call()], &ctx())).expect("json");
    assert_eq!(a, b);
}
