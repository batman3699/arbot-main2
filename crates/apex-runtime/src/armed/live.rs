//! The armed lane's live worker (Task 8.5 R36): each arm the engine writes
//! becomes a validity transaction from its slot's own sender.
//!
//! **Before a send.** The worker checks five things:
//! - the lane is ready, and heads keep the configured cadence;
//! - the owner's stops, read on the last head: the executor's circuit is
//!   closed and still lets the sender trade;
//! - no kill file exists;
//! - the ledger holds no halt;
//! - the arm's worst case fits both the sender's balance and the day's
//!   budget, with everything still pending counted at its own worst case.
//!
//! **Write-ahead.** The submission is synced to the ledger before a byte is
//! sent. An answer is the endpoint's word on that one hash: an acceptance or
//! a silence leaves it pending, and a refusal ends it alone.
//!
//! **Replacement.** A refused replacement is re-signed at an eighth more, at
//! most twice and never over the cap, and the transaction it would have
//! replaced stays tracked. A refusal for a used nonce is a landing to look
//! for, not a reason to send again.
//!
//! **The block bound.** It is taken from the freshest head at submission, and
//! never runs past the arm's own window: the calldata's deadline covers that
//! window and no more, and past it the executor reverts.
//!
//! **After a send.** Once a head passes a pending transaction's bound, the
//! sender's nonce says what came of it:
//! - unmoved, it can no longer land, and expires;
//! - one past, one of ours landed, and its receipt is the account;
//! - anything else trips the lane until an operator resets it.
//!
//! A landing that spends the day's budget halts the lane for the day. The
//! boot sends nothing until the reopened ledger's pending transactions are
//! settled this way.

use super::lines::{Arm, ArmedLine};
use super::live_ledger::{nonce_views, Halt, LiveLedger, LiveRecord, LiveState, PendingTx};
use super::predicate::{self, Op, Predicate};
use crate::canary::chain::read_receipt;
use crate::live::frontier::WETH;
use crate::sign::{Eip1559Tx, LaneKey};
use alloy_primitives::{Address, B256};
use apex_chain::base::send::SendAnswer;
use apex_chain::base::validity::ValiditySubmitter;
use ethers_core::types::U256;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tracing::{error, info, warn};

/// How long a sender's balance is trusted without a landing in between.
const BALANCE_TTL_MS: u64 = 60_000;
/// Re-signings after a refused replacement, each at an eighth more.
const MAX_BUMPS: usize = 2;
/// Head intervals the cadence guard takes the median of.
const CADENCE_SAMPLES: usize = 20;
const DAY_MS: u64 = 86_400_000;

#[async_trait::async_trait]
pub trait ValiditySend: Send + Sync {
    async fn send(&self, raw: &[u8], validity: &Value) -> SendAnswer;
}

#[async_trait::async_trait]
impl ValiditySend for ValiditySubmitter {
    async fn send(&self, raw: &[u8], validity: &Value) -> SendAnswer {
        Self::send(self, raw, validity).await
    }
}

/// What the worker reads from the chain.
#[async_trait::async_trait]
pub trait LiveChain: Send + Sync {
    async fn head(&self) -> Result<u64, String>;
    async fn nonce(&self, a: Address) -> Result<u64, String>;
    /// The nonce as of `block`'s end. A node without that block errs, which
    /// is what settlement wants from one behind the head.
    async fn nonce_at(&self, a: Address, block: u64) -> Result<u64, String>;
    async fn balance(&self, a: Address) -> Result<u128, String>;
    async fn receipt(&self, tx: B256) -> Result<Option<Value>, String>;
    /// The owner's stops on `executor` as of `block`, one answer per sender.
    async fn stops(&self, executor: Address, senders: &[Address], block: u64) -> Result<Stops, String>;
}

/// The owner's two stops on the executor: its circuit, and which senders it
/// lets trade (in the order asked).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stops {
    pub circuit_open: bool,
    pub allowed: Vec<bool>,
}

/// The caps the worker holds every submission to, all in wei.
#[derive(Clone, Debug)]
pub struct LivePolicy {
    /// The day's loss budget: realised losses and pending worst cases share it.
    pub budget_wei: u128,
    pub max_priority_wei_per_gas: u128,
    pub max_size_wei: U256,
    /// R37: the largest USDC arm sent, in USDC's units.
    pub max_size_usdc: U256,
    /// A bound on one transaction's L1 data fee.
    pub l1_fee_bound_wei: u128,
    /// Left in every sender after its worst case.
    pub sender_reserve_wei: u128,
    pub window_blocks: u64,
    /// The block time windows are counted in. Sends stop while heads arrive
    /// more than half of it faster or slower.
    pub block_time_ms: u64,
    /// While this file exists, nothing new is sent.
    pub kill_file: PathBuf,
}

