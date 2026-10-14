//! Task 8.5 R28: settlement and reconciliation (`apex_runtime::canary::chain`),
//! over receipts built to Base's format and the executor's events.

use alloy_primitives::{address, Address, B256, U256};
use apex_chain::adapter::AdapterError;
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_runtime::canary::chain::{read_receipt, CanaryChain, ChainSettings};
use apex_runtime::canary::ledger::Sent;
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::canary::send::InFlight;
use apex_runtime::live::adapter::Attribution;
use apex_runtime::plane::SettlementFeed;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod support;

const WETH: Address = address!("4200000000000000000000000000000000000006");
const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");
const OWNER: Address = address!("69D54e5fC0b9325D7250f0D0A11690327A3dd8A3");
const TRADER: Address = address!("CB436Ba3acb945b3fc8EE6345857262584356595");
const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const REALISED: &str = "0x230bad4a4399a7f96d7dc55ada0f3e590e98017ef0fb0ef2ae55914350e5d832";

fn word(v: u128) -> String {
    format!("{:064x}", v)
}
fn topic_addr(a: Address) -> String {
    format!("0x{:0>64}", alloy_primitives::hex::encode(a))
}

/// A Base receipt for `tx`: the executor pays `fee` and `rest` WETH to the owner
/// and says it realised `realised`.
fn receipt(tx: B256, ok: bool, fee: u128, rest: u128, realised: u128) -> Value {
    let transfer = |amount: u128| json!({
        "address": format!("{WETH:#x}"),
        "topics": [TRANSFER, topic_addr(EXECUTOR), topic_addr(OWNER)],
        "data": format!("0x{}", word(amount)),
    });
    let logs = if ok {
        vec![
            transfer(fee),
            transfer(rest),
            json!({ "address": format!("{EXECUTOR:#x}"), "topics": [REALISED, topic_addr(WETH)], "data": format!("0x{}{}", word(realised), word(fee)) }),
        ]
    } else {
        vec![]
    };
    json!({
        "transactionHash": format!("{tx:#x}"),
        "blockNumber": "0x3200000",
        "status": if ok { "0x1" } else { "0x0" },
        "gasUsed": "0x61a80",            // 400,000
        "effectiveGasPrice": "0x5f5e100", // 0.1 gwei
        "l1Fee": "0x3b9aca00",            // 1 gwei of wei
        "logs": logs,
    })
}

#[test]
fn a_landed_trade_reads_its_gross_from_the_transfers() {
    let tx = B256::repeat_byte(1);
    let r = read_receipt(&receipt(tx, true, 30, 970, 1_000), WETH, EXECUTOR, &[OWNER]).unwrap();
    assert!(r.receipt.success);
    assert_eq!((r.paid_wei, r.realised_wei), (1_000, Some(1_000)));
    assert_eq!(r.receipt.gas_used, 400_000);
    assert_eq!(r.receipt.l1_fee_wei, 1_000_000_000);
}

/// Review focus 2: no `l1Fee`, no reconciliation. A zero would understate the
/// cost of every trade.
#[test]
fn a_receipt_without_an_l1_fee_is_not_reconciled() {
    let mut v = receipt(B256::repeat_byte(1), true, 30, 970, 1_000);
    v.as_object_mut().unwrap().remove("l1Fee");
    assert!(read_receipt(&v, WETH, EXECUTOR, &[OWNER]).is_err());
}

/// Transfers to someone else are not our gross.
#[test]
fn only_transfers_to_the_recipients_count() {
    let r = read_receipt(&receipt(B256::repeat_byte(1), true, 30, 970, 1_000), WETH, EXECUTOR, &[TRADER]).unwrap();
    assert_eq!(r.paid_wei, 0);
}

/// Only the executor's transfers are our gross: WETH reaching the owner from
/// anyone else in the same transaction is not the trade's profit.
#[test]
fn only_the_executors_transfers_count() {
    let mut v = receipt(B256::repeat_byte(1), true, 30, 970, 1_000);
    v["logs"].as_array_mut().unwrap().push(json!({
        "address": format!("{WETH:#x}"),
        "topics": [TRANSFER, topic_addr(TRADER), topic_addr(OWNER)],
        "data": format!("0x{}", word(5_000)),
    }));
    let r = read_receipt(&v, WETH, EXECUTOR, &[OWNER]).unwrap();
    assert_eq!(r.paid_wei, 1_000, "the trader's 5,000 is not the executor's");
}

