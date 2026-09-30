//! Read-only subscriptions over a provider websocket (Task 8.5 R1, §31).
//!
//! # What it subscribes to, and why those three
//!
//! Probed against BlockPI's keyed Base endpoint 2026-09-30:
//!
//! - `newHeads` — every sealed block (2 s). The block ordinal, the base fee, and
//!   the liveness signal: a feed that has seen no head for `stall_after` is
//!   treated as dead even if the socket is open.
//! - `logs` — confirmed logs. What a sealed block actually did.
//! - `pendingLogs` — **preconfirmed** logs, at flashblock latency. Over 20 s
//!   against Base's busiest WETH/USDC pool these arrived before, and outnumbered,
//!   the confirmed ones. The event census found the edge in the block or two
//!   after a large swap, so this is the subscription the capture path lives on.
//!   publicnode no longer delivers it; BlockPI does.
//!
//! [`Subscription`] has exactly these three variants, and the only request this
//! module can make is `eth_subscribe` of one of them. There is no send path to
//! guard, because there is nothing here that could express one.
//!
//! # A gap is reported, never smoothed over
//!
//! A socket that drops has missed whatever happened while it was down. The feed
//! reconnects and resubscribes on its own, and emits
//! [`Notification::Reconnected`] **before anything from the new session**, so
//! whoever built state from the old session knows it may be stale — §5.6's rule
//! that a gap blocks live tickets until state is rebuilt. A consumer too slow to
//! keep up loses notifications rather than stalling the socket (a feed that
//! falls behind reports stale state as fresh), and the loss is announced the same
//! way, as [`Notification::Gap`].
//!
//! # INV-46: the key is the URL path
//!
//! Held as a `Secret` and named by [`endpoint_label`]. Failures are recorded by
//! kind, and a test holds the recorded text to not containing the key.

use super::endpoint_label;
use alloy_primitives::{Address, B256};
use apex_config::Secret;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{info, warn};

/// Which contracts, and which events, a log subscription covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogFilter {
    pub addresses: Vec<Address>,
    /// Any of these as `topic0`.
    pub topics0: Vec<B256>,
}

/// The three reads a feed can subscribe to. Nothing else is expressible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subscription {
    NewHeads,
    /// Confirmed: in a sealed block.
    Logs(LogFilter),
    /// Preconfirmed: in a flashblock the sequencer has published.
    PendingLogs(LogFilter),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    NewHeads,
    Logs,
    PendingLogs,
}

impl Subscription {
    pub const fn kind(&self) -> Kind {
        match self {
            Self::NewHeads => Kind::NewHeads,
            Self::Logs(_) => Kind::Logs,
            Self::PendingLogs(_) => Kind::PendingLogs,
        }
    }
}

/// The `eth_subscribe` request for one subscription.
pub fn subscribe_request(id: u64, s: &Subscription) -> Value {
    let params = match s {
        Subscription::NewHeads => json!(["newHeads"]),
        Subscription::Logs(f) => json!(["logs", filter_json(f)]),
        Subscription::PendingLogs(f) => json!(["pendingLogs", filter_json(f)]),
    };
    json!({ "jsonrpc": "2.0", "id": id, "method": "eth_subscribe", "params": params })
}

fn filter_json(f: &LogFilter) -> Value {
    json!({ "address": f.addresses, "topics": [f.topics0] })
}

/// A sealed block's header, as much of it as the plane reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub number: u64,
    pub hash: B256,
    pub timestamp: u64,
    pub base_fee_per_gas: Option<u128>,
    pub gas_used: u64,
    pub gas_limit: u64,
}

/// One log, and which of the two feeds it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawLog {
    /// From `pendingLogs`: preconfirmed, not yet sealed.
    pub pending: bool,
    /// A reorg retracted this log. Applying it would apply something that did
    /// not happen.
    pub removed: bool,
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
    pub block_number: Option<u64>,
    pub transaction_hash: Option<B256>,
    pub log_index: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    Head(Head),
    Log(RawLog),
    /// The socket was re-established after `outage`. Whatever happened in that
    /// window was not seen. Emitted before anything from the new session.
    Reconnected { outage: Duration, attempt: u32 },
    /// The consumer fell behind and `dropped` notifications were discarded.
    Gap { dropped: u64 },
}

