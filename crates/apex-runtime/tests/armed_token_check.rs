//! Task 8.5 R37: a v4 route's other token is admitted only if it moves
//! exactly: 1,000 units out of a pool to a fresh address and back, in one
//! simulation, each leg landing whole. A fee-on-transfer token, or one whose
//! transfer reverts or returns false, leaves its pools unarmed.

use alloy_primitives::{address, Address};
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_runtime::armed::token_check::{moves_exactly, FRESH};
use serde_json::{json, Value};
use std::sync::Mutex;

const AXIS: Address = address!("5be75e3b0aff5d1a042a5363ae36acb1d6d755ea");
const HOLDER: Address = address!("498581ff718922c3f8e6a244956af099b2652b2b");

/// A node answering one `eth_simulateV1` with `results`, and keeping what it
/// was asked.
struct Sim {
    results: Vec<Value>,
    asked: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl RpcTransport for Sim {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        assert_eq!(method, "eth_simulateV1");
        self.asked.lock().unwrap().push(params);
        Ok(json!([{ "calls": self.results }]))
    }
}

fn ok(word: &str) -> Value {
    json!({ "status": "0x1", "returnData": word, "gasUsed": "0x5208", "logs": [] })
}

fn word(v: u64) -> String {
    format!("0x{v:064x}")
}

fn sim(results: Vec<Value>) -> Sim {
    Sim { results, asked: Mutex::new(Vec::new()) }
}

/// The fresh address's balance before, after 1,000 in, after 1,000 out; the
/// two transfers between.
fn round_trip(before: u64, received: u64, after: u64) -> Vec<Value> {
    vec![ok(&word(before)), ok(&word(1)), ok(&word(received)), ok(&word(1)), ok(&word(after))]
}

#[tokio::test]
async fn a_plain_token_moves_exactly() {
    let node = sim(round_trip(0, 1_000, 0));
    assert_eq!(moves_exactly(&node, AXIS, HOLDER, 53_155_787).await, Ok(true));
    let asked = node.asked.lock().unwrap()[0].clone();
    assert_eq!(asked[1], "0x32b17cb", "at the block given");
    assert_eq!(asked[0]["validation"], false, "a pool sends: no signature, no EOA check");
    let calls = asked[0]["blockStateCalls"][0]["calls"].as_array().unwrap().clone();
    assert_eq!(calls.len(), 5);
    let balance_of = format!("0x70a08231{:0>64}", alloy_primitives::hex::encode(FRESH));
    let transfer = |to: Address| format!("0xa9059cbb{:0>64}{:064x}", alloy_primitives::hex::encode(to), 1_000);
    for i in [0, 2, 4] {
        assert_eq!((calls[i]["to"].as_str(), calls[i]["data"].as_str()), (Some(format!("{AXIS:#x}").as_str()), Some(balance_of.as_str())));
    }
    assert_eq!((calls[1]["from"].as_str(), calls[1]["data"].as_str()), (Some(format!("{HOLDER:#x}").as_str()), Some(transfer(FRESH).as_str())));
    assert_eq!((calls[3]["from"].as_str(), calls[3]["data"].as_str()), (Some(format!("{FRESH:#x}").as_str()), Some(transfer(HOLDER).as_str())));
    // A token whose transfer returns nothing (no bool) still moves.
    let silent = sim(vec![ok(&word(0)), ok("0x"), ok(&word(1_000)), ok("0x"), ok(&word(0))]);
    assert_eq!(moves_exactly(&silent, AXIS, HOLDER, 1).await, Ok(true));
    // Someone's dust at the fresh address does not fail the check.
    assert_eq!(moves_exactly(&sim(round_trip(7, 1_007, 7)), AXIS, HOLDER, 1).await, Ok(true));
}

#[tokio::test]
async fn a_fee_on_transfer_token_does_not() {
    assert_eq!(moves_exactly(&sim(round_trip(0, 990, 0)), AXIS, HOLDER, 1).await, Ok(false), "1% kept going out");
    assert_eq!(moves_exactly(&sim(round_trip(0, 1_000, 10)), AXIS, HOLDER, 1).await, Ok(false), "1% kept coming back");
}

#[tokio::test]
async fn a_reverting_transfer_does_not() {
    let reverted = json!({ "status": "0x0", "returnData": "0x", "gasUsed": "0x5208", "logs": [] });
    let mut results = round_trip(0, 1_000, 0);
    results[1] = reverted;
    assert_eq!(moves_exactly(&sim(results), AXIS, HOLDER, 1).await, Ok(false));
    let mut refused = round_trip(0, 1_000, 0);
    refused[3] = ok(&word(0));
    assert_eq!(moves_exactly(&sim(refused), AXIS, HOLDER, 1).await, Ok(false), "a transfer that returns false");
    assert!(moves_exactly(&sim(vec![ok(&word(0))]), AXIS, HOLDER, 1).await.is_err(), "an answer short of five calls");
}
