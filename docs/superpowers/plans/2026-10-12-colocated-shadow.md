# Co-located Shadow and Race Ledger Implementation Plan (Task 8.5 R25)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure, per contested gap, which flashblock our transaction would land in versus the winning rival's, from a host next to Base's sequencer.

**Architecture:**
- A measurement-only reader for Base's public flashblock stream (`apex-chain`) feeds two things:
  - an `apex probe` subcommand, used to choose the region;
  - a shadow worker that writes a flashblock ledger.
- The capture loop writes one race line per handled proposal, without blocking.
- An offline analyzer joins both ledgers and the journal with on-chain lookups.
- The host runs the shadow under systemd. A nonce tripwire guards the reused trader key.

**Tech Stack:** Rust (`apex-chain`, `apex-runtime`, tokio, tokio-tungstenite, `brotli-decompressor` 5), Python 3 (`scripts/data`), systemd, AWS EC2 + SSM, Foundry `cast`.

**Spec:** `docs/superpowers/specs/2026-10-11-colocated-shadow-design.md`

## Global Constraints

- Measurement only: nothing in this plan changes what is priced, signed or sent. The null dispatcher stays the only lane.
- The capture path never waits on a ledger: race lines go through a bounded channel, and a full channel drops the line and counts it.
- Every timestamp is host-clock Unix nanoseconds, taken on receipt, before any decoding.
- Flashblock stream: `wss://mainnet.flashblocks.base.org/ws`. Frames are brotli-compressed JSON. Index 0 carries `base.block_number` and `base.timestamp` (hex). Every frame carries `index`, `metadata.block_number` (a JSON number) and `diff.transactions`.
- Gates: G1, `flashblock_k` → `received`, p50 ≤ 80 ms and p90 ≤ 130 ms. G2, the would-land flashblock at `proposed` is ≤ *k* + 1 on at least 50% of contested gaps, reported at ε = 0, 20 and 40 ms.
- Never `cargo fmt`. Use `-j 3`. Stage named paths only. Run all five CI gates before every commit. Verify each commit in a clean worktree with its own `CARGO_TARGET_DIR`.
- Never put `BLOCKPI_KEY` or the trader key in argv or logs. Pass `BLOCKPI_KEY` by environment variable and foundry.toml's `base` alias.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

**Refinements of spec §5** (measurement-equivalent; they keep the capture path minimal):
- The race line records the trigger as `{ block, log_index, tx }`. The analyzer resolves its transaction index from the receipt, and its flashblock from the ledger.
- Ticket stage times (admitted, Tier 2 start and end) are joined from `shadow.journal` by ticket id rather than duplicated.

## File Structure

- Create `crates/apex-chain/src/base/flashblock_stream.rs`: `Frame`, `decode_frame`, `FlashblockSeen`, `FlashblockIndexer`, `FlashblockStream`.
- Modify:
  - `crates/apex-chain/src/base/mod.rs`, `crates/apex-chain/Cargo.toml` (`brotli-decompressor = "5"`).
  - `crates/apex-chain/src/rpc/ws.rs`: `RawLog::transaction_index`.
- Create:
  - `crates/apex-runtime/src/probe.rs`: `apex probe`.
  - `crates/apex-runtime/src/shadow/ledger.rs`: the flashblock ledger worker.
  - `crates/apex-runtime/src/shadow/race.rs`: `RaceLine`, `RaceWriter`.
- Modify:
  - `crates/apex-runtime/src/lib.rs` (`pub mod probe;`), `crates/apex-runtime/src/main.rs` (dispatch).
  - `crates/apex-runtime/src/shadow/mod.rs` (the worker, the capture hook, the tripwire, the report), `shadow/queue.rs` (`Pending::received`), `shadow/config.rs` (`race`, `flashblocks`).
  - `ops/shadow.base.yaml`, `scripts/shadow-status.sh`.
- Create:
  - `scripts/data/race_analysis.py`, `scripts/data/test_race_analysis.py`.
  - `ops/host/{apex-shadow.service,provision.sh,deploy.sh,shadow.env.example,README.md}`.
- Tests:
  - `crates/apex-chain/tests/flashblock_stream.rs` (fixture `tests/fixtures/flashblocks_base.json`, recorded 2026-10-12).
  - `crates/apex-chain/tests/ws_feed.rs` (or wherever `parse_notification` is tested).
  - `crates/apex-runtime/tests/{probe.rs,shadow_race.rs,shadow_queue.rs,shadow_parts.rs}`.

---

### Task 1: The flashblock reader

**Files:** create `crates/apex-chain/src/base/flashblock_stream.rs`; modify `crates/apex-chain/src/base/mod.rs` and `crates/apex-chain/Cargo.toml`; test `crates/apex-chain/tests/flashblock_stream.rs`.

**Interfaces — Produces:**
- `pub const BASE_FLASHBLOCKS_WS: &str`.
- `pub struct Frame { pub block: u64, pub index: u32, pub transactions: u32, pub block_timestamp: Option<u64> }`.
- `pub enum FrameError { Brotli, TooLarge, Json, Missing(&'static str), Inconsistent }`.
- `pub fn decode_frame(bytes: &[u8], binary: bool) -> Result<Frame, FrameError>`.
- `pub struct FlashblockSeen { pub block: u64, pub index: u32, pub arrived_at_ns: u64, pub first_tx: Option<u32>, pub tx_count: u32, pub block_timestamp: Option<u64> }` (Serialize).
- `pub struct FlashblockIndexer`: `new()`, `observe(&mut self, f: &Frame, arrived_at_ns: u64) -> Option<FlashblockSeen>`, `incomplete_blocks(&self) -> u64`, `locate(&self, block: u64, tx_index: u32) -> Option<FlashblockSeen>`.
- `pub struct FlashblockStream`: `new(url: &str) -> Self`, `stats() -> Arc<FlashblockStreamStats>`, `spawn(self, stop: watch::Receiver<bool>) -> (JoinHandle<()>, mpsc::Receiver<(Frame, u64)>)`.
- `FlashblockStreamStats`: `sessions()`, `frames()`, `decode_failures()`.

- [ ] **Step 1: Write the failing tests** in `crates/apex-chain/tests/flashblock_stream.rs`:

```rust
//! R25: Base's public flashblock stream, decoded from frames recorded on
//! 2026-10-12 (block 52,477,080 whole; 52,477,081 indexes 0-3).

use apex_chain::base::flashblock_stream::{decode_frame, FlashblockIndexer, FrameError};
use serde_json::Value;

fn frames() -> Vec<(Vec<u8>, bool, Value)> {
    let f: Value = serde_json::from_str(include_str!("fixtures/flashblocks_base.json")).unwrap();
    f["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (alloy_primitives::hex::decode(x["hex"].as_str().unwrap()).unwrap(), x["binary"].as_bool().unwrap(), x["expect"].clone()))
        .collect()
}

#[test]
fn every_recorded_frame_decodes_to_what_node_read_from_it() {
    for (bytes, binary, expect) in frames() {
        let f = decode_frame(&bytes, binary).expect("decodes");
        assert_eq!(u64::from(f.index), expect["index"].as_u64().unwrap());
        assert_eq!(f.block, expect["block"].as_u64().unwrap());
        assert_eq!(u64::from(f.transactions), expect["txs"].as_u64().unwrap());
        assert_eq!(f.block_timestamp, expect["base_ts"].as_u64());
    }
}

/// Block 52,477,080's indexes carry 1, 39, 9, 9, 8, 9, 7, 11, 10, 5 and 26
/// transactions: each flashblock's range starts where the last one ended.
#[test]
fn a_whole_block_gets_contiguous_transaction_ranges() {
    let mut ix = FlashblockIndexer::new();
    let seen: Vec<_> = frames()
        .iter()
        .enumerate()
        .filter_map(|(i, (b, bin, _))| ix.observe(&decode_frame(b, *bin).unwrap(), 1_000 + i as u64))
        .collect();
    let firsts: Vec<Option<u32>> = seen.iter().take(11).map(|s| s.first_tx).collect();
    assert_eq!(firsts, [0, 1, 40, 49, 58, 66, 75, 82, 93, 103, 108].map(Some).to_vec());
    assert_eq!(seen[11].first_tx, Some(0), "the next block starts again at 0");
    assert_eq!(seen[11].block_timestamp, Some(1_791_743_509));
    assert_eq!(ix.incomplete_blocks(), 0);
    // A transaction is found by its index: 92 is index 7's last, 93 index 8's first.
    assert_eq!(ix.locate(52_477_080, 92).map(|s| s.index), Some(7));
    assert_eq!(ix.locate(52_477_080, 93).map(|s| s.index), Some(8));
    assert_eq!(ix.locate(52_477_080, 134), None, "past the block's last transaction");
}

/// A block joined mid-way, or with a missing index, has no trustworthy
/// ranges: marked incomplete, ranges unknown, never guessed.
#[test]
fn a_hole_makes_a_block_incomplete() {
    let fr = frames();
    let mut ix = FlashblockIndexer::new();
    let d = |i: usize| decode_frame(&fr[i].0, fr[i].1).unwrap();
    ix.observe(&d(0), 1);
    ix.observe(&d(1), 2);
    let after_hole = ix.observe(&d(3), 3).unwrap(); // index 2 skipped
    assert_eq!(after_hole.first_tx, None);
    assert_eq!(ix.incomplete_blocks(), 1);
    assert_eq!(ix.locate(52_477_080, 50), None);
    let mut late = FlashblockIndexer::new();
    assert_eq!(late.observe(&d(5), 1).unwrap().first_tx, None, "joined at index 5");
    assert!(ix.observe(&d(1), 9).is_none(), "a repeated index is ignored");
}

#[test]
fn a_frame_that_does_not_decode_says_why() {
    let (bytes, _, _) = &frames()[0];
    // A truncated stream fails in the decompressor or, if it yields a prefix, in the JSON.
    assert!(matches!(decode_frame(&bytes[..bytes.len() / 2], true), Err(FrameError::Brotli | FrameError::Json)));
    assert_eq!(decode_frame(b"{\"index\":0}", false), Err(FrameError::Missing("metadata.block_number")));
    assert_eq!(decode_frame(b"not json", false), Err(FrameError::Json));
}
```