/// Parse one message into a notification, if it is one this feed subscribed to.
///
/// `None` for anything else — an unknown subscription, a malformed payload, a
/// response rather than a notification. Never a default: a head with no number
/// is not block zero, and a log with an unparseable address is not the zero
/// address's.
pub fn parse_notification(msg: &Value, ids: &BTreeMap<String, Kind>) -> Option<Notification> {
    if msg.get("method")?.as_str()? != "eth_subscription" {
        return None;
    }
    let params = msg.get("params")?;
    let kind = *ids.get(params.get("subscription")?.as_str()?)?;
    let r = params.get("result")?;
    match kind {
        Kind::NewHeads => Some(Notification::Head(Head {
            number: quantity(r.get("number")?)?,
            hash: r.get("hash")?.as_str()?.parse().ok()?,
            timestamp: quantity(r.get("timestamp")?)?,
            base_fee_per_gas: r.get("baseFeePerGas").and_then(quantity_u128),
            gas_used: quantity(r.get("gasUsed")?)?,
            gas_limit: quantity(r.get("gasLimit")?)?,
        })),
        Kind::Logs | Kind::PendingLogs => {
            let topics = r
                .get("topics")?
                .as_array()?
                .iter()
                .map(|t| t.as_str().and_then(|s| s.parse().ok()))
                .collect::<Option<Vec<B256>>>()?;
            Some(Notification::Log(RawLog {
                pending: kind == Kind::PendingLogs,
                removed: r.get("removed").and_then(Value::as_bool).unwrap_or(false),
                address: r.get("address")?.as_str()?.parse().ok()?,
                topics,
                data: alloy_primitives::hex::decode(r.get("data")?.as_str()?).ok()?,
                block_number: r.get("blockNumber").and_then(quantity),
                transaction_hash: r
                    .get("transactionHash")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok()),
                log_index: r.get("logIndex").and_then(quantity),
            }))
        }
    }
}

fn quantity(v: &Value) -> Option<u64> {
    super::parse_quantity(v)
}

fn quantity_u128(v: &Value) -> Option<u128> {
    let s = v.as_str()?.strip_prefix("0x")?;
    u128::from_str_radix(s, 16).ok()
}

/// Timing and buffering policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WsSettings {
    pub connect_timeout: Duration,
    /// Silence longer than this is a dead feed, open socket or not. Base seals a
    /// block every 2 s and `newHeads` is always subscribed, so 30 s is fifteen
    /// missed heads.
    pub stall_after: Duration,
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    /// Notifications buffered for the consumer before they are dropped.
    pub channel_capacity: usize,
}

impl Default for WsSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            stall_after: Duration::from_secs(30),
            min_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(10),
            channel_capacity: 4_096,
        }
    }
}

/// What the feed has been through, readable while it runs.
#[derive(Debug, Default)]
pub struct WsStats {
    sessions: AtomicU64,
    refusals: AtomicU64,
    dropped: AtomicU64,
    last_failure: Mutex<Option<String>>,
}

impl WsStats {
    pub fn sessions(&self) -> u64 {
        self.sessions.load(Ordering::Relaxed)
    }
    /// Subscriptions the provider refused.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }
    /// Notifications discarded because the consumer was behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    /// The most recent failure, described by kind. Never carries the URL.
    pub fn last_failure(&self) -> Option<String> {
        self.last_failure.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn fail(&self, detail: &str) {
        *self.last_failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(detail.to_string());
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FeedError {
    /// Not a `ws://` or `wss://` URL. Named by label.
    NotAWebsocket { endpoint: String },
    NoSubscriptions,
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAWebsocket { endpoint } => write!(f, "{endpoint} is not a websocket URL"),
            Self::NoSubscriptions => f.write_str("a feed with no subscriptions receives nothing"),
        }
    }
}

impl std::error::Error for FeedError {}

/// One provider websocket, supervised.
pub struct WsFeed {
    url: Secret<String>,
    label: String,
    subs: Vec<Subscription>,
    settings: WsSettings,
    stats: Arc<WsStats>,
}

impl std::fmt::Debug for WsFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsFeed")
            .field("endpoint", &self.label)
            .field("subscriptions", &self.subs.len())
            .finish_non_exhaustive()
    }
}

