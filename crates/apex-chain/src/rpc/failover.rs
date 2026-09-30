//! Multi-endpoint JSON-RPC over HTTP, with failover (§31, §44).
//!
//! See the module above for the policy and why it is the legacy one. This file
//! is the mechanism.

use super::{endpoint_label, is_read, parse_quantity, RpcError, RpcTransport};
use apex_config::Secret;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{error, warn};

/// Timing policy for one transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailoverSettings {
    /// One attempt against one endpoint, connect included. **The legacy client
    /// had no bound at all**, so a hung endpoint hung whoever asked.
    pub request_timeout: Duration,
    /// Whole passes over the endpoint list after the first.
    pub extra_passes: usize,
    /// Sleep before the second pass; doubled per pass after that, capped at 32×.
    pub base_backoff: Duration,
}

impl Default for FailoverSettings {
    /// Three seconds per attempt is generous for a read against BlockPI, whose
    /// measured round trip is ~250 ms warm, and deliberately so: this is the
    /// *correctness* bound — an attempt that has not answered by then is
    /// abandoned rather than waited on. The capture path's latency budget is
    /// §29.5's to set from measurement, and a caller on it passes its own.
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(3),
            extra_passes: 1,
            base_backoff: Duration::from_millis(150),
        }
    }
}

