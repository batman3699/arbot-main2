//! Task 8.5 R36 — the validity submitter: `base_sendRawTransactionValidity`,
//! the signed transaction with its predicates beside it, on Base's sequencer;
//! its answers read by the rules the raw submitter already follows.

mod support;

use alloy_primitives::keccak256;
use apex_chain::base::send::{read_answer, HttpSubmitter, RawTransport, SendAnswer};
use apex_chain::base::validity::{validity_request, ValiditySubmitter};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{dead_url, MockNode, Reply};

const RAW: [u8; 2] = [0x02, 0xab];

fn predicates() -> Value {
    json!([{ "type": "block_number", "params": { "op": "<=", "value": "0x10" } }])
}

#[test]
fn the_request_is_base_s_method_with_the_predicates_beside_the_transaction() {
    assert_eq!(
        validity_request(7, &RAW, &predicates()),
        json!({
            "jsonrpc": "2.0", "id": 7, "method": "base_sendRawTransactionValidity",
            "params": ["0x02ab", { "validity": [{ "type": "block_number", "params": { "op": "<=", "value": "0x10" } }] }],
        })
    );
}

#[test]
fn answers_read_as_the_raw_sender_reads_them() {
    let ours = format!("{:#x}", keccak256(RAW));
    let ok = json!({ "jsonrpc": "2.0", "id": 1, "result": ours });
    assert_eq!(read_answer(&RAW, 200, Some(ok)), SendAnswer::Accepted(keccak256(RAW)));
    let no = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32000, "message": "replacement transaction underpriced" } });
    assert_eq!(read_answer(&RAW, 200, Some(no)), SendAnswer::Refused("-32000: replacement transaction underpriced".into()));
    assert!(matches!(read_answer(&RAW, 503, None), SendAnswer::Silent(_)), "a server error may have sent it");
    assert!(matches!(read_answer(&RAW, 400, None), SendAnswer::Refused(_)), "a client error sent nothing");
    assert!(matches!(read_answer(&RAW, 200, None), SendAnswer::Silent(_)), "no JSON-RPC body");
    let other = json!({ "jsonrpc": "2.0", "id": 1, "result": format!("{:#x}", keccak256(b"another")) });
    assert!(matches!(read_answer(&RAW, 200, Some(other)), SendAnswer::Silent(_)), "another hash: not what we signed");
}

#[tokio::test]
async fn the_submitter_sends_base_s_method_to_its_endpoint() {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = Arc::clone(&seen);
    let ours = format!("{:#x}", keccak256(RAW));
    let node = MockNode::start(move |method, params| {
        log.lock().unwrap().push(json!({ "method": method, "params": params }));
        Reply::Result(json!(ours))
    })
    .await;
    let s = ValiditySubmitter::new(&node.url("/v1/rpc/k3y-that-must-never-print"), Duration::from_secs(5)).unwrap();
    assert_eq!(s.send(&RAW, &predicates()).await, SendAnswer::Accepted(keccak256(RAW)));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [json!({ "method": "base_sendRawTransactionValidity", "params": ["0x02ab", { "validity": predicates() }] })]
    );
    assert_eq!(node.total_hits(), 1, "one request, nothing else");
    assert!(!format!("{s:?}").contains("k3y"), "the key never shows");
}

#[tokio::test]
async fn the_raw_sender_still_reads_answers_the_same_way() {
    let ours = format!("{:#x}", keccak256(RAW));
    let node = MockNode::start(move |_, _| Reply::Result(json!(ours))).await;
    let raw = HttpSubmitter::new(&node.url("/"), Duration::from_secs(5)).unwrap();
    assert_eq!(raw.send_raw(&RAW).await, SendAnswer::Accepted(keccak256(RAW)));
    node.set(|_, _| Reply::Error { code: -32000, message: "nonce too low".into(), data: None });
    assert_eq!(raw.send_raw(&RAW).await, SendAnswer::Refused("-32000: nonce too low".into()));
    node.set(|_, _| Reply::Http(503));
    assert!(matches!(raw.send_raw(&RAW).await, SendAnswer::Silent(_)));
    let dead = HttpSubmitter::new(&dead_url("/").await, Duration::from_secs(5)).unwrap();
    assert!(matches!(dead.send_raw(&RAW).await, SendAnswer::Refused(_)), "never connected: nothing sent");
    let vdead = ValiditySubmitter::new(&dead_url("/").await, Duration::from_secs(5)).unwrap();
    assert!(matches!(vdead.send(&RAW, &predicates()).await, SendAnswer::Refused(_)), "the same for the validity submitter");
}
