//! `apex probe` (Task 8.5 R25): how far this host is from Base's builder, and
//! what BlockPI adds. Runs no trading component; prints one JSON report.
//!
//! - **arrival offset**: index-0 flashblock arrival minus its slot's opening
//!   (block timestamp − 2 s): the distance to the builder;
//! - **round trips** to Base's RPC origin and its Cloudflare edge, the
//!   sequencer's endpoint and BlockPI;
//! - **BlockPI's delay**: a `pendingLogs` swap's arrival minus the raw
//!   flashblock that carried its transaction.
//!
//! Chooses the shadow's region (spec §1): the lower p50 arrival offset wins,
//! BlockPI's p90 delay breaks a tie within 5 ms, and half the winner's round
//! trip to the sequencer is the analyzer's send delay δ.

use crate::live::feed::{PANCAKE_SWAP, SWAP};
use apex_chain::base::flashblock_stream::{FlashblockIndexer, FlashblockSeen, FlashblockStream, BASE_FLASHBLOCKS_WS};
use apex_chain::rpc::ws::{LogFilter, Notification, Subscription, WsFeed, WsSettings};
use apex_config::Env;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Four deep WETH/USDC pools: Uniswap v3 5 bp and 30 bp, PancakeSwap 1 bp,
/// Slipstream spacing 100. Enough swaps an hour to time BlockPI.
const PROBE_POOLS: [&str; 4] = [
    "0xd0b53D9277642d899DF5C87A3966A349A798F224",
    "0x6c561B446416E1A00E8E93E221854d6eA4171372",
    "0x72ab388e2E2F6FaceF59E3C3FA2C4E29011c2D38",
    "0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59",
];

/// Nearest-rank p10, p50 and p90; `None` for no samples.
pub fn percentiles(v: &mut [i64]) -> Option<(i64, i64, i64)> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let at = |p: usize| v[((v.len() * p).div_ceil(100)).saturating_sub(1).min(v.len() - 1)];
    Some((at(10), at(50), at(90)))
}

/// Each index-0 flashblock's arrival after its slot opened, in ms.
pub fn arrival_offsets_ms(seen: &[FlashblockSeen]) -> Vec<i64> {
    seen.iter()
        .filter(|s| s.index == 0)
        .filter_map(|s| {
            let opened_ns = s.block_timestamp?.checked_sub(2)?.checked_mul(1_000_000_000)?;
            Some((s.arrived_at_ns as i64 - opened_ns as i64) / 1_000_000)
        })
        .collect()
}

/// Each `(block, tx_index, arrived_ns)` log's arrival after the flashblock that
/// carried its transaction, in ms. A log whose flashblock was not seen whole is
/// left out.
pub fn pending_log_delays_ms(ix: &FlashblockIndexer, logs: &[(u64, u32, u64)]) -> Vec<i64> {
    logs.iter()
        .filter_map(|(block, tx, at)| {
            let f = ix.locate(*block, *tx)?;
            Some((*at as i64 - f.arrived_at_ns as i64) / 1_000_000)
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct Quantiles {
    pub n: usize,
    pub p10: i64,
    pub p50: i64,
    pub p90: i64,
}

#[derive(Debug, Serialize)]
pub struct RoundTrip {
    pub name: String,
    pub p50_ms: u64,
    pub min_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub minutes: u64,
    pub flashblocks: u64,
    pub incomplete_blocks: u64,
    pub arrival_offset_ms: Option<Quantiles>,
    pub blockpi_delay_ms: Option<Quantiles>,
    pub round_trips: Vec<RoundTrip>,
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

/// Run the probe for `minutes`. BlockPI's figures need
/// `APEX_SECRET_BLOCKPI_KEY`; without it they are left out.
pub async fn run(minutes: u64, env: &Env) -> ProbeReport {
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let (_stream, mut frames) = FlashblockStream::new(BASE_FLASHBLOCKS_WS).spawn(stop.clone());
    let key = env.get("APEX_SECRET_BLOCKPI_KEY");
    let mut logs_rx = key.as_ref().and_then(|k| {
        let filter = LogFilter {
            addresses: PROBE_POOLS.iter().filter_map(|a| a.parse().ok()).collect(),
            topics0: vec![SWAP, PANCAKE_SWAP],
            topics1: Vec::new(),
        };
        WsFeed::new(&format!("wss://base.blockpi.network/v1/ws/{k}"), vec![Subscription::PendingLogs(filter)], WsSettings::default())
            .ok()
            .map(|f| f.spawn(stop.clone()).1)
    });
    let client = match reqwest::Client::builder().user_agent("apex-probe").timeout(Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(_) => reqwest::Client::new(),
    };
    let blockpi = key.map(|k| format!("https://base.blockpi.network/v1/rpc/{k}"));
    let mut rtt: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    let mut ix = FlashblockIndexer::new();
    let mut seen = Vec::new();
    let mut logs = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(minutes.saturating_mul(60));
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    const BLOCK_NUMBER: &str = r#"{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}"#;
    const CHAIN_ID: &str = r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            Some((f, at)) = frames.recv() => {
                if let Some(s) = ix.observe(&f, at) {
                    seen.push(s);
                }
            }
            Some(n) = async {
                match logs_rx.as_mut() {
                    Some(r) => r.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Notification::Log(l) = n {
                    if let (Some(b), Some(t)) = (l.block_number, l.transaction_index.and_then(|t| u32::try_from(t).ok())) {
                        logs.push((b, t, now_ns()));
                    }
                }
            }
            _ = tick.tick() => {
                for (name, url, body) in [
                    ("base_edge", "https://mainnet.base.org/cdn-cgi/trace", None),
                    ("base_rpc", "https://mainnet.base.org", Some(BLOCK_NUMBER)),
                    ("sequencer", "https://mainnet-sequencer.base.org", Some(CHAIN_ID)),
                ] {
                    if let Some(ms) = round_trip(&client, url, body).await {
                        rtt.entry(name).or_default().push(ms);
                    }
                }
                if let Some(u) = blockpi.as_deref() {
                    if let Some(ms) = round_trip(&client, u, Some(BLOCK_NUMBER)).await {
                        rtt.entry("blockpi").or_default().push(ms);
                    }
                }
            }
        }
    }
    let _ = stop_tx.send(true);
    let round_trips = rtt
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(name, mut v)| {
            v.sort_unstable();
            RoundTrip { name: name.to_string(), p50_ms: v[v.len() / 2], min_ms: v[0] }
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
