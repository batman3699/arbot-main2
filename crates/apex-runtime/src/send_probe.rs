//! `apex send-probe` (Task 8.5 R32): how long a transaction takes from leaving
//! this host to the flashblock that includes it, by route.
//!
//! Every one of the canary's fifteen sends to 2026-10-20 landed 0.5-1.5 s after
//! it left, three to seven flashblocks late, through BlockPI's keyed endpoint
//! (whose MEV protection is on). This sends zero-value self-transfers from the
//! trader, alternating that endpoint and Base's sequencer endpoint at the same
//! tip, and times each from the send to the arrival, on Base's flashblock
//! stream, of the flashblock that carried it.
//!
//! The trader's nonce moves, so the canary is stopped while it runs (its
//! tripwire takes its baseline at boot). The key and the keyed URL are never
//! printed; the routes are reported by name.

use crate::sign::{Eip1559Tx, LaneKey};
use alloy_primitives::{Address, B256};
use apex_chain::base::flashblock_stream::{FlashblockIndexer, FlashblockSeen, FlashblockStream, BASE_FLASHBLOCKS_WS};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Base's sequencer endpoint: it takes `eth_sendRawTransaction` and little else.
pub const SEQUENCER: &str = "https://mainnet-sequencer.base.org";

/// The probe transaction: nothing, to itself, at `tip`, with twice the base
/// fee of headroom, as the canary signs.
pub fn probe_tx(chain_id: u64, nonce: u64, base_fee: u128, tip: u128, trader: Address) -> Eip1559Tx {
    Eip1559Tx {
        chain_id,
        nonce,
        max_priority_fee_per_gas: tip,
        max_fee_per_gas: base_fee.saturating_mul(2).saturating_add(tip),
        gas_limit: 21_000,
        to: trader,
        data: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Inclusion {
    pub flashblock: u32,
    /// The carrying flashblock's arrival on the stream, after the send.
    pub arrived_after_ms: i64,
    /// The one before it: a send that reached the sequencer in time would
    /// have been in it.
    pub before_arrived_after_ms: Option<i64>,
}

/// The flashblock of `block` that carried transaction `tx_index`, timed from
/// `sent_ns`; `None` when the stream did not show it (or its block) whole.
pub fn inclusion(seen: &[FlashblockSeen], block: u64, tx_index: u32, sent_ns: u64) -> Option<Inclusion> {
    let carrier = seen
        .iter()
        .find(|f| f.block == block && f.first_tx.is_some_and(|first| first <= tx_index && tx_index < first + f.tx_count))?;
    let after = |f: &FlashblockSeen| (f.arrived_at_ns as i64 - sent_ns as i64) / 1_000_000;
    let before = (carrier.index > 0)
        .then(|| seen.iter().find(|f| f.block == block && f.index + 1 == carrier.index))
        .flatten();
    Some(Inclusion { flashblock: carrier.index, arrived_after_ms: after(carrier), before_arrived_after_ms: before.map(after) })
}

#[derive(Debug, Serialize)]
pub struct Sample {
    pub route: &'static str,
    pub tx: Option<B256>,
    pub ack_ms: Option<i64>,
    pub block: Option<u64>,
    pub tx_index: Option<u32>,
    pub inclusion: Option<Inclusion>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SendProbeReport {
    pub tip_wei_per_gas: u128,
    pub samples: Vec<Sample>,
    /// Per route: the carrying flashblock's arrival after the send, ms.
    pub arrived_after_ms: BTreeMap<&'static str, Vec<i64>>,
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value, String> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let r: Value = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{method}: transport: {}", e.without_url()))?
        .json()
        .await
        .map_err(|e| format!("{method}: body: {}", e.without_url()))?;
    match (r.get("result"), r.get("error")) {
        (Some(v), _) => Ok(v.clone()),
        (None, Some(e)) => Err(format!("{method}: {e}")),
        _ => Err(format!("{method}: no result")),
    }
}

fn quantity(v: &Value) -> Option<u64> {
    u64::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}

/// Send `count` probes from `key`, alternating `routes` (name, URL), reading
/// through `reads`. Waits up to 30 s for each to be included.
pub async fn run(key: &LaneKey, chain_id: u64, reads: &str, routes: &[(&'static str, String)], count: usize, tip: u128) -> SendProbeReport {
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let (_stream, mut frames) = FlashblockStream::new(BASE_FLASHBLOCKS_WS).spawn(stop);
    let seen: Arc<Mutex<Vec<FlashblockSeen>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = {
        let seen = Arc::clone(&seen);
        tokio::spawn(async move {
            let mut ix = FlashblockIndexer::new();
            while let Some((f, at)) = frames.recv().await {
                if let Some(s) = ix.observe(&f, at) {
                    if let Ok(mut v) = seen.lock() {
                        v.push(s);
                    }
                }
            }
        })
    };
    let client = reqwest::Client::builder()
        .user_agent("apex-send-probe")
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    // A block or two of the stream before the first send.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let trader = key.address();
    let mut samples = Vec::new();
    for i in 0..count {
        let (route, url) = (&routes[i % routes.len()].0, &routes[i % routes.len()].1);
        let mut s = Sample { route, tx: None, ack_ms: None, block: None, tx_index: None, inclusion: None, error: None };
        let prepared = async {
            let nonce = quantity(&rpc(&client, reads, "eth_getTransactionCount", json!([trader, "pending"])).await?).ok_or("a nonce")?;
            let head = rpc(&client, reads, "eth_getBlockByNumber", json!(["latest", false])).await?;
            let base = quantity(&head["baseFeePerGas"]).ok_or("a base fee")?;
            key.sign_eip1559(&probe_tx(chain_id, nonce, u128::from(base), tip, trader)).map_err(|_| "signing".to_string())
        }
        .await;
        let signed = match prepared {
            Ok(t) => t,
            Err(e) => {
                s.error = Some(e);
                samples.push(s);
                continue;
            }
        };
        s.tx = Some(signed.hash);
        let sent_ns = now_ns();
        match rpc(&client, url, "eth_sendRawTransaction", json!([format!("0x{}", alloy_primitives::hex::encode(&signed.raw))])).await {
            Ok(_) => s.ack_ms = Some((now_ns() - sent_ns) as i64 / 1_000_000),
            Err(e) => {
                s.error = Some(e);
                samples.push(s);
                continue;
            }
        }
        // Its receipt, read every 100 ms for 30 s.
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(rc) = rpc(&client, reads, "eth_getTransactionReceipt", json!([signed.hash])).await {
                if !rc.is_null() {
                    s.block = quantity(&rc["blockNumber"]);
                    s.tx_index = quantity(&rc["transactionIndex"]).and_then(|x| u32::try_from(x).ok());
                    break;
                }
            }
        }
        // The stream may trail the receipt by a moment.
        if let (Some(b), Some(t)) = (s.block, s.tx_index) {
            for _ in 0..30 {
                let found = seen.lock().ok().and_then(|v| inclusion(&v, b, t, sent_ns));
                if found.is_some() {
                    s.inclusion = found;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } else {
            s.error = Some("not included within 30 s".into());
        }
        samples.push(s);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let _ = stop_tx.send(true);
    recorder.abort();
    let mut arrived_after_ms: BTreeMap<&'static str, Vec<i64>> = BTreeMap::new();
    for s in &samples {
        if let Some(inc) = s.inclusion {
            arrived_after_ms.entry(s.route).or_default().push(inc.arrived_after_ms);
        }
    }
    SendProbeReport { tip_wei_per_gas: tip, samples, arrived_after_ms }
}
