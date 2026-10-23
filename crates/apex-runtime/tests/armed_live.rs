//! Task 8.5 R36 — the armed lane's live worker: an arm signed from its own
//! sender, written to the ledger before it is sent, bounded from the freshest
//! head, held inside the day's budget at its worst case, replaced only at
//! higher fees, and never sent while the lane is stopped. Then what came of
//! it: every pending nonce settled before the first send, a landing
//! accounted from its receipt, the loss cap and the tripwire written before
//! anything more is sent, and sends only while heads keep the configured
//! cadence.

use alloy_primitives::{keccak256, Address, B256};
use apex_chain::base::send::SendAnswer;
use apex_config::Secret;
use apex_runtime::armed::lines::{Arm, ArmedLine};
use apex_runtime::armed::live::{ArmedLive, LiveChain, LivePolicy, ValiditySend};
use apex_runtime::armed::live_ledger::{Halt, LiveLedger, LiveRecord};
use apex_runtime::armed::predicate::{expiry, Op, Predicate};
use apex_runtime::sign::LaneKey;
use ethers_core::types::transaction::eip2718::TypedTransaction;
use ethers_core::types::U256;
use ethers_core::utils::rlp::Rlp;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const ETH: u128 = 1_000_000_000_000_000_000;
const EXECUTOR: Address = Address::repeat_byte(0xe1);
const RECIPIENT: Address = Address::repeat_byte(0xe2);
const NOW: u64 = 1_792_886_400_000;

fn key(n: u8) -> LaneKey {
    LaneKey::from_hex(&Secret::new(format!("0x{}", hex_of(&[n; 32])))).unwrap()
}

fn hex_of(b: &[u8]) -> String {
    alloy_primitives::hex::encode(b)
}

struct Chain {
    head: Mutex<u64>,
    nonces: Mutex<BTreeMap<Address, u64>>,
    balance: Mutex<u128>,
    balance_asks: AtomicUsize,
    receipts: Mutex<BTreeMap<B256, Value>>,
    receipt_asks: AtomicUsize,
    receipts_fail: AtomicBool,
}

impl Chain {
    /// Head 100, 1 ETH each, every sender at `nonce`.
    fn new(nonce: u64) -> Arc<Self> {
        let nonces = (1..=16).map(|n| (key(n).address(), nonce)).collect();
        Arc::new(Self {
            head: Mutex::new(100),
            nonces: Mutex::new(nonces),
            balance: Mutex::new(ETH),
            balance_asks: AtomicUsize::new(0),
            receipts: Mutex::new(BTreeMap::new()),
            receipt_asks: AtomicUsize::new(0),
            receipts_fail: AtomicBool::new(false),
        })
    }
}

#[async_trait::async_trait]
impl LiveChain for Chain {
    async fn head(&self) -> Result<u64, String> {
        Ok(*self.head.lock().unwrap())
    }
    async fn nonce(&self, a: Address) -> Result<u64, String> {
        self.nonces.lock().unwrap().get(&a).copied().ok_or_else(|| "unknown".into())
    }
    async fn balance(&self, _: Address) -> Result<u128, String> {
        self.balance_asks.fetch_add(1, Ordering::Relaxed);
        Ok(*self.balance.lock().unwrap())
    }
    async fn receipt(&self, tx: B256) -> Result<Option<Value>, String> {
        self.receipt_asks.fetch_add(1, Ordering::Relaxed);
        if self.receipts_fail.load(Ordering::Relaxed) {
            return Err("502 Bad Gateway".into());
        }
        Ok(self.receipts.lock().unwrap().get(&tx).cloned())
    }
}

/// Answers from a script (then accepts), and keeps what was sent. With a
/// ledger path, checks the submission was on disk before the send.
#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<SendAnswer>>,
    sent: Mutex<Vec<(Vec<u8>, Value)>>,
    ledger: Option<PathBuf>,
    written_first: AtomicUsize,
}

