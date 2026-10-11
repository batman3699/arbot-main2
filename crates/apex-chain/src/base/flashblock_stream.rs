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
/// Blocks kept for [`FlashblockIndexer::locate`]: two minutes of Base.
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
        state.next_index = f.index.saturating_add(1);
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
        self.blocks.retain(|b, _| b.saturating_add(KEEP_BLOCKS) > newest);
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
                            _ = stop.changed() => {
                                if *stop.borrow() {
                                    let _ = ws.close(None).await;
                                    return;
                                }
                                continue;
                            }
                            n = tokio::time::timeout(Duration::from_secs(10), ws.next()) => n,
                        };
                        // Stamped on receipt, before any decoding.
                        let at = now_ns();
                        let decoded = match next {
                            Ok(Some(Ok(Message::Binary(b)))) => decode_frame(&b, true),
                            Ok(Some(Ok(Message::Text(t)))) => decode_frame(t.as_bytes(), false),
                            Ok(Some(Ok(Message::Ping(p)))) => {
                                let _ = ws.send(Message::Pong(p)).await;
                                continue;
                            }
                            Ok(Some(Ok(_))) => continue,
                            // Stalled, closed or failed: reconnect.
                            _ => break,
                        };
                        self.stats.frames.fetch_add(1, Ordering::Relaxed);
                        match decoded {
                            Ok(f) => {
                                let _ = tx.try_send((f, at));
                            }
                            Err(_) => {
                                self.stats.decode_failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    tracing::warn!(target: "flashblocks", "flashblock stream ended; reconnecting");
                }
                _ => tracing::warn!(target: "flashblocks", "flashblock stream did not connect; retrying"),
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = stop.changed() => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }
}
