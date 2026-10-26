//! Task 8.5 R36 — the move statistics survive a restart, and a day of the
//! pools' own logs seeds them. Learned only in memory, they began again at
//! every deploy and every daily census restart, and while they were young the
//! ranking's floor of 1/(windows + 2) gave the far rungs every sender.

use alloy_primitives::{address, Address, B256, U256};
use apex_chain::rpc::ws::RawLog;
use apex_runtime::armed::moves::{observation_of, seed, MoveStats, MovesSnapshot, Observation};
use apex_runtime::live::feed::{SWAP, SYNC};

const UNI: Address = address!("00000000000000000000000000000000000000a1");
const AERO: Address = address!("00000000000000000000000000000000000000b2");
const S: u64 = 1_000_000_000;
const W: u64 = 10 * S;

/// Four closed windows on UNI: up 30, flat, down 20, flat; the open one empty.
fn learned() -> MoveStats {
    let mut m = MoveStats::new(W);
    for (t, x) in [(0, 0.0), (5, 30.0), (10, 30.0), (19, 30.0), (21, 30.0), (25, 10.0), (31, 10.0), (45, 12.0)] {
        m.observe(UNI, x, t * S);
    }
    m
}

#[test]
fn a_snapshot_restores_the_same_probabilities() {
    let m = learned();
    let text = serde_json::to_string(&m.snapshot(45 * S)).unwrap();
    let snap: MovesSnapshot = serde_json::from_str(&text).unwrap();
    let mut back = MoveStats::new(W);
    assert_eq!(back.restore(&snap).unwrap(), 1, "one pool");
    for up in [true, false] {
        for step in [1, 10, 20, 30, 40, 80] {
            assert_eq!(back.p_at_least(UNI, up, step), m.p_at_least(UNI, up, step), "up {up}, step {step}");
        }
    }
}

/// A restored pool's first observation opens its window there: the gap since
/// the save is not a window of no move.
#[test]
fn a_restored_pool_opens_a_fresh_window() {
    let snap = learned().snapshot(45 * S);
    let mut back = MoveStats::new(W);
    back.restore(&snap).unwrap();
    back.observe(UNI, 500.0, 900 * S);
    back.observe(UNI, 505.0, 905 * S);
    assert_eq!(back.snapshot(905 * S).pools[&UNI], snap.pools[&UNI], "no window added yet");
    back.observe(UNI, 505.0, 911 * S);
    assert_eq!(back.snapshot(911 * S).pools[&UNI].last(), Some(&(5.0, 0.0)), "the first live window, from 500");
}

#[test]
fn a_snapshot_for_another_window_is_refused() {
    let snap = learned().snapshot(45 * S);
    let err = MoveStats::new(5 * S).restore(&snap).unwrap_err();
    assert!(err.contains("window"), "{err}");
}

#[test]
fn a_snapshot_saves_and_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("moves.json");
    let snap = learned().snapshot(45 * S);
    snap.save(&path).unwrap();
    assert_eq!(MovesSnapshot::load(&path).unwrap(), snap);
    assert!(!dir.path().join("moves.json.saving").exists(), "the staged file is renamed into place");
}

/// The engine observes every pass, so a quiet ten seconds is a window with no
/// move in it. A replay of logs must count those too: between a pool's logs,
/// its position held, block by block.
#[test]
fn seeding_from_logs_counts_the_quiet_windows() {
    let obs = [Observation { pool: UNI, at_ns: 0, position: 0.0 }, Observation { pool: UNI, at_ns: 4 * S, position: 25.0 }];
    let m = seed(W, &obs, 2 * S, 64 * S);
    // Closed at 10, 20, 30, 40, 50 and 60 s: one up 25, five quiet.
    assert_eq!(m.p_at_least(UNI, true, 25), (1.0 + 1.0) / (6.0 + 2.0));
    assert_eq!(m.snapshot(64 * S).pools[&UNI].len(), 6);
}

#[test]
fn a_pool_s_logs_read_as_the_engine_s_positions() {
    let log = |address, topic: B256, words: Vec<U256>| RawLog {
        pending: false,
        removed: false,
        address,
        topics: vec![topic],
        data: words.iter().flat_map(|w| w.to_be_bytes::<32>()).collect(),
        block_number: Some(1),
        transaction_hash: Some(B256::ZERO),
        log_index: Some(0),
        transaction_index: Some(0),
    };
    // A Uniswap-style Swap at tick -200: the tick, sign-extended in its word.
    let tick = U256::MAX - U256::from(199);
    let swap = log(UNI, SWAP, vec![U256::ZERO, U256::ZERO, U256::from(1u64) << 96, U256::from(1u64), tick]);
    assert_eq!(observation_of(&swap, 7), Some(Observation { pool: UNI, at_ns: 7, position: -200.0 }));
    // A Sync: the log-price in bps, as the engine takes a constant-product pool's.
    let sync = log(AERO, SYNC, vec![U256::from(1_000u64), U256::from(2_000u64)]);
    let p = observation_of(&sync, 9).unwrap();
    assert!((p.position - 2f64.ln() * 10_000.0).abs() < 1e-9);
    assert_eq!(observation_of(&log(UNI, B256::repeat_byte(1), vec![]), 9), None, "not a pool's state");
}

