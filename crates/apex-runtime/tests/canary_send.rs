//! Task 8.5 R28: the canary's sender (`apex_runtime::canary::send`).

use alloy_primitives::{keccak256, B256, U256};
use apex_capture::dispatch::DispatchError;
use apex_chain::base::send::{RawTransport, SendAnswer};
use apex_runtime::canary::ledger::{Record, Settled};
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::canary::send::{CanaryDispatcher, InFlight};
use apex_runtime::plane::{FeeCaps, SendContext};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn ctx() -> SendContext {
    SendContext {
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        route_hash: B256::repeat_byte(0x44),
        venues: vec![VenueId(1)],
        gas_limit: GasLimit(500_000),
        fees: FeeCaps { max_fee_per_gas: 20_000_000, max_priority_fee_per_gas: 1_000_000 },
        expected_gross_wei: U256::from(900u64),
        expected_net_wei: 400,
        waived: vec!["route_authorization_valid".into()],
    }
}

/// Answers as scripted, and checks the ledger already holds the send record
/// when it is called: the write-ahead property, observed from the transport.
struct Scripted {
    answer: SendAnswer,
    ledger: PathBuf,
    saw_record: Mutex<Option<bool>>,
}

#[async_trait::async_trait]
impl RawTransport for Scripted {
    async fn send_raw(&self, raw: &[u8]) -> SendAnswer {
        let text = std::fs::read_to_string(&self.ledger).unwrap_or_default();
        let tx = format!("{:#x}", keccak256(raw));
        *self.saw_record.lock().unwrap() = Some(text.contains(&tx));
        match &self.answer {
            SendAnswer::Accepted(_) => SendAnswer::Accepted(keccak256(raw)),
            other => other.clone(),
        }
    }
}

fn rig(dir: &Path, answer: SendAnswer) -> (Arc<Canary>, Arc<Scripted>, CanaryDispatcher) {
    let ledger = dir.join("canary.jsonl");
    let canary = Arc::new(
        Canary::open(&ledger, &dir.join("canary.stop"), Limits { loss_cap_wei: 1_000, max_consecutive_reverts: 5, max_consecutive_refusals: 2, max_landed: 100 })
            .unwrap(),
    );
    let t = Arc::new(Scripted { answer, ledger, saw_record: Mutex::new(None) });
    let d = CanaryDispatcher::new(Arc::clone(&canary), Arc::clone(&t) as Arc<dyn RawTransport>, Arc::new(InFlight::default()));
    (canary, t, d)
}

#[tokio::test]
async fn the_send_record_is_on_disk_before_the_send_leaves() {
    let d = tempfile::tempdir().unwrap();
    let (_c, t, sender) = rig(d.path(), SendAnswer::Accepted(B256::ZERO));
    let tx = sender.send_raw(7, 3, b"raw-bytes", &ctx()).await.expect("accepted");
    assert_eq!(tx, keccak256(b"raw-bytes"));
    assert_eq!(*t.saw_record.lock().unwrap(), Some(true), "the transport saw the record already written");
}

#[tokio::test]
async fn a_refusal_is_rejected_counted_and_recorded() {
    let d = tempfile::tempdir().unwrap();
    let (c, _t, sender) = rig(d.path(), SendAnswer::Refused("-32605: flagged".into()));
    let err = sender.send_raw(7, 3, b"raw", &ctx()).await.expect_err("refused");
    assert!(matches!(err, DispatchError::Rejected { ref detail, .. } if detail.contains("-32605")), "{err:?}");
    assert_eq!(c.report().totals.refused, 1);
    assert_eq!(c.open_sends().len(), 0, "a refusal reached nothing");
}

#[tokio::test]
async fn silence_is_no_response_and_halts_as_unresolved() {
    let d = tempfile::tempdir().unwrap();
    let (c, _t, sender) = rig(d.path(), SendAnswer::Silent("timed out".into()));
    let err = sender.send_raw(7, 3, b"raw", &ctx()).await.expect_err("silent");
    assert!(matches!(err, DispatchError::NoResponse { .. }), "{err:?}");
    assert!(c.halted().is_some());
    assert_eq!(c.open_sends().len(), 1, "it may have landed: recovery looks for it");
}

/// Review focus 5: a hold that appears after the gate is honoured at the
/// sender, without counting as an endpoint's refusal and without a record.
#[tokio::test]
async fn a_hold_at_the_sender_is_not_an_endpoint_refusal() {
    let d = tempfile::tempdir().unwrap();
    let (c, t, sender) = rig(d.path(), SendAnswer::Accepted(B256::ZERO));
    std::fs::write(d.path().join("canary.stop"), b"").unwrap();
    let err = sender.send_raw(7, 3, b"raw", &ctx()).await.expect_err("held");
    assert!(matches!(err, DispatchError::Rejected { ref detail, .. } if detail.contains("stop file")), "{err:?}");
    assert_eq!(*t.saw_record.lock().unwrap(), None, "the transport was never called");
    assert_eq!(c.report().totals.refused, 0);
    assert_eq!(c.report().totals.sent, 0);
}

/// Review focus 1: a success naming another transaction is not an acceptance.
#[tokio::test]
async fn a_send_answered_with_another_hash_is_silent() {
    struct OtherHash;
    #[async_trait::async_trait]
    impl RawTransport for OtherHash {
        async fn send_raw(&self, _raw: &[u8]) -> SendAnswer {
            SendAnswer::Accepted(B256::repeat_byte(0x99))
        }
    }
    let d = tempfile::tempdir().unwrap();
    let canary = Arc::new(Canary::open(&d.path().join("l"), &d.path().join("s"), Limits { loss_cap_wei: 1, max_consecutive_reverts: 5, max_consecutive_refusals: 2, max_landed: 100 }).unwrap());
    let sender = CanaryDispatcher::new(Arc::clone(&canary), Arc::new(OtherHash), Arc::new(InFlight::default()));
    let err = sender.send_raw(1, 0, b"raw", &ctx()).await.expect_err("not ours");
    assert!(matches!(err, DispatchError::NoResponse { .. }), "{err:?}");
    let text = std::fs::read_to_string(d.path().join("l")).unwrap();
    assert!(text.lines().any(|l| matches!(serde_json::from_str::<Record>(l), Ok(Record::Outcome(o)) if matches!(o.settled, Settled::Silent { .. }))));
}