pub struct ArmedLive {
    keys: Vec<LaneKey>,
    chain: Arc<dyn LiveChain>,
    submit: Arc<dyn ValiditySend>,
    ledger: LiveLedger,
    policy: LivePolicy,
    executor: Address,
    recipient: Address,
    chain_id: u64,
    ready: AtomicBool,
    /// One submission or settlement at a time: each is judged against every
    /// record made before it.
    turn: tokio::sync::Mutex<()>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The reopened ledger's records, until `begin` replays them.
    boot: Vec<LiveRecord>,
    /// `begin` has replayed the ledger and read every nonce.
    begun: bool,
    state: LiveState,
    /// Each sender's next nonce, as the chain last said.
    nonces: BTreeMap<Address, u64>,
    /// Each sender's balance as last read.
    balances: BTreeMap<Address, Balance>,
    /// Each slot's arm, as last armed or renewed.
    arms: BTreeMap<u16, Arm>,
    /// The owner's stops, and when they were read.
    stops: Option<(Stops, u64)>,
    /// The last head and when it was seen, and the per-block intervals before it.
    last_head: Option<(u64, u64)>,
    intervals: VecDeque<u64>,
    /// What this process has recorded since boot.
    counts: Counts,
    /// Each landing's cost, by sender, over the last day.
    spends: VecDeque<(u64, Address, u128)>,
}

struct Balance {
    wei: u128,
    read_ms: u64,
    /// A landing has spent from it since.
    stale: bool,
}

#[derive(Default)]
struct Counts {
    submitted: u64,
    accepted: u64,
    refused: u64,
    silent: u64,
    skipped: BTreeMap<String, u64>,
    landed: u64,
    won: u64,
    gross_wei: u128,
    cost_wei: u128,
}

impl Inner {
    fn count(&mut self, record: &LiveRecord) {
        let c = &mut self.counts;
        match record {
            LiveRecord::Submitting { .. } => c.submitted += 1,
            LiveRecord::Answered { accepted: Some(true), .. } => c.accepted += 1,
            LiveRecord::Answered { accepted: Some(false), .. } => c.refused += 1,
            LiveRecord::Answered { accepted: None, .. } => c.silent += 1,
            LiveRecord::Skipped { reason, .. } => *c.skipped.entry(reason.clone()).or_default() += 1,
            LiveRecord::Landed { sender, gross_wei, paid_wei, cost_wei, at_ms, paid_start, .. } => {
                c.landed += 1;
                c.won += u64::from(*paid_wei > 0 || paid_start.as_deref().is_some_and(|p| p != "0"));
                c.gross_wei = c.gross_wei.saturating_add(*gross_wei);
                c.cost_wei = c.cost_wei.saturating_add(*cost_wei);
                self.spends.push_back((*at_ms, *sender, *cost_wei));
            }
            _ => {}
        }
    }
}