#[async_trait::async_trait]
impl ValiditySend for Script {
    async fn send(&self, raw: &[u8], validity: &Value) -> SendAnswer {
        if let Some(p) = &self.ledger {
            let hash = format!("{:#x}", keccak256(raw));
            let text = std::fs::read_to_string(p).unwrap_or_default();
            if text.lines().any(|l| l.contains("\"submitting\"") && l.contains(&hash)) {
                self.written_first.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.sent.lock().unwrap().push((raw.to_vec(), validity.clone()));
        self.answers.lock().unwrap().pop_front().unwrap_or(SendAnswer::Accepted(keccak256(raw)))
    }
}

fn arm(arm_id: u64, slot: u16, bid: u128, gas_limit: u64) -> Arm {
    Arm {
        arm_id,
        at_ns: 0,
        armed_block: 99,
        to_block: 104,
        route_hash: "0x00".into(),
        trigger_leg: 0,
        rung: 1,
        step: 3,
        pools: [format!("{:#x}", Address::repeat_byte(0xa1)), format!("{:#x}", Address::repeat_byte(0xb2))],
        venues: ["UniswapV3".into(), "Slipstream".into()],
        predicates: vec![
            Predicate::Storage { address: "0xa1".into(), slot: "0x0".into(), mask: None, op: Op::AtLeast, value: "0x1".into() },
            expiry(104),
        ],
        start_words: Vec::new(),
        start_ticks: Vec::new(),
        size_wei: "1000000000000000000".into(),
        mid: "1".into(),
        output_wei: "1".into(),
        surplus_wei: "1".into(),
        cost_wei: "1".into(),
        gas_expected: gas_limit / 2,
        gas_limit,
        base_fee_wei: "0".into(),
        other_wei: "0".into(),
        deadline: 1,
        calldata: "0xabcd".into(),
        calldata_hash: "0x00".into(),
        sender_slot: slot,
        lender: "morpho".into(),
        bid_wei_per_gas: bid.to_string(),
        p_win: 0.5,
        renewals: Vec::new(),
    }
}

fn policy(budget_wei: u128, cap: u128) -> LivePolicy {
    LivePolicy {
        budget_wei,
        max_priority_wei_per_gas: cap,
        max_size_wei: U256::from(10u128 * ETH),
        l1_fee_bound_wei: 0,
        sender_reserve_wei: 0,
        window_blocks: 5,
        block_time_ms: 2_000,
        kill_file: PathBuf::from("/nonexistent/armed.disable"),
    }
}

struct Fixture {
    live: ArmedLive,
    chain: Arc<Chain>,
    script: Arc<Script>,
    ledger: PathBuf,
    _dir: tempfile::TempDir,
}

fn fixture(policy: LivePolicy, answers: Vec<SendAnswer>, records: Vec<LiveRecord>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("armed-live.jsonl");
    let (ledger, _) = LiveLedger::open(&path).unwrap();
    for r in &records {
        ledger.append(r).unwrap();
    }
    let chain = Chain::new(7);
    let script = Arc::new(Script { answers: Mutex::new(answers.into()), ledger: Some(path.clone()), ..Default::default() });
    let keys = (1..=16).map(key).collect();
    let live = ArmedLive::new(keys, chain.clone(), script.clone(), ledger, records, policy, EXECUTOR, RECIPIENT, 8453);
    Fixture { live, chain, script, ledger: path, _dir: dir }
}

/// Twenty head intervals of `every_ms`, the last at head `last`, seen at `at_ms`.
async fn heads(f: &Fixture, last: u64, at_ms: u64, every_ms: u64) {
    for head in last - 20..=last {
        let seen = at_ms - (last - head) * every_ms;
        f.live.on_head(head, seen, seen).await;
    }
}

/// Booted, with heads at the configured two seconds up to head 100.
async fn start(f: &Fixture) {
    f.live.begin(NOW).await.unwrap();
    heads(f, 100, NOW, 2_000).await;
}

fn records(path: &Path) -> Vec<LiveRecord> {
    LiveLedger::open(path).unwrap().1
}

fn word(v: u128) -> String {
    format!("0x{v:064x}")
}

fn topic(a: Address) -> String {
    format!("0x{:0>64}", alloy_primitives::hex::encode(a))
}

/// A Base receipt for `tx`: `paid` WETH from the executor to the recipient
/// and `Profit(gross)`, unless it reverted.
fn receipt(tx: B256, success: bool, gas_used: u64, price: u128, l1_fee: u128, paid: u128, gross: u128) -> Value {
    let weth: Address = "0x4200000000000000000000000000000000000006".parse().unwrap();
    let logs = if success {
        serde_json::json!([
            {
                "address": format!("{weth:#x}"),
                "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", topic(EXECUTOR), topic(RECIPIENT)],
                "data": word(paid),
            },
            {
                "address": format!("{EXECUTOR:#x}"),
                "topics": ["0x357d905f1831209797df4d55d79c5c5bf1d9f7311c976afd05e13d881eab9bc8"],
                "data": word(gross),
            },
        ])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "transactionHash": format!("{tx:#x}"),
        "blockNumber": "0x68",
        "status": if success { "0x1" } else { "0x0" },
        "gasUsed": format!("{gas_used:#x}"),
        "effectiveGasPrice": format!("{price:#x}"),
        "l1Fee": format!("{l1_fee:#x}"),
        "logs": logs,
    })
}