- [ ] **Step 2: Run, expect a compile failure:** `cargo test -j 3 -p apex-chain --test flashblock_stream`

- [ ] **Step 3: Implement.**
  - `Cargo.toml` `[dependencies]`: `brotli-decompressor = "5"`.
  - `base/mod.rs`: `pub mod flashblock_stream;`.
  - `flashblock_stream.rs`:

```rust
//! Base's public flashblock stream (Task 8.5 R25): when each flashblock
//! reached this host, and which transaction indexes it added to its block.
//!
//! **Measurement only.** Nothing priced or signed reads it: it is the clock the
//! race ledger is measured against. The stream carries raw transactions and no
//! receipts or logs (2026-10-11), so it cannot replace `pendingLogs`.
//!
//! A frame is brotli-compressed JSON. Index 0 carries `base` (hex block number
//! and timestamp). Every frame carries `index`, `metadata.block_number` (a
//! number) and `diff.transactions`. A block is trusted only whole: joined
//! mid-way or with a missing index, its ranges are unknown and never guessed.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

pub const BASE_FLASHBLOCKS_WS: &str = "wss://mainnet.flashblocks.base.org/ws";

/// A decompressed frame is a few kilobytes; one past this is not a flashblock.
const MAX_FRAME_BYTES: u64 = 8 << 20;
/// Blocks kept for `locate`: two minutes of Base.
const KEEP_BLOCKS: u64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub block: u64,
    pub index: u32,
    pub transactions: u32,
    /// From index 0's `base`.
    pub block_timestamp: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Brotli,
    TooLarge,
    Json,
    Missing(&'static str),
    /// `base.block_number` disagrees with `metadata.block_number`.
    Inconsistent,
}

fn hex_u64(v: &Value) -> Option<u64> {
    u64::from_str_radix(v.as_str()?.strip_prefix("0x")?, 16).ok()
}

pub fn decode_frame(bytes: &[u8], binary: bool) -> Result<Frame, FrameError> {
    let text = if binary {
        let mut out = Vec::new();
        brotli_decompressor::Decompressor::new(bytes, 4096)
            .take(MAX_FRAME_BYTES + 1)
            .read_to_end(&mut out)
            .map_err(|_| FrameError::Brotli)?;
        if out.len() as u64 > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        out
    } else {
        bytes.to_vec()
    };
    let v: Value = serde_json::from_slice(&text).map_err(|_| FrameError::Json)?;
    let index = v
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|i| u32::try_from(i).ok())
        .ok_or(FrameError::Missing("index"))?;
    let block = v
        .get("metadata")
        .and_then(|m| m.get("block_number"))
        .and_then(Value::as_u64)
        .ok_or(FrameError::Missing("metadata.block_number"))?;
    let transactions = v
        .get("diff")
        .and_then(|d| d.get("transactions"))
        .and_then(Value::as_array)
        .and_then(|a| u32::try_from(a.len()).ok())
        .ok_or(FrameError::Missing("diff.transactions"))?;
    let base = v.get("base");
    if let Some(b) = base.and_then(|b| b.get("block_number")).and_then(hex_u64) {
        if b != block {
            return Err(FrameError::Inconsistent);
        }
    }
    Ok(Frame { block, index, transactions, block_timestamp: base.and_then(|b| b.get("timestamp")).and_then(hex_u64) })
}

/// One flashblock, as it reached this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FlashblockSeen {
    pub block: u64,
    pub index: u32,
    pub arrived_at_ns: u64,
    /// The first transaction index it added; `None` when the block was not
    /// seen whole up to here.
    pub first_tx: Option<u32>,
    pub tx_count: u32,
    pub block_timestamp: Option<u64>,
}

#[derive(Debug)]
struct BlockState {
    next_index: u32,
    txs: u32,
    complete: bool,
    seen: Vec<FlashblockSeen>,
}

/// Each block's flashblocks, in order, with their transaction ranges.
#[derive(Debug, Default)]
pub struct FlashblockIndexer {
    blocks: BTreeMap<u64, BlockState>,
    incomplete: u64,
}

impl FlashblockIndexer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `None` for a repeated or out-of-order index, which adds nothing.
    pub fn observe(&mut self, f: &Frame, arrived_at_ns: u64) -> Option<FlashblockSeen> {
        let state = self.blocks.entry(f.block).or_insert_with(|| BlockState {
            next_index: 0,
            txs: 0,
            complete: true,
            seen: Vec::new(),
        });
        if f.index < state.next_index {
            return None;
        }
        if f.index > state.next_index && state.complete {
            state.complete = false;
            self.incomplete += 1;
        }
        let first_tx = state.complete.then_some(state.txs);
        state.txs = state.txs.saturating_add(f.transactions);
        state.next_index = f.index + 1;
        let seen = FlashblockSeen {
            block: f.block,
            index: f.index,
            arrived_at_ns,
            first_tx,
            tx_count: f.transactions,
            block_timestamp: f.block_timestamp,
        };
        state.seen.push(seen);
        let newest = f.block;
        self.blocks.retain(|b, _| *b + KEEP_BLOCKS > newest);
        Some(seen)
    }

    /// Blocks marked incomplete since the start.
    pub fn incomplete_blocks(&self) -> u64 {
        self.incomplete
    }

    /// The flashblock that added transaction `tx_index` to `block`, if the block
    /// was seen whole that far.
    pub fn locate(&self, block: u64, tx_index: u32) -> Option<FlashblockSeen> {
        self.blocks.get(&block)?.seen.iter().copied().find(|s| {
            s.first_tx.is_some_and(|first| tx_index >= first && tx_index < first.saturating_add(s.tx_count))
        })
    }
}

/// What the stream has been through.
#[derive(Debug, Default)]
pub struct FlashblockStreamStats {
    sessions: AtomicU64,
    frames: AtomicU64,
    decode_failures: AtomicU64,
}

impl FlashblockStreamStats {
    pub fn sessions(&self) -> u64 {
        self.sessions.load(Ordering::Relaxed)
    }
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
    pub fn decode_failures(&self) -> u64 {
        self.decode_failures.load(Ordering::Relaxed)
    }
}

/// The public stream, supervised: reconnects with backoff, stamps each frame on
/// receipt, and never blocks on its consumer.
pub struct FlashblockStream {
    url: String,
    stats: Arc<FlashblockStreamStats>,
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

impl FlashblockStream {
    pub fn new(url: &str) -> Self {
        Self { url: url.to_string(), stats: Arc::new(FlashblockStreamStats::default()) }
    }

    pub fn stats(&self) -> Arc<FlashblockStreamStats> {
        Arc::clone(&self.stats)
    }

    pub fn spawn(self, stop: watch::Receiver<bool>) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<(Frame, u64)>) {
        let (tx, rx) = mpsc::channel(1_024);
        (tokio::spawn(async move { self.run(tx, stop).await }), rx)
    }

    async fn run(self, tx: mpsc::Sender<(Frame, u64)>, mut stop: watch::Receiver<bool>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let mut backoff = Duration::from_millis(250);
        while !*stop.borrow() {
            match tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(self.url.as_str())).await {
                Ok(Ok((mut ws, _))) => {
                    self.stats.sessions.fetch_add(1, Ordering::Relaxed);
                    backoff = Duration::from_millis(250);
                    loop {
                        let next = tokio::select! {
                            _ = stop.changed() => { if *stop.borrow() { let _ = ws.close(None).await; return; } continue; }
                            n = tokio::time::timeout(Duration::from_secs(10), ws.next()) => n,
                        };
                        let at = now_ns();
                        let decoded = match next {
                            Ok(Some(Ok(Message::Binary(b)))) => decode_frame(&b, true),
                            Ok(Some(Ok(Message::Text(t)))) => decode_frame(t.as_bytes(), false),
                            Ok(Some(Ok(Message::Ping(p)))) => { let _ = ws.send(Message::Pong(p)).await; continue; }
                            Ok(Some(Ok(_))) => continue,
                            _ => break, // stalled, closed or failed: reconnect
                        };
                        self.stats.frames.fetch_add(1, Ordering::Relaxed);
                        match decoded {
                            Ok(f) => { let _ = tx.try_send((f, at)); }
                            Err(_) => { self.stats.decode_failures.fetch_add(1, Ordering::Relaxed); }
                        }
                    }
                    tracing::warn!(target: "flashblocks", "flashblock stream ended; reconnecting");
                }
                _ => tracing::warn!(target: "flashblocks", "flashblock stream did not connect; retrying"),
            }
            tokio::select! { _ = tokio::time::sleep(backoff) => {} _ = stop.changed() => {} }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }
}
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-chain --test flashblock_stream`

