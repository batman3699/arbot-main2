//! `apex validity-probe` (Task 8.5 R36): what Base's sequencer does with
//! validity transactions, asked with one armed sender's zero-value
//! self-transfers before armed-live sends an arm.
//!
//! Every case ends where the chain says it did: included (a receipt), or
//! expired (its bound passed with the sender's nonce unmoved). An acceptance
//! alone is never a pass. The six cases, about two 21,000-gas inclusions:
//! 1. `never`: a predicate that cannot hold is not included by its bound, and
//!    the nonce and balance do not move;
//! 2. `now`: one already true is included within two blocks;
//! 3. `held`: a never-true transaction bounded 25 blocks out, then another at
//!    its nonce and fees: the second's answer, the first's status, and the
//!    first expires;
//! 4. `after`: once that bound has passed, another at the nonce and fees;
//! 5. `eight`: an arm's eight predicates, all true, included;
//! 6. `bumps`: five replacements of a never-true transaction, back to back,
//!    an eighth more each.
//!
//! Base takes a bound at most 60 s out, so `held` is 25 blocks, not 30.
//! The sender's nonce moves: run it with the armed lane in shadow, and before
//! armed-live's ledger has a record for that sender.

use crate::armed::live::LiveChain;
use crate::armed::predicate::{expiry, liquidity_mask, price_mask, Op, Predicate};
use crate::live::frontier::WETH;
use crate::sign::{Eip1559Tx, LaneKey};
use alloy_primitives::{Address, B256};
use apex_chain::base::send::SendAnswer;
use apex_chain::base::validity::ValiditySubmitter;
use apex_chain::rpc::RpcTransport;
use ethers_core::types::U256;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Room for a byte of data over 21,000.
const GAS_LIMIT: u64 = 25_000;
/// As `apex send-probe` tips: 0.001 gwei.
pub const TIP_WEI_PER_GAS: u128 = 1_000_000;

/// The probe's predicate sets at `head`, named, exactly as sent.
pub fn probe_predicates(head: u64, sender: Address) -> Vec<(String, Value)> {
    let never = json!({"type": "balance", "params": {"address": format!("{sender:#x}"), "op": ">=", "value": format!("{:#x}", U256::one() << 128)}});
    let bound = |blocks: u64| json!(expiry(head + blocks));
    // Masked words of WETH's storage, bounded by nothing a word can exceed:
    // an arm's shapes, true on any state.
    let word = |slot: u64, mask: U256, op: Op, value: U256| {
        json!(Predicate::Storage {
            address: format!("{WETH:#x}"),
            slot: format!("{slot:#x}"),
            mask: Some(format!("{mask:#x}")),
            op,
            value: format!("{value:#x}"),
        })
    };
    let (price, liquidity) = (price_mask(), liquidity_mask());
    let eight = vec![
        // The trigger's two.
        word(0, price, Op::AtLeast, U256::zero()),
        word(0, price, Op::AtMost, price),
        // The band's four.
        word(1, price, Op::AtLeast, U256::zero()),
        word(1, price, Op::AtMost, price),
        word(2, price, Op::AtLeast, U256::zero()),
        word(2, price, Op::AtMost, price),
        // The liquidity floor.
        word(4, liquidity, Op::AtLeast, U256::zero()),
        bound(6),
    ];
    vec![
        ("never".into(), json!([never, bound(6)])),
        ("now".into(), json!([Predicate::BlockNumber { op: Op::AtLeast, value: format!("{head:#x}") }, bound(6)])),
        ("held".into(), json!([never, bound(25)])),
        ("eight".into(), Value::Array(eight)),
    ]
}

/// One send: its hash, nonce and tip, the endpoint's answer and how long it took.
#[derive(Debug, Serialize)]
pub struct Sent {
    pub tx: B256,
    pub nonce: u64,
    pub priority_wei_per_gas: u128,
    pub answer: String,
    pub accepted: bool,
    pub answer_ms: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Included { tx: B256, block: u64, after_blocks: u64 },
    /// Its bound passed with the nonce unmoved.
    Expired { bound: u64 },
    /// Nothing was accepted, so nothing can land.
    Refused,
    Unsettled { why: String },
}

