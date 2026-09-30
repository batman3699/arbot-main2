//! Task 8.5 R1 — the websocket feed (`apex-chain::rpc::ws`).
//!
//! Parsing is tested against recorded message shapes; the session — subscribe,
//! forward, reconnect — against [`support::ws::WsNode`], a scripted node on a
//! real local socket.

mod support;

use alloy_primitives::{address, b256, Address, B256};
use apex_chain::rpc::ws::{
    parse_notification, subscribe_request, Kind, LogFilter, Notification, Subscription, WsFeed,
    WsSettings,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use support::ws::{Push, Script, WsNode};

const POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SWAP: B256 = b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");

fn filter() -> LogFilter {
    LogFilter { addresses: vec![POOL], topics0: vec![SWAP] }
}

fn subs() -> Vec<Subscription> {
    vec![
        Subscription::NewHeads,
        Subscription::Logs(filter()),
        Subscription::PendingLogs(filter()),
    ]
}

fn fast() -> WsSettings {
    WsSettings {
        connect_timeout: Duration::from_millis(500),
        stall_after: Duration::from_secs(5),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(100),
        channel_capacity: 64,
    }
}

fn head_json(n: u64) -> Value {
    json!({
        "number": format!("0x{n:x}"),
        "hash": format!("0x{:064x}", n),
        "timestamp": "0x6720d0a0",
        "baseFeePerGas": "0x5f5e10",
        "gasUsed": "0x1c9c380",
        "gasLimit": "0x8f0d180"
    })
}

fn log_json(block: Option<u64>, removed: bool) -> Value {
    let mut v = json!({
        "address": POOL,
        "topics": [SWAP, format!("0x{:064x}", 1), format!("0x{:064x}", 2)],
        "data": "0x00ff",
        "removed": removed,
    });
    if let Some(b) = block {
        v["blockNumber"] = json!(format!("0x{b:x}"));
        v["transactionHash"] = json!(format!("0x{:064x}", 0xabcu64));
        v["logIndex"] = json!("0x3");
    }
    v
}

fn ids() -> BTreeMap<String, Kind> {
    BTreeMap::from([
        ("0x1".to_string(), Kind::NewHeads),
        ("0x2".to_string(), Kind::Logs),
        ("0x3".to_string(), Kind::PendingLogs),
    ])
}

fn note(sub: &str, result: Value) -> Value {
    json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":sub,"result":result}})
}

// ------------------------------------------------------------------ parsing

#[test]
fn a_new_head_parses() {
    match parse_notification(&note("0x1", head_json(51_985_044)), &ids()) {
        Some(Notification::Head(h)) => {
            assert_eq!(h.number, 51_985_044);
            assert_eq!(h.base_fee_per_gas, Some(0x5f5e10));
            assert_eq!(h.gas_used, 0x1c9c380);
            assert_eq!(h.gas_limit, 0x8f0d180);
            assert_eq!(h.timestamp, 0x6720d0a0);
        }
        other => panic!("expected a head, got {other:?}"),
    }
}

/// A preconfirmed log says so. The two kinds are different facts — a
/// flashblock the sequencer may still reorder, and a sealed block — and a
/// consumer that could not tell them apart would treat one as the other.
#[test]
fn a_pending_log_is_marked_pending_and_a_confirmed_one_is_not() {
    let Some(Notification::Log(pending)) = parse_notification(&note("0x3", log_json(None, false)), &ids())
    else {
        panic!("pending log did not parse")
    };
    assert!(pending.pending);
    assert_eq!(pending.address, POOL);
    assert_eq!(pending.topics[0], SWAP);
    assert_eq!(pending.data, vec![0x00, 0xff]);

    let Some(Notification::Log(confirmed)) =
        parse_notification(&note("0x2", log_json(Some(51_985_044), false)), &ids())
    else {
        panic!("confirmed log did not parse")
    };
    assert!(!confirmed.pending);
    assert_eq!(confirmed.block_number, Some(51_985_044));
    assert_eq!(confirmed.log_index, Some(3));
    assert!(confirmed.transaction_hash.is_some());
}

/// A reorged log arrives again with `removed: true`. Dropping the flag would
/// apply a swap that did not happen.
#[test]
fn a_removed_log_is_flagged() {
    let Some(Notification::Log(l)) = parse_notification(&note("0x2", log_json(Some(7), true)), &ids())
    else {
        panic!("did not parse")
    };
    assert!(l.removed);
}

/// A notification for a subscription this feed did not make is ignored, not
/// filed under whichever kind happened to be nearby.
#[test]
fn an_unknown_subscription_is_ignored() {
    assert!(parse_notification(&note("0x99", log_json(None, false)), &ids()).is_none());
    assert!(parse_notification(&json!({"jsonrpc":"2.0","id":1,"result":"0x1"}), &ids()).is_none());
}

/// Malformed payloads are ignored rather than defaulted: a head with no number
/// is not block zero.
#[test]
fn a_malformed_payload_is_not_defaulted() {
    let mut h = head_json(5);
    h.as_object_mut().unwrap().remove("number");
    assert!(parse_notification(&note("0x1", h), &ids()).is_none());
    let mut l = log_json(None, false);
    l["address"] = json!("not an address");
    assert!(parse_notification(&note("0x3", l), &ids()).is_none());
}

/// The only requests this module can make are `eth_subscribe` of the three
/// read kinds. There is no send path to guard: the enum has no variant for one.
#[test]
fn subscribe_requests_are_reads() {
    for (i, s) in subs().iter().enumerate() {
        let r = subscribe_request(i as u64 + 1, s);
        assert_eq!(r["method"], json!("eth_subscribe"));
        let kind = r["params"][0].as_str().unwrap();
        assert!(["newHeads", "logs", "pendingLogs"].contains(&kind), "{kind}");
    }
    let r = subscribe_request(2, &Subscription::Logs(filter()));
    assert_eq!(r["params"][1]["address"], json!([POOL]));
    assert_eq!(r["params"][1]["topics"], json!([[SWAP]]));
}

// ------------------------------------------------------------------ the session

async fn recv(rx: &mut tokio::sync::mpsc::Receiver<Notification>) -> Notification {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a notification within 5s")
        .expect("the channel is open")
}

/// Subscribes to all three, then forwards what arrives — heads and both kinds of
/// log — in order.
#[tokio::test]
async fn the_feed_subscribes_and_forwards() {
    let node = WsNode::start(vec![Script {
        pushes: vec![
            Push { subscription: 1, result: head_json(100) },
            Push { subscription: 3, result: log_json(None, false) },
            Push { subscription: 2, result: log_json(Some(100), false) },
        ],
        ..Script::default()
    }])
    .await;
    let feed = WsFeed::new(&node.url("/v1/ws/key"), subs(), fast()).expect("a feed");
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let (handle, mut rx) = feed.spawn(stop);

    assert!(matches!(recv(&mut rx).await, Notification::Head(h) if h.number == 100));
    assert!(matches!(recv(&mut rx).await, Notification::Log(l) if l.pending));
    assert!(matches!(recv(&mut rx).await, Notification::Log(l) if !l.pending));

    let requests = node.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r["method"] == json!("eth_subscribe")));

    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle).await.expect("stops").expect("joins");
}