/// The submission a crash left open: signed and recorded, never answered.
fn open_submission(sender: Address, nonce: u64, tx: B256, to_block: u64) -> LiveRecord {
    LiveRecord::Submitting {
        arm_id: 9,
        pools: [Address::repeat_byte(0xa1), Address::repeat_byte(0xb2)],
        sender,
        nonce,
        tx,
        to_block,
        gas_limit: 600_000,
        max_fee_per_gas: 1_000_000,
        max_priority_fee_per_gas: 1_000_000,
        worst_case_wei: 600_000_000_000,
        at_ms: NOW - 60_000,
    }
}

fn decoded(raw: &[u8]) -> (TypedTransaction, Address) {
    let (tx, sig) = TypedTransaction::decode_signed(&Rlp::new(raw)).unwrap();
    let from = Address::from(sig.recover(tx.sighash()).unwrap().0);
    (tx, from)
}

fn fees(raw: &[u8]) -> (u128, u128, u64) {
    let (TypedTransaction::Eip1559(t), _) = decoded(raw) else { panic!("not EIP-1559") };
    (t.max_priority_fee_per_gas.unwrap().as_u128(), t.max_fee_per_gas.unwrap().as_u128(), t.nonce.unwrap().as_u64())
}

fn skipped(path: &Path) -> Vec<String> {
    let (_, records) = LiveLedger::open(path).unwrap();
    records.into_iter().filter_map(|r| if let LiveRecord::Skipped { reason, .. } = r { Some(reason) } else { None }).collect()
}

#[tokio::test]
async fn nothing_is_sent_before_ready() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    assert!(!f.live.ready());
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_arm_is_signed_from_its_slot_s_sender_with_a_bound_from_the_fresh_head() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    assert!(f.live.ready());
    let a = Arm { to_block: 110, ..arm(1, 2, 1_000_000, 600_000) };
    f.live.on_lines(&[ArmedLine::Arm(Box::new(a))], 5_000_000, NOW).await;
    let sent = f.script.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let (raw, validity) = &sent[0];
    let (tx, from) = decoded(raw);
    assert_eq!(from, key(3).address(), "slot 2 is the third key");
    let (prio, max_fee, nonce) = fees(raw);
    assert_eq!((prio, max_fee, nonce), (1_000_000, 2 * 5_000_000 + 1_000_000, 7));
    assert_eq!(tx.to().and_then(|t| t.as_address()).map(|a| Address::from(a.0)), Some(EXECUTOR));
    assert_eq!(tx.data().map(|d| d.to_vec()), Some(vec![0xab, 0xcd]));
    assert_eq!(validity[1], serde_json::to_value(expiry(105)).unwrap(), "head 100 + 5 blocks");
    assert_eq!(validity[0], serde_json::to_value(&arm(1, 2, 0, 0).predicates[0]).unwrap(), "the rest as armed");
}