enum SessionEnd {
    Stopped,
    /// The session ended, with a description of why. Retried.
    Ended(String),
}

impl WsFeed {
    pub fn new(url: &str, subs: Vec<Subscription>, settings: WsSettings) -> Result<Self, FeedError> {
        let trimmed = url.trim();
        let label = endpoint_label(trimmed);
        let is_ws = reqwest::Url::parse(trimmed)
            .map(|u| matches!(u.scheme(), "ws" | "wss"))
            .unwrap_or(false);
        if !is_ws {
            return Err(FeedError::NotAWebsocket { endpoint: label });
        }
        if subs.is_empty() {
            return Err(FeedError::NoSubscriptions);
        }
        Ok(Self {
            url: Secret::new(trimmed.to_string()),
            label,
            subs,
            settings,
            stats: Arc::new(WsStats::default()),
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn stats(&self) -> Arc<WsStats> {
        Arc::clone(&self.stats)
    }

    /// Run until `stop` is set, reconnecting as needed. The receiver gets every
    /// notification, and a `Reconnected` or `Gap` wherever continuity was lost.
    pub fn spawn(
        self,
        stop: watch::Receiver<bool>,
    ) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<Notification>) {
        let (tx, rx) = mpsc::channel(self.settings.channel_capacity.max(1));
        let handle = tokio::spawn(async move { self.run(tx, stop).await });
        (handle, rx)
    }

    async fn run(self, tx: mpsc::Sender<Notification>, mut stop: watch::Receiver<bool>) {
        let mut attempt: u32 = 0;
        let mut ended_at: Option<Instant> = None;
        let mut backoff = self.settings.min_backoff;
        let mut gap = 0u64;
        loop {
            if *stop.borrow() {
                return;
            }
            match self.session(&tx, &mut stop, attempt, ended_at, &mut gap).await {
                SessionEnd::Stopped => return,
                SessionEnd::Ended(detail) => {
                    self.stats.fail(&detail);
                    warn!(target: "rpc", endpoint = %self.label, %detail, attempt, "websocket session ended; reconnecting");
                    ended_at = Some(Instant::now());
                    attempt = attempt.saturating_add(1);
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = stop.changed() => {}
                    }
                    backoff = (backoff * 2).min(self.settings.max_backoff);
                }
            }
        }
    }

    async fn session(
        &self,
        tx: &mpsc::Sender<Notification>,
        stop: &mut watch::Receiver<bool>,
        attempt: u32,
        ended_at: Option<Instant>,
        gap: &mut u64,
    ) -> SessionEnd {
        let connect = tokio::time::timeout(
            self.settings.connect_timeout,
            tokio_tungstenite::connect_async(self.url.expose().as_str()),
        );
        let mut ws = match connect.await {
            Err(_) => return SessionEnd::Ended("connect timed out".to_string()),
            Ok(Err(e)) => return SessionEnd::Ended(describe(&e)),
            Ok(Ok((ws, _))) => ws,
        };
        self.stats.sessions.fetch_add(1, Ordering::Relaxed);

        // The gap comes first: nothing from this session may be read as
        // continuous with the last one.
        if let Some(t) = ended_at {
            self.forward(tx, Notification::Reconnected { outage: t.elapsed(), attempt }, gap);
            info!(target: "rpc", endpoint = %self.label, attempt, "websocket reconnected");
        }

        let mut awaiting: BTreeMap<u64, Kind> = BTreeMap::new();
        for (i, s) in self.subs.iter().enumerate() {
            let id = i as u64 + 1;
            awaiting.insert(id, s.kind());
            if let Err(e) = ws.send(Message::Text(subscribe_request(id, s).to_string())).await {
                return SessionEnd::Ended(describe(&e));
            }
        }

        let mut ids: BTreeMap<String, Kind> = BTreeMap::new();
        loop {
            let next = tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() {
                        let _ = ws.close(None).await;
                        return SessionEnd::Stopped;
                    }
                    continue;
                }
                next = tokio::time::timeout(self.settings.stall_after, ws.next()) => next,
            };
            let msg = match next {
                Err(_) => return SessionEnd::Ended("stalled: nothing received".to_string()),
                Ok(None) => return SessionEnd::Ended("closed by the endpoint".to_string()),
                Ok(Some(Err(e))) => return SessionEnd::Ended(describe(&e)),
                Ok(Some(Ok(m))) => m,
            };
            match msg {
                Message::Text(text) => {
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    if let Some(id) = v.get("id").and_then(Value::as_u64) {
                        let Some(kind) = awaiting.remove(&id) else { continue };
                        match v.get("result").and_then(Value::as_str) {
                            Some(sid) => {
                                ids.insert(sid.to_string(), kind);
                            }
                            // A feed missing one of its subscriptions is not a
                            // degraded feed, it is a different one — without
                            // `pendingLogs` it is a block slower. Not run.
                            None => {
                                self.stats.refusals.fetch_add(1, Ordering::Relaxed);
                                let _ = ws.close(None).await;
                                return SessionEnd::Ended(format!("{kind:?} subscription refused"));
                            }
                        }
                        continue;
                    }
                    if let Some(n) = parse_notification(&v, &ids) {
                        self.forward(tx, n, gap);
                    }
                }
                Message::Ping(p) => {
                    let _ = ws.send(Message::Pong(p)).await;
                }
                Message::Close(_) => return SessionEnd::Ended("closed by the endpoint".to_string()),
                _ => {}
            }
        }
    }

    /// Hand one notification on without ever blocking the socket. A full queue
    /// drops it and counts it, and the next notification that fits is preceded
    /// by a `Gap` saying how many were lost.
    fn forward(&self, tx: &mpsc::Sender<Notification>, n: Notification, gap: &mut u64) {
        if *gap > 0 {
            match tx.try_send(Notification::Gap { dropped: *gap }) {
                Ok(()) => *gap = 0,
                Err(_) => {
                    *gap = gap.saturating_add(1);
                    self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
        if tx.try_send(n).is_err() {
            *gap = gap.saturating_add(1);
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A failure's kind: short, stable, and countable in `WsStats`.
///
/// **Not the credential defence, and it does not pretend to be.** Mutation
/// showed tungstenite 0.20's own text carries no URL — printing an I/O error raw
/// leaked nothing — so the tripwire for INV-46 is
/// `the_credential_reaches_no_label_or_failure`, which fails whichever function
/// produces the text if a future version starts printing the URL.
fn describe(e: &WsError) -> String {
    match e {
        WsError::ConnectionClosed | WsError::AlreadyClosed => "closed".to_string(),
        WsError::Io(io) => format!("io: {:?}", io.kind()),
        WsError::Tls(_) => "tls failure".to_string(),
        WsError::Capacity(_) => "message too large".to_string(),
        WsError::Protocol(_) => "websocket protocol error".to_string(),
        WsError::WriteBufferFull(_) => "write buffer full".to_string(),
        WsError::Utf8 => "invalid utf-8".to_string(),
        WsError::AttackAttempt => "handshake attack attempt".to_string(),
        WsError::Url(_) => "invalid url".to_string(),
        WsError::Http(resp) => format!("http {}", resp.status()),
        WsError::HttpFormat(_) => "malformed http".to_string(),
    }
}
