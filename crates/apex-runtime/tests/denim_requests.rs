//! Task 8.5 R41: under Denim nothing the runtime asks names the `pending`
//! tag. Base's docs give it "no direct equivalent", and BlockPI's answer to
//! it after the fork is unknown.
//!
//! Each runtime read that names a tag or a block is driven through one
//! recording transport; then the source is held to it: the only `"pending"`
//! literal left is the pre-Denim tag in `live::sim::state_tag`, and the
//! operator's `send-probe`.

use alloy_primitives::{keccak256, Address, B256};
use apex_chain::rpc::ws::Head;
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_runtime::armed::live::LiveChain;
use apex_runtime::armed::live_chain::RpcChain;
use apex_runtime::live::book::PoolBook;
use apex_runtime::live::costs::read_l1;
use apex_runtime::live::frontier::{BALANCER_VAULT, WETH};
use apex_runtime::live::reader::{ChainReader, ReaderConfig};
use apex_runtime::live::reads::ChainReads;
use apex_types::state::ReconstructionStatus;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Records every request, and answers none.
#[derive(Default)]
struct Recorder {
    asked: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl RpcTransport for Recorder {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.asked.lock().unwrap().push((method.to_string(), params));
        Err(RpcError::Exhausted { method: method.into(), endpoints: 1, last: "recorded".into() })
    }
}

fn config(denim: bool) -> ReaderConfig {
    ReaderConfig {
        executor: Address::repeat_byte(0xe1),
        executor_code_hash: keccak256([1u8]),
        plan_version: 3,
        signer: Address::repeat_byte(0xa1),
        lender: BALANCER_VAULT,
        morpho: Some(Address::repeat_byte(0xbb)),
        loan_token: WETH,
        fee_ceiling_wei: 100_000_000,
        min_profit: 1,
        max_age: Duration::from_secs(6),
        denim,
    }
}

fn head() -> Head {
    Head {
        number: 101,
        hash: B256::repeat_byte(9),
        timestamp: 1_792_605_601,
        timestamp_ms: Some(1_792_605_601_400),
        base_fee_per_gas: Some(5_000_000),
        gas_used: 1,
        gas_limit: 120_000_000,
    }
}

fn nonce_tags(asked: &[(String, Value)]) -> Vec<Value> {
    asked.iter().filter(|(m, _)| m == "eth_getTransactionCount").map(|(_, p)| p[1].clone()).collect()
}

#[tokio::test]
async fn no_request_names_pending_under_denim() {
    let rec = Arc::new(Recorder::default());
    let rpc: Arc<dyn RpcTransport> = rec.clone();
    let book = Arc::new(PoolBook::from_snapshots([], ReconstructionStatus::Verified));
    let _ = ChainReader::new(Arc::clone(&rpc), book, config(true)).refresh(&head(), 0).await;
    let chain = RpcChain::new(Arc::clone(&rpc));
    let a = Address::repeat_byte(0xa1);
    let _ = chain.head().await;
    let _ = chain.nonce(a).await;
    let _ = chain.nonce_at(a, 101).await;
    let _ = chain.balance(a).await;
    let _ = chain.stops(Address::repeat_byte(0xe1), &[a], 101).await;
    let _ = chain.receipt(B256::repeat_byte(3)).await;
    let _ = read_l1(&ChainReads::new(Arc::clone(&rpc)), 101).await;
    let asked = rec.asked.lock().unwrap();
    assert!(nonce_tags(&asked).contains(&Value::from("latest")), "the reader's nonce, at latest");
    for (method, params) in asked.iter() {
        assert!(!params.to_string().contains("\"pending\""), "{method} {params}");
    }
}

/// Before Denim the reader's nonce is the pending one, as it always was.
#[tokio::test]
async fn before_denim_the_reader_s_nonce_is_pending() {
    let rec = Arc::new(Recorder::default());
    let book = Arc::new(PoolBook::from_snapshots([], ReconstructionStatus::Verified));
    let _ = ChainReader::new(rec.clone(), book, config(false)).refresh(&head(), 0).await;
    assert_eq!(nonce_tags(&rec.asked.lock().unwrap()), vec![Value::from("pending")]);
}

/// The source holds no other `"pending"`: a new read naming it would ask
/// Denim's nodes for what they do not have.
#[test]
fn the_only_pending_literals_are_the_pre_denim_tag_and_the_probe() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    let mut dirs = vec![src.clone()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                for line in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
                    for _ in 0..line.matches("\"pending\"").count() {
                        found.push(p.strip_prefix(&src).unwrap().display().to_string());
                    }
                }
            }
        }
    }
    found.sort();
    assert_eq!(found, vec!["live/sim.rs".to_string(), "send_probe.rs".to_string()]);
}