/// The calldata's deadline covers the arm's window and no more: the
/// executor reverts past it. A head that moved on since arming shortens the
/// bound, never lengthens it.
#[tokio::test]
async fn the_bound_never_outlasts_the_arm_s_own_window() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    let sent = f.script.sent.lock().unwrap().clone();
    assert_eq!(sent[0].1[1], serde_json::to_value(expiry(104)).unwrap(), "armed at 99 to 104; head 100");
}

#[tokio::test]
async fn an_arm_whose_window_has_passed_is_not_sent() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    let a = Arm { to_block: 100, ..arm(1, 0, 1_000_000, 600_000) };
    f.live.on_lines(&[ArmedLine::Arm(Box::new(a))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty());
    assert_eq!(skipped(&f.ledger), vec!["expired".to_string()]);
}

#[tokio::test]
async fn the_submission_is_written_before_it_is_sent() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert_eq!(f.script.written_first.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn concurrent_worst_cases_never_exceed_the_budget() {
    // 1M gas at 3 gwei: 0.003 ETH each at worst; 0.01 ETH holds three.
    let f = fixture(policy(ETH / 100, u128::MAX), vec![], vec![]);
    start(&f).await;
    let lines: Vec<ArmedLine> = (0..16).map(|i| ArmedLine::Arm(Box::new(arm(i, i as u16, 3_000_000_000, 1_000_000)))).collect();
    f.live.on_lines(&lines, 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 3);
    assert_eq!(skipped(&f.ledger).iter().filter(|r| *r == "budget").count(), 13);
}

#[tokio::test]
async fn a_silent_answer_is_a_possible_send() {
    let f = fixture(policy(ETH / 200, u128::MAX), vec![SendAnswer::Silent("timeout".into())], vec![]);
    start(&f).await;
    let lines = [arm(1, 0, 3_000_000_000, 1_000_000), arm(2, 1, 3_000_000_000, 1_000_000)].map(|a| ArmedLine::Arm(Box::new(a)));
    f.live.on_lines(&lines, 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 1, "the silent one still holds its 0.003 of the 0.005");
    assert_eq!(skipped(&f.ledger), vec!["budget".to_string()]);
}

/// At most one transaction lands per nonce: a renewal holds only what it adds.
#[tokio::test]
async fn a_renewal_reserves_only_what_it_adds_to_its_nonce() {
    let f = fixture(policy(ETH / 200, u128::MAX), vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 3_000_000_000, 1_000_000)))], 0, NOW).await;
    let renew = ArmedLine::Renew { arm_id: 1, to_block: 110, at_ns: 0, deadline: Some(2), calldata: Some("0xabce".into()) };
    f.live.on_lines(&[renew], 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 2, "0.003 of the 0.005 either way");
    assert_eq!(f.live.state(NOW).reserved_wei, 3_000_000_000_000_000);
}