/// An `Error(string)` revert's data, as a node's trace returns it.
fn error_string(msg: &str) -> String {
    let mut data = msg.as_bytes().to_vec();
    data.resize(data.len().div_ceil(32) * 32, 0);
    format!("0x08c379a0{:064x}{:064x}{}", 32, msg.len(), alloy_primitives::hex::encode(data))
}

/// Answers receipts, traces and nonces from a script; counts receipt polls.
struct Node {
    receipt: Mutex<Option<Value>>,
    latest_nonce: u64,
    polls: Mutex<u32>,
}

#[async_trait::async_trait]
impl RpcTransport for Node {
    async fn call(&self, method: &str, _params: Value) -> Result<Value, RpcError> {
        match method {
            "eth_getTransactionReceipt" => {
                *self.polls.lock().unwrap() += 1;
                Ok(self.receipt.lock().unwrap().clone().unwrap_or(Value::Null))
            }
            "eth_getTransactionCount" => Ok(json!(format!("{:#x}", self.latest_nonce))),
            // The executor's MinOutNotMet: Uniswap's "Too little received".
            "debug_traceTransaction" => Ok(json!({ "error": "execution reverted", "output": error_string("Too little received") })),
            other => panic!("unexpected {other}"),
        }
    }
}

fn sent(tx: B256, nonce: u64) -> Sent {
    Sent {
        ticket: 9, nonce, tx, at_ms: 1, chain: ChainId::BASE, strategy: StrategyId(1),
        route_hash: B256::repeat_byte(0x44), venues: vec![VenueId(1)], gas_limit: 500_000,
        max_fee_per_gas: 200_000_000, max_priority_fee_per_gas: 0,
        expected_gross_wei: U256::from(1_000u64), expected_net_wei: 1,
        waived: vec![],
    }
}

fn rig(dir: &std::path::Path, node: Node) -> (Arc<Canary>, Arc<InFlight>, CanaryChain) {
    let canary = Arc::new(Canary::open(&dir.join("l"), &dir.join("s"), Limits { loss_cap_wei: u128::MAX, max_consecutive_reverts: 5, max_consecutive_refusals: 2, max_landed: 100 }).unwrap());
    let in_flight = Arc::new(InFlight::default());
    let chain = CanaryChain::new(
        Arc::new(node),
        Arc::clone(&canary),
        Arc::clone(&in_flight),
        ChainSettings { weth: WETH, executor: EXECUTOR, trader: TRADER, recipients: vec![OWNER], poll: Duration::from_millis(5), timeout: Duration::from_millis(50) },
    );
    (canary, in_flight, chain)
}

#[tokio::test]
async fn a_landed_trade_settles_reconciles_and_is_recorded() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(1);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, true, 30_000_000_000, 970_000_000_000, 1_000_000_000_000))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    let seen = chain.observe(tx).await.unwrap();
    assert!(seen.iter().any(|o| o.stage == apex_types::ack::LifecycleStage::Included));
    let pnl = chain.attribute(tx).await.expect("reconciles");
    // Cost: 400,000 gas at 0.1 gwei, plus the 1 gwei L1 fee.
    let cost = 400_000u128 * 100_000_000 + 1_000_000_000;
    assert_eq!(pnl.gross_profit, 1_000_000_000_000);
    assert_eq!(pnl.net_profit_token, 1_000_000_000_000 - i128::try_from(cost).unwrap());
    assert_eq!(canary.report().totals.landed, 1);
    assert_eq!(canary.open_sends().len(), 0);
}

#[tokio::test]
async fn a_revert_is_classified_recorded_and_returned_as_a_revert() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(2);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, false, 0, 0, 0))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    chain.observe(tx).await.unwrap();
    let err = chain.attribute(tx).await.expect_err("reverted");
    let AdapterError::Reverted { class, gas_used, .. } = err else { panic!("{err:?}") };
    assert_eq!(class, apex_types::sim::RevertClass::MinOutNotMet);
    assert_eq!(gas_used, 400_000);
    let t = canary.report().totals;
    assert_eq!((t.reverted_late, t.reverted_early), (1, 0), "400,000 gas is a late revert");
}