/// Why a transport could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// No non-empty URL was supplied.
    NoEndpoints,
    /// A URL that does not parse. Named by label, which for an unparseable
    /// string is `***`.
    InvalidUrl { endpoint: String },
    /// An endpoint answered a different chain id. A configuration error, and
    /// the boot stops for it rather than dropping the endpoint quietly.
    WrongChain { endpoint: String, expected: u64, answered: u64 },
    /// Not one endpoint answered `eth_chainId` at boot.
    NothingAnswered { endpoints: usize },
    /// The HTTP client itself could not be built.
    Client { detail: String },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEndpoints => f.write_str("no RPC endpoints configured"),
            Self::InvalidUrl { endpoint } => write!(f, "endpoint {endpoint} is not a valid URL"),
            Self::WrongChain { endpoint, expected, answered } => write!(
                f,
                "endpoint {endpoint} is on chain {answered}, not {expected}; fix the configuration"
            ),
            Self::NothingAnswered { endpoints } => {
                write!(f, "none of {endpoints} endpoints answered eth_chainId")
            }
            Self::Client { detail } => write!(f, "could not build the HTTP client: {detail}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Whether an endpoint has proved which chain it is on.
const UNVERIFIED: u8 = 0;
const VERIFIED: u8 = 1;
/// Permanent. An endpoint that answered another chain id is never asked
/// anything again for the life of the process.
const WRONG_CHAIN: u8 = 2;

struct Endpoint {
    url: Secret<String>,
    label: String,
    chain: AtomicU8,
    failures: AtomicU64,
}

/// What one attempt against one endpoint produced.
enum Attempt {
    Answer(Value),
    /// The chain's answer. Not rotated.
    Reverted { message: String, data: Option<Value> },
    /// The endpoint's problem. Rotated.
    Fault(String),
}

enum ChainCheck {
    Verified,
    Wrong(u64),
    Unreachable(String),
}

/// A cloneable-by-`Arc`, multi-endpoint, read-only JSON-RPC client.
pub struct FailoverTransport {
    endpoints: Vec<Endpoint>,
    client: reqwest::Client,
    chain_id: u64,
    /// The preferred endpoint: the last one that answered.
    cursor: AtomicUsize,
    next_id: AtomicU64,
    requests: AtomicU64,
    settings: FailoverSettings,
}

impl std::fmt::Debug for FailoverTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailoverTransport")
            .field("chain_id", &self.chain_id)
            .field("endpoints", &self.labels())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl FailoverTransport {
    /// Build a transport and verify, at boot, which chain every endpoint is on.
    ///
    /// - An endpoint on **another chain** refuses the boot ([`ConnectError::WrongChain`]).
    /// - An endpoint that **does not answer** is kept but unverified, and is
    ///   checked again before its first use. Dropping it would make a transient
    ///   blip at boot permanent for the life of the process — for a 14-day run,
    ///   potentially the paid endpoint.
    /// - If **nothing** answers, the boot is refused: a plane that cannot read
    ///   the chain has nothing to do, and starting anyway would only produce a
    ///   process that looks alive.
    ///
    /// The configured order is the preference order: the first verified
    /// endpoint is where calls start.
    pub async fn connect(
        urls: &[String],
        chain_id: u64,
        settings: FailoverSettings,
    ) -> Result<Self, ConnectError> {
        let mut endpoints = Vec::with_capacity(urls.len());
        for raw in urls {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let label = endpoint_label(trimmed);
            if reqwest::Url::parse(trimmed).is_err() {
                return Err(ConnectError::InvalidUrl { endpoint: label });
            }
            endpoints.push(Endpoint {
                url: Secret::new(trimmed.to_string()),
                label,
                chain: AtomicU8::new(UNVERIFIED),
                failures: AtomicU64::new(0),
            });
        }
        if endpoints.is_empty() {
            return Err(ConnectError::NoEndpoints);
        }

        // HTTP/1.1 with a real pool, carried over from the legacy client with its
        // measurement: under bursty concurrent quoting, HTTP/2 multiplexed every
        // request over one connection and ran ~5x slower, and exhausted h2
        // streams outright.
        let client = reqwest::Client::builder()
            .http1_only()
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .connect_timeout(settings.request_timeout)
            .build()
            .map_err(|e| ConnectError::Client { detail: e.without_url().to_string() })?;

        let transport = Self {
            endpoints,
            client,
            chain_id,
            cursor: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            requests: AtomicU64::new(0),
            settings,
        };

        let mut first_verified = None;
        for (i, ep) in transport.endpoints.iter().enumerate() {
            match transport.check_chain(ep).await {
                ChainCheck::Verified => {
                    first_verified.get_or_insert(i);
                }
                ChainCheck::Wrong(answered) => {
                    return Err(ConnectError::WrongChain {
                        endpoint: ep.label.clone(),
                        expected: chain_id,
                        answered,
                    });
                }
                ChainCheck::Unreachable(detail) => {
                    warn!(
                        target: "rpc",
                        endpoint = %ep.label,
                        %detail,
                        "endpoint did not answer at boot; it will be checked before first use"
                    );
                }
            }
        }
        let Some(first) = first_verified else {
            return Err(ConnectError::NothingAnswered { endpoints: transport.endpoints.len() });
        };
        transport.cursor.store(first, Ordering::Relaxed);
        Ok(transport)
    }

    /// Every endpoint, by label. Safe to log.
    pub fn labels(&self) -> Vec<String> {
        self.endpoints.iter().map(|e| e.label.clone()).collect()
    }

    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Logical calls made — once per [`RpcTransport::call`], however many
    /// endpoints it took. The figure that sizes a provider plan, and the one
    /// that tells "no arbitrage" apart from "rate-limited into blindness".
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Failures since each endpoint last answered, by label.
    pub fn failures(&self) -> Vec<(String, u64)> {
        self.endpoints
            .iter()
            .map(|e| (e.label.clone(), e.failures.load(Ordering::Relaxed)))
            .collect()
    }

    async fn check_chain(&self, ep: &Endpoint) -> ChainCheck {
        match self.attempt(ep, "eth_chainId", json!([])).await {
            Attempt::Answer(v) => match parse_quantity(&v) {
                Some(id) if id == self.chain_id => {
                    ep.chain.store(VERIFIED, Ordering::Release);
                    ChainCheck::Verified
                }
                Some(id) => {
                    ep.chain.store(WRONG_CHAIN, Ordering::Release);
                    ChainCheck::Wrong(id)
                }
                // An answer that is not a chain id proves nothing either way; the
                // endpoint stays unverified and is asked again next time.
                None => ChainCheck::Unreachable(format!("eth_chainId answered {v}")),
            },
            Attempt::Reverted { message, .. } => ChainCheck::Unreachable(message),
            Attempt::Fault(detail) => ChainCheck::Unreachable(detail),
        }
    }

    /// One request to one endpoint, classified.
    async fn attempt(&self, ep: &Endpoint, method: &str, params: Value) -> Attempt {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = match self
            .client
            .post(ep.url.expose().as_str())
            .timeout(self.settings.request_timeout)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            // `without_url`: reqwest's Display embeds the request URL, and the
            // URL is the credential.
            Err(e) => return Attempt::Fault(e.without_url().to_string()),
        };
        let status = response.status();
        if !status.is_success() {
            return Attempt::Fault(format!("http {status}"));
        }
        let answer: Value = match response.json().await {
            Ok(v) => v,
            Err(e) => return Attempt::Fault(format!("not JSON-RPC: {}", e.without_url())),
        };
        classify(&answer, id)
    }
}