/// Armed-live in the run's report (R36).
#[derive(Clone, Debug, Serialize)]
pub struct LiveReport {
    pub submitted: u64,
    pub accepted: u64,
    pub refused: u64,
    pub silent: u64,
    pub skipped: BTreeMap<String, u64>,
    pub landed: u64,
    /// Landings that paid the recipient.
    pub won: u64,
    pub gross_wei: String,
    pub cost_wei: String,
    pub net_today_wei: String,
    pub reserved_wei: String,
    pub budget_wei: String,
    pub halted: Option<String>,
    pub cadence_ms_measured: Option<u64>,
    pub cadence_ms_configured: u64,
    /// The furthest pending bound past the last head.
    pub expiry_horizon_blocks: Option<u64>,
    pub senders: Vec<SenderReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SenderReport {
    pub address: Address,
    /// As last read; none before the first.
    pub balance_wei: Option<String>,
    pub nonce: Option<u64>,
    /// The largest worst case pending at the sender.
    pub worst_case_wei: String,
    /// The balance over the last day's landing costs; none with no spend.
    pub days_left: Option<f64>,
}

impl std::fmt::Debug for ArmedLive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArmedLive")
            .field("senders", &self.keys.iter().map(LaneKey::address).collect::<Vec<_>>())
            .field("executor", &self.executor)
            .field("recipient", &self.recipient)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// An arm's fields, read for sending.
struct Readable {
    pools: [Address; 2],
    size: U256,
    /// R37: the start token when not WETH.
    start: Option<Address>,
    bid: u128,
    data: Vec<u8>,
}

/// What the chain said of a sender: its nonce, and the receipt of whichever
/// of our transactions used the nonce before it.
struct NonceRead {
    nonce: u64,
    landed: Option<(B256, Value)>,
    /// A receipt read failed, so "none of ours" is not known.
    unsure: bool,
}

impl ArmedLive {
    /// `records` are the reopened ledger's; `keys[slot]` sends slot `slot`'s
    /// arms. Nothing is sent before `begin`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        keys: Vec<LaneKey>,
        chain: Arc<dyn LiveChain>,
        submit: Arc<dyn ValiditySend>,
        ledger: LiveLedger,
        records: Vec<LiveRecord>,
        policy: LivePolicy,
        executor: Address,
        recipient: Address,
        chain_id: u64,
    ) -> Self {
        let inner = Inner {
            boot: records,
            begun: false,
            state: LiveState::new(0),
            nonces: BTreeMap::new(),
            balances: BTreeMap::new(),
            arms: BTreeMap::new(),
            stops: None,
            last_head: None,
            intervals: VecDeque::with_capacity(CADENCE_SAMPLES),
            counts: Counts::default(),
            spends: VecDeque::new(),
        };
        Self {
            keys,
            chain,
            submit,
            ledger,
            policy,
            executor,
            recipient,
            chain_id,
            ready: AtomicBool::new(false),
            turn: tokio::sync::Mutex::new(()),
            inner: Mutex::new(inner),
        }
    }

    /// Begun, and every transaction the reopened ledger left pending is
    /// settled. A halt stops sends on its own.
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Once, at boot: replay the reopened ledger as of `now_ms` and read every
    /// sender's nonce. A sender with nothing pending must be where the ledger
    /// left it; a pending one is settled by the heads past its bound.
    pub async fn begin(&self, now_ms: u64) -> Result<(), String> {
        let _turn = self.turn.lock().await;
        let mut nonces = BTreeMap::new();
        for key in &self.keys {
            let sender = key.address();
            let nonce = self.chain.nonce(sender).await.map_err(|e| format!("the nonce of {sender:#x}: {e}"))?;
            nonces.insert(sender, nonce);
        }
        let views = {
            let mut inner = self.inner();
            let boot = std::mem::take(&mut inner.boot);
            inner.state = LiveState::from_records(&boot, now_ms, self.policy.budget_wei);
            inner.nonces = nonces;
            inner.begun = true;
            nonce_views(&boot)
        };
        for (sender, view) in views {
            let (chain, pending) = {
                let inner = self.inner();
                (inner.nonces.get(&sender).copied(), inner.state.pending.keys().any(|(s, _)| *s == sender))
            };
            if let Some(chain) = chain.filter(|c| !pending && *c != view) {
                self.trip(sender, format!("the nonce is {chain}, and the ledger left it at {view}"), now_ms);
            }
        }
        self.check_ready();
        Ok(())
    }

    /// The ledger's state as of `now_ms`.
    pub fn state(&self, now_ms: u64) -> LiveState {
        let mut inner = self.inner();
        inner.state.roll(now_ms);
        inner.state.clone()
    }

    /// The median per-block interval of the last twenty heads, once there are
    /// twenty.
    pub fn cadence_ms(&self) -> Option<u64> {
        median(&self.inner().intervals)
    }

