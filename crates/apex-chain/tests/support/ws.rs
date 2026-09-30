//! A websocket node that answers subscriptions from a script, on a real socket.
//!
//! Each accepted connection answers every `eth_subscribe` with an id (`0x1`,
//! `0x2`, … in request order), then pushes the scripted notifications —
//! addressed to those ids — and optionally closes. Connections are counted, so
//! a test can see a reconnect happen.

#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// One pushed notification: which subscription (by request order, from 1) and
/// the `result` payload.
#[derive(Clone, Debug)]
pub struct Push {
    pub subscription: usize,
    pub result: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Script {
    pub pushes: Vec<Push>,
    /// Close the socket after the pushes, forcing the client to reconnect.
    pub close_after: bool,
    /// Answer this subscription (by request order, from 1) with an error.
    pub refuse: Option<usize>,
    /// Subscriptions to answer before pushing; `None` is the feed tests' three.
    pub subscriptions: Option<usize>,
}

pub struct WsNode {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WsNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WsNode {
    /// `scripts[i]` is what connection `i` does; connections past the end use
    /// the last script.
    pub async fn start(scripts: Vec<Script>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (c, r) = (Arc::clone(&connections), Arc::clone(&requests));
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { return };
                let n = c.fetch_add(1, Ordering::SeqCst);
                let script = scripts.get(n).or(scripts.last()).cloned().unwrap_or_default();
                let r = Arc::clone(&r);
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else { return };
                    let mut subscribed = 0usize;
                    let expected = expected_subscriptions(&script);
                    // Answer subscriptions as they arrive, then push.
                    while subscribed < expected {
                        let Some(Ok(Message::Text(t))) = ws.next().await else { return };
                        let req: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        r.lock().unwrap().push(req.clone());
                        subscribed += 1;
                        let reply = if script.refuse == Some(subscribed) {
                            json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32601,"message":"not supported"}})
                        } else {
                            json!({"jsonrpc":"2.0","id":req["id"],"result":format!("0x{subscribed:x}")})
                        };
                        if ws.send(Message::Text(reply.to_string())).await.is_err() {
                            return;
                        }
                    }
                    for p in &script.pushes {
                        let msg = json!({
                            "jsonrpc":"2.0","method":"eth_subscription",
                            "params":{"subscription":format!("0x{:x}", p.subscription),"result":p.result}
                        });
                        if ws.send(Message::Text(msg.to_string())).await.is_err() {
                            return;
                        }
                    }
                    if script.close_after {
                        let _ = ws.close(None).await;
                        return;
                    }
                    // Hold the connection open, draining whatever arrives.
                    while let Some(Ok(_)) = ws.next().await {}
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                });
            }
        });
        Self { addr, connections, requests, task }
    }

    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{}", self.addr, path)
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// The node waits for every subscription before pushing, so no notification
/// races its own subscription id. The feed tests subscribe to three kinds; a
/// flashblock sample, to one.
fn expected_subscriptions(s: &Script) -> usize {
    s.subscriptions.unwrap_or(3)
}