/// **A dropped socket is a gap, and the feed says so.** It reconnects, resubscribes,
/// and emits `Reconnected` before anything from the new session — so whoever
/// holds state built from the old session knows it may have missed updates
/// (§5.6: a gap blocks live tickets until state is rebuilt).
#[tokio::test]
async fn a_dropped_socket_reconnects_and_reports_the_gap() {
    let node = WsNode::start(vec![
        Script { pushes: vec![Push { subscription: 1, result: head_json(1) }], close_after: true, ..Script::default() },
        Script { pushes: vec![Push { subscription: 1, result: head_json(2) }], ..Script::default() },
    ])
    .await;
    let feed = WsFeed::new(&node.url("/"), subs(), fast()).expect("a feed");
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let (handle, mut rx) = feed.spawn(stop);

    assert!(matches!(recv(&mut rx).await, Notification::Head(h) if h.number == 1));
    match recv(&mut rx).await {
        Notification::Reconnected { attempt, .. } => assert!(attempt >= 1),
        other => panic!("expected Reconnected before the new session, got {other:?}"),
    }
    assert!(matches!(recv(&mut rx).await, Notification::Head(h) if h.number == 2));
    assert!(node.connections() >= 2);
    assert_eq!(node.requests().len(), 6, "resubscribed to all three");

    stop_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

/// A subscription the provider refuses is a configuration failure, not
/// something to run without: a feed missing `pendingLogs` would silently be a
/// block slower. The session ends and is retried, and the refusal is counted.
#[tokio::test]
async fn a_refused_subscription_is_not_run_without() {
    let node = WsNode::start(vec![Script { refuse: Some(3), ..Script::default() }]).await;
    let feed = WsFeed::new(&node.url("/"), subs(), fast()).expect("a feed");
    let stats = feed.stats();
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let (handle, _rx) = feed.spawn(stop);

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(stats.refusals() >= 1, "the refusal was not counted");
    assert!(node.connections() >= 2, "it did not retry");

    stop_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

/// INV-46: the key is in the URL path, and neither the label nor the failure a
/// refused connection produces may carry it.
#[tokio::test]
async fn the_credential_reaches_no_label_or_failure() {
    const KEY: &str = "SUPERSECRETWSKEY0123456789";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let feed = WsFeed::new(&format!("ws://{addr}/v1/ws/{KEY}"), subs(), fast()).expect("a feed");
    assert!(!feed.label().contains(KEY), "label: {}", feed.label());
    assert!(!format!("{feed:?}").contains(KEY));

    let stats = feed.stats();
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let (handle, _rx) = feed.spawn(stop);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let last = stats.last_failure().expect("the refused connection was recorded");
    assert!(!last.contains(KEY), "failure leaks: {last}");
    stop_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

#[test]
fn a_non_websocket_url_is_refused() {
    assert!(WsFeed::new("https://example.org/rpc", subs(), fast()).is_err());
    assert!(WsFeed::new("not a url", subs(), fast()).is_err());
    assert!(WsFeed::new("wss://example.org/ws", Vec::new(), fast()).is_err(), "no subscriptions");
}