    /// What armed-live has done since boot, and where it stands, as of `now_ms`.
    pub fn report(&self, now_ms: u64) -> LiveReport {
        let mut inner = self.inner();
        inner.state.roll(now_ms);
        while inner.spends.front().is_some_and(|(at, _, _)| now_ms.saturating_sub(*at) > DAY_MS) {
            inner.spends.pop_front();
        }
        let inner = &*inner;
        let senders = self
            .keys
            .iter()
            .map(|key| {
                let address = key.address();
                let balance = inner.balances.get(&address).map(|b| b.wei);
                let spent: u128 = inner.spends.iter().filter(|(_, s, _)| *s == address).map(|(_, _, cost)| cost).sum();
                let worst = inner
                    .state
                    .pending
                    .iter()
                    .filter(|((s, _), _)| *s == address)
                    .flat_map(|(_, txs)| txs.iter().map(|t| t.worst_case_wei))
                    .max()
                    .unwrap_or(0);
                SenderReport {
                    address,
                    balance_wei: balance.map(|b| b.to_string()),
                    nonce: inner.nonces.get(&address).copied(),
                    worst_case_wei: worst.to_string(),
                    #[allow(clippy::cast_precision_loss)]
                    days_left: balance.filter(|_| spent > 0).map(|b| b as f64 / spent as f64),
                }
            })
            .collect();
        let horizon = inner.last_head.and_then(|(head, _)| {
            inner.state.pending.values().flatten().map(|t| t.to_block).max().map(|bound| bound.saturating_sub(head))
        });
        let c = &inner.counts;
        LiveReport {
            submitted: c.submitted,
            accepted: c.accepted,
            refused: c.refused,
            silent: c.silent,
            skipped: c.skipped.clone(),
            landed: c.landed,
            won: c.won,
            gross_wei: c.gross_wei.to_string(),
            cost_wei: c.cost_wei.to_string(),
            net_today_wei: inner.state.day_net.to_string(),
            reserved_wei: inner.state.reserved_wei.to_string(),
            budget_wei: self.policy.budget_wei.to_string(),
            halted: inner.state.halted.as_ref().map(|h| match h {
                Halt::LossCap { day } => format!("the loss cap, for {day}"),
                Halt::Tripwire { sender, detail } => format!("the tripwire, on {sender:#x}: {detail}"),
            }),
            cadence_ms_measured: median(&inner.intervals),
            cadence_ms_configured: self.policy.block_time_ms,
            expiry_horizon_blocks: horizon,
            senders,
        }
    }

    /// Submit each arm and renewal, in order.
    pub async fn on_lines(&self, lines: &[ArmedLine], base_fee_wei: u128, now_ms: u64) {
        let _turn = self.turn.lock().await;
        for line in lines {
            let arm = {
                let mut inner = self.inner();
                match line {
                    ArmedLine::Arm(arm) => {
                        inner.arms.insert(arm.sender_slot, (**arm).clone());
                        (**arm).clone()
                    }
                    ArmedLine::Renew { arm_id, to_block, deadline, calldata, .. } => {
                        let Some(arm) = inner.arms.values_mut().find(|a| a.arm_id == *arm_id) else { continue };
                        arm.to_block = *to_block;
                        if let Some(deadline) = deadline {
                            arm.deadline = *deadline;
                        }
                        if let Some(calldata) = calldata {
                            arm.calldata.clone_from(calldata);
                        }
                        arm.clone()
                    }
                }
            };
            self.submit_arm(&arm, base_fee_wei, now_ms).await;
        }
    }

    /// A new head: the cadence it shows, then every sender with a pending
    /// transaction whose bound it has passed. The nonces are read outside the
    /// turn, so submissions do not wait on them, and judged inside it against
    /// the state as it then is.
    pub async fn on_head(&self, head: u64, head_seen_ms: u64, now_ms: u64) {
        let due = {
            let mut inner = self.inner();
            match inner.last_head {
                Some((last, seen)) if head > last => {
                    if inner.intervals.len() == CADENCE_SAMPLES {
                        inner.intervals.pop_front();
                    }
                    inner.intervals.push_back(head_seen_ms.saturating_sub(seen) / (head - last));
                    inner.last_head = Some((head, head_seen_ms));
                }
                Some(_) => {}
                None => inner.last_head = Some((head, head_seen_ms)),
            }
            inner.state.roll(now_ms);
            due(&inner.state, head)
        };
        let senders: Vec<Address> = self.keys.iter().map(LaneKey::address).collect();
        match self.chain.stops(self.executor, &senders, head.saturating_sub(1)).await {
            Ok(stops) if stops.allowed.len() == senders.len() => self.inner().stops = Some((stops, now_ms)),
            Ok(_) => warn!("armed live: the stops answered for the wrong number of senders"),
            Err(e) => warn!(error = %e, "armed live: the owner's stops did not read"),
        }
        let mut reads = Vec::with_capacity(due.len());
        for sender in due {
            reads.push((sender, self.read_nonce(sender, head).await));
        }
        let _turn = self.turn.lock().await;
        for (sender, read) in reads {
            self.settle(sender, read, head, now_ms);
        }
        self.check_ready();
    }

