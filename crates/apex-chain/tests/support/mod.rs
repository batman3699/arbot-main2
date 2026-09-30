//! A JSON-RPC node that answers from a script, on a real socket.
//!
//! # Why not `httpmock`
//!
//! It is already in the lockfile, and it cannot do the one thing these tests
//! most need: a JSON-RPC answer has to carry the **id of the request it
//! answers**, and a canned body cannot echo it. A transport that accepted an
//! answer with the wrong id would be accepting the answer to a different
//! question, so the check is worth having — and a mock that forced it off, or
//! forced the tests to predict the transport's id sequence, would test the
//! mock.
//!
//! So this is ~100 lines of HTTP/1.1 over a `TcpListener`: it reads one request
//! per connection, answers with `Connection: close`, echoes the id, and counts
//! hits **per method**. Counting per method is what lets a test assert an
//! absence — "the fallback was never asked" — which is most of what a failover
//! policy is.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Base mainnet, as `eth_chainId` reports it.
pub const BASE_HEX: &str = "0x2105";

/// What the node does with one request.
#[derive(Clone, Debug)]
pub enum Reply {
    /// `{"result": …}` with the request's own id.
    Result(Value),
    /// `{"error": {code, message, data}}` with the request's own id.
    Error { code: i64, message: String, data: Option<Value> },
    /// A bare HTTP status with no JSON-RPC body: a proxy, a rate limiter, a 503.
    Http(u16),
    /// A well-formed answer carrying somebody else's id.
    WrongId(Value),
    /// Not JSON at all.
    Garbage,
    /// Wait, then do the inner thing.
    Delay(Duration, Box<Reply>),
}

type Handler = Box<dyn Fn(&str, &Value) -> Reply + Send + Sync>;

struct NodeState {
    handler: Handler,
    hits: BTreeMap<String, usize>,
}

pub struct MockNode {
    addr: SocketAddr,
    state: Arc<Mutex<NodeState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockNode {
    pub async fn start(handler: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a local port");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(NodeState {
            handler: Box::new(handler),
            hits: BTreeMap::new(),
        }));
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { return };
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    let _ = serve(socket, shared).await;
                });
            }
        });
        Self { addr, state, task }
    }

    /// A node on Base that answers `eth_chainId` itself and hands everything
    /// else to `handler`.
    pub async fn base(handler: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static) -> Self {
        Self::start(move |method, params| {
            if method == "eth_chainId" {
                Reply::Result(json!(BASE_HEX))
            } else {
                handler(method, params)
            }
        })
        .await
    }

    /// Replace the script. Hit counts are kept.
    pub fn set(&self, handler: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static) {
        self.state.lock().unwrap().handler = Box::new(handler);
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn hits(&self, method: &str) -> usize {
        self.state.lock().unwrap().hits.get(method).copied().unwrap_or(0)
    }

    pub fn total_hits(&self) -> usize {
        self.state.lock().unwrap().hits.values().sum()
    }
}

/// A URL nothing is listening on. Bound and immediately released, so the port
/// was free a moment ago and a connect to it is refused rather than hanging.
pub async fn dead_url(path: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    format!("http://{addr}{path}")
}

async fn serve(mut socket: TcpStream, state: Arc<Mutex<NodeState>>) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    while buf.len() < header_end + len {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body: Value = serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
    let method = body["method"].as_str().unwrap_or("").to_string();
    let id = body["id"].clone();
    let reply = {
        let mut s = state.lock().unwrap();
        *s.hits.entry(method.clone()).or_default() += 1;
        (s.handler)(&method, &body["params"])
    };
    let (status, payload) = render(reply, &id).await;
    let response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

async fn render(reply: Reply, id: &Value) -> (u16, String) {
    let mut reply = reply;
    while let Reply::Delay(d, inner) = reply {
        tokio::time::sleep(d).await;
        reply = *inner;
    }
    match reply {
        Reply::Result(v) => (200, json!({"jsonrpc": "2.0", "id": id, "result": v}).to_string()),
        Reply::Error { code, message, data } => {
            let mut err = json!({"code": code, "message": message});
            if let Some(d) = data {
                err["data"] = d;
            }
            (200, json!({"jsonrpc": "2.0", "id": id, "error": err}).to_string())
        }
        Reply::Http(status) => (status, String::new()),
        Reply::WrongId(v) => {
            let other = id.as_u64().map_or(json!(999_999), |n| json!(n + 1_000));
            (200, json!({"jsonrpc": "2.0", "id": other, "result": v}).to_string())
        }
        Reply::Garbage => (200, "<html>definitely not json-rpc</html>".to_string()),
        Reply::Delay(..) => unreachable!("unwrapped above"),
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