/// One case, as printed.
#[derive(Debug, Serialize)]
pub struct Action {
    pub case: u8,
    pub name: &'static str,
    pub sends: Vec<Sent>,
    /// `base_transactionStatus` of the first send, where asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Value>,
    pub outcome: Outcome,
    pub pass: bool,
}

pub struct Probe<'a> {
    pub key: &'a LaneKey,
    pub chain_id: u64,
    pub rpc: &'a dyn RpcTransport,
    pub chain: &'a dyn LiveChain,
    pub submit: &'a ValiditySubmitter,
}

impl Probe<'_> {
    /// Every case in order, each handed to `emit` once settled. Whether all
    /// passed; a read that fails ends the run there.
    pub async fn run(&self, mut emit: impl FnMut(&Action)) -> bool {
        match self.cases(&mut emit).await {
            Ok(all) => all,
            Err(why) => {
                emit(&Action { case: 0, name: "read", sends: Vec::new(), status: None, outcome: Outcome::Unsettled { why }, pass: false });
                false
            }
        }
    }

    async fn cases(&self, emit: &mut impl FnMut(&Action)) -> Result<bool, String> {
        let me = self.key.address();
        let mut all = true;
        let mut nonce = self.chain.nonce(me).await?;
        let mut done = |a: Action| {
            all &= a.pass;
            emit(&a);
        };

        // 1: never true.
        let balance = self.chain.balance(me).await?;
        let (head, sets) = self.sets().await?;
        let fees = self.fees().await?;
        let s = self.send(nonce, fees, Vec::new(), &sets[0].1).await?;
        let outcome = self.settle(nonce, &[&s], head, head + 6).await?;
        let pass = outcome == Outcome::Expired { bound: head + 6 } && self.chain.balance(me).await? == balance;
        done(Action { case: 1, name: "never", sends: vec![s], status: None, outcome, pass });

        // 2: true at once.
        let (head, sets) = self.sets().await?;
        let fees = self.fees().await?;
        let s = self.send(nonce, fees, Vec::new(), &sets[1].1).await?;
        let outcome = self.settle(nonce, &[&s], head, head + 6).await?;
        let pass = matches!(outcome, Outcome::Included { after_blocks, .. } if after_blocks <= 2);
        nonce += u64::from(matches!(outcome, Outcome::Included { .. }));
        done(Action { case: 2, name: "now", sends: vec![s], status: None, outcome, pass });

        // 3: held, then another at its nonce and fees.
        let (head, sets) = self.sets().await?;
        let held = self.fees().await?;
        let first = self.send(nonce, held, Vec::new(), &sets[2].1).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let status = self.rpc.call("base_transactionStatus", json!([format!("{:#x}", first.tx)])).await.ok();
        let second = self.send(nonce, held, vec![0x01], &sets[2].1).await?;
        let outcome = self.settle(nonce, &[&first, &second], head, head + 25).await?;
        let pass = outcome == Outcome::Expired { bound: head + 25 };
        done(Action { case: 3, name: "held", sends: vec![first, second], status, outcome, pass });

        // 4: after that bound, the nonce and fees again.
        let (head, sets) = self.sets().await?;
        let s = self.send(nonce, held, vec![0x02], &sets[0].1).await?;
        let outcome = self.settle(nonce, &[&s], head, head + 6).await?;
        let pass = matches!(outcome, Outcome::Expired { .. } | Outcome::Refused);
        done(Action { case: 4, name: "after", sends: vec![s], status: None, outcome, pass });

        // 5: an arm's eight, all true.
        let (head, sets) = self.sets().await?;
        let fees = self.fees().await?;
        let s = self.send(nonce, fees, Vec::new(), &sets[3].1).await?;
        let outcome = self.settle(nonce, &[&s], head, head + 6).await?;
        let pass = matches!(outcome, Outcome::Included { .. });
        nonce += u64::from(pass);
        done(Action { case: 5, name: "eight", sends: vec![s], status: None, outcome, pass });

        // 6: five replacements, back to back, an eighth more each.
        let (head, sets) = self.sets().await?;
        let (mut tip, mut max_fee) = self.fees().await?;
        let mut sends = Vec::new();
        for _ in 0..5 {
            sends.push(self.send(nonce, (tip, max_fee), Vec::new(), &sets[0].1).await?);
            (tip, max_fee) = (bumped(tip), bumped(max_fee));
        }
        let outcome = self.settle(nonce, &sends.iter().collect::<Vec<_>>(), head, head + 6).await?;
        let pass = outcome == Outcome::Expired { bound: head + 6 };
        done(Action { case: 6, name: "bumps", sends, status: None, outcome, pass });
        Ok(all)
    }

    async fn sets(&self) -> Result<(u64, Vec<(String, Value)>), String> {
        let head = self.chain.head().await?;
        Ok((head, probe_predicates(head, self.key.address())))
    }

    /// The tip, and twice the base fee over it, as the canary signs.
    async fn fees(&self) -> Result<(u128, u128), String> {
        let block = self.rpc.call("eth_getBlockByNumber", json!(["latest", false])).await.map_err(|e| e.to_string())?;
        let base = block
            .get("baseFeePerGas")
            .and_then(Value::as_str)
            .and_then(|s| u128::from_str_radix(s.strip_prefix("0x")?, 16).ok())
            .ok_or("the latest block has no base fee")?;
        Ok((TIP_WEI_PER_GAS, base.saturating_mul(2).saturating_add(TIP_WEI_PER_GAS)))
    }

    async fn send(&self, nonce: u64, (tip, max_fee): (u128, u128), data: Vec<u8>, validity: &Value) -> Result<Sent, String> {
        let me = self.key.address();
        let tx = Eip1559Tx { chain_id: self.chain_id, nonce, max_priority_fee_per_gas: tip, max_fee_per_gas: max_fee, gas_limit: GAS_LIMIT, to: me, data };
        let signed = self.key.sign_eip1559(&tx).map_err(|e| e.to_string())?;
        let started = Instant::now();
        let answer = self.submit.send(&signed.raw, validity).await;
        let answer_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (answer, accepted) = match answer {
            SendAnswer::Accepted(_) => ("accepted".to_string(), true),
            SendAnswer::Refused(why) => (format!("refused: {why}"), false),
            SendAnswer::Silent(why) => (format!("silent: {why}"), false),
        };
        Ok(Sent { tx: signed.hash, nonce, priority_wei_per_gas: tip, answer, accepted, answer_ms })
    }

    /// Where `sends` ended: a receipt for one of them, or `bound` passed with
    /// the nonce still `nonce`. A silent send may have landed, so it is
    /// watched as an accepted one is.
    async fn settle(&self, nonce: u64, sends: &[&Sent], head: u64, bound: u64) -> Result<Outcome, String> {
        if sends.iter().all(|s| s.answer.starts_with("refused")) {
            return Ok(Outcome::Refused);
        }
        // Two seconds a block, and a minute's grace.
        let give_up = Instant::now() + Duration::from_secs((bound.saturating_sub(head) + 30) * 2);
        loop {
            let now = self.chain.head().await?;
            for s in sends {
                if let Some(receipt) = self.chain.receipt(s.tx).await? {
                    let block = receipt
                        .get("blockNumber")
                        .and_then(Value::as_str)
                        .and_then(|b| u64::from_str_radix(b.strip_prefix("0x")?, 16).ok())
                        .ok_or("a receipt without a block number")?;
                    return Ok(Outcome::Included { tx: s.tx, block, after_blocks: block.saturating_sub(head) });
                }
            }
            if now > bound {
                let moved = self.chain.nonce(self.key.address()).await?;
                return Ok(if moved == nonce {
                    Outcome::Expired { bound }
                } else {
                    Outcome::Unsettled { why: format!("the nonce moved to {moved} with no receipt among ours") }
                });
            }
            if Instant::now() > give_up {
                return Ok(Outcome::Unsettled { why: format!("head {now} never passed {bound}") });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// An eighth more, rounded up, as armed-live bumps.
fn bumped(wei: u128) -> u128 {
    wei.saturating_mul(1125).div_ceil(1000)
}
