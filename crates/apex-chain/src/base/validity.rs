//! The second type in this crate that can spend money (Task 8.5 R36): Base's
//! `base_sendRawTransactionValidity`, a signed transaction and the predicates
//! the builder holds it to, sent beside it and never recorded on chain.
//!
//! Like [`super::send::HttpSubmitter`], it holds one endpoint and one method,
//! and its answers mean what `send`'s do ([`read_answer`]): an error object or
//! a refused connection sent nothing; a timeout, a server error, or a hash not
//! ours may have. An accepted validity transaction is held, not included: it
//! lands only once its predicates hold, and costs nothing until then.
//!
//! The URL is held as a `Secret`, named by [`crate::rpc::endpoint_label`], and
//! stripped from every error (INV-46).

use super::send::{read_answer, SendAnswer};
use crate::rpc::endpoint_label;
use apex_config::Secret;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The JSON-RPC request: the raw transaction, then `{"validity": predicates}`.
pub fn validity_request(id: u64, raw: &[u8], validity: &Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "base_sendRawTransactionValidity",
        "params": [format!("0x{}", alloy_primitives::hex::encode(raw)), { "validity": validity }],
    })
}

pub struct ValiditySubmitter {
    url: Secret<String>,
    label: String,
    client: reqwest::Client,
    timeout: Duration,
    next_id: AtomicU64,
}

impl std::fmt::Debug for ValiditySubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValiditySubmitter").field("endpoint", &self.label).finish_non_exhaustive()
    }
}

impl ValiditySubmitter {
    pub fn new(url: &str, timeout: Duration) -> Result<Self, String> {
        let label = endpoint_label(url);
        reqwest::Url::parse(url).map_err(|_| format!("not a URL: {label}"))?;
        let client = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(timeout)
            .build()
            .map_err(|e| e.without_url().to_string())?;
        Ok(Self { url: Secret::new(url.to_string()), label, client, timeout, next_id: AtomicU64::new(1) })
    }

    /// The endpoint, safe to log.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Send `raw` with its `validity` predicates.
    pub async fn send(&self, raw: &[u8], validity: &Value) -> SendAnswer {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = validity_request(id, raw, validity);
        let response = match self.client.post(self.url.expose().as_str()).timeout(self.timeout).json(&body).send().await {
            Ok(r) => r,
            // Never connected: nothing left.
            Err(e) if e.is_connect() => return SendAnswer::Refused(format!("connect: {}", e.without_url())),
            Err(e) => return SendAnswer::Silent(e.without_url().to_string()),
        };
        let status = response.status().as_u16();
        let body = if (200..300).contains(&status) { response.json().await.ok() } else { None };
        read_answer(raw, status, body)
    }
}
