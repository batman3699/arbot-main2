//! The `eth_call` backend — fallback, and quorum verifier (§24.6).
//!
//! Kept, and kept in its place. Base documents that `eth_call` against
//! `pending` may return a cached block context, so this cannot be the backend
//! a live dispatch rests on. It is still worth having: a second opinion from a
//! weaker instrument catches a class of failure a single instrument cannot,
//! and the quorum check (§20) depends on there being two.
//!
//! The block is pinned here too. A verifier answering about a different block
//! from the primary is not a verifier — it is a second source of
//! disagreement, and the quorum would count its disagreement as evidence.

use super::SimCall;
use serde_json::{json, Value};

pub const METHOD: &str = "eth_call";

/// Build an `eth_call` request, pinned to an explicit block.
pub fn request(id: u64, call: &SimCall, block_number_hex: &str) -> Value {
    let mut tx = json!({ "from": call.from, "to": call.to, "input": call.data });
    if let Some(v) = &call.value {
        tx["value"] = json!(v);
    }
    if let Some(g) = &call.gas {
        tx["gas"] = json!(g);
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": METHOD,
        "params": [tx, block_number_hex],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> SimCall {
        SimCall {
            from: "0x1111111111111111111111111111111111111111".into(),
            to: "0x2222222222222222222222222222222222222222".into(),
            data: "0xdeadbeef".into(),
            value: None,
            gas: None,
        }
    }

    /// The verifier pins its block too. An unpinned verifier answers about a
    /// different state and its disagreement means nothing.
    #[test]
    fn the_fallback_pins_its_block_as_well() {
        let req = request(1, &call(), "0x1312d00");
        assert_eq!(req["method"], json!("eth_call"));
        assert_eq!(req["params"][1], json!("0x1312d00"));
        assert_ne!(req["params"][1], json!("pending"));
    }
}