---

### Task 2: `transaction_index` and `apex probe`

**Files:**
- Modify `crates/apex-chain/src/rpc/ws.rs` (`RawLog::transaction_index`).
- Create `crates/apex-runtime/src/probe.rs`; modify `crates/apex-runtime/src/lib.rs` and `src/main.rs`.
- Test `crates/apex-runtime/tests/probe.rs`, plus `RawLog` literals across the tests (`live_feed.rs`'s `swap()` and `sync_log()`, and any others the compiler names).

**Interfaces:**
- Consumes: Task 1.
- Produces:
  - `RawLog::transaction_index: Option<u64>`.
  - `probe::percentiles(v: &mut Vec<i64>) -> Option<(i64, i64, i64)>` (p10, p50, p90).
  - `probe::arrival_offsets_ms(seen: &[FlashblockSeen]) -> Vec<i64>`.
  - `probe::pending_log_delays_ms(ix: &FlashblockIndexer, logs: &[(u64, u32, u64)]) -> Vec<i64>`.
  - `probe::run(minutes: u64, env: &Env) -> ProbeReport` (Serialize).

- [ ] **Step 1: Write the failing tests** in `crates/apex-runtime/tests/probe.rs`:

```rust
//! R25: the region probe's arithmetic.

use apex_chain::base::flashblock_stream::{FlashblockIndexer, FlashblockSeen, Frame};
use apex_runtime::probe::{arrival_offsets_ms, pending_log_delays_ms, percentiles};

/// Index 0 of a block stamped 1,000 s opens at 998 s; one reaching us at
/// 998.128 s arrived 128 ms after its slot opened. Other indexes don't count.
#[test]
fn the_arrival_offset_is_measured_from_the_slot_opening() {
    let seen = |index, at_ms: u64, ts| FlashblockSeen {
        block: 1, index, arrived_at_ns: at_ms * 1_000_000, first_tx: Some(0), tx_count: 1, block_timestamp: ts,
    };
    let v = arrival_offsets_ms(&[seen(0, 998_128, Some(1_000)), seen(1, 998_300, None), seen(0, 1_000_131, Some(1_002))]);
    assert_eq!(v, vec![128, 131]);
}

/// A log is matched to the flashblock that added its transaction index.
#[test]
fn a_pending_log_is_timed_against_the_flashblock_that_carried_it() {
    let mut ix = FlashblockIndexer::new();
    let f = |index, n| Frame { block: 7, index, transactions: n, block_timestamp: None };
    ix.observe(&f(0, 1), 1_000_000_000);
    ix.observe(&f(1, 10), 1_200_000_000);
    // tx 5 rode index 1 (txs 1..=10), arriving 45 ms after it; tx 99 matches nothing.
    let d = pending_log_delays_ms(&ix, &[(7, 5, 1_245_000_000), (7, 99, 1_300_000_000)]);
    assert_eq!(d, vec![45]);
}

#[test]
fn percentiles_are_nearest_rank() {
    let mut v: Vec<i64> = (1..=100).collect();
    assert_eq!(percentiles(&mut v), Some((10, 50, 90)));
    assert_eq!(percentiles(&mut Vec::new()), None);
}
```

  Also, wherever `parse_notification` is tested in `apex-chain`, add: a `pendingLogs` result carrying `"transactionIndex": "0x12f"` parses `transaction_index == Some(303)`; one without it parses `None`.

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 -p apex-runtime --test probe`

- [ ] **Step 3: Implement.**
  - `ws.rs`:
    - Add `pub transaction_index: Option<u64>,` after `log_index` in `RawLog`, documented "the log's transaction's position in its block, which a preconfirmed log already has".
    - Fill it in `parse_notification` with `transaction_index: r.get("transactionIndex").and_then(quantity),`.
    - Add `transaction_index: None,` to every `RawLog { … }` literal the compiler names.
  - `probe.rs`:

```rust
//! `apex probe` (Task 8.5 R25): how far this host is from Base's builder, and
//! what BlockPI adds. Runs no trading component; prints one JSON report.
//!
//! - **arrival offset**: index-0 flashblock arrival minus its slot's opening
//!   (block timestamp − 2 s);
//! - **round trips** to Base's RPC origin and its edge, the sequencer's
//!   endpoint and BlockPI;
//! - **BlockPI's delay**: a `pendingLogs` swap's arrival minus the raw
//!   flashblock that carried its transaction.

use apex_chain::base::flashblock_stream::{FlashblockIndexer, FlashblockSeen, FlashblockStream, BASE_FLASHBLOCKS_WS};
use apex_chain::rpc::ws::{LogFilter, Notification, Subscription, WsFeed, WsSettings};
use crate::live::feed::{PANCAKE_SWAP, SWAP};
use apex_config::Env;
use serde::Serialize;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Four deep WETH/USDC pools: Uniswap v3 5 bp and 30 bp, PancakeSwap 1 bp,
/// Slipstream spacing 100. Enough swaps an hour to time BlockPI.
const PROBE_POOLS: [&str; 4] = [
    "d0b53D9277642d899DF5C87A3966A349A798F224",
    "6c561B446416E1A00E8E93E221854d6eA4171372",
    "72ab388e2E2F6FaceF59E3C3FA2C4E29011c2D38",
    "b2cc224c1c9feE385f8ad6a55b4d94E92359DC59",
];

pub fn percentiles(v: &mut Vec<i64>) -> Option<(i64, i64, i64)> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let at = |p: usize| v[((v.len() * p).div_ceil(100)).saturating_sub(1).min(v.len() - 1)];
    Some((at(10), at(50), at(90)))
}

pub fn arrival_offsets_ms(seen: &[FlashblockSeen]) -> Vec<i64> {
    seen.iter()
        .filter(|s| s.index == 0)
        .filter_map(|s| {
            let opened_ns = s.block_timestamp?.checked_sub(2)?.checked_mul(1_000_000_000)?;
            Some((s.arrived_at_ns as i64 - opened_ns as i64) / 1_000_000)
        })
        .collect()
}