#[tokio::test]
async fn transfers_disagreeing_with_the_event_halt_as_unexplained() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(3);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, true, 30, 970, 999))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    chain.observe(tx).await.unwrap();
    assert!(chain.attribute(tx).await.is_err());
    assert!(matches!(canary.halted(), Some(apex_runtime::canary::Halt::Unexplained { .. })));
}

/// Final review: a transaction that landed and whose receipt cannot be read is
/// money the canary cannot account for. It halts as unexplained at settlement,
/// rather than closing the ticket with nothing recorded and trading on.
#[tokio::test]
async fn a_landed_receipt_that_cannot_be_read_halts_as_unexplained() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(5);
    let mut unreadable = receipt(tx, true, 30, 970, 1_000);
    unreadable.as_object_mut().unwrap().remove("l1Fee");
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(unreadable)), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    assert!(chain.observe(tx).await.is_err(), "no observation built from an unreadable receipt");
    assert!(matches!(canary.halted(), Some(apex_runtime::canary::Halt::Unexplained { .. })), "{:?}", canary.halted());
}

#[tokio::test]
async fn no_receipt_by_the_timeout_halts_as_unresolved() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(4);
    let (canary, in_flight, chain) = rig(d.path(), Node { receipt: Mutex::new(None), latest_nonce: 0, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    in_flight.insert(sent(tx, 0));
    let seen = chain.observe(tx).await.unwrap();
    assert!(seen.is_empty(), "not included");
    assert!(matches!(canary.halted(), Some(apex_runtime::canary::Halt::Unresolved { .. })));
}

use apex_capture::recover::ChainOutcomeSource;
use apex_runtime::canary::recover::CanaryRecovery;
use apex_types::ticket::{TerminalFailure, TicketOutcome, TicketStatus};

fn open_ticket(id: u64) -> apex_types::ticket::OpportunityTicket {
    let mut t = support::ticket_at(TicketStatus::Dispatching);
    t.ticket_id = apex_types::ids::TicketId(id);
    t.status = TicketStatus::Dispatching;
    t
}

#[tokio::test]
async fn a_ticket_with_no_send_record_never_left_the_process() {
    let d = tempfile::tempdir().unwrap();
    let (canary, _f, chain) = rig(d.path(), Node { receipt: Mutex::new(None), latest_nonce: 0, polls: Mutex::new(0) });
    let rec = CanaryRecovery::build(&canary, &chain).await;
    let out = rec.resolve(&open_ticket(5)).unwrap();
    let TicketOutcome::ExplicitFailure { code: TerminalFailure::Abandoned { .. }, cause, .. } = out else { panic!() };
    assert!(cause.contains("no send record"), "{cause}");
    assert_eq!(canary.halted(), None);
}

#[tokio::test]
async fn a_sent_ticket_with_a_receipt_is_reconciled() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(6);
    let (canary, _f, chain) = rig(d.path(), Node { receipt: Mutex::new(Some(receipt(tx, true, 30_000_000_000, 970_000_000_000, 1_000_000_000_000))), latest_nonce: 1, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap(); // ticket 9
    let rec = CanaryRecovery::build(&canary, &chain).await;
    assert!(matches!(rec.resolve(&open_ticket(9)).unwrap(), TicketOutcome::Success { stage: TicketStatus::Reconciled, .. }));
    assert_eq!(canary.report().totals.landed, 1);
}

#[tokio::test]
async fn a_sent_ticket_with_no_receipt_is_abandoned_and_halts() {
    let d = tempfile::tempdir().unwrap();
    let tx = B256::repeat_byte(7);
    let (canary, _f, chain) = rig(d.path(), Node { receipt: Mutex::new(None), latest_nonce: 0, polls: Mutex::new(0) });
    canary.sent(sent(tx, 0)).unwrap();
    let rec = CanaryRecovery::build(&canary, &chain).await;
    let TicketOutcome::ExplicitFailure { cause, .. } = rec.resolve(&open_ticket(9)).unwrap() else { panic!() };
    assert!(cause.contains("unresolved at boot"), "{cause}");
    assert!(matches!(canary.halted(), Some(apex_runtime::canary::Halt::Unresolved { .. })));
}
