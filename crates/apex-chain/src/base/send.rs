//! The one type in this crate that can spend money (Task 8.5 R28).
//!
//! `rpc`'s transports are read instruments by type: an allowlist refuses
//! every write before a byte leaves. Submission is this separate type, holding
//! one endpoint and one method, `eth_sendRawTransaction`, so the process that
//! reads state for every candidate is not handed a transport that can send.
//!
//! **What an answer means.**
//! - **An error object** is the endpoint's no: nothing was sent.
//! - **A refused connection** reached nothing.
//! - **A timeout or a server error** may or may not have sent it, and is
//!   reported as silence.
//! - **A success naming another hash** is silence too: something may have
//!   been sent, and it was not what we signed.
//!
//! The URL carries the provider key: it is held as a `Secret`, named by
//! [`crate::rpc::endpoint_label`], and stripped from every error (INV-46).

use crate::rpc::endpoint_label;
use alloy_primitives::{keccak256, B256};
use apex_config::Secret;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendAnswer {
    /// The endpoint took it and named our transaction's hash.
    Accepted(B256),
    /// The endpoint answered no. Nothing was sent.
    Refused(String),
    /// No usable answer. It may or may not have been sent.
    Silent(String),
}

#[async_trait::async_trait]
pub trait RawTransport: Send + Sync {
    async fn send_raw(&self, raw: &[u8]) -> SendAnswer;
}

pub struct HttpSubmitter {
    url: Secret<String>,
    label: String,
    client: reqwest::Client,
    timeout: Duration,
    next_id: AtomicU64,
}

impl std::fmt::Debug for HttpSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSubmitter").field("endpoint", &self.label).finish_non_exhaustive()
    }
}

impl HttpSubmitter {
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
}

#[async_trait::async_trait]
impl RawTransport for HttpSubmitter {
    async fn send_raw(&self, raw: &[u8]) -> SendAnswer {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0", "id": id, "method": "eth_sendRawTransaction",
            "params": [format!("0x{}", alloy_primitives::hex::encode(raw))],
        });
        let response = match self.client.post(self.url.expose().as_str()).timeout(self.timeout).json(&body).send().await {
            Ok(r) => r,
            // Never connected: nothing left.
            Err(e) if e.is_connect() => return SendAnswer::Refused(format!("connect: {}", e.without_url())),
            Err(e) => return SendAnswer::Silent(e.without_url().to_string()),
        };
        let status = response.status();
        if status.is_client_error() {
            return SendAnswer::Refused(format!("http {status}"));
        }
        if !status.is_success() {
            return SendAnswer::Silent(format!("http {status}"));
        }
        let answer: Value = match response.json().await {
            Ok(v) => v,
            Err(e) => return SendAnswer::Silent(format!("not JSON-RPC: {}", e.without_url())),
        };
        if let Some(err) = answer.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            return SendAnswer::Refused(format!("{code}: {message}"));
        }
        let ours = keccak256(raw);
        match answer.get("result").and_then(Value::as_str).and_then(|s| s.parse::<B256>().ok()) {
            Some(h) if h == ours => SendAnswer::Accepted(h),
            Some(h) => SendAnswer::Silent(format!("the endpoint named {h:#x}, and ours is {ours:#x}")),
            None => SendAnswer::Silent(format!("no transaction hash in {answer}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-shot HTTP server answering `body` with `status`.
    async fn serve(status: &'static str, body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = s.read(&mut buf).await;
            let reply = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            let _ = s.write_all(reply.as_bytes()).await;
        });
        format!("http://{addr}/v1/rpc/secret-key")
    }

    #[tokio::test]
    async fn an_answer_with_our_hash_is_accepted() {
        let raw = b"\x02signed".to_vec();
        let hash = alloy_primitives::keccak256(&raw);
        let url = serve("200 OK", format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{hash:#x}"}}"#)).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        assert_eq!(s.send_raw(&raw).await, SendAnswer::Accepted(hash));
    }

    #[tokio::test]
    async fn an_error_object_is_a_refusal_with_its_code_and_message() {
        let url = serve("200 OK", r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32605,"message":"flagged"}}"#.into()).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        assert_eq!(s.send_raw(b"x").await, SendAnswer::Refused("-32605: flagged".into()));
    }

    #[tokio::test]
    async fn a_server_error_is_silence_and_names_no_key() {
        let url = serve("502 Bad Gateway", String::new()).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        let SendAnswer::Silent(detail) = s.send_raw(b"x").await else { panic!("not silent") };
        assert!(!detail.contains("secret-key"), "{detail}");
        assert!(!s.label().contains("secret-key"));
    }

    #[tokio::test]
    async fn an_answer_naming_another_hash_is_silence() {
        let other = B256::repeat_byte(0x99);
        let url = serve("200 OK", format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{other:#x}"}}"#)).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        assert!(matches!(s.send_raw(b"ours").await, SendAnswer::Silent(_)));
    }

    /// A 4xx is the endpoint refusing the request itself (a rate limit, a bad
    /// key): nothing was forwarded.
    #[tokio::test]
    async fn a_client_error_is_a_refusal() {
        let url = serve("429 Too Many Requests", String::new()).await;
        let s = HttpSubmitter::new(&url, Duration::from_secs(2)).unwrap();
        assert_eq!(s.send_raw(b"x").await, SendAnswer::Refused("http 429 Too Many Requests".into()));
    }

    #[tokio::test]
    async fn nothing_listening_is_a_refusal() {
        let s = HttpSubmitter::new("http://127.0.0.1:9/x", Duration::from_secs(2)).unwrap();
        assert!(matches!(s.send_raw(b"x").await, SendAnswer::Refused(_)));
    }
}
