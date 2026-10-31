//! `apex denim-check` (Task 8.5 R41): the real feed and the real Denim guard
//! against any chain for a while, and one JSON verdict. Gate S runs it on
//! public Sepolia; gate M on mainnet against BlockPI.
//!
//! It opens the capture feed's own subscriptions (`subscriptions`, over the
//! inventory's pools), counts heads and logs, checks each head's
//! `timestampMs` against the 200 ms lattice as the switch does, runs the
//! guard ([`HeadWindow::holds`]) and the read clock ([`ReadClock`]) over every
//! head as the armed lane would, and counts the feed's bytes by
//! subscription. It passes when every subscription was accepted on one
//! session, the guard held for 99% of heads after a 20-head warm-up, head lag
//! p99 is within `max_head_lag_ms`, and read rounds came no faster than the
//! clock allows.
//!
//! It sends nothing and reads no key the configuration does not name.

use crate::denim::{detect, Detect, HeadWindow, LatticeBlock, ReadClock, DENIM_BLOCK_MS, HEAD_WINDOW};
use crate::shadow::config::ShadowConfig;
use apex_chain::rpc::ws::{Kind, LogFilter, Notification, WsFeed, WsSettings};
use serde::Serialize;
use std::time::Duration;

/// The feed's text bytes by subscription.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Bytes {
    pub heads: u64,
    pub logs: u64,
    pub pending_logs: u64,
    pub other: u64,
}

/// Head lag, arrival less `timestampMs`, in ms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Lag {
    pub p50: Option<u64>,
    pub p99: Option<u64>,
    pub max: Option<u64>,
}

/// The check's one line.
#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    /// What the endpoint answered to `eth_chainId`: the file's `chain_id` is
    /// Base's whatever the endpoint is.
    pub chain_id: Option<u64>,
    pub for_s: u64,
    pub heads: u64,
    pub logs: u64,
    pub skips: u64,
    pub repeats: u64,
    pub without_ms: u64,
    pub off_lattice: u64,
    pub guard_checked: u64,
    pub guard_held: u64,
    pub lag_ms: Lag,
    pub read_rounds: u64,
    pub read_min_gap_ms: Option<u64>,
    pub bytes: Bytes,
    pub refusals: u64,
    pub sessions: u64,
    pub pass: bool,
    pub reasons: Vec<String>,
}

/// What the check has seen, head by head.
#[derive(Clone, Debug)]
pub struct Tally {
    block_time_ms: u64,
    max_head_lag_ms: u64,
    head_reads_every_ms: u64,
    window: HeadWindow,
    clock: ReadClock,
    last: Option<(u64, Option<u64>)>,
    heads: u64,
    logs: u64,
    repeats: u64,
    off_lattice: u64,
    lags: Vec<u64>,
    guard_checked: u64,
    guard_held: u64,
    read_rounds: u64,
    last_round: Option<u64>,
    read_min_gap_ms: Option<u64>,
}

impl Tally {
    pub fn new(block_time_ms: u64, max_head_lag_ms: u64, head_reads_every_ms: u64) -> Self {
        Self {
            block_time_ms,
            max_head_lag_ms,
            head_reads_every_ms,
            window: HeadWindow::new(),
            clock: ReadClock::new(head_reads_every_ms),
            last: None,
            heads: 0,
            logs: 0,
            repeats: 0,
            off_lattice: 0,
            lags: Vec::new(),
            guard_checked: 0,
            guard_held: 0,
            read_rounds: 0,
            last_round: None,
            read_min_gap_ms: None,
        }
    }

    /// One head, seen at `arrival_ms`. A head not newer than the last is a
    /// repeat and counts for nothing else.
    pub fn head(&mut self, number: u64, timestamp: u64, timestamp_ms: Option<u64>, arrival_ms: u64) {
        if self.last.is_some_and(|(last, _)| number <= last) {
            self.repeats += 1;
            return;
        }
        self.heads += 1;
        if let Some(ms) = timestamp_ms {
            let stepped_off = self
                .last
                .and_then(|(last, prev)| prev.filter(|_| number == last + 1))
                .is_some_and(|prev| ms.saturating_sub(prev) != DENIM_BLOCK_MS);
            if ms % DENIM_BLOCK_MS != 0 || ms / 1_000 != timestamp || stepped_off {
                self.off_lattice += 1;
            }
            self.lags.push(arrival_ms.saturating_sub(ms));
        }
        self.last = Some((number, timestamp_ms));
        self.window.observe(number, timestamp_ms);
        if self.heads > HEAD_WINDOW as u64 {
            self.guard_checked += 1;
            self.guard_held += u64::from(self.window.holds(self.block_time_ms, self.max_head_lag_ms, arrival_ms));
        }
        if self.clock.due(arrival_ms) {
            self.read_rounds += 1;
            if let Some(gap) = self.last_round.map(|r| arrival_ms.saturating_sub(r)) {
                self.read_min_gap_ms = Some(self.read_min_gap_ms.map_or(gap, |g| g.min(gap)));
            }
            self.last_round = Some(arrival_ms);
        }
    }