#[tokio::test]
async fn a_balance_is_read_at_most_once_a_minute() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    let renew = |calldata: &str| ArmedLine::Renew { arm_id: 1, to_block: 110, at_ns: 0, deadline: None, calldata: Some(calldata.into()) };
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    f.live.on_lines(&[renew("0xabce")], 0, NOW + 59_999).await;
    assert_eq!(f.chain.balance_asks.load(Ordering::Relaxed), 1);
    f.live.on_lines(&[renew("0xabcf")], 0, NOW + 60_000).await;
    assert_eq!(f.chain.balance_asks.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn a_refused_replacement_keeps_the_earlier_one() {
    let under = || SendAnswer::Refused("-32000: replacement transaction underpriced".into());
    let f = fixture(policy(ETH, u128::MAX), vec![SendAnswer::Accepted(B256::ZERO), under(), under(), under()], vec![]);
    start(&f).await;
    let a = arm(1, 0, 1_000_000, 600_000);
    f.live.on_lines(&[ArmedLine::Arm(Box::new(a))], 0, NOW).await;
    let renew = ArmedLine::Renew { arm_id: 1, to_block: 110, at_ns: 0, deadline: Some(2), calldata: Some("0xabce".into()) };
    f.live.on_lines(&[renew], 0, NOW).await;
    let sent = f.script.sent.lock().unwrap().clone();
    let prios: Vec<u128> = sent.iter().map(|(raw, _)| fees(raw).0).collect();
    assert_eq!(prios, vec![1_000_000, 1_000_000, 1_125_000, 1_265_625], "the renewal, then two bumps of an eighth");
    assert!(sent.iter().all(|(raw, _)| fees(raw).2 == 7), "one nonce throughout");
    assert_eq!(skipped(&f.ledger), vec!["underpriced".to_string()]);
    let first = keccak256(&sent[0].0);
    let pending = &f.live.state(NOW).pending[&(key(1).address(), 7)];
    assert!(pending.iter().any(|p| p.tx == first), "the accepted one is still tracked");
}

#[tokio::test]
async fn no_bump_goes_over_the_cap() {
    let under = SendAnswer::Refused("-32000: replacement transaction underpriced".into());
    let f = fixture(policy(ETH, 1_100_000), vec![under], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 1, "1,125,000 would cross the 1,100,000 cap");
    assert_eq!(skipped(&f.ledger), vec!["underpriced".to_string()]);
}

/// The arm's first transaction was taken; its renewal finds the nonce used.
#[tokio::test]
async fn a_used_nonce_is_a_landing_to_find_not_a_retry() {
    let used = SendAnswer::Refused("-32000: nonce too low".into());
    let f = fixture(policy(ETH, u128::MAX), vec![SendAnswer::Accepted(B256::ZERO), used], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    let first = keccak256(&f.script.sent.lock().unwrap()[0].0);
    f.chain.nonces.lock().unwrap().insert(key(1).address(), 8);
    f.chain.receipts.lock().unwrap().insert(first, receipt(first, true, 300_000, 1_000_000, 0, 1, 1));
    let renew = ArmedLine::Renew { arm_id: 1, to_block: 110, at_ns: 0, deadline: Some(2), calldata: Some("0xabce".into()) };
    f.live.on_lines(&[renew], 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 2, "no resend");
    assert_eq!(f.chain.receipt_asks.load(Ordering::Relaxed), 1, "the first transaction's landing is looked for");
    assert!(records(&f.ledger).iter().any(|r| matches!(r, LiveRecord::Landed { tx, .. } if *tx == first)));
}

/// No key for its slot, a field that does not parse, or nothing to wait for
/// but its bound (it would land in the next block, as plain transactions do).
#[tokio::test]
async fn an_arm_that_cannot_be_sent_as_armed_is_skipped() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    let unreadable = Arm { calldata: "0xzz".into(), ..arm(2, 1, 1_000_000, 600_000) };
    let unconditional = Arm { predicates: vec![expiry(104)], ..arm(3, 2, 1_000_000, 600_000) };
    let lines = [arm(1, 16, 1_000_000, 600_000), unreadable, unconditional].map(|a| ArmedLine::Arm(Box::new(a)));
    f.live.on_lines(&lines, 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty());
    assert_eq!(skipped(&f.ledger), ["slot", "unreadable", "unconditional"].map(String::from).to_vec());
}

#[tokio::test]
async fn an_arm_over_the_size_cap_is_skipped() {
    let f = fixture(LivePolicy { max_size_wei: U256::from(ETH - 1), ..policy(ETH, u128::MAX) }, vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty());
    assert_eq!(skipped(&f.ledger), vec!["size".to_string()]);
}

/// 700k gas at 1 gwei: 0.0007 ETH at worst, which the sender pays and still
/// keeps its reserve.
#[tokio::test]
async fn a_sender_that_cannot_pay_its_worst_case_and_keep_its_reserve_is_skipped() {
    let one = || [ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000_000, 700_000)))];
    for (balance, reserve, sends) in [(600_000_000_000_000, 0, 0), (700_000_000_000_000, 1, 0), (700_000_000_000_000, 0, 1)] {
        let f = fixture(LivePolicy { sender_reserve_wei: reserve, ..policy(ETH, u128::MAX) }, vec![], vec![]);
        *f.chain.balance.lock().unwrap() = balance;
        start(&f).await;
        f.live.on_lines(&one(), 0, NOW).await;
        assert_eq!(f.script.sent.lock().unwrap().len(), sends, "balance {balance}, reserve {reserve}");
        assert_eq!(skipped(&f.ledger).len(), 1 - sends);
    }
}