/// A node scripted for the seed: blocks two seconds apart, and one Swap log a
/// chunk, its tick its block: blocks 101, 202 and 303, off the windows' edges
/// (a jump on an edge opens the next window already moved, as live).
struct Node {
    asked: std::sync::Mutex<Vec<(u64, u64)>>,
    /// The widest range this node answers; wider is refused (drpc's free plan
    /// takes 100 blocks, Blast's 10).
    widest: u64,
    /// The most addresses a call may name (PublicNode's: 10).
    most_addresses: usize,
}

#[async_trait::async_trait]
impl apex_chain::rpc::RpcTransport for Node {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, apex_chain::rpc::RpcError> {
        let hex = |v: &serde_json::Value| u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
        match method {
            "eth_getBlockByNumber" => {
                let b = hex(&params[0]);
                Ok(serde_json::json!({ "timestamp": format!("{:#x}", 1_000 + 2 * b) }))
            }
            "eth_getLogs" => {
                let (from, to) = (hex(&params[0]["fromBlock"]), hex(&params[0]["toBlock"]));
                if to - from + 1 > self.widest || params[0]["address"].as_array().unwrap().len() > self.most_addresses {
                    return Err(apex_chain::rpc::RpcError::Exhausted { method: method.into(), endpoints: 1, last: "http 400 Bad Request".into() });
                }
                self.asked.lock().unwrap().push((from, to));
                if !params[0]["address"].as_array().unwrap().iter().any(|a| a == &serde_json::json!(format!("{UNI:#x}"))) {
                    return Ok(serde_json::json!([]));
                }
                let b = from + from / 100;
                let tick = U256::from(b);
                let data: Vec<u8> =
                    [U256::ZERO, U256::ZERO, U256::from(1u64) << 96, U256::from(1u64), tick].iter().flat_map(|w| w.to_be_bytes::<32>()).collect();
                Ok(serde_json::json!([{
                    "address": format!("{UNI:#x}"),
                    "topics": [format!("{SWAP:#x}")],
                    "data": format!("0x{}", alloy_primitives::hex::encode(data)),
                    "blockNumber": format!("{b:#x}"),
                    "transactionHash": format!("{:#x}", B256::ZERO),
                    "logIndex": "0x0",
                    "transactionIndex": "0x0",
                    "removed": false,
                }]))
            }
            _ => panic!("{method}"),
        }
    }
}

#[tokio::test]
async fn the_seed_reads_the_window_in_chunks_and_times_each_log_by_its_block() {
    use apex_runtime::armed::moves::seed_from_logs;
    let node = Node { asked: std::sync::Mutex::new(Vec::new()), widest: u64::MAX, most_addresses: usize::MAX };
    let (m, read) = seed_from_logs(&node, &[UNI], 100, 349, 100, 10, W, 2 * S).await.unwrap();
    assert_eq!(*node.asked.lock().unwrap(), vec![(100, 199), (200, 299), (300, 349)]);
    assert_eq!(read.logs, 3);
    // Ticks 101, 202 and 303: two moves of 101, each inside one window.
    assert_eq!(m.p_at_least(UNI, true, 100), (2.0 + 1.0) / (m.snapshot(0).pools[&UNI].len() as f64 + 2.0));
    assert_eq!(read.until_ns, (1_000 + 2 * 349) * S);
}

/// A range the endpoint refuses is halved until it answers, then grown back:
/// every block is still read, once.
#[tokio::test]
async fn a_range_the_endpoint_refuses_is_halved_and_every_block_still_read() {
    use apex_runtime::armed::moves::seed_from_logs;
    let node = Node { asked: std::sync::Mutex::new(Vec::new()), widest: 50, most_addresses: usize::MAX };
    let (_, read) = seed_from_logs(&node, &[UNI], 100, 349, 100, 10, W, 2 * S).await.unwrap();
    let asked = node.asked.lock().unwrap().clone();
    assert!(asked.iter().all(|(a, b)| b - a < 50), "{asked:?}");
    assert_eq!(asked.first().unwrap().0, 100);
    assert_eq!(asked.last().unwrap().1, 349);
    assert!(asked.windows(2).all(|w| w[1].0 == w[0].1 + 1), "contiguous, no block twice: {asked:?}");
    assert_eq!(read.until_ns, (1_000 + 2 * 349) * S);
}

/// PublicNode names at most ten addresses a call: the pools go ten at a time,
/// each range asked once for each ten.
#[tokio::test]
async fn the_pools_are_asked_ten_at_a_time() {
    use apex_runtime::armed::moves::seed_from_logs;
    let node = Node { asked: std::sync::Mutex::new(Vec::new()), widest: u64::MAX, most_addresses: 10 };
    let pools: Vec<Address> = std::iter::once(UNI).chain((1..25u8).map(Address::repeat_byte)).collect();
    let (m, read) = seed_from_logs(&node, &pools, 100, 349, 100, 10, W, 2 * S).await.unwrap();
    assert_eq!(node.asked.lock().unwrap().len(), 3 * 3, "three ranges, three tens each");
    assert_eq!(read.logs, 3, "UNI's, once a range");
    assert_eq!(m.snapshot(0).pools.len(), 1);
}
