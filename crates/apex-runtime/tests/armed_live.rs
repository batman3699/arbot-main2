//! Task 8.5 R36 — the armed lane's live worker: an arm signed from its own
//! sender, written to the ledger before it is sent, bounded from the freshest
//! head, held inside the day's budget at its worst case, replaced only at
//! higher fees, and never sent while the lane is stopped.

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
use std::sync::atomic::{AtomicUsize, Ordering};
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
    receipt_asks: AtomicUsize,
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
            receipt_asks: AtomicUsize::new(0),
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
    async fn receipt(&self, _: B256) -> Result<Option<Value>, String> {
        self.receipt_asks.fetch_add(1, Ordering::Relaxed);
        Ok(None)
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
    f.live.begin(NOW).await.unwrap();
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
    f.live.begin(NOW).await.unwrap();
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    let sent = f.script.sent.lock().unwrap().clone();
    assert_eq!(sent[0].1[1], serde_json::to_value(expiry(104)).unwrap(), "armed at 99 to 104; head 100");
}

#[tokio::test]
async fn an_arm_whose_window_has_passed_is_not_sent() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    f.live.begin(NOW).await.unwrap();
    let a = Arm { to_block: 100, ..arm(1, 0, 1_000_000, 600_000) };
    f.live.on_lines(&[ArmedLine::Arm(Box::new(a))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty());
    assert_eq!(skipped(&f.ledger), vec!["expired".to_string()]);
}

#[tokio::test]
async fn the_submission_is_written_before_it_is_sent() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    f.live.begin(NOW).await.unwrap();
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert_eq!(f.script.written_first.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn concurrent_worst_cases_never_exceed_the_budget() {
    // 1M gas at 3 gwei: 0.003 ETH each at worst; 0.01 ETH holds three.
    let f = fixture(policy(ETH / 100, u128::MAX), vec![], vec![]);
    f.live.begin(NOW).await.unwrap();
    let lines: Vec<ArmedLine> = (0..16).map(|i| ArmedLine::Arm(Box::new(arm(i, i as u16, 3_000_000_000, 1_000_000)))).collect();
    f.live.on_lines(&lines, 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 3);
    assert_eq!(skipped(&f.ledger).iter().filter(|r| *r == "budget").count(), 13);
}

#[tokio::test]
async fn a_silent_answer_is_a_possible_send() {
    let f = fixture(policy(ETH / 200, u128::MAX), vec![SendAnswer::Silent("timeout".into())], vec![]);
    f.live.begin(NOW).await.unwrap();
    let lines = [arm(1, 0, 3_000_000_000, 1_000_000), arm(2, 1, 3_000_000_000, 1_000_000)].map(|a| ArmedLine::Arm(Box::new(a)));
    f.live.on_lines(&lines, 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 1, "the silent one still holds its 0.003 of the 0.005");
    assert_eq!(skipped(&f.ledger), vec!["budget".to_string()]);
}

/// At most one transaction lands per nonce: a renewal holds only what it adds.
#[tokio::test]
async fn a_renewal_reserves_only_what_it_adds_to_its_nonce() {
    let f = fixture(policy(ETH / 200, u128::MAX), vec![], vec![]);
    f.live.begin(NOW).await.unwrap();
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 3_000_000_000, 1_000_000)))], 0, NOW).await;
    let renew = ArmedLine::Renew { arm_id: 1, to_block: 110, at_ns: 0, deadline: Some(2), calldata: Some("0xabce".into()) };
    f.live.on_lines(&[renew], 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 2, "0.003 of the 0.005 either way");
    assert_eq!(f.live.state(NOW).reserved_wei, 3_000_000_000_000_000);
}

#[tokio::test]
async fn a_balance_is_read_at_most_once_a_minute() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    f.live.begin(NOW).await.unwrap();
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
    f.live.begin(NOW).await.unwrap();
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
    f.live.begin(NOW).await.unwrap();
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 1, "1,125,000 would cross the 1,100,000 cap");
    assert_eq!(skipped(&f.ledger), vec!["underpriced".to_string()]);
}

/// The arm's first transaction was taken; its renewal finds the nonce used.
#[tokio::test]
async fn a_used_nonce_is_a_landing_to_find_not_a_retry() {
    let used = SendAnswer::Refused("-32000: nonce too low".into());
    let f = fixture(policy(ETH, u128::MAX), vec![SendAnswer::Accepted(B256::ZERO), used], vec![]);
    f.live.begin(NOW).await.unwrap();
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    let renew = ArmedLine::Renew { arm_id: 1, to_block: 110, at_ns: 0, deadline: Some(2), calldata: Some("0xabce".into()) };
    f.live.on_lines(&[renew], 0, NOW).await;
    assert_eq!(f.script.sent.lock().unwrap().len(), 2, "no resend");
    assert_eq!(f.chain.receipt_asks.load(Ordering::Relaxed), 1, "the first transaction's landing is looked for");
}

/// No key for its slot, a field that does not parse, or nothing to wait for
/// but its bound (it would land in the next block, as plain transactions do).
#[tokio::test]
async fn an_arm_that_cannot_be_sent_as_armed_is_skipped() {
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![]);
    f.live.begin(NOW).await.unwrap();
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
    f.live.begin(NOW).await.unwrap();
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
        f.live.begin(NOW).await.unwrap();
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
    f.live.begin(NOW).await.unwrap();
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty(), "the kill file");

    let tripped = LiveRecord::Halted { reason: Halt::Tripwire { sender: key(1).address(), detail: "x".into() }, at_ms: NOW };
    let f = fixture(policy(ETH, u128::MAX), vec![], vec![tripped]);
    f.live.begin(NOW).await.unwrap();
    f.live.on_lines(&[ArmedLine::Arm(Box::new(arm(1, 0, 1_000_000, 600_000)))], 0, NOW).await;
    assert!(f.script.sent.lock().unwrap().is_empty(), "the halt");
}