#[tokio::test]
async fn the_kill_file_and_a_halt_stop_new_submissions() {
    let dir = tempfile::tempdir().unwrap();
    let kill = dir.path().join("armed.disable");
    std::fs::write(&kill, "").unwrap();
    let f = fixture(LivePolicy { kill_file: kill, ..policy(ETH, u128::MAX) }, vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty(), "the kill file");

    let tripped = LiveRecord::Halted { reason: Halt::Tripwire { sender: key(1).address(), detail: "x".into() }, at_ms: NOW };
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![tripped]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty(), "the halt");
}

/// A crash left a submission unanswered at nonce 7, and it landed while the
/// worker was down.
#[tokio::test]
async fn boot_settles_every_pending_submission_before_any_send() {
    let sender = key(1).address();
    let open = B256::repeat_byte(0x11);
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![open_submission(sender, 7, open, 103)]);
    f.chain.nonces.lock().unwrap().insert(sender, 8);
    f.chain.receipts.lock().unwrap().insert(open, receipt(open, true, 300_000, 1_000_000, 0, 5, 5));
    f.live.begin(NOW).await.unwrap();
    heads(&f, 103, NOW, 2_000).await;
    assert!(!f.live.ready(), "head 103 is its bound, not past it");
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty(), "nothing before the boot is settled");

    f.live.on_head(104, NOW + 2_000, NOW + 2_000).await;
    assert!(f.live.ready());
    let landed: Vec<_> = records(&f.ledger).into_iter().filter(|r| matches!(r, LiveRecord::Landed { .. })).collect();
    assert!(matches!(&landed[..], [LiveRecord::Landed { tx, nonce: 7, arm_id: 9, .. }] if *tx == open), "{landed:?}");
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW + 2_000).await;
    let sent = f.script.sent.lock().unwrap().clone();
    assert_eq!(fees(&sent[0].0).2, 8, "the next nonce");
}

#[tokio::test]
async fn a_submission_that_expired_unseen_is_expired_not_lost() {
    let sender = key(1).address();
    let open = B256::repeat_byte(0x11);
    let accepted = LiveRecord::Answered { tx: open, answer: "accepted".into(), accepted: Some(true), at_ms: NOW - 59_000 };
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![open_submission(sender, 7, open, 103), accepted]);
    f.live.begin(NOW).await.unwrap();
    assert_eq!(f.live.state(NOW).reserved_wei, 600_000_000_000, "held until it is known not to have landed");
    heads(&f, 104, NOW, 2_000).await;
    assert!(f.live.ready());
    assert_eq!(f.live.state(NOW).reserved_wei, 0);
    assert!(records(&f.ledger).iter().any(|r| matches!(r, LiveRecord::Expired { nonce: 7, tx: Some(t), .. } if *t == open)));
    assert_eq!(f.chain.receipt_asks.load(Ordering::Relaxed), 0, "an unmoved nonce has no receipt to find");
}