    /// One arm or renewal: checked, then signed, recorded and sent; re-signed
    /// at most twice when refused as an underpriced replacement.
    async fn submit_arm(&self, arm: &Arm, base_fee_wei: u128, now_ms: u64) {
        if !self.ready() || !self.cadence_ok() || self.policy.kill_file.exists() || self.state(now_ms).halted.is_some() {
            return;
        }
        let Some(allowed) = self.stops_allow(now_ms) else { return };
        let Some(key) = self.keys.get(usize::from(arm.sender_slot)) else {
            return self.skip(arm, Address::ZERO, "slot", now_ms);
        };
        let sender = key.address();
        if !allowed.get(usize::from(arm.sender_slot)).copied().unwrap_or(false) {
            return self.skip(arm, sender, "revoked", now_ms);
        }
        // R37: only to the executor it was built for: another's calldata
        // reverts there, and pays its gas.
        if arm.executor != Some(self.executor) {
            return self.skip(arm, sender, "executor", now_ms);
        }
        let Some(read) = readable(arm) else {
            return self.skip(arm, sender, "unreadable", now_ms);
        };
        // With nothing to wait for, it would land in the next block.
        if arm.predicates.iter().all(is_bound) {
            return self.skip(arm, sender, "unconditional", now_ms);
        }
        // R37: a USDC arm's size is USDC's units, held to USDC's cap.
        let cap = if read.start == Some(apex_exec::lean2::USDC) { self.policy.max_size_usdc } else { self.policy.max_size_wei };
        if read.size > cap {
            return self.skip(arm, sender, "size", now_ms);
        }
        let head = match self.chain.head().await {
            Ok(head) => head,
            Err(e) => {
                warn!(error = %e, "armed live: no head");
                return self.skip(arm, sender, "chain", now_ms);
            }
        };
        let to_block = head.saturating_add(self.policy.window_blocks).min(arm.to_block);
        if to_block <= head {
            return self.skip(arm, sender, "expired", now_ms);
        }
        let Ok(validity) = serde_json::to_value(bounded(&arm.predicates, to_block)) else {
            return self.skip(arm, sender, "unreadable", now_ms);
        };
        let nonce = self.inner().nonces[&sender];
        let cap = self.policy.max_priority_wei_per_gas;
        let mut priority = read.bid.min(cap);
        let mut max_fee = base_fee_wei.saturating_mul(2).saturating_add(priority);
        for bump in 0..=MAX_BUMPS {
            let worst = u128::from(arm.gas_limit).saturating_mul(max_fee).saturating_add(self.policy.l1_fee_bound_wei);
            match self.balance(sender, now_ms).await {
                Ok(balance) if balance >= worst.saturating_add(self.policy.sender_reserve_wei) => {}
                Ok(_) => return self.skip(arm, sender, "balance", now_ms),
                Err(e) => {
                    warn!(error = %e, %sender, "armed live: no balance");
                    return self.skip(arm, sender, "chain", now_ms);
                }
            }
            if !self.admits(sender, nonce, worst, now_ms) {
                return self.skip(arm, sender, "budget", now_ms);
            }
            let tx = Eip1559Tx {
                chain_id: self.chain_id,
                nonce,
                max_priority_fee_per_gas: priority,
                max_fee_per_gas: max_fee,
                gas_limit: arm.gas_limit,
                to: self.executor,
                data: read.data.clone(),
            };
            let Ok(signed) = key.sign_eip1559(&tx) else {
                return self.skip(arm, sender, "unsignable", now_ms);
            };
            let submitting = LiveRecord::Submitting {
                arm_id: arm.arm_id,
                pools: read.pools,
                sender,
                nonce,
                tx: signed.hash,
                to_block,
                gas_limit: arm.gas_limit,
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: priority,
                worst_case_wei: worst,
                at_ms: now_ms,
                start: read.start,
            };
            if let Err(e) = self.ledger.append(&submitting) {
                error!(error = %e, arm = arm.arm_id, "armed live: the submission could not be recorded, so it was not sent");
                return;
            }
            self.apply(&submitting, now_ms);
            let answer = self.submit.send(&signed.raw, &validity).await;
            // Only a refusal that says what was refused sent nothing; any other
            // error may have, and keeps its reservation.
            let (said, accepted) = match &answer {
                SendAnswer::Accepted(_) => ("accepted".to_string(), Some(true)),
                SendAnswer::Refused(why) if refused_for_certain(why) => (why.clone(), Some(false)),
                SendAnswer::Refused(why) | SendAnswer::Silent(why) => (why.clone(), None),
            };
            self.record(LiveRecord::Answered { tx: signed.hash, answer: said.clone(), accepted, at_ms: now_ms }, now_ms);
            if accepted != Some(false) {
                if accepted.is_none() {
                    warn!(arm = arm.arm_id, tx = %signed.hash, answer = %said, "armed live: no clear answer, so it may have been sent");
                }
                return;
            }
            let why = said;
            let why = why.to_ascii_lowercase();
            if why.contains("nonce too low") {
                return self.landing_check(sender, head, now_ms).await;
            }
            if !why.contains("underpriced") {
                return;
            }
            let (next_priority, next_fee) = (bumped(priority), bumped(max_fee));
            if bump == MAX_BUMPS || next_priority > cap {
                return self.skip(arm, sender, "underpriced", now_ms);
            }
            (priority, max_fee) = (next_priority, next_fee);
        }
    }

