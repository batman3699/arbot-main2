//! Multi-endpoint, self-healing JSON-RPC HTTP transport.
//!
//! The previous execution path pinned a single `Provider<Http>` to one URL at
//! boot and never rotated or retried. A single endpoint hiccup (rate limit,
//! 5xx, connection reset) therefore turned into a permanent failure loop — the
//! root cause of ~40 days of `rpc_error` with zero trades in the accounting log.
//!
//! `FailoverClient` implements `JsonRpcClient`, so it drops into the existing
//! generic `Provider<C>` / `Runner<M, C>` machinery transparently. On every
//! request it:
//!   * starts from the last-known-good endpoint (affinity, avoids split views),
//!   * rotates to the next endpoint on any transport/HTTP error,
//!   * backs off (bounded, exponential) only after a full cycle of all endpoints,
//!   * returns an error only when every endpoint has failed.

use std::fmt::Debug;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use ethers::providers::{Http, JsonRpcClient, ProviderError};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tracing::warn;

#[derive(Debug)]
struct Endpoint {
    url: String,
    client: Http,
    failures: AtomicUsize,
}

#[derive(Debug)]
struct Inner {
    endpoints: Vec<Endpoint>,
    /// Index of the currently preferred (last successful) endpoint.
    cursor: AtomicUsize,
    /// Additional full cycles attempted (with backoff) after the first pass.
    extra_passes: usize,
    base_backoff: Duration,
}

/// A cloneable, multi-endpoint HTTP JSON-RPC client with per-request failover.
#[derive(Clone, Debug)]
pub struct FailoverClient {
    inner: Arc<Inner>,
}

impl FailoverClient {
    /// Build a failover client over one or more http(s) endpoints.
    pub fn new(urls: &[String]) -> Result<Self> {
        Self::with_settings(urls, 2, Duration::from_millis(150))
    }

    pub fn with_settings(
        urls: &[String],
        extra_passes: usize,
        base_backoff: Duration,
    ) -> Result<Self> {
        let mut endpoints = Vec::with_capacity(urls.len());
        for url in urls {
            let trimmed = url.trim();
            if trimmed.is_empty() {
                continue;
            }
            let url = reqwest::Url::parse(trimmed)
                .map_err(|err| anyhow!("invalid rpc url '{trimmed}': {err}"))?;
            // Force HTTP/1.1 with a real connection pool. Under bursty concurrent
            // quoting, HTTP/2 multiplexes every request over a single connection
            // (head-of-line blocking + provider per-connection stream caps), which
            // measured ~5x slower than an HTTP/1.1 pool of parallel connections and
            // previously triggered h2 "locally-reset streams" exhaustion. A pooled
            // HTTP/1.1 client opens parallel sockets so 24-48 concurrent quotes run
            // truly in parallel.
            let http_client = reqwest::Client::builder()
                .http1_only()
                .pool_max_idle_per_host(64)
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_keepalive(Some(Duration::from_secs(60)))
                .build()
                .map_err(|err| anyhow!("failed to build http client for '{trimmed}': {err}"))?;
            let client = Http::new_with_client(url, http_client);
            endpoints.push(Endpoint {
                url: trimmed.to_string(),
                client,
                failures: AtomicUsize::new(0),
            });
        }
        if endpoints.is_empty() {
            bail!("FailoverClient requires at least one non-empty http endpoint");
        }
        Ok(Self {
            inner: Arc::new(Inner {
                endpoints,
                cursor: AtomicUsize::new(0),
                extra_passes,
                base_backoff,
            }),
        })
    }

    /// Comma-joined endpoint list, for logging/metrics labels.
    pub fn endpoint_label(&self) -> String {
        self.inner
            .endpoints
            .iter()
            .map(|ep| ep.url.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn endpoint_count(&self) -> usize {
        self.inner.endpoints.len()
    }
}

#[async_trait]
impl JsonRpcClient for FailoverClient {
    type Error = ProviderError;

    async fn request<T, R>(&self, method: &str, params: T) -> std::result::Result<R, Self::Error>
    where
        T: Debug + Serialize + Send + Sync,
        R: DeserializeOwned + Send,
    {
        // Serialize params once so we can forward the same payload to multiple
        // endpoints (the trait's `T` is not `Clone`, but `serde_json::Value` is).
        let value = serde_json::to_value(&params)
            .map_err(|err| ProviderError::CustomError(format!("serialize rpc params: {err}")))?;

        let n = self.inner.endpoints.len();
        let start = self.inner.cursor.load(Ordering::Relaxed) % n;
        let total = n + self.inner.extra_passes * n;
        let mut last_err: Option<String> = None;

        for attempt in 0..total.max(1) {
            let idx = (start + attempt) % n;
            let ep = &self.inner.endpoints[idx];
            match ep.client.request::<Value, R>(method, value.clone()).await {
                Ok(res) => {
                    ep.failures.store(0, Ordering::Relaxed);
                    // Stick to the endpoint that just worked.
                    self.inner.cursor.store(idx, Ordering::Relaxed);
                    return Ok(res);
                }
                Err(err) => {
                    ep.failures.fetch_add(1, Ordering::Relaxed);
                    last_err = Some(err.to_string());
                    warn!(
                        target: "rpc",
                        endpoint = %ep.url,
                        method,
                        attempt,
                        error = %err,
                        "rpc request failed; rotating endpoint"
                    );
                    // Only sleep once we've tried every endpoint at least once,
                    // so a single bad endpoint costs ~0 added latency.
                    if attempt + 1 >= n {
                        let pass = ((attempt + 1 - n) / n) as u32;
                        let shift = pass.min(5);
                        let backoff = self.inner.base_backoff.saturating_mul(1u32 << shift);
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        Err(ProviderError::CustomError(format!(
            "all {n} rpc endpoints failed for '{method}': {}",
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_endpoint_set() {
        assert!(FailoverClient::new(&[]).is_err());
        assert!(FailoverClient::new(&["   ".to_string()]).is_err());
    }

    #[test]
    fn builds_with_multiple_endpoints_and_labels() {
        let client = FailoverClient::new(&[
            "https://rpc.one.example/v2/key".to_string(),
            "https://rpc.two.example".to_string(),
        ])
        .expect("build failover client");
        assert_eq!(client.endpoint_count(), 2);
        assert!(client.endpoint_label().contains("rpc.one.example"));
        assert!(client.endpoint_label().contains("rpc.two.example"));
    }

    #[test]
    fn rejects_malformed_url() {
        assert!(FailoverClient::new(&["not a url".to_string()]).is_err());
    }
}