#[tokio::test]
async fn a_landing_is_accounted_from_its_receipt() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000_000, 600_000)))], 0, NOW).await;
    let sent = keccak256(&f.script.sent.lock().unwrap()[0].0);
    f.chain.nonces.lock().unwrap().insert(key(1).address(), 8);
    // 300k gas at 1 gwei and 0.00005 ETH of L1 fee, against 0.002 ETH paid.
    let landed = receipt(sent, true, 300_000, 1_000_000_000, 50_000_000_000_000, 2_000_000_000_000_000, 2_100_000_000_000_000);
    f.chain.receipts.lock().unwrap().insert(sent, landed);
    f.live.on_head(105, NOW + 10_000, NOW + 10_000).await;
    let landed: Vec<_> = records(&f.ledger).into_iter().filter(|r| matches!(r, LiveRecord::Landed { .. })).collect();
    let [LiveRecord::Landed { arm_id, pools, sender, nonce, tx, gross_wei, paid_wei, cost_wei, net_wei, .. }] = &landed[..] else {
        panic!("{landed:?}")
    };
    assert_eq!((*arm_id, *sender, *nonce, *tx), (1, key(1).address(), 7, sent));
    assert_eq!(*pools, [Address::repeat_byte(0xa1), Address::repeat_byte(0xb2)]);
    assert_eq!((*gross_wei, *paid_wei, *cost_wei), (2_100_000_000_000_000, 2_000_000_000_000_000, 350_000_000_000_000));
    assert_eq!(*net_wei, 1_650_000_000_000_000, "paid less cost");
    assert_eq!(f.live.state(NOW + 10_000).day_net, 1_650_000_000_000_000);

    let asked = f.chain.balance_asks.load(Ordering::Relaxed);
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(2, 0, 1_000_000, 600_000)))], 0, NOW + 10_000).await;
    assert_eq!(f.chain.balance_asks.load(Ordering::Relaxed), asked + 1, "a landing spent from the balance: read it again");
    let next = f.script.sent.lock().unwrap()[1].0.clone();
    assert_eq!(fees(&next).2, 8, "the landing used nonce 7");
}

#[tokio::test]
async fn a_nonce_that_moved_without_our_transaction_trips_everything() {
    for (nonce, with_receipt) in [(9, true), (8, false)] {
        let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
        start(&f).await;
        f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
        let sent = keccak256(&f.script.sent.lock().unwrap()[0].0);
        f.chain.nonces.lock().unwrap().insert(key(1).address(), nonce);
        if with_receipt {
            f.chain.receipts.lock().unwrap().insert(sent, receipt(sent, true, 300_000, 1_000_000, 0, 1, 1));
        }
        f.live.on_head(105, NOW + 10_000, NOW + 10_000).await;
        let halted = f.live.state(NOW + 10_000).halted;
        assert!(matches!(halted, Some(Halt::Tripwire { sender, .. }) if sender == key(1).address()), "nonce {nonce}: {halted:?}");
        assert!(records(&f.ledger).iter().any(|r| matches!(r, LiveRecord::Halted { reason: Halt::Tripwire { .. }, .. })));
        f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(2, 1, 1_000_000, 600_000)))], 0, NOW + 10_000).await;
        assert_eq!(f.script.sent.lock().unwrap().len(), 1, "nonce {nonce}: nothing after");
    }
}

#[tokio::test]
async fn the_loss_cap_halts_before_the_next_send() {
    let f = fixture(policy(ETH / 100, u128::MAX), vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 3_000_000_000, 1_000_000)))], 0, NOW).await;
    let sent = keccak256(&f.script.sent.lock().unwrap()[0].0);
    f.chain.nonces.lock().unwrap().insert(key(1).address(), 8);
    // Reverted: 1M gas at 11 gwei, 0.011 ETH against a 0.01 ETH budget.
    f.chain.receipts.lock().unwrap().insert(sent, receipt(sent, false, 1_000_000, 11_000_000_000, 0, 0, 0));
    f.live.on_head(105, NOW + 10_000, NOW + 10_000).await;
    let all = records(&f.ledger);
    let landed = all.iter().position(|r| matches!(r, LiveRecord::Landed { net_wei: -11_000_000_000_000_000, .. }));
    let capped = all.iter().position(|r| matches!(r, LiveRecord::Halted { reason: Halt::LossCap { .. }, .. }));
    assert!(landed.is_some() && capped > landed, "{all:?}");
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(2, 1, 1_000_000, 600_000)))], 0, NOW + 10_000).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 1, "nothing signed after the cap");
    assert!(skipped(&f.ledger).is_empty(), "stopped by the halt, before the budget is asked");
}

