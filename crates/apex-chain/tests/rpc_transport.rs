//! Task 8.5 — the JSON-RPC transport (`apex-chain::rpc`).
//!
//! Every test runs against [`support::MockNode`], a scripted node on a real local
//! socket. Most of what a failover policy *is* is a set of absences — the
//! fallback was not asked, the broadcast never left the process, the credential
//! never reached a string — and an absence can only be checked by counting.

mod support;

use apex_chain::rpc::{
    endpoint_label, ConnectError, FailoverSettings, FailoverTransport, RpcError, RpcTransport,
    READ_METHODS,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use support::{dead_url, MockNode, Reply};

const BASE: u64 = 8453;

/// Fast enough that a test suite with a dozen timeouts in it still runs in
/// seconds; slow enough that a loaded CI box does not time out a healthy local
/// answer.
fn fast() -> FailoverSettings {
    FailoverSettings {
        request_timeout: Duration::from_millis(400),
        extra_passes: 0,
        base_backoff: Duration::from_millis(5),
    }
}

async fn connect(urls: Vec<String>, settings: FailoverSettings) -> FailoverTransport {
    FailoverTransport::connect(&urls, BASE, settings).await.expect("connect")
}

fn block(_: &str, _: &Value) -> Reply {
    Reply::Result(json!("0x3190a39"))
}

// ------------------------------------------------------------ read-only by type

/// **A shadow run must be unable to broadcast, not merely configured not to.**
/// The transport this crate builds is a read instrument: a method off the
/// allowlist is refused before any byte leaves the process, so no caller —
/// however it is wired — can send a transaction through it.
#[tokio::test]
async fn a_read_transport_refuses_to_broadcast() {
    let node = MockNode::base(block).await;
    let rpc = connect(vec![node.url("/")], fast()).await;

    for method in [
        "eth_sendRawTransaction",
        "eth_sendTransaction",
        "eth_sendBundle",
        "eth_sendPrivateTransaction",
        "eth_sendRawTransactionConditional",
        "eth_sign",
        "personal_sign",
    ] {
        let err = rpc.call(method, json!(["0x02f8"])).await.unwrap_err();
        assert_eq!(err, RpcError::NotARead { method: method.to_string() });
        assert_eq!(node.hits(method), 0, "{method} reached the node");
    }
}

/// The allowlist is the whole guarantee above, so its contents are checked for
/// the shape of a write rather than trusted.
#[test]
fn the_allowlist_holds_reads_only() {
    for m in READ_METHODS {
        let lower = m.to_ascii_lowercase();
        for forbidden in ["send", "sign", "personal_", "admin_", "miner_", "unlock", "debug_set"] {
            assert!(!lower.contains(forbidden), "{m} is on the read allowlist");
        }
    }
    for needed in [
        "eth_chainId",
        "eth_blockNumber",
        "eth_getTransactionCount",
        "eth_getBalance",
        "eth_getCode",
        "eth_call",
        "eth_simulateV1",
        "eth_getTransactionReceipt",
        "base_transactionStatus",
    ] {
        assert!(READ_METHODS.contains(&needed), "{needed} is missing from the allowlist");
    }
}

// ------------------------------------------------------------ what rotates

/// A dead endpoint is rotated past, and the one that answered is **kept** for
/// the next call — affinity, so consecutive reads do not alternate between two
/// nodes' views of the chain.
///
/// Both endpoints are healthy at boot and the preferred one dies afterwards.
/// A first draft had the dead one down *at* boot, which proved nothing:
/// `connect` starts at the first **verified** endpoint, so the dead one was never
/// tried and a transport that forgot where it last succeeded passed anyway.
#[tokio::test]
async fn a_dead_endpoint_is_rotated_past_and_the_good_one_kept() {
    let preferred = MockNode::base(block).await;
    let fallback = MockNode::base(block).await;
    let rpc = connect(vec![preferred.url("/"), fallback.url("/")], fast()).await;
    preferred.set(|_, _| Reply::Http(503));

    assert_eq!(rpc.call("eth_blockNumber", json!([])).await.unwrap(), json!("0x3190a39"));
    assert_eq!(preferred.hits("eth_blockNumber"), 1, "the preferred endpoint was not tried first");

    assert_eq!(rpc.call("eth_blockNumber", json!([])).await.unwrap(), json!("0x3190a39"));
    assert_eq!(preferred.hits("eth_blockNumber"), 1, "the second call went back to the dead endpoint");
    assert_eq!(fallback.hits("eth_blockNumber"), 2);
}

/// A refused connection is the same class of fault as a 503.
#[tokio::test]
async fn a_refused_connection_is_rotated_past() {
    let good = MockNode::base(block).await;
    let rpc = connect(vec![dead_url("/").await, good.url("/")], fast()).await;
    assert_eq!(rpc.call("eth_blockNumber", json!([])).await.unwrap(), json!("0x3190a39"));
}

/// **A revert is the chain's answer, not the endpoint's fault.** Every endpoint
/// replays the same call against the same state and says the same thing, so
/// rotating is guaranteed waste — the legacy failover measured 75% of its
/// rotations spent on reverts before this rule existed. The revert data comes
/// back intact, because it is how a caller tells `Too little received` from an
/// access-control failure.
#[tokio::test]
async fn a_revert_is_the_chains_answer_and_is_not_rotated() {
    let revert = json!("0x08c379a0");
    let data = revert.clone();
    let primary = MockNode::base(move |_, _| Reply::Error {
        code: 3,
        message: "execution reverted: Too little received".into(),
        data: Some(data.clone()),
    })
    .await;
    let fallback = MockNode::base(block).await;
    let rpc = connect(vec![primary.url("/"), fallback.url("/")], fast()).await;

    let err = rpc.call("eth_call", json!([{}, "0x1"])).await.unwrap_err();
    match err {
        RpcError::Reverted { message, data } => {
            assert!(message.contains("Too little received"));
            assert_eq!(data, Some(revert));
        }
        other => panic!("expected a revert, got {other:?}"),
    }
    assert_eq!(fallback.hits("eth_call"), 0, "a deterministic answer was re-asked elsewhere");
}

/// A rate limit says nothing about the chain. It is the endpoint's problem, and
/// the next endpoint may well answer.
#[tokio::test]
async fn a_rate_limit_is_rotated() {
    let limited = MockNode::base(|_, _| Reply::Error {
        code: -32005,
        message: "Monthly capacity limit exceeded".into(),
        data: None,
    })
    .await;
    let fallback = MockNode::base(block).await;
    let rpc = connect(vec![limited.url("/"), fallback.url("/")], fast()).await;
    assert_eq!(rpc.call("eth_blockNumber", json!([])).await.unwrap(), json!("0x3190a39"));
}

/// Public fallbacks do not all serve `eth_simulateV1` or
/// `base_transactionStatus`. A method one endpoint lacks is that endpoint's
/// limitation, not an answer.
#[tokio::test]
async fn a_method_the_endpoint_lacks_is_rotated() {
    let public = MockNode::base(|_, _| Reply::Error {
        code: -32601,
        message: "the method eth_simulateV1 does not exist/is not available".into(),
        data: None,
    })
    .await;
    let keyed = MockNode::base(|_, _| Reply::Result(json!([{"calls": []}]))).await;
    let rpc = connect(vec![public.url("/"), keyed.url("/")], fast()).await;
    assert_eq!(
        rpc.call("eth_simulateV1", json!([{}, "0x1"])).await.unwrap(),
        json!([{"calls": []}])
    );
}

/// `null` is an answer. A receipt that does not exist yet is `result: null`, and
/// treating it as a failure would rotate every pending-transaction poll through
/// every endpoint.
#[tokio::test]
async fn a_null_result_is_an_answer() {
    let primary = MockNode::base(|_, _| Reply::Result(Value::Null)).await;
    let fallback = MockNode::base(block).await;
    let rpc = connect(vec![primary.url("/"), fallback.url("/")], fast()).await;
    assert_eq!(rpc.call("eth_getTransactionReceipt", json!(["0x01"])).await.unwrap(), Value::Null);
    assert_eq!(fallback.total_hits() - fallback.hits("eth_chainId"), 0);
}

/// A slow endpoint is abandoned at the timeout rather than waited on, and the
/// call still answers from the next one.
#[tokio::test]
async fn a_slow_endpoint_is_abandoned_at_the_timeout() {
    let slow = MockNode::base(|_, _| {
        Reply::Delay(Duration::from_secs(5), Box::new(Reply::Result(json!("0xslow"))))
    })
    .await;
    let fallback = MockNode::base(block).await;
    let rpc = connect(vec![slow.url("/"), fallback.url("/")], fast()).await;

    let started = Instant::now();
    assert_eq!(rpc.call("eth_blockNumber", json!([])).await.unwrap(), json!("0x3190a39"));
    assert!(started.elapsed() < Duration::from_secs(2), "waited {:?}", started.elapsed());
}

/// An answer carrying a different id is the answer to a different question.
#[tokio::test]
async fn an_answer_to_a_different_request_is_not_an_answer() {
    let confused = MockNode::base(|_, _| Reply::WrongId(json!("0xdead"))).await;
    let fallback = MockNode::base(block).await;
    let rpc = connect(vec![confused.url("/"), fallback.url("/")], fast()).await;
    assert_eq!(rpc.call("eth_blockNumber", json!([])).await.unwrap(), json!("0x3190a39"));
}

#[tokio::test]
async fn a_body_that_is_not_json_rpc_is_rotated() {
    let proxy = MockNode::base(|_, _| Reply::Garbage).await;
    let fallback = MockNode::base(block).await;
    let rpc = connect(vec![proxy.url("/"), fallback.url("/")], fast()).await;
    assert_eq!(rpc.call("eth_blockNumber", json!([])).await.unwrap(), json!("0x3190a39"));
}

/// Every endpoint failing is an **error**, never a default. The failure mode
/// this guards is §5.6's: a chain that cannot be asked must never read as a chain
/// that said "nothing".
#[tokio::test]
async fn every_endpoint_failing_is_an_error_never_a_default() {
    let a = MockNode::base(|_, _| Reply::Http(503)).await;
    let b = MockNode::base(|_, _| Reply::Http(429)).await;
    let rpc = connect(vec![a.url("/"), b.url("/")], fast()).await;
    match rpc.call("eth_blockNumber", json!([])).await {
        Err(RpcError::Exhausted { method, endpoints, .. }) => {
            assert_eq!(method, "eth_blockNumber");
            assert_eq!(endpoints, 2);
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
}

/// Each pass asks each endpoint once; `extra_passes` adds whole passes, with
/// backoff only after the first full cycle so one bad endpoint costs no sleep.
#[tokio::test]
async fn each_pass_asks_each_endpoint_once() {
    let a = MockNode::base(|_, _| Reply::Http(503)).await;
    let b = MockNode::base(|_, _| Reply::Http(503)).await;
    let settings = FailoverSettings { extra_passes: 2, ..fast() };
    let rpc = connect(vec![a.url("/"), b.url("/")], settings).await;
    let _ = rpc.call("eth_blockNumber", json!([])).await;
    assert_eq!(a.hits("eth_blockNumber"), 3);
    assert_eq!(b.hits("eth_blockNumber"), 3);
}

/// A logical call counts once however many endpoints it took. The count is what
/// sizes a provider plan, and it is useless if a flaky endpoint inflates it.
#[tokio::test]
async fn requests_are_counted_per_call_not_per_attempt() {
    let bad = MockNode::base(|_, _| Reply::Http(503)).await;
    let good = MockNode::base(block).await;
    let rpc = connect(vec![bad.url("/"), good.url("/")], fast()).await;
    let before = rpc.requests();
    rpc.call("eth_blockNumber", json!([])).await.unwrap();
    rpc.call("eth_blockNumber", json!([])).await.unwrap();
    assert_eq!(rpc.requests() - before, 2);
}

// ------------------------------------------------------------ which chain

/// `wrong_chain_submission` is a hard-zero counter, and a read transport pointed
/// at the wrong chain would feed it everything upstream of a submission: state,
/// prices, simulations. An endpoint answering another chain id is a
/// configuration error, and the boot refuses rather than dropping it quietly.
#[tokio::test]
async fn an_endpoint_on_another_chain_refuses_the_boot() {
    let base = MockNode::base(block).await;
    // Ethereum mainnet: chain 1, and it answers everything with `0x1`.
    let mainnet = MockNode::start(|_, _| Reply::Result(json!("0x1"))).await;
    let err = FailoverTransport::connect(&[base.url("/"), mainnet.url("/v1/rpc/zzsecretzz")], BASE, fast())
        .await
        .unwrap_err();
    match err {
        ConnectError::WrongChain { expected, answered, endpoint } => {
            assert_eq!(expected, BASE);
            assert_eq!(answered, 1);
            assert!(!endpoint.contains("zzsecretzz"), "the label carries the path: {endpoint}");
        }
        other => panic!("expected WrongChain, got {other}"),
    }
}

/// An endpoint that was down at boot has not been checked, so it is checked
/// **before its first use** — and one that turns out to be on another chain is
/// never asked anything else. Dropping it at boot instead would make a transient
/// blip permanent for the life of the process; trusting it unchecked would make
/// the boot check decorative.
#[tokio::test]
async fn an_endpoint_unverified_at_boot_is_checked_before_its_first_use() {
    let primary = MockNode::base(block).await;
    let late = MockNode::start(|_, _| Reply::Http(503)).await;
    let rpc = connect(vec![primary.url("/"), late.url("/")], fast()).await;

    // The late endpoint comes up -- on the wrong chain -- and the primary dies.
    late.set(|m, _| {
        if m == "eth_chainId" {
            Reply::Result(json!("0xa"))
        } else {
            Reply::Result(json!("0xwrongchainblock"))
        }
    });
    primary.set(|_, _| Reply::Http(503));

    let err = rpc.call("eth_blockNumber", json!([])).await.unwrap_err();
    assert!(matches!(err, RpcError::Exhausted { .. }), "got {err:?}");
    assert_eq!(late.hits("eth_blockNumber"), 0, "a wrong-chain endpoint was asked for state");
    assert!(late.hits("eth_chainId") >= 2, "the late endpoint was never re-checked");
}

#[tokio::test]
async fn a_boot_where_nothing_answers_is_refused() {
    let a = MockNode::start(|_, _| Reply::Http(503)).await;
    let err = FailoverTransport::connect(&[a.url("/"), dead_url("/").await], BASE, fast())
        .await
        .unwrap_err();
    assert!(matches!(err, ConnectError::NothingAnswered { endpoints: 2 }), "got {err}");
}

#[tokio::test]
async fn no_endpoints_is_refused() {
    let err = FailoverTransport::connect(&["  ".to_string()], BASE, fast()).await.unwrap_err();
    assert!(matches!(err, ConnectError::NoEndpoints), "got {err}");
}

// ------------------------------------------------------------ INV-46

/// **The credential is the URL path**, which is the shape `.env` actually uses
/// (`https://base.blockpi.network/v1/rpc/<key>`). It must not survive into a
/// label, a `Debug` dump, or an error — and reqwest's own error text embeds the
/// full URL, which is the path this test exists to close.
#[tokio::test]
async fn the_credential_reaches_no_label_error_or_debug() {
    const KEY: &str = "SUPERSECRETKEY0123456789abcdef";
    let refusing = MockNode::base(|_, _| Reply::Http(503)).await;
    let refused = dead_url(&format!("/v1/rpc/{KEY}")).await;
    let good = MockNode::base(block).await;
    // The refused connection is tried LAST, because `Exhausted` carries the last
    // failure: that is the reqwest error whose default text embeds the URL.
    let urls = vec![good.url("/"), refusing.url(&format!("/v1/rpc/{KEY}")), refused];
    let rpc = connect(urls, fast()).await;

    for label in rpc.labels() {
        assert!(!label.contains(KEY), "label leaks: {label}");
    }
    assert!(!format!("{rpc:?}").contains(KEY), "Debug leaks");

    good.set(|_, _| Reply::Http(503));
    let err = rpc.call("eth_blockNumber", json!([])).await.unwrap_err();
    assert!(!err.to_string().contains(KEY), "error leaks: {err}");
    assert!(!format!("{err:?}").contains(KEY), "error Debug leaks: {err:?}");
}

#[test]
fn a_label_keeps_the_host_and_hides_the_rest() {
    let keyed = endpoint_label("https://base.blockpi.network/v1/rpc/0123456789abcdef");
    assert!(keyed.starts_with("https://base.blockpi.network/***#"), "{keyed}");
    assert!(!keyed.contains("0123456789abcdef"));

    // Two keys on one host stay distinguishable without either being shown.
    assert_ne!(keyed, endpoint_label("https://base.blockpi.network/v1/rpc/fedcba9876543210"));

    // Nothing to hide: the host is the whole identity.
    assert_eq!(endpoint_label("https://mainnet.base.org"), "https://mainnet.base.org");

    // Userinfo and query strings are credentials too.
    assert!(!endpoint_label("https://user:pw@host.example/rpc").contains("pw"));
    assert!(!endpoint_label("https://host.example/?apikey=abc123").contains("abc123"));
    assert_eq!(endpoint_label("not a url"), "***");
}