pub fn pending_log_delays_ms(ix: &FlashblockIndexer, logs: &[(u64, u32, u64)]) -> Vec<i64> {
    logs.iter()
        .filter_map(|(block, tx, at)| {
            let f = ix.locate(*block, *tx)?;
            Some((*at as i64 - f.arrived_at_ns as i64) / 1_000_000)
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct Quantiles { pub n: usize, pub p10: i64, pub p50: i64, pub p90: i64 }

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub minutes: u64,
    pub flashblocks: u64,
    pub incomplete_blocks: u64,
    pub arrival_offset_ms: Option<Quantiles>,
    pub blockpi_delay_ms: Option<Quantiles>,
    /// `(name, p50_ms, min_ms)`.
    pub round_trips: Vec<(String, u64, u64)>,
}

fn quantiles(mut v: Vec<i64>) -> Option<Quantiles> {
    let n = v.len();
    percentiles(&mut v).map(|(p10, p50, p90)| Quantiles { n, p10, p50, p90 })
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

async fn round_trip(client: &reqwest::Client, url: &str, body: Option<&'static str>) -> Option<u64> {
    let start = Instant::now();
    let r = match body {
        Some(b) => client.post(url).header("content-type", "application/json").body(b).send().await,
        None => client.get(url).send().await,
    };
    r.ok()?.bytes().await.ok()?;
    Some(start.elapsed().as_millis() as u64)
}

pub async fn run(minutes: u64, env: &Env) -> ProbeReport {
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let stream = FlashblockStream::new(BASE_FLASHBLOCKS_WS);
    let (_fb, mut frames) = stream.spawn(stop.clone());
    let key = env.get("APEX_SECRET_BLOCKPI_KEY");
    let mut logs_rx = key.as_ref().and_then(|k| {
        let filter = LogFilter {
            addresses: PROBE_POOLS.iter().filter_map(|a| a.parse().ok()).collect(),
            topics0: vec![SWAP, PANCAKE_SWAP],
        };
        WsFeed::new(&format!("wss://base.blockpi.network/v1/ws/{k}"), vec![Subscription::PendingLogs(filter)], WsSettings::default())
            .ok()
            .map(|f| f.spawn(stop.clone()).1)
    });
    let client = reqwest::Client::builder().user_agent("apex-probe").build().expect("a client");
    let blockpi = key.map(|k| format!("https://base.blockpi.network/v1/rpc/{k}"));
    let mut rtt: std::collections::BTreeMap<&'static str, Vec<u64>> = Default::default();
    let mut ix = FlashblockIndexer::new();
    let mut seen = Vec::new();
    let mut logs = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    while Instant::now() < deadline {
        tokio::select! {
            Some((f, at)) = frames.recv() => { if let Some(s) = ix.observe(&f, at) { seen.push(s); } }
            Some(n) = async { match logs_rx.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => {
                if let Notification::Log(l) = n {
                    if let (Some(b), Some(t)) = (l.block_number, l.transaction_index) {
                        logs.push((b, t as u32, now_ns()));
                    }
                }
            }
            _ = tick.tick() => {
                const BN: &str = r#"{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}"#;
                const CID: &str = r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#;
                for (name, url, body) in [
                    ("base_edge", "https://mainnet.base.org/cdn-cgi/trace", None),
                    ("base_rpc", "https://mainnet.base.org", Some(BN)),
                    ("sequencer", "https://mainnet-sequencer.base.org", Some(CID)),
                ] {
                    if let Some(ms) = round_trip(&client, url, body).await { rtt.entry(name).or_default().push(ms); }
                }
                if let Some(u) = blockpi.as_deref() {
                    if let Some(ms) = round_trip(&client, u, Some(BN)).await { rtt.entry("blockpi").or_default().push(ms); }
                }
            }
        }
    }
    let _ = stop_tx.send(true);
    let round_trips = rtt
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_unstable();
            (k.to_string(), v[v.len() / 2], v[0])
        })
        .collect();
    ProbeReport {
        minutes,
        flashblocks: seen.len() as u64,
        incomplete_blocks: ix.incomplete_blocks(),
        arrival_offset_ms: quantiles(arrival_offsets_ms(&seen)),
        blockpi_delay_ms: quantiles(pending_log_delays_ms(&ix, &logs)),
        round_trips,
    }
}
```

  - `lib.rs`: `pub mod probe;`.
  - `main.rs`, before the `shadow` dispatch:

```rust
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return probe().await;
    }
```

  and:

```rust
/// `apex probe [--minutes N]`: the region probe (R25). Reads only
/// `APEX_SECRET_BLOCKPI_KEY`, and runs without it (no BlockPI figures).
async fn probe() -> ExitCode {
    let mut minutes = 60u64;
    let mut argv = std::env::args().skip(2);
    while let Some(flag) = argv.next() {
        match (flag.as_str(), argv.next().and_then(|v| v.parse().ok())) {
            ("--minutes", Some(m)) if m > 0 => minutes = m,
            _ => {
                error!("usage: apex probe [--minutes N]");
                return ExitCode::FAILURE;
            }
        }
    }
    let report = apex_runtime::probe::run(minutes, &Env::secrets_only()).await;
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    ExitCode::SUCCESS
}
```

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-runtime --test probe` and `cargo test -j 3 -p apex-chain`

- [ ] **Step 5: Run the probe on the laptop** (spec verification 2): `BLOCKPI_KEY=…` mapped as `APEX_SECRET_BLOCKPI_KEY=$(grep ^BLOCKPI_KEY= .env | cut -d= -f2-) target/debug/apex probe --minutes 5`. Expected: an arrival offset p50 ≈ 128 ms (± 20 ms) and a BlockPI delay p50 ≈ 45 ms (± 20 ms), matching the spec's laptop figures.

---

### Task 3: The shadow's ledgers, tripwire and report

**Files:**
- Create `crates/apex-runtime/src/shadow/ledger.rs` and `crates/apex-runtime/src/shadow/race.rs`.
- Modify `shadow/mod.rs`, `shadow/queue.rs`, `shadow/config.rs`, `ops/shadow.base.yaml` and `scripts/shadow-status.sh`.
- Test `crates/apex-runtime/tests/shadow_race.rs`; update `tests/shadow_queue.rs` (merge's new argument) and `tests/shadow_parts.rs` (the config yaml).

**Interfaces:**
- Consumes: Task 1's reader and indexer.
- Produces:
  - `Pending { event, proposal, received: UnixNanos }`.
  - `PendingQueue::merge(&mut self, event: &Arc<StateEvent>, proposals: Vec<RouteProposal>, received: UnixNanos)`.
  - `race::RaceLine` (Serialize), `race::RaceWriter::{new(capacity) -> (Self, Receiver), record(&self, line), written(), dropped()}`.
  - `race::outcome_of(&Handled) -> (RaceOutcome, Option<u64>)` (the outcome, and the ticket id when one was admitted).
  - `ledger::daily_path(base: &Path, at_ns: u64) -> PathBuf`.
  - `shadow::nonce_moved(boot: Option<u64>, now: u64) -> bool`.
  - Config: `race: PathBuf`, `flashblocks: FlashblocksConfig { stream: String, ledger: PathBuf }`.

- [ ] **Step 1: Write the failing tests** in `crates/apex-runtime/tests/shadow_race.rs`:

```rust
//! R25: the race ledger never blocks capture, says what happened, and the
//! nonce tripwire fires on any move.

use apex_runtime::shadow::ledger::daily_path;
use apex_runtime::shadow::nonce_moved;
use apex_runtime::shadow::race::{RaceLine, RaceOutcome, RaceTrigger, RaceWriter};

fn line(n: u8) -> RaceLine {
    RaceLine {
        route_hash: format!("0x{n:064x}"),
        net_hint_wei: Some("1000".into()),
        trigger: RaceTrigger { block: 52_342_205, log_index: 51, tx: format!("0x{n:064x}") },
        observed_ns: 1,
        received_ns: 2,
        proposed_ns: 3,
        picked_ns: 4,
        handled_ns: 5,
        outcome: RaceOutcome::Stale,
        ticket: None,
    }
}

/// A full channel drops the line and counts it; capture never waits.
#[test]
fn a_full_race_channel_drops_and_counts() {
    let (w, _rx) = RaceWriter::new(2);
    for i in 0..5 {
        w.record(line(i));
    }
    assert_eq!((w.written(), w.dropped()), (2, 3));
}

#[test]
fn a_race_line_serializes_its_trigger_and_stages() {
    let v = serde_json::to_value(line(7)).unwrap();
    assert_eq!(v["trigger"]["block"], 52_342_205);
    assert_eq!(v["trigger"]["log_index"], 51);
    assert_eq!(v["received_ns"], 2);
    assert_eq!(v["outcome"], "stale");
}

/// One file per UTC day: the analyzer globs them.
#[test]
fn ledgers_rotate_by_utc_day() {
    let p = daily_path(std::path::Path::new("var/apex/race.jsonl"), 1_791_743_507_000_000_000);
    assert_eq!(p, std::path::PathBuf::from("var/apex/race-2026-10-12.jsonl"));
}

/// A shadow never broadcasts, so the signer's nonce cannot move.
#[test]
fn the_tripwire_fires_on_any_move_of_the_nonce() {
    assert!(!nonce_moved(None, 7), "no boot reading yet");
    assert!(!nonce_moved(Some(7), 7));
    assert!(nonce_moved(Some(7), 8));
    assert!(nonce_moved(Some(7), 6));
}
```

  In `shadow_parts.rs`'s yaml, after `misses: var/m.jsonl`, add:

```yaml
race: var/r.jsonl
flashblocks:
  stream: wss://mainnet.flashblocks.base.org/ws
  ledger: var/f.jsonl
```

  Assert in its config test that `c.race == PathBuf::from("var/r.jsonl")` and `c.flashblocks.stream == "wss://mainnet.flashblocks.base.org/ws"`. In `shadow_queue.rs`, every `merge(&ev, props)` becomes `merge(&ev, props, UnixNanos(0))`.

- [ ] **Step 2: Run, expect compile failures:** `cargo test -j 3 -p apex-runtime --test shadow_race --test shadow_parts --test shadow_queue`

- [ ] **Step 3: Implement.**

`race.rs`:

```rust
//! The race ledger (Task 8.5 R25): one line per handled proposal, written off
//! the capture path. A full channel drops the line and counts it; capture
//! never waits on a file.

use crate::plane::Handled;
use apex_types::ticket::TicketOutcome;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RaceTrigger {
    pub block: u64,
    pub log_index: u64,
    pub tx: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RaceOutcome {
    Stale,
    Declined { reason: String },
    Suppressed,
    Redelivered,
    Dispatched,
    Failed { class: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RaceLine {
    pub route_hash: String,
    pub net_hint_wei: Option<String>,
    pub trigger: RaceTrigger,
    pub observed_ns: u64,
    pub received_ns: u64,
    pub proposed_ns: u64,
    pub picked_ns: u64,
    pub handled_ns: u64,
    pub outcome: RaceOutcome,
    pub ticket: Option<u64>,
}

pub fn outcome_of(h: &Handled) -> (RaceOutcome, Option<u64>) {
    match h {
        Handled::Closed { ticket, outcome } => {
            let o = match &**outcome {
                TicketOutcome::Success { .. } | TicketOutcome::ShadowDispatched { .. } => RaceOutcome::Dispatched,
                other => RaceOutcome::Failed { class: format!("{other:?}").chars().take(80).collect() },
            };
            (o, Some(ticket.0))
        }
        Handled::Declined(d) => (RaceOutcome::Declined { reason: d.to_string() }, None),
        Handled::Suppressed(_) => (RaceOutcome::Suppressed, None),
        Handled::Redelivered { .. } => (RaceOutcome::Redelivered, None),
    }
}

pub struct RaceWriter {
    tx: mpsc::Sender<RaceLine>,
    written: AtomicU64,
    dropped: AtomicU64,
}

impl RaceWriter {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<RaceLine>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (Self { tx, written: AtomicU64::new(0), dropped: AtomicU64::new(0) }, rx)
    }

    pub fn record(&self, line: RaceLine) {
        match self.tx.try_send(line) {
            Ok(()) => self.written.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.dropped.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
```

(If `TicketId`'s inner field is not `.0: u64`, use its accessor; the compiler names it.)

`ledger.rs`:

```rust
//! The flashblock ledger (Task 8.5 R25): Base's public flashblock stream, as
//! it reached this host, one JSON line per flashblock, a file per UTC day.

use std::path::{Path, PathBuf};

/// `var/apex/race.jsonl` at 2026-10-12 is `var/apex/race-2026-10-12.jsonl`.
pub fn daily_path(base: &Path, at_ns: u64) -> PathBuf {
    let days = (at_ns / 1_000_000_000 / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("ledger");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    base.with_file_name(format!("{stem}-{y:04}-{m:02}-{d:02}.{ext}"))
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Howard
/// Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Append one JSON line to the day's file, creating it as needed. A failed
/// write is returned to the caller, which counts it and carries on.
pub fn append_line(base: &Path, at_ns: u64, line: &impl serde::Serialize) -> std::io::Result<()> {
    use std::io::Write;
    let path = daily_path(base, at_ns);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    f.write_all(text.as_bytes())
}
```

`queue.rs`:
- `Pending` gains `pub received: UnixNanos`, documented "when the capture loop took the event: the end of detection (G1)".
- `merge` takes `received: UnixNanos` and stores it in each `Pending`.
- `shadow/mod.rs`'s `take` passes `SystemClock.now()` taken at the top of `take`.

`config.rs`:
- `Raw` gains `race: PathBuf` and `flashblocks: FlashblocksConfig`.
- `ShadowConfig` gains `pub race: PathBuf, pub flashblocks: FlashblocksConfig`, mapped through.

```rust
/// Base's public flashblock stream and where its ledger goes (R25).
/// Measurement only.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashblocksConfig {
    pub stream: String,
    pub ledger: PathBuf,
}
```

`ops/shadow.base.yaml`, after `misses:`:

```yaml
# R25: the race ledger (one line per handled proposal) and Base's public
# flashblock stream with its ledger. Measurement only; nothing priced reads
# them. Each file is one per UTC day: race-YYYY-MM-DD.jsonl.
race: var/apex/race.jsonl
flashblocks:
  stream: wss://mainnet.flashblocks.base.org/ws
  ledger: var/apex/flashblocks.jsonl
```

`shadow/mod.rs`:
- `pub mod ledger; pub mod race;`.
- `Shadow` gains:
  - `race: Arc<race::RaceWriter>`;
  - `flashblock_stats: OnceLock<Arc<FlashblockStreamStats>>`;
  - `incomplete_blocks: AtomicU64`;
  - `ledger_failures: AtomicU64`;
  - `boot_nonce: OnceLock<u64>`;
  - `nonce_moved: AtomicBool`.
- In `boot`, build `let (race, race_rx) = race::RaceWriter::new(4_096);`, and keep `race_rx` for the writer task.
- Spawn two workers beside the others. The race writer:

```rust
async fn race_writer(&self, mut rx: tokio::sync::mpsc::Receiver<race::RaceLine>) {
    while let Some(line) = rx.recv().await {
        if ledger::append_line(&self.config.race, line.handled_ns, &line).is_err() {
            self.ledger_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}
```

  And the flashblock ledger:

```rust
async fn flashblock_ledger(&self, stop: tokio::sync::watch::Receiver<bool>) {
    let stream = FlashblockStream::new(&self.config.flashblocks.stream);
    let _ = self.flashblock_stats.set(stream.stats());
    let (_h, mut rx) = stream.spawn(stop);
    let mut ix = FlashblockIndexer::new();
    while let Some((f, at)) = rx.recv().await {
        if let Some(seen) = ix.observe(&f, at) {
            if ledger::append_line(&self.config.flashblocks.ledger, at, &seen).is_err() {
                self.ledger_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.incomplete_blocks.store(ix.incomplete_blocks(), Ordering::Relaxed);
    }
}
```

- The capture hook, in `capture` after `handle_proposal`, records the best entry; each stale entry is recorded with `RaceOutcome::Stale`:

```rust
fn race_line(p: &Pending, picked: UnixNanos, handled_at: UnixNanos, outcome: (race::RaceOutcome, Option<u64>)) -> race::RaceLine {
    let tx = match &p.event.kind {
        EventKind::PendingSwap { target, .. } => format!("{target:#x}"),
        _ => String::new(),
    };
    race::RaceLine {
        route_hash: format!("{:#x}", p.proposal.route.route_hash),
        net_hint_wei: p.proposal.net_hint.map(|n| n.to_string()),
        trigger: race::RaceTrigger { block: p.event.at.block, log_index: p.event.at.log_index, tx },
        observed_ns: p.event.observed_at.0,
        received_ns: p.received.0,
        proposed_ns: p.proposal.found_at.0,
        picked_ns: picked.0,
        handled_ns: handled_at.0,
        outcome: outcome.0,
        ticket: outcome.1,
    }
}
```

  In `capture`, replace `queue.pop_best(SystemClock.now())` with `let now = SystemClock.now(); let popped = queue.pop_best(now);`. Then:
  - after `handle_proposal`, call `self.race.record(race_line(&best, now, SystemClock.now(), race::outcome_of(&handled)))`;
  - for each `(p, _)` in `popped.stale`, call `self.race.record(race_line(p, now, now, (race::RaceOutcome::Stale, None)))`.
  - The flashblock-ledger worker takes the run's existing `feed_stopped` watch receiver as its `stop`.
- The tripwire:

```rust
/// Whether the signer's nonce differs from its first reading. A shadow never
/// broadcasts, so any move means the key was used elsewhere (R25).
pub fn nonce_moved(boot: Option<u64>, now: u64) -> bool {
    boot.is_some_and(|b| b != now)
}
```

  In `head_loop`, after reading the view:

```rust
if let Some(v) = self.reader.view() {
    let boot = *self.boot_nonce.get_or_init(|| v.signer_pending_nonce);
    if nonce_moved(Some(boot), v.signer_pending_nonce) {
        self.nonce_moved.store(true, Ordering::Relaxed);
        tracing::error!(boot, now = v.signer_pending_nonce, "the signer's nonce moved: a shadow never sends — the trader key was used elsewhere; revoke it (ops/host/README.md)");
    }
}
```

- The report gains:
  - `flashblocks: FlashblockReport { sessions, frames, decode_failures, incomplete_blocks, ledger_failures }`;
  - `race: RaceReport { written, dropped }`;
  - `signer: SignerReport { boot_nonce: Option<u64>, nonce_moved: bool }`.

`scripts/shadow-status.sh`, after the `reads` line:

```python
    fb = r.get('flashblocks', {}); ra = r.get('race', {}); sg = r.get('signer', {})
    print(f"  flashblocks {fb.get('frames', 0)} frames, {fb.get('sessions', 0)} sessions, {fb.get('decode_failures', 0)} undecoded, {fb.get('incomplete_blocks', 0)} incomplete, {fb.get('ledger_failures', 0)} write failures")
    print(f"  race        {ra.get('written', 0)} lines, {ra.get('dropped', 0)} dropped")
    if sg.get('nonce_moved'):
        print("  SIGNER NONCE MOVED -- revoke the trader key (ops/host/README.md)")
```

And print `/opt/apex/REVISION` when present.

- [ ] **Step 4: Run, expect PASS:** `cargo test -j 3 -p apex-runtime` (whole crate)

---

### Task 4: The contest analyzer

**Files:** create `scripts/data/race_analysis.py` and `scripts/data/test_race_analysis.py`.

**Interfaces:**
- Consumes: `race-*.jsonl` (Task 3), `flashblocks-*.jsonl` (Task 3), `shadow.journal`, and an RPC object with `call(method, params)` (`arb_census.rpc.Rpc`).
- Produces:
  - `Ledger(lines).locate(block, tx_index)`;
  - `Ledger.would_land(t_ns, delta_ns, eps_ns) -> (block, index) | None`;
  - `rival(rpc, pools, block, tx_index, blocks=4) -> dict | None`;
  - `analyze(race, ledger, journal, rpc, route_pools, delta_ns) -> (rows, summary)`.

- [ ] **Step 1: Write the failing tests** in `scripts/data/test_race_analysis.py`. They use a synthetic ledger and a fake RPC in ticket 263's shape: trigger at transaction 303, rivals at 304–308.

```python
import unittest
import race_analysis as R

MS = 1_000_000
POOL_A, POOL_B = "0xb90fe999be6869af0afc557dccfbe169ea3403d6", "0xa6da283cf7d06b2279626e787c329d6acf7e2994"
SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"

def fb(block, index, first, n, at_ms, ts=None):
    return {"block": block, "index": index, "first_tx": first, "tx_count": n, "arrived_at_ns": at_ms * MS, "block_timestamp": ts}

# Block 100: index 2 ends at tx 303 (the trigger), index 3 carries 304-310.
LEDGER = R.Ledger([fb(100, 0, 0, 1, 0, 1), fb(100, 1, 1, 200, 200), fb(100, 2, 201, 103, 370),
                   fb(100, 3, 304, 7, 540), fb(100, 4, 311, 5, 710), fb(101, 0, 0, 1, 2000, 3)])

class FakeRpc:
    def call(self, method, params):
        if method == "eth_getTransactionReceipt":
            return {"transactionIndex": hex(303), "blockNumber": hex(100)}
        if method == "eth_getLogs":
            def log(pool, txi, tx):
                return {"address": pool, "topics": [SWAP], "blockNumber": hex(100), "transactionIndex": hex(txi), "transactionHash": tx}
            return [log(POOL_A, 303, "0xt"), log(POOL_A, 304, "0xr"), log(POOL_B, 304, "0xr")]
        if method == "eth_getTransactionByHash":
            return {"gasPrice": hex(1_802_500_000), "maxPriorityFeePerGas": None}
        raise AssertionError(method)

class AnalyzerTest(unittest.TestCase):
    def test_a_transaction_is_found_in_the_flashblock_that_added_it(self):
        self.assertEqual(R.Ledger.index_of(LEDGER.locate(100, 303)), 2)
        self.assertEqual(R.Ledger.index_of(LEDGER.locate(100, 304)), 3)
        self.assertIsNone(LEDGER.locate(100, 999))

    def test_would_land_is_the_first_flashblock_after_send_plus_delay_and_margin(self):
        # Sent at 420 ms with delta 5 and eps 20: 445 ms -> index 3 (540 ms).
        self.assertEqual(LEDGER.would_land(420 * MS, 5 * MS, 20 * MS), (100, 3))
        # Sent at 530 ms: 555 ms -> index 4.
        self.assertEqual(LEDGER.would_land(530 * MS, 5 * MS, 20 * MS), (100, 4))

    def test_the_rival_is_the_first_later_transaction_touching_both_pools(self):
        r = R.rival(FakeRpc(), [POOL_A, POOL_B], 100, 303)
        self.assertEqual((r["block"], r["tx_index"], r["tx"]), (100, 304, "0xr"))
        self.assertAlmostEqual(r["priority_gwei"], 1.8025, places=4)

    def test_a_contested_gap_reports_offsets_from_the_trigger_flashblock(self):
        race = [{"route_hash": "0xroute", "trigger": {"block": 100, "log_index": 51, "tx": "0xt"},
                 "observed_ns": 380 * MS, "received_ns": 400 * MS, "proposed_ns": 410 * MS,
                 "picked_ns": 411 * MS, "handled_ns": 650 * MS, "outcome": {"failed": {"class": "MinOutNotMet"}}, "ticket": 263}]
        journal = {263: {"exacting_ns": 412 * MS, "closed_ns": 642 * MS}}
        rows, summary = R.analyze(race, LEDGER, journal, FakeRpc(), {"0xroute": [POOL_A, POOL_B]}, delta_ns=5 * MS)
        row = rows[0]
        self.assertEqual(row["trigger_flashblock"], (100, 2))
        self.assertEqual(row["rival_offset"], 1)              # rival in index 3
        self.assertEqual(row["ours_at_proposed"][20], 1)      # 410+25 -> index 3
        self.assertEqual(row["ours_after_tier2"][20], 2)      # Tier 2 ends 642: 667 -> index 4 (710), offset 2
        self.assertEqual(row["g1_ms"], 30)                    # 400 - 370
        self.assertEqual(summary["g2_share"][20], 1.0)

if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run, expect `ModuleNotFoundError`:** `cd scripts/data && python3 -m unittest test_race_analysis`

- [ ] **Step 3: Implement `scripts/data/race_analysis.py`:**

```python
#!/usr/bin/env python3
"""The contest analyzer (Task 8.5 R25). Per contested gap: the trigger's
flashblock k, the rival's, and the flashblock our transaction would have landed
in, at proposal (step 2's fast path) and after Tier 2 (today). Gates G1 and G2.

Run from scripts/data:
  python3 race_analysis.py --dir ../../var/apex --delta-ms <half the sequencer RTT>
"""
import argparse
import glob
import json
import os
import sys
from pathlib import Path

SWAP_TOPICS = ["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67",
               "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83",
               "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"]
EPSILONS_MS = (0, 20, 40)


class Ledger:
    def __init__(self, lines):
        self.by_block = {}
        for x in lines:
            self.by_block.setdefault(x["block"], []).append(x)
        for v in self.by_block.values():
            v.sort(key=lambda x: x["index"])
        self.ordered = sorted((x for v in self.by_block.values() for x in v), key=lambda x: x["arrived_at_ns"])

    @staticmethod
    def index_of(x):
        return None if x is None else x["index"]

    def locate(self, block, tx_index):
        for x in self.by_block.get(block, []):
            if x["first_tx"] is not None and x["first_tx"] <= tx_index < x["first_tx"] + x["tx_count"]:
                return x
        return None

    def would_land(self, t_ns, delta_ns, eps_ns):
        due = t_ns + delta_ns + eps_ns
        for x in self.ordered:
            if x["arrived_at_ns"] >= due:
                return (x["block"], x["index"])
        return None

    def offset(self, frm, to):
        """Flashblocks from `frm` to `to`, both (block, index), counting across blocks."""
        if frm is None or to is None:
            return None
        seq = [(x["block"], x["index"]) for x in self.ordered]
        try:
            return seq.index(to) - seq.index(frm)
        except ValueError:
            return None


def rival(rpc, pools, block, tx_index, blocks=4):
    logs = rpc.call("eth_getLogs", [{"address": pools, "topics": [SWAP_TOPICS],
                                     "fromBlock": hex(block), "toBlock": hex(block + blocks)}])
    by_tx = {}
    for l in logs:
        b, t = int(l["blockNumber"], 16), int(l["transactionIndex"], 16)
        if (b, t) <= (block, tx_index):
            continue
        by_tx.setdefault((b, t, l["transactionHash"]), set()).add(l["address"].lower())
    for (b, t, tx), touched in sorted(by_tx.items()):
        if touched >= {p.lower() for p in pools}:
            r = rpc.call("eth_getTransactionByHash", [tx])
            prio = r.get("maxPriorityFeePerGas") or r.get("gasPrice") or "0x0"
            return {"block": b, "tx_index": t, "tx": tx, "priority_gwei": int(prio, 16) / 1e9}
    return None


def pct(v, p):
    v = sorted(v)
    return v[max(0, min(len(v) - 1, -(-len(v) * p // 100) - 1))] if v else None


def analyze(race, ledger, journal, rpc, route_pools, delta_ns):
    rows = []
    for line in race:
        pools = route_pools.get(line["route_hash"])
        if not pools:
            continue
        rc = rpc.call("eth_getTransactionReceipt", [line["trigger"]["tx"]]) if line["trigger"]["tx"] else None
        if not rc:
            continue
        tb, ti = int(rc["blockNumber"], 16), int(rc["transactionIndex"], 16)
        k = ledger.locate(tb, ti)
        if k is None:
            continue
        kk = (k["block"], k["index"])
        riv = rival(rpc, pools, tb, ti)
        if riv is None:
            continue
        rf = ledger.locate(riv["block"], riv["tx_index"])
        j = journal.get(line.get("ticket")) or {}
        end = j.get("closed_ns")
        rows.append({
            "route_hash": line["route_hash"], "ticket": line.get("ticket"), "trigger_flashblock": kk,
            "rival_offset": ledger.offset(kk, (rf["block"], rf["index"])) if rf else None,
            "rival_priority_gwei": riv["priority_gwei"],
            "ours_at_proposed": {e: ledger.offset(kk, ledger.would_land(line["proposed_ns"], delta_ns, e * 1_000_000)) for e in EPSILONS_MS},
            "ours_after_tier2": {e: ledger.offset(kk, ledger.would_land(end, delta_ns, e * 1_000_000)) if end else None for e in EPSILONS_MS},
            "g1_ms": (line["received_ns"] - k["arrived_at_ns"]) // 1_000_000,
            "stage_ms": {"observed_to_received": (line["received_ns"] - line["observed_ns"]) // 1_000_000,
                         "received_to_proposed": (line["proposed_ns"] - line["received_ns"]) // 1_000_000,
                         "tier2": ((end - j["exacting_ns"]) // 1_000_000) if end and j.get("exacting_ns") else None},
            "outcome": line["outcome"],
        })
    g1 = [r["g1_ms"] for r in rows]
    summary = {
        "contested": len(rows),
        "g1_ms": {"p50": pct(g1, 50), "p90": pct(g1, 90)},
        "g1_pass": bool(g1) and pct(g1, 50) <= 80 and pct(g1, 90) <= 130,
        "g2_share": {e: (sum(1 for r in rows if r["ours_at_proposed"][e] is not None and r["ours_at_proposed"][e] <= 1) / len(rows)) if rows else None
                     for e in EPSILONS_MS},
    }
    summary["g2_pass"] = bool(rows) and summary["g2_share"][20] >= 0.5
    return rows, summary


def load_journal(path):
    """ticket id -> {"exacting_ns", "closed_ns"} from the shadow journal."""
    out = {}
    for line in open(path):
        r = json.loads(line)
        if "Advanced" in r and r["Advanced"].get("to") == "Exacting":
            out.setdefault(r["Advanced"]["id"], {})["exacting_ns"] = r["Advanced"]["at"]
        elif "Closed" in r:
            out.setdefault(r["Closed"]["id"], {})["closed_ns"] = r["Closed"]["at"]
    return out


def route_pools_from_journal(path):
    """route hash -> its pools, from each admitted ticket's commitment."""
    out = {}
    for line in open(path):
        r = json.loads(line)
        if "Admitted" in r:
            rc = r["Admitted"]["ticket"]["route_commitment"]
            out[rc["route_hash"]] = [h["pool"]["address"].lower() for h in rc["hops"]]
    return out


def main():
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from arb_census.rpc import Rpc
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default="../../var/apex")
    ap.add_argument("--delta-ms", type=float, required=True)
    ap.add_argument("--out", default="race_rows.jsonl")
    a = ap.parse_args()
    d = Path(a.dir)
    read = lambda pat: [json.loads(l) for f in sorted(glob.glob(str(d / pat))) for l in open(f) if l.strip()]
    ledger = Ledger(read("flashblocks-*.jsonl"))
    race = read("race-*.jsonl")
    journal_path = d / "shadow.journal"
    rows, summary = analyze(race, ledger, load_journal(journal_path), Rpc(), route_pools_from_journal(journal_path),
                            int(a.delta_ms * 1_000_000))
    with open(a.out, "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
```

  Note: `route_pools_from_journal` covers ticketed routes, which is where contested gaps end up. A declined proposal's pools are not in the journal and are skipped. The summary's `contested` count says how many were analysed.

- [ ] **Step 4: Run, expect PASS:** `cd scripts/data && python3 -m unittest test_race_analysis`

---

### Task 5: The host

**Files:** create `ops/host/apex-shadow.service`, `ops/host/provision.sh`, `ops/host/deploy.sh`, `ops/host/shadow.env.example` and `ops/host/README.md`.

- [ ] **Step 1: The unit**, `ops/host/apex-shadow.service`:

```ini
[Unit]
Description=APEX shadow (Task 8.5): the capture path over the null dispatcher; nothing is broadcast
Wants=network-online.target chrony.service
After=network-online.target chrony.service

[Service]
Type=simple
User=apex
Group=apex
WorkingDirectory=/opt/apex
EnvironmentFile=/etc/apex/shadow.env
ExecStart=/opt/apex/bin/apex shadow --config ops/shadow.base.yaml
Restart=on-failure
RestartSec=10
KillSignal=SIGINT
TimeoutStopSec=90
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/opt/apex/var
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

- [ ] **Step 2: Provisioning**, `ops/host/provision.sh`, run once as root through SSM:

```bash
#!/usr/bin/env bash
# One-time host setup for the co-located shadow (Task 8.5 R25). Run as root.
set -euo pipefail
apt-get update -y
apt-get install -y build-essential pkg-config git chrony jq curl ca-certificates
# Amazon Time Sync: every race timestamp is this clock's.
grep -q 169.254.169.123 /etc/chrony/chrony.conf || sed -i '1i server 169.254.169.123 prefer iburst minpoll 4 maxpoll 4' /etc/chrony/chrony.conf
systemctl restart chrony
# 4 GB swap: the release build's link step.
if ! swapon --show | grep -q /swapfile; then
  fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi
id apex >/dev/null 2>&1 || useradd --system --create-home --home-dir /home/apex --shell /usr/sbin/nologin apex
install -d -o apex -g apex /opt/apex /opt/apex/var /opt/apex/var/apex
install -d -o root -g apex -m 0750 /etc/apex
# The toolchain rust-toolchain.toml pins, for the apex user.
sudo -u apex -H bash -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.97.0 --profile minimal'
echo "provisioned: $(chronyc tracking | grep 'Reference ID')"
```

- [ ] **Step 3: Deploy**, `ops/host/deploy.sh`. Run on the **laptop**; it prepares an archive and a data manifest that the host verifies:

```bash
#!/usr/bin/env bash
# Package a pinned revision and data/base for the host (Task 8.5 R25).
# Usage: ops/host/deploy.sh <revision>   -> dist/apex-<rev>.tar.gz, dist/data-base.tar.gz, dist/MANIFEST.sha256
set -euo pipefail
rev="${1:?a committed revision}"
git rev-parse --verify "$rev^{commit}" >/dev/null
mkdir -p dist
git archive --format=tar.gz --prefix=apex/ -o "dist/apex-$rev.tar.gz" "$rev"
tar -czf dist/data-base.tar.gz data/base
( cd dist && sha256sum "apex-$rev.tar.gz" data-base.tar.gz > MANIFEST.sha256 )
echo "$rev" > dist/REVISION
echo "packaged $rev: $(cat dist/MANIFEST.sha256 | wc -l) files; copy dist/ to the host and run the README's install step"
```

  `dist/` is added to `.gitignore`.

- [ ] **Step 4: The secrets template**, `ops/host/shadow.env.example`. It holds names only, never values:

```bash
# /etc/apex/shadow.env -- root:apex 0640. Read by apex-shadow.service only.
# Values come from the operator's .env: BLOCKPI_KEY and TRADER_PRIVATE_KEY.
# Never PRIVATE_KEY (the owner's key).
APEX_SECRET_BLOCKPI_KEY=
APEX_SECRET_TRADER_KEY=
```

- [ ] **Step 5: The runbook**, `ops/host/README.md`, with these sections, each with exact commands:
  1. **AWS setup** (operator):
     - an IAM role `apex-shadow-ssm` with `AmazonSSMManagedInstanceCore`;
     - a security group `apex-shadow` with **no inbound** rules, and outbound 443 and 80;
     - Ubuntu 24.04 LTS, IMDSv2 `required`, an encrypted 50 GB gp3 root volume;
     - c7i.large for probe instances, c7i.xlarge for the host.
  2. **The probe** on each candidate (us-east-1, us-east-2):
     - copy the archive, build with `CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build -j 3 --release --bin apex`;
     - then run `APEX_SECRET_BLOCKPI_KEY=… target/release/apex probe --minutes 60`, the key from a root-only file via `set -a; . /root/probe.env`;
     - record the JSON;
     - choose by the spec's §1 rule;
     - terminate the probe instances.
  3. **Install** on the host:
     - `provision.sh`;
     - verify `MANIFEST.sha256`;
     - unpack to `/opt/apex`; build as `apex`; copy the binary to `/opt/apex/bin/apex`;
     - write `/opt/apex/REVISION`;
     - write `/etc/apex/shadow.env` from the example (`chmod 0640`, `chown root:apex`);
     - `systemctl enable --now apex-shadow`.
  4. **Checks:**
     - `systemctl status`;
     - `scripts/shadow-status.sh`;
     - `kill -9 $(systemctl show -p MainPID --value apex-shadow)`, which must restart within 15 s;
     - a reboot, after which it must return;
     - `systemctl stop`, which must drain;
     - `stat -c '%a %U:%G' /etc/apex/shadow.env` = `640 root:apex`;
     - no listening ports beyond the SSM agent (`ss -ltnp`).
  5. **Revocation**, prepared and dry-run before the host starts:
     - `cast calldata "setExecutor(address,bool)" 0xCB436Ba3acb945b3fc8EE6345857262584356595 false`, wrapped in the BatchRouter's `multicall(address[],bytes[])` to `0x8940B565D050974b2b589B70De43bCb753b1F93B`;
     - dry-run with `cast call --from <owner> 0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2 …`, with `BLOCKPI_KEY` passed via the environment and `--rpc-url base`;
     - the calldata is stored in the runbook. The operator broadcasts it only if `SIGNER NONCE MOVED` appears.
  6. **Updating:** package, verify, build, `systemctl restart apex-shadow` (same revision: the same run; a new revision: a new 14-day clock).

- [ ] **Step 6: Check locally:**
  - `systemd-analyze verify ops/host/apex-shadow.service` (expect only the "user apex does not exist" and missing-path notes);
  - `bash -n ops/host/provision.sh ops/host/deploy.sh`;
  - `shellcheck ops/host/*.sh` if installed;
  - `ops/host/deploy.sh HEAD` produces the archive and a manifest that `sha256sum -c` accepts.

---

### Task 6: Gates, mutations, PLAN.md, commit

- [ ] **Step 1:** `bash $SCRATCH/gates.sh`: all green but the two known-red scripts. The `env_coverage` test (inside `cargo test --workspace`) must pass with `apex probe` reading `APEX_SECRET_BLOCKPI_KEY` through `Env`.
- [ ] **Step 2: Mutations** (restore and `touch` after each; run with `--no-fail-fast`). Mutate each of these and confirm the named test kills it:
  - `observe`'s hole rule (`f.index > state.next_index` → `false`): `flashblock_stream`;
  - `locate`'s upper bound (`<` → `<=`): `flashblock_stream`;
  - `decode_frame`'s consistency check (removed): add a frame with mismatched `base.block_number` to the decode test if it survives;
  - `arrival_offsets_ms`'s `- 2`: `probe`;
  - `pending_log_delays_ms`'s subtraction order: `probe`;
  - `RaceWriter::record`'s counters swapped: `shadow_race`;
  - `nonce_moved` → `boot.is_some_and(|b| b < now)`: `shadow_race`;
  - `daily_path`'s month or day: `shadow_race`;
  - the analyzer's `would_land` `>=` → `>`: `test_race_analysis`.

  A survivor is either tested or deleted.
- [ ] **Step 3: PLAN.md** gets an R25 entry recording what was built, the laptop probe's figures and the mutants.
- [ ] **Step 4: Commit** by named paths:
  - `crates/apex-chain/{Cargo.toml,src/base/mod.rs,src/base/flashblock_stream.rs,src/rpc/ws.rs,tests/flashblock_stream.rs,tests/fixtures/flashblocks_base.json}`;
  - the `crates/apex-chain` tests the compiler touched;
  - `crates/apex-runtime/{src/lib.rs,src/main.rs,src/probe.rs,src/shadow/mod.rs,src/shadow/queue.rs,src/shadow/config.rs,src/shadow/ledger.rs,src/shadow/race.rs,tests/probe.rs,tests/shadow_race.rs,tests/shadow_queue.rs,tests/shadow_parts.rs}`;
  - the `RawLog` test literals;
  - `ops/shadow.base.yaml`, `ops/host/*`, `scripts/shadow-status.sh`, `scripts/data/race_analysis.py`, `scripts/data/test_race_analysis.py`;
  - `.gitignore`, `Cargo.lock`, `PLAN.md`.

  Then `git diff --stat -- <paths>` must be empty. Verify the commit in a clean worktree with its own `CARGO_TARGET_DIR`: `cargo test -j 3 -p apex-chain -p apex-runtime`.

---

### Task 7: On AWS (operator-dependent)

- [ ] **Step 1:** The operator creates the IAM role, the security group and two probe instances (us-east-1, us-east-2) per the README. Run the probe for 60 minutes on each and choose the region by the spec's §1 rule. Record δ. Terminate the probe instances.
- [ ] **Step 2:** The operator launches the host in that region:
  - package the pinned revision (`deploy.sh`), install, and write the secrets file;
  - dry-run the revocation;
  - start `apex-shadow` and run the README's checks.

  Tell the operator the new 14-day clock's start.
- [ ] **Step 3: After one hour** (spec verification 4): both ledgers are non-empty; race lines resolve their trigger's flashblock for whole blocks; G1's figures are reported by `race_analysis.py --delta-ms <δ>`.
- [ ] **Step 4: After 7 days:** run the analyzer, decide by §7 (G1, G2, the own-node trigger), and record the decision in PLAN.md and memory.