#[tokio::test]
async fn a_cadence_off_the_configuration_stops_sends() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    f.live.begin(NOW).await.unwrap();
    heads(&f, 100, NOW, 200).await;
    assert_eq!(f.live.cadence_ms(), Some(200));
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty(), "heads ten times too fast");

    heads(&f, 120, NOW + 40_000, 2_000).await;
    assert_eq!(f.live.cadence_ms(), Some(2_000));
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW + 40_000).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 1, "back to two seconds");
}

/// The ledger left the sender at nonce 7, with nothing pending: a nonce of 8
/// at boot is a transaction it never wrote.
#[tokio::test]
async fn a_boot_trips_on_a_nonce_the_ledger_did_not_leave() {
    let sender = key(1).address();
    let open = B256::repeat_byte(0x11);
    let expired = LiveRecord::Expired { sender, nonce: 7, tx: Some(open), at_ms: NOW - 50_000 };
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![open_submission(sender, 7, open, 103), expired.clone()]);
    f.chain.nonces.lock().unwrap().insert(sender, 8);
    f.live.begin(NOW).await.unwrap();
    assert!(matches!(f.live.state(NOW).halted, Some(Halt::Tripwire { sender: s, .. }) if s == sender));

    let g = fixture(policy(ETH, u128::MAX), vec![], vec![open_submission(sender, 7, open, 103), expired]);
    g.live.begin(NOW).await.unwrap();
    assert_eq!(g.live.state(NOW).halted, None, "still at 7, where it was left");
}

/// The renewal went out while the arm's first transaction was still in its
/// window, at the same nonce. Once the head passes the first one's bound, it
/// goes alone: the renewal may still land.
#[tokio::test]
async fn a_renewal_s_earlier_transaction_expires_alone() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    let renew = ArmedLine::Renew { arm_id: 1, to_block: 105, at_ns: 0, deadline: Some(2), calldata: Some("0xabce".into()) };
    f.live.on_lines(&[renew], 0, NOW).await;
    let sent: Vec<B256> = f.script.sent.lock().unwrap().iter().map(|(raw, _)| keccak256(raw)).collect();
    f.live.on_head(105, NOW + 10_000, NOW + 10_000).await;
    let left: Vec<B256> = f.live.state(NOW + 10_000).pending[&(key(1).address(), 7)].iter().map(|p| p.tx).collect();
    assert_eq!(left, vec![sent[1]], "104 is past at head 105; 105 is not");
}

#[tokio::test]
async fn a_receipt_read_that_fails_is_asked_again_not_tripped() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    start(&f).await;
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    let sent = keccak256(&f.script.sent.lock().unwrap()[0].0);
    f.chain.nonces.lock().unwrap().insert(key(1).address(), 8);
    f.chain.receipts.lock().unwrap().insert(sent, receipt(sent, true, 300_000, 1_000_000, 0, 1, 1));
    f.chain.receipts_fail.store(true, Ordering::Relaxed);
    f.live.on_head(105, NOW + 10_000, NOW + 10_000).await;
    assert_eq!(f.live.state(NOW + 10_000).halted, None);
    f.chain.receipts_fail.store(false, Ordering::Relaxed);
    f.live.on_head(106, NOW + 12_000, NOW + 12_000).await;
    assert!(records(&f.ledger).iter().any(|r| matches!(r, LiveRecord::Landed { tx, .. } if *tx == sent)));
}

/// Nothing of ours was pending, so whatever used the nonce was not ours.
#[tokio::test]
async fn a_used_nonce_with_nothing_of_ours_pending_trips() {
    let f = fixture(policy(ETH, u128::MAX), vec![SendAnswer::Refused("-32000: nonce too low".into())], vec![]);
    start(&f).await;
    f.chain.nonces.lock().unwrap().insert(key(1).address(), 8);
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(matches!(f.live.state(NOW).halted, Some(Halt::Tripwire { .. })));
}
