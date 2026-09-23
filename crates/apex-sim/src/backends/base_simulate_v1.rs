//! The `eth_simulateV1` backend (§24.6, Task 4.2).
//!
//! # Why not `eth_call`
//!
//! Base documents that `eth_call` against `pending` may return a **cached
//! block context**. A simulation is a claim about what happens when this
//! transaction executes on top of a specific state; if the node is free to
//! answer about a different block, the claim is not about anything in
//! particular. That is tolerable for a sanity check and not tolerable for the
//! thing a live dispatch rests on.
//!
//! `eth_simulateV1` takes the block and the state overrides **explicitly**, so
//! what was simulated is a property of the request rather than of the node's
//! mood — and the request is recorded, so a divergence can be attributed
//! afterwards.
//!
//! # `validation: true` is not optional
//!
//! With validation off, the node executes the calls and reports what happens.
//! With it on, it also applies the checks that decide whether the transaction
//! would be *accepted*: nonce, balance, intrinsic gas, fee caps. A simulation
//! that skips those answers "would this succeed if it ran", when the question
//! is "would this run".

use super::{SimCall, SimContext};
use serde_json::{json, Value};

pub const METHOD: &str = "eth_simulateV1";

/// Build the JSON-RPC request for a simulated block of calls.
///
/// Pure: returns the request rather than sending it, so the shape is testable
/// without a node. See `backends/mod.rs` for why that matters here more than
/// usual.
pub fn request(id: u64, calls: &[SimCall], ctx: &SimContext) -> Value {
    let encoded: Vec<Value> = calls
        .iter()
        .map(|c| {
            let mut call = json!({ "from": c.from, "to": c.to, "input": c.data });
            if let Some(v) = &c.value {
                call["value"] = json!(v);
            }
            if let Some(g) = &c.gas {
                call["gas"] = json!(g);
            }
            call
        })
        .collect();

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": METHOD,
        "params": [
            {
                "blockStateCalls": [{
                    "blockOverrides": ctx.block_overrides,
                    "stateOverrides": ctx.state_overrides,
                    "calls": encoded,
                }],
                // See the module docs: this is what makes the answer about
                // acceptance rather than about execution.
                "validation": true,
                // Balance deltas per token come from transfer traces;
                // §20 requires them measured, not inferred from nominal
                // amounts, because a fee-on-transfer token moves less than it
                // is told to.
                "traceTransfers": true,
                "returnFullTransactions": false,
            },
            // An explicit block, never a tag.
            ctx.block_number_hex,
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SimContext {
        SimContext {
            block_number_hex: "0x1312d00".into(),
            state_overrides: json!({}),
            block_overrides: json!({ "baseFeePerGas": "0x3b9aca00" }),
        }
    }

    fn call() -> SimCall {
        SimCall {
            from: "0x1111111111111111111111111111111111111111".into(),
            to: "0x2222222222222222222222222222222222222222".into(),
            data: "0xdeadbeef".into(),
            value: None,
            gas: Some("0x3d090".into()),
        }
    }

    #[test]
    fn the_request_carries_explicit_block_and_state_context() {
        let req = request(7, &[call()], &ctx());
        assert_eq!(req["method"], json!(METHOD));
        assert_ne!(req["method"], json!("eth_call"));
        assert!(req["params"][0]["blockStateCalls"].is_array());
        assert_eq!(req["params"][0]["validation"], json!(true));
        assert_eq!(req["params"][0]["traceTransfers"], json!(true));
        assert_eq!(req["params"][1], json!("0x1312d00"));
    }

    /// The block is a quantity, never a tag. `pending` is the specific thing
    /// §24.6 rules out.
    #[test]
    fn the_block_is_never_a_tag() {
        let req = request(1, &[call()], &ctx());
        let block = req["params"][1].as_str().expect("a string");
        assert!(block.starts_with("0x"), "block was {block}");
        for tag in ["pending", "latest", "safe", "finalized", "earliest"] {
            assert_ne!(block, tag);
        }
    }

    /// Optional fields are omitted rather than defaulted. A `value` of `0x0`
    /// and an absent `value` are the same to a node, but a `gas` of `0x0` is
    /// not the same as letting the node estimate — so the builder must not
    /// invent either.
    #[test]
    fn absent_fields_are_omitted_not_zeroed() {
        let mut c = call();
        c.gas = None;
        c.value = None;
        let req = request(1, &[c], &ctx());
        let encoded = &req["params"][0]["blockStateCalls"][0]["calls"][0];
        assert!(encoded.get("gas").is_none(), "gas was invented: {encoded}");
        assert!(encoded.get("value").is_none(), "value was invented: {encoded}");
        assert_eq!(encoded["input"], json!("0xdeadbeef"));
    }

    /// Several calls simulate as one block, in order. Simulating them
    /// separately would answer a different question: each would see the
    /// pre-trade state rather than the state its predecessor left.
    #[test]
    fn a_multi_call_route_simulates_as_one_block_in_order() {
        let mut second = call();
        second.data = "0xfeedface".into();
        let req = request(1, &[call(), second], &ctx());
        let calls = req["params"][0]["blockStateCalls"][0]["calls"]
            .as_array()
            .expect("an array");
        assert_eq!(calls.len(), 2, "the route must be one block, not two");
        assert_eq!(calls[0]["input"], json!("0xdeadbeef"));
        assert_eq!(calls[1]["input"], json!("0xfeedface"));
    }

    #[test]
    fn state_and_block_overrides_reach_the_request() {
        let mut c = ctx();
        c.state_overrides = json!({
            "0x1111111111111111111111111111111111111111": { "balance": "0xde0b6b3a7640000" }
        });
        let req = request(1, &[call()], &c);
        let entry = &req["params"][0]["blockStateCalls"][0];
        assert_eq!(
            entry["stateOverrides"]["0x1111111111111111111111111111111111111111"]["balance"],
            json!("0xde0b6b3a7640000")
        );
        assert_eq!(entry["blockOverrides"]["baseFeePerGas"], json!("0x3b9aca00"));
    }
}
