//! The armed lane's live worker (Task 8.5 R36): each arm the engine writes
//! becomes a validity transaction from its slot's own sender.
//!
//! **Before a send.** The worker checks four things:
//! - the lane is ready;
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

use super::lines::{Arm, ArmedLine};
use super::live_ledger::{LiveLedger, LiveRecord, LiveState};
use super::predicate::{self, Op, Predicate};
use crate::sign::{Eip1559Tx, LaneKey};
use alloy_primitives::{Address, B256};
use apex_chain::base::send::SendAnswer;
use apex_chain::base::validity::ValiditySubmitter;
use ethers_core::types::U256;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tracing::{error, info, warn};

/// How long a sender's balance is trusted without a landing in between.
const BALANCE_TTL_MS: u64 = 60_000;
/// Re-signings after a refused replacement, each at an eighth more.
const MAX_BUMPS: usize = 2;

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
    async fn balance(&self, a: Address) -> Result<u128, String>;
    async fn receipt(&self, tx: B256) -> Result<Option<Value>, String>;
}

/// The caps the worker holds every submission to, all in wei.
#[derive(Clone, Debug)]
pub struct LivePolicy {
    /// The day's loss budget: realised losses and pending worst cases share it.
    pub budget_wei: u128,
    pub max_priority_wei_per_gas: u128,
    pub max_size_wei: U256,
    /// A bound on one transaction's L1 data fee.
    pub l1_fee_bound_wei: u128,
    /// Left in every sender after its worst case.
    pub sender_reserve_wei: u128,
    pub window_blocks: u64,
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
    /// One submission at a time: each is admitted against every reservation
    /// made before it.
    turn: tokio::sync::Mutex<()>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The reopened ledger's records, until `begin` replays them.
    boot: Vec<LiveRecord>,
    state: LiveState,
    /// Each sender's next nonce, as the chain last said.
    nonces: BTreeMap<Address, u64>,
    /// Each sender's balance, and when it was read.
    balances: BTreeMap<Address, (u128, u64)>,
    /// Each slot's arm, as last armed or renewed.
    arms: BTreeMap<u16, Arm>,
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
    bid: u128,
    data: Vec<u8>,
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
            state: LiveState::new(0),
            nonces: BTreeMap::new(),
            balances: BTreeMap::new(),
            arms: BTreeMap::new(),
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

    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Replay the reopened ledger as of `now_ms` and read every sender's
    /// nonce. Until both are done, nothing is sent.
    pub async fn begin(&self, now_ms: u64) -> Result<(), String> {
        let mut nonces = BTreeMap::new();
        for key in &self.keys {
            let sender = key.address();
            let nonce = self.chain.nonce(sender).await.map_err(|e| format!("the nonce of {sender:#x}: {e}"))?;
            nonces.insert(sender, nonce);
        }
        let mut inner = self.inner();
        let boot = std::mem::take(&mut inner.boot);
        inner.state = LiveState::from_records(&boot, now_ms, self.policy.budget_wei);
        inner.nonces = nonces;
        drop(inner);
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    /// The ledger's state as of `now_ms`.
    pub fn state(&self, now_ms: u64) -> LiveState {
        let mut inner = self.inner();
        inner.state.roll(now_ms);
        inner.state.clone()
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

    /// One arm or renewal: checked, then signed, recorded and sent; re-signed
    /// at most twice when refused as an underpriced replacement.
    async fn submit_arm(&self, arm: &Arm, base_fee_wei: u128, now_ms: u64) {
        if !self.ready() || self.policy.kill_file.exists() || self.state(now_ms).halted.is_some() {
            return;
        }
        let Some(key) = self.keys.get(usize::from(arm.sender_slot)) else {
            return self.skip(arm, Address::ZERO, "slot", now_ms);
        };
        let sender = key.address();
        let Some(read) = readable(arm) else {
            return self.skip(arm, sender, "unreadable", now_ms);
        };
        // With nothing to wait for, it would land in the next block.
        if arm.predicates.iter().all(is_bound) {
            return self.skip(arm, sender, "unconditional", now_ms);
        }
        if read.size > self.policy.max_size_wei {
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
            };
            if let Err(e) = self.ledger.append(&submitting) {
                error!(error = %e, arm = arm.arm_id, "armed live: the submission could not be recorded, so it was not sent");
                return;
            }
            self.inner().state.apply(&submitting, self.policy.budget_wei);
            let answer = self.submit.send(&signed.raw, &validity).await;
            let (said, accepted) = match &answer {
                SendAnswer::Accepted(_) => ("accepted".to_string(), Some(true)),
                SendAnswer::Refused(why) => (why.clone(), Some(false)),
                SendAnswer::Silent(why) => (why.clone(), None),
            };
            self.note(LiveRecord::Answered { tx: signed.hash, answer: said, accepted, at_ms: now_ms });
            let SendAnswer::Refused(why) = answer else {
                if accepted.is_none() {
                    warn!(arm = arm.arm_id, tx = %signed.hash, "armed live: no answer, so it may have been sent");
                }
                return;
            };
            let why = why.to_ascii_lowercase();
            if why.contains("nonce too low") {
                return self.landing_check(sender, now_ms).await;
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

    /// Look for a landing among `sender`'s pending transactions, and stop
    /// trusting its balance. Task 10 accounts for what it finds.
    pub async fn landing_check(&self, sender: Address, now_ms: u64) {
        let pending: Vec<B256> = self
            .state(now_ms)
            .pending
            .iter()
            .filter(|((s, _), _)| *s == sender)
            .flat_map(|(_, txs)| txs.iter().map(|t| t.tx))
            .collect();
        for tx in pending {
            match self.chain.receipt(tx).await {
                Ok(Some(_)) => info!(%sender, %tx, "armed live: a pending transaction has a receipt"),
                Ok(None) => {}
                Err(e) => warn!(error = %e, %tx, "armed live: no receipt read"),
            }
        }
        self.inner().balances.remove(&sender);
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
        let cached = self.inner().balances.get(&sender).copied();
        if let Some((balance, at)) = cached {
            if now_ms.saturating_sub(at) < BALANCE_TTL_MS {
                return Ok(balance);
            }
        }
        let balance = self.chain.balance(sender).await?;
        self.inner().balances.insert(sender, (balance, now_ms));
        Ok(balance)
    }

    fn skip(&self, arm: &Arm, sender: Address, reason: &str, now_ms: u64) {
        self.note(LiveRecord::Skipped { arm_id: arm.arm_id, sender, reason: reason.to_string(), at_ms: now_ms });
    }

    /// A record the worker need not stop for. It is applied even when the
    /// file refuses it: a replay without it is only more cautious.
    fn note(&self, record: LiveRecord) {
        if let Err(e) = self.ledger.append(&record) {
            warn!(error = %e, "armed live: a ledger record was not written");
        }
        self.inner().state.apply(&record, self.policy.budget_wei);
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn readable(arm: &Arm) -> Option<Readable> {
    Some(Readable {
        pools: [arm.pools[0].parse().ok()?, arm.pools[1].parse().ok()?],
        size: U256::from_dec_str(&arm.size_wei).ok()?,
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

/// An eighth more, rounded up.
fn bumped(wei: u128) -> u128 {
    wei.saturating_mul(1125).div_ceil(1000)
}