/// An answer, a revert, or the endpoint's fault.
///
/// Fail-safe in the same direction as `apex_math::quote_common::is_execution_revert`,
/// which is the single classifier for "the chain said no": anything it does not
/// recognise is treated as the endpoint's problem and rotated. Over-rotating
/// costs RPC budget; mistaking an endpoint fault for the chain's verdict is how
/// a rate limit became a multi-month outage.
fn classify(answer: &Value, id: u64) -> Attempt {
    if answer.get("id") != Some(&json!(id)) {
        return Attempt::Fault(format!(
            "answer carries id {}, the request was {id}",
            answer.get("id").map_or_else(|| "none".to_string(), Value::to_string)
        ));
    }
    if let Some(err) = answer.get("error") {
        let message = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
        if apex_math::quote_common::is_execution_revert(&message) {
            return Attempt::Reverted { message, data: err.get("data").cloned() };
        }
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        return Attempt::Fault(format!("node error {code}: {message}"));
    }
    match answer.get("result") {
        // `null` included: a receipt that does not exist yet is an answer.
        Some(result) => Attempt::Answer(result.clone()),
        None => Attempt::Fault("neither a result nor an error".to_string()),
    }
}

#[async_trait]
impl RpcTransport for FailoverTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        if !is_read(method) {
            return Err(RpcError::NotARead { method: method.to_string() });
        }
        self.requests.fetch_add(1, Ordering::Relaxed);

        let n = self.endpoints.len();
        let start = self.cursor.load(Ordering::Relaxed) % n;
        let total = n.saturating_mul(1 + self.settings.extra_passes);
        let mut last = "no endpoint on the expected chain was available".to_string();

        for attempt in 0..total {
            let idx = (start + attempt) % n;
            let ep = &self.endpoints[idx];

            let outcome = match ep.chain.load(Ordering::Acquire) {
                WRONG_CHAIN => None,
                UNVERIFIED => match self.check_chain(ep).await {
                    ChainCheck::Verified => Some(self.attempt(ep, method, params.clone()).await),
                    ChainCheck::Wrong(answered) => {
                        error!(
                            target: "rpc",
                            endpoint = %ep.label,
                            expected = self.chain_id,
                            answered,
                            "endpoint is on another chain and will not be asked again"
                        );
                        last = format!("{} is on chain {answered}", ep.label);
                        None
                    }
                    ChainCheck::Unreachable(detail) => Some(Attempt::Fault(detail)),
                },
                _ => Some(self.attempt(ep, method, params.clone()).await),
            };

            match outcome {
                Some(Attempt::Answer(v)) => {
                    ep.failures.store(0, Ordering::Relaxed);
                    self.cursor.store(idx, Ordering::Relaxed);
                    return Ok(v);
                }
                Some(Attempt::Reverted { message, data }) => {
                    // A healthy endpoint gave the chain's answer. Keep it.
                    ep.failures.store(0, Ordering::Relaxed);
                    self.cursor.store(idx, Ordering::Relaxed);
                    return Err(RpcError::Reverted { message, data });
                }
                Some(Attempt::Fault(detail)) => {
                    ep.failures.fetch_add(1, Ordering::Relaxed);
                    warn!(target: "rpc", endpoint = %ep.label, method, attempt, %detail, "rotating");
                    last = format!("{}: {detail}", ep.label);
                }
                None => {}
            }

            // Backoff only between whole passes, never after the last one: a
            // single bad endpoint costs no sleep, and a caller that has run out of
            // endpoints is told so immediately.
            let done = attempt + 1;
            if done % n == 0 && done < total {
                let pass = u32::try_from(done / n - 1).unwrap_or(u32::MAX).min(5);
                tokio::time::sleep(self.settings.base_backoff.saturating_mul(1 << pass)).await;
            }
        }

        Err(RpcError::Exhausted { method: method.to_string(), endpoints: n, last })
    }
}