    pub fn log(&mut self) {
        self.logs += 1;
    }

    /// The verdict, with the feed's own counts: subscriptions refused,
    /// sessions opened and bytes by subscription.
    pub fn verdict(&self, chain_id: Option<u64>, for_s: u64, refusals: u64, sessions: u64, bytes: Bytes) -> Verdict {
        let mut sorted = self.lags.clone();
        sorted.sort_unstable();
        let at = |q: f64| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let i = (q * (sorted.len().saturating_sub(1)) as f64) as usize;
            sorted.get(i).copied()
        };
        let lag_ms = Lag { p50: at(0.5), p99: at(0.99), max: sorted.last().copied() };
        let mut reasons = Vec::new();
        if refusals > 0 {
            reasons.push(format!("{refusals} subscriptions refused"));
        }
        if sessions != 1 {
            reasons.push(format!("{sessions} sessions: the feed did not hold one"));
        }
        if self.guard_checked == 0 || self.guard_held.saturating_mul(100) < self.guard_checked.saturating_mul(99) {
            reasons.push(format!("the guard held for {} of {} heads after the warm-up", self.guard_held, self.guard_checked));
        }
        match lag_ms.p99 {
            Some(p99) if p99 > self.max_head_lag_ms => {
                reasons.push(format!("head lag p99 {p99} ms is over max_head_lag_ms {}", self.max_head_lag_ms));
            }
            None => reasons.push("no head carried timestampMs: no lag to measure".into()),
            Some(_) => {}
        }
        if let Some(gap) = self.read_min_gap_ms.filter(|g| *g < self.head_reads_every_ms) {
            reasons.push(format!("read rounds {gap} ms apart, under head_reads_every_ms {}", self.head_reads_every_ms));
        }
        Verdict {
            chain_id,
            for_s,
            heads: self.heads,
            logs: self.logs,
            skips: self.window.skips(),
            repeats: self.repeats,
            without_ms: self.window.without_ms(),
            off_lattice: self.off_lattice,
            guard_checked: self.guard_checked,
            guard_held: self.guard_held,
            lag_ms,
            read_rounds: self.read_rounds,
            read_min_gap_ms: self.read_min_gap_ms,
            bytes,
            refusals,
            sessions,
            pass: reasons.is_empty(),
            reasons,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What the configuration's first HTTP endpoint answers to `eth_chainId`.
/// A failure is `None`, never an error naming the URL, which can carry a key.
async fn chain_id(config: &ShadowConfig) -> Option<u64> {
    let url = config.rpc_http.first()?;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok()?;
    let text = client.post(url.expose().as_str()).header("content-type", "application/json").body(body).send().await.ok()?.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    apex_chain::rpc::parse_quantity(v.get("result")?)
}

/// The check: the configuration's feed for `for_s` seconds.
pub async fn run(config: &ShadowConfig, for_s: u64) -> Result<Verdict, String> {
    let hooks = config.universe.hooks()?;
    let specs = crate::shadow::universe(
        &config.inventory,
        &crate::live::inventory::Venue::ALL,
        config.universe.filter(),
        &std::collections::BTreeSet::new(),
        &hooks,
    )
    .map_err(|e| format!("the inventory: {e}"))?;
    let filter = LogFilter {
        addresses: specs.iter().map(|s| s.pool).collect(),
        topics0: vec![crate::live::feed::SWAP, crate::live::feed::PANCAKE_SWAP, crate::live::feed::MINT, crate::live::feed::BURN, crate::live::feed::SYNC],
        topics1: Vec::new(),
    };
    let v4_ids: Vec<_> = specs.iter().filter_map(|s| Some(s.v4?.pool_id)).collect();
    let subs = crate::shadow::subscriptions(&config.feed, std::iter::once(filter).chain(crate::live::feed::v4_filter(&v4_ids)).collect());
    let chain_id = chain_id(config).await;
    let feed = WsFeed::new(config.rpc_ws.expose(), subs, WsSettings::default()).map_err(|e| e.to_string())?;
    let stats = feed.stats();
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let (task, mut rx) = feed.spawn(stop);
    let mut tally = Tally::new(config.armed.block_time_ms, config.armed.max_head_lag_ms, config.policy.head_reads_every_ms);
    let until = tokio::time::Instant::now() + Duration::from_secs(for_s);
    loop {
        match tokio::time::timeout_at(until, rx.recv()).await {
            Ok(Some(Notification::Head(h))) => tally.head(h.number, h.timestamp, h.timestamp_ms, now_ms()),
            Ok(Some(Notification::Log(_))) => tally.log(),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    let bytes = Bytes {
        heads: stats.bytes(Kind::NewHeads),
        logs: stats.bytes(Kind::Logs),
        pending_logs: stats.bytes(Kind::PendingLogs),
        other: stats.bytes_other(),
    };
    Ok(tally.verdict(chain_id, for_s, stats.refusals(), stats.sessions(), bytes))
}

// ------------------------------------------------------------- the switch's round

/// One block as `apex denim-detect` prints it.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct DetectBlock {
    pub number: u64,
    pub timestamp: u64,
    #[serde(rename = "timestampMs")]
    pub timestamp_ms: Option<u64>,
}

/// `apex denim-detect`'s one line: which source answered (its host only), the
/// chain it named, its ten newest blocks, and the verdict.
#[derive(Clone, Debug, Serialize)]
pub struct DetectLine {
    pub source: Option<String>,
    pub chain_id: Option<u64>,
    pub blocks: Vec<DetectBlock>,
    /// `denim`, `not_yet`, `anomaly`, or `no_source`.
    pub verdict: &'static str,
    pub reason: Option<String>,
}

impl DetectLine {
    /// The exit code the switch reads: 0 Denim, 1 not yet, 2 an anomaly, 3
    /// no source answered.
    pub const fn exit_code(&self) -> u8 {
        match self.verdict.as_bytes() {
            b"denim" => 0,
            b"not_yet" => 1,
            b"anomaly" => 2,
            _ => 3,
        }
    }
}

async fn post(client: &reqwest::Client, url: &str, body: String) -> Option<serde_json::Value> {
    let text = client.post(url).header("content-type", "application/json").body(body).send().await.ok()?.text().await.ok()?;
    serde_json::from_str(&text).ok()
}

/// One answer's `result`, by id, from a batch.
fn result_of(batch: &serde_json::Value, id: u64) -> Option<&serde_json::Value> {
    batch.as_array()?.iter().find(|r| r.get("id").and_then(serde_json::Value::as_u64) == Some(id))?.get("result")
}

/// The chain ID and the newest ten blocks from one endpoint, or `None` if it
/// did not answer all of it.
async fn read_ten(client: &reqwest::Client, url: &str) -> Option<(u64, Vec<LatticeBlock>)> {
    let q = apex_chain::rpc::parse_quantity;
    let first = post(
        client,
        url,
        r#"[{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]},{"jsonrpc":"2.0","id":2,"method":"eth_blockNumber","params":[]}]"#.into(),
    )
    .await?;
    let (chain, head) = (q(result_of(&first, 1)?)?, q(result_of(&first, 2)?)?);
    let ids = head.checked_sub(9)?..=head;
    let calls: Vec<serde_json::Value> = ids
        .clone()
        .map(|n| serde_json::json!({"jsonrpc": "2.0", "id": n, "method": "eth_getBlockByNumber", "params": [format!("{n:#x}"), false]}))
        .collect();
    let batch = post(client, url, serde_json::Value::Array(calls).to_string()).await?;
    let mut blocks = Vec::with_capacity(10);
    for n in ids {
        let b = result_of(&batch, n)?;
        blocks.push(LatticeBlock {
            number: q(b.get("number")?)?,
            timestamp: q(b.get("timestamp")?)?,
            timestamp_ms: b.get("timestampMs").and_then(q),
        });
    }
    Some((chain, blocks))
}

/// One round of the switch: the first of `urls` that answers decides.
pub async fn detect_round(urls: &[String], want_chain: u64, fork_at: u64) -> DetectLine {
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(5)).build() else {
        return DetectLine { source: None, chain_id: None, blocks: Vec::new(), verdict: "no_source", reason: Some("no HTTP client".into()) };
    };
    for url in urls {
        let Some((chain, blocks)) = read_ten(&client, url).await else { continue };
        let source = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string));
        let (verdict, reason) = match detect(chain, want_chain, &blocks, fork_at) {
            Detect::Denim => ("denim", None),
            Detect::NotYet(why) => ("not_yet", Some(why)),
            Detect::Anomaly(why) => ("anomaly", Some(why)),
        };
        let blocks = blocks.iter().map(|b| DetectBlock { number: b.number, timestamp: b.timestamp, timestamp_ms: b.timestamp_ms }).collect();
        return DetectLine { source, chain_id: Some(chain), blocks, verdict, reason };
    }
    DetectLine { source: None, chain_id: None, blocks: Vec::new(), verdict: "no_source", reason: Some(format!("none of {} sources answered", urls.len())) }
}