    /// A refusal said `sender`'s nonce is used: find out by what, at once.
    /// Under the turn.
    async fn landing_check(&self, sender: Address, head: u64, now_ms: u64) {
        let read = self.read_nonce(sender, head).await;
        self.settle(sender, read, head, now_ms);
    }

    /// The nonce as of the block before `head`, which every read node within a
    /// block of the head's feed has.
    async fn read_nonce(&self, sender: Address, head: u64) -> Option<NonceRead> {
        let nonce = match self.chain.nonce_at(sender, head.saturating_sub(1)).await {
            Ok(nonce) => nonce,
            Err(e) => {
                warn!(error = %e, %sender, "armed live: no nonce read");
                return None;
            }
        };
        let used: Vec<B256> = self
            .inner()
            .state
            .pending
            .iter()
            .filter(|((s, n), _)| *s == sender && n.saturating_add(1) == nonce)
            .flat_map(|(_, txs)| txs.iter().map(|t| t.tx))
            .collect();
        let mut unsure = false;
        for tx in used {
            match self.chain.receipt(tx).await {
                Ok(Some(receipt)) => return Some(NonceRead { nonce, landed: Some((tx, receipt)), unsure }),
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, %tx, "armed live: no receipt read");
                    unsure = true;
                }
            }
        }
        Some(NonceRead { nonce, landed: None, unsure })
    }

    /// Judge what was read of `sender` against its pending nonces, under the
    /// turn. With no read, the next head asks again. A read behind what this
    /// worker has already seen is a node behind the chain, and is not judged.
    fn settle(&self, sender: Address, read: Option<NonceRead>, head: u64, now_ms: u64) {
        let Some(read) = read else { return };
        let (pending, known) = {
            let inner = self.inner();
            let pending: Vec<(u64, Vec<PendingTx>)> =
                inner.state.pending.iter().filter(|((s, _), _)| *s == sender).map(|((_, n), txs)| (*n, txs.clone())).collect();
            (pending, inner.nonces.get(&sender).copied())
        };
        let lowest = pending.first().map(|(n, _)| *n).or(known);
        if lowest.is_some_and(|n| read.nonce < n) {
            return;
        }
        if pending.is_empty() {
            if let Some(known) = known.filter(|k| *k != read.nonce) {
                return self.trip(sender, format!("the nonce is {}, with nothing of ours pending at {known}", read.nonce), now_ms);
            }
        }
        for (nonce, txs) in &pending {
            let nonce = *nonce;
            if read.nonce == nonce {
                for t in txs.iter().filter(|t| t.to_block < head) {
                    self.record(LiveRecord::Expired { sender, nonce, tx: Some(t.tx), at_ms: now_ms }, now_ms);
                }
            } else if read.nonce == nonce.saturating_add(1) {
                let ours = read.landed.as_ref().and_then(|(tx, receipt)| txs.iter().find(|t| t.tx == *tx).map(|t| (t, receipt)));
                match ours {
                    Some((sent, receipt)) => {
                        if !self.land(sender, nonce, sent, receipt, now_ms) {
                            return self.abandon(sender, &pending, read.nonce, "a receipt of ours did not read", now_ms);
                        }
                    }
                    None if read.unsure => return,
                    None => {
                        let detail = format!("the nonce passed {nonce} with no receipt among our {} transactions at it", txs.len());
                        return self.abandon(sender, &pending, read.nonce, &detail, now_ms);
                    }
                }
            } else {
                let detail = format!("the nonce is {}, and ours are pending at {nonce}", read.nonce);
                return self.abandon(sender, &pending, read.nonce, &detail, now_ms);
            }
        }
        let mut inner = self.inner();
        inner.nonces.insert(sender, read.nonce);
        if known != Some(read.nonce) {
            // A landing spent from the balance.
            if let Some(b) = inner.balances.get_mut(&sender) {
                b.stale = true;
            }
        }
    }

    /// The chain passed `sender`'s pending nonces below `chain_nonce` in a
    /// way this worker cannot account for: each is abandoned at its worst
    /// case, so the state holds nothing a boot would trip on again, and the
    /// lane trips until an operator resets it.
    fn abandon(&self, sender: Address, pending: &[(u64, Vec<PendingTx>)], chain_nonce: u64, detail: &str, now_ms: u64) {
        for (nonce, txs) in pending.iter().filter(|(n, _)| *n < chain_nonce) {
            let worst_case_wei = txs.iter().map(|t| t.worst_case_wei).max().unwrap_or(0);
            self.record(LiveRecord::Abandoned { sender, nonce: *nonce, worst_case_wei, at_ms: now_ms }, now_ms);
        }
        self.trip(sender, detail.to_string(), now_ms);
    }

    /// Account a landing from its receipt, and halt the day if it spent the
    /// budget. False when the receipt does not read.
    fn land(&self, sender: Address, nonce: u64, sent: &PendingTx, receipt: &Value, now_ms: u64) -> bool {
        // R37: a USDC trade pays USDC; the ETH budget nets only its gas.
        let token = sent.start.unwrap_or(WETH);
        let read = match read_receipt(receipt, token, self.executor, &[self.recipient]) {
            Ok(read) => read,
            Err(e) => {
                error!(%sender, tx = %sent.tx, error = %e, "armed live: a receipt of ours did not read");
                return false;
            }
        };
        let r = &read.receipt;
        let cost = u128::from(r.gas_used).saturating_mul(r.effective_gas_price_wei).saturating_add(r.l1_fee_wei);
        let in_weth = token == WETH;
        let paid_wei = if in_weth { read.paid_wei } else { 0 };
        let net = i128::try_from(paid_wei).unwrap_or(i128::MAX).saturating_sub(i128::try_from(cost).unwrap_or(i128::MAX));
        info!(%sender, nonce, tx = %sent.tx, success = r.success, net_wei = net, "armed live: landed");
        let landed = LiveRecord::Landed {
            arm_id: sent.arm_id,
            pools: sent.pools,
            sender,
            nonce,
            tx: sent.tx,
            gross_wei: if in_weth { read.realised_wei.unwrap_or(0) } else { 0 },
            paid_wei,
            cost_wei: cost,
            net_wei: net,
            at_ms: now_ms,
            start: sent.start,
            paid_start: (!in_weth).then(|| read.paid_wei.to_string()),
        };
        self.record(landed, now_ms);
        let (day, day_net) = {
            let inner = self.inner();
            (inner.state.day.clone(), inner.state.day_net)
        };
        if day_net < 0 && day_net.unsigned_abs() >= self.policy.budget_wei {
            error!(%day, day_net, budget_wei = self.policy.budget_wei, "armed live: the day's loss reached its budget; nothing more is sent today");
            self.record(LiveRecord::Halted { reason: Halt::LossCap { day }, at_ms: now_ms }, now_ms);
        }
        true
    }

    fn trip(&self, sender: Address, detail: String, now_ms: u64) {
        error!(%sender, %detail, "armed live: the tripwire fired; nothing more is sent until an operator resets it");
        self.record(LiveRecord::Halted { reason: Halt::Tripwire { sender, detail }, at_ms: now_ms }, now_ms);
    }

    fn check_ready(&self) {
        let inner = self.inner();
        if inner.begun && inner.state.pending.is_empty() {
            self.ready.store(true, Ordering::Release);
        }
    }

    /// Which senders may send, from the owner's stops: none while they were
    /// last read more than three block times ago (the head feed has gone
    /// quiet, and settlement with it) or the circuit is open.
    fn stops_allow(&self, now_ms: u64) -> Option<Vec<bool>> {
        let inner = self.inner();
        let (stops, read_ms) = inner.stops.as_ref()?;
        let fresh = now_ms.saturating_sub(*read_ms) <= self.policy.block_time_ms.saturating_mul(3);
        (fresh && !stops.circuit_open).then(|| stops.allowed.clone())
    }

    /// Heads keep the configured cadence, within half of it either way.
    fn cadence_ok(&self) -> bool {
        let configured = self.policy.block_time_ms;
        self.cadence_ms().is_some_and(|m| m.abs_diff(configured).saturating_mul(2) <= configured)
    }

    /// Whether `worst` fits the day's budget at `(sender, nonce)`. At most one
    /// transaction lands per nonce, so a replacement adds only what it costs
    /// over the largest one already pending there.
    fn admits(&self, sender: Address, nonce: u64, worst: u128, now_ms: u64) -> bool {
        let mut inner = self.inner();
        inner.state.roll(now_ms);
        let held = inner.state.pending.get(&(sender, nonce)).and_then(|txs| txs.iter().map(|t| t.worst_case_wei).max()).unwrap_or(0);
        inner.state.admits(worst.saturating_sub(held), self.policy.budget_wei)
    }

    async fn balance(&self, sender: Address, now_ms: u64) -> Result<u128, String> {
        let cached = self.inner().balances.get(&sender).filter(|b| !b.stale && now_ms.saturating_sub(b.read_ms) < BALANCE_TTL_MS).map(|b| b.wei);
        if let Some(wei) = cached {
            return Ok(wei);
        }
        let wei = self.chain.balance(sender).await?;
        self.inner().balances.insert(sender, Balance { wei, read_ms: now_ms, stale: false });
        Ok(wei)
    }

    fn skip(&self, arm: &Arm, sender: Address, reason: &str, now_ms: u64) {
        self.record(LiveRecord::Skipped { arm_id: arm.arm_id, sender, reason: reason.to_string(), at_ms: now_ms }, now_ms);
    }

    /// Append, then apply. The state takes the record even when the file
    /// refuses it, so this process acts on what happened; the error is logged.
    fn record(&self, record: LiveRecord, now_ms: u64) {
        if let Err(e) = self.ledger.append(&record) {
            error!(error = %e, "armed live: a ledger record was not written");
        }
        self.apply(&record, now_ms);
    }

    fn apply(&self, record: &LiveRecord, now_ms: u64) {
        let mut inner = self.inner();
        inner.state.roll(now_ms);
        inner.state.apply(record, self.policy.budget_wei);
        inner.count(record);
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn readable(arm: &Arm) -> Option<Readable> {
    Some(Readable {
        pools: [arm.pools[0].parse().ok()?, arm.pools[1].parse().ok()?],
        size: U256::from_dec_str(&arm.size_wei).ok()?,
        start: arm.start,
        bid: arm.bid_wei_per_gas.parse().ok()?,
        data: alloy_primitives::hex::decode(&arm.calldata).ok()?,
    })
}

/// A block bound, which submission replaces.
fn is_bound(p: &Predicate) -> bool {
    matches!(p, Predicate::BlockNumber { op: Op::AtMost, .. })
}

/// The arm's predicates, bounded at `to_block`.
fn bounded(predicates: &[Predicate], to_block: u64) -> Vec<Predicate> {
    predicates.iter().filter(|p| !is_bound(p)).cloned().chain([predicate::expiry(to_block)]).collect()
}

/// The upper median of a full window of head intervals.
fn median(intervals: &VecDeque<u64>) -> Option<u64> {
    (intervals.len() == CADENCE_SAMPLES).then(|| {
        let mut sorted: Vec<u64> = intervals.iter().copied().collect();
        sorted.sort_unstable();
        sorted[CADENCE_SAMPLES / 2]
    })
}

/// An eighth more, rounded up.
fn bumped(wei: u128) -> u128 {
    wei.saturating_mul(1125).div_ceil(1000)
}

/// Senders with a pending transaction whose bound `head` has passed.
fn due(state: &LiveState, head: u64) -> Vec<Address> {
    let mut due: Vec<Address> =
        state.pending.iter().filter(|(_, txs)| txs.iter().any(|t| t.to_block < head)).map(|((sender, _), _)| *sender).collect();
    due.dedup();
    due
}

/// A refusal that says what was refused, so nothing was sent: never
/// connected, an HTTP 4xx, or a reason the node names.
fn refused_for_certain(why: &str) -> bool {
    let why = why.to_ascii_lowercase();
    why.starts_with("connect:")
        || why.starts_with("http 4")
        || ["nonce too low", "underpriced", "insufficient funds", "invalid"].iter().any(|k| why.contains(k))
}
