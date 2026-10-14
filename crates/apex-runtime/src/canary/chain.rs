//! Settlement and reconciliation (spec §7): poll the receipt, read what the
//! executor paid, reconcile it, record it.

use super::ledger::{Outcome, Sent, Settled};
use super::limits::Canary;
use super::send::InFlight;
use crate::live::adapter::Attribution;
use crate::plane::{Decline, SettlementFeed};
use alloy_primitives::{Address, B256, U256};
use apex_chain::adapter::{AdapterError, AdapterResult};
use apex_chain::base::observe::{Receipt, TransactionObservation};
use apex_chain::base::reconcile::{reconcile, BalanceDelta, ReconcileInputs};
use apex_chain::rpc::RpcTransport;
use apex_types::cost::GasLimit;
use apex_types::ids::TicketId;
use apex_types::pnl::{OptimizationLayer, PnlAttribution, UsdBounds};
use apex_types::sim::RevertClass;
use apex_types::time::UnixNanos;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const PROFIT_REALISED: &str = "0x230bad4a4399a7f96d7dc55ada0f3e590e98017ef0fb0ef2ae55914350e5d832";
/// A revert under this much gas stopped before the swaps: the deadline, the
/// commitment, the caller (spec §7).
pub const EARLY_REVERT_GAS: u64 = 100_000;

/// A receipt, and what the executor paid and said it realised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    pub receipt: Receipt,
    /// WETH transferred from the executor to the recipients.
    pub paid_wei: u128,
    /// `ProfitRealised`'s gross, when the executor emitted it.
    pub realised_wei: Option<u128>,
}

fn quantity(v: &Value, field: &str) -> Result<u128, String> {
    v.get(field)
        .and_then(Value::as_str)
        .and_then(|s| u128::from_str_radix(s.strip_prefix("0x")?, 16).ok())
        .ok_or_else(|| format!("the receipt's {field} is missing or not a quantity"))
}

fn topic_address(t: &Value) -> Option<Address> {
    let s = t.as_str()?.strip_prefix("0x")?;
    (s.len() == 64).then(|| s[24..].parse::<Address>().ok()).flatten()
}

fn data_word(log: &Value, i: usize) -> Option<u128> {
    let d = log.get("data")?.as_str()?.strip_prefix("0x")?;
    let w = d.get(i * 64..(i + 1) * 64)?;
    let v = U256::from_str_radix(w, 16).ok()?;
    (v.bit_len() <= 128).then(|| v.to::<u128>())
}

/// Read a Base receipt. **Fails on a missing `l1Fee`**: a zero would understate
/// the cost of every trade (review focus 2).
pub fn read_receipt(v: &Value, weth: Address, executor: Address, recipients: &[Address]) -> Result<Read, String> {
    let tx = v.get("transactionHash").and_then(Value::as_str).and_then(|s| s.parse::<B256>().ok())
        .ok_or("the receipt has no transaction hash")?;
    let receipt = Receipt {
        tx,
        block_number: u64::try_from(quantity(v, "blockNumber")?).map_err(|e| e.to_string())?,
        success: quantity(v, "status")? == 1,
        gas_used: u64::try_from(quantity(v, "gasUsed")?).map_err(|e| e.to_string())?,
        effective_gas_price_wei: quantity(v, "effectiveGasPrice")?,
        l1_fee_wei: quantity(v, "l1Fee")?,
    };
    let logs = v.get("logs").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut paid_wei = 0u128;
    let mut realised_wei = None;
    for log in &logs {
        let address = log.get("address").and_then(Value::as_str).and_then(|s| s.parse::<Address>().ok());
        let topics = log.get("topics").and_then(Value::as_array).cloned().unwrap_or_default();
        let topic0 = topics.first().and_then(Value::as_str).map(str::to_lowercase);
        if address == Some(weth)
            && topic0.as_deref() == Some(TRANSFER)
            && topics.get(1).and_then(topic_address) == Some(executor)
            && topics.get(2).and_then(topic_address).is_some_and(|to| recipients.contains(&to))
        {
            paid_wei = paid_wei.saturating_add(data_word(log, 0).ok_or("a Transfer's amount did not decode")?);
        }
        if address == Some(executor) && topic0.as_deref() == Some(PROFIT_REALISED) {
            realised_wei = Some(data_word(log, 0).ok_or("ProfitRealised's gross did not decode")?);
        }
    }
    Ok(Read { receipt, paid_wei, realised_wei })
}

#[derive(Clone, Debug)]
pub struct ChainSettings {
    pub weth: Address,
    pub executor: Address,
    pub trader: Address,
    pub recipients: Vec<Address>,
    pub poll: Duration,
    pub timeout: Duration,
}

/// What recovery found for a send a crash left open.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    Landed(Box<PnlAttribution>),
    Reverted(RevertClass),
    NotFound { nonce_consumed: bool },
}

pub struct CanaryChain {
    rpc: Arc<dyn RpcTransport>,
    canary: Arc<Canary>,
    in_flight: Arc<InFlight>,
    s: ChainSettings,
}

fn now_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()))
        .unwrap_or(u64::MAX)
}

fn unix_nanos() -> UnixNanos {
    UnixNanos(u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())).unwrap_or(u64::MAX))
}

impl CanaryChain {
    pub fn new(rpc: Arc<dyn RpcTransport>, canary: Arc<Canary>, in_flight: Arc<InFlight>, s: ChainSettings) -> Self {
        Self { rpc, canary, in_flight, s }
    }

    async fn receipt(&self, tx: B256) -> Option<Value> {
        match self.rpc.call("eth_getTransactionReceipt", json!([tx])).await {
            Ok(v) if !v.is_null() => Some(v),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(%tx, error = %e, "a receipt poll failed; polling on");
                None
            }
        }
    }

    async fn latest_nonce(&self) -> Option<u64> {
        let v = self.rpc.call("eth_getTransactionCount", json!([self.s.trader, "latest"])).await.ok()?;
        u64::from_str_radix(v.as_str()?.strip_prefix("0x")?, 16).ok()
    }

    /// The revert's class from a trace, or `Unknown` where the endpoint will
    /// not trace.
    async fn revert_class(&self, tx: B256) -> RevertClass {
        let Ok(t) = self.rpc.call("debug_traceTransaction", json!([tx, { "tracer": "callTracer" }])).await else {
            return RevertClass::Unknown;
        };
        let data = t.get("output").and_then(Value::as_str)
            .and_then(|s| alloy_primitives::hex::decode(s).ok())
            .unwrap_or_default();
        crate::live::sim::classify(&data, t.get("error").and_then(Value::as_str))
    }

    fn record(&self, tx: B256, settled: Settled) {
        if let Err(e) = self.canary.settle(Outcome { tx, at_ms: now_ms(), settled }) {
            tracing::error!(%tx, error = %e, "the canary ledger refused an outcome");
        }
    }

    /// Reconcile `tx` from the receipt `observe` found (or one fetched now).
    async fn reconcile_tx(&self, sent: &Sent, receipt: Value) -> AdapterResult<PnlAttribution> {
        let tx = sent.tx;
        let read = read_receipt(&receipt, self.s.weth, self.s.executor, &self.s.recipients).map_err(|detail| {
            self.record(tx, Settled::Unexplained { detail: detail.clone() });
            AdapterError::Uninterpretable { detail }
        })?;
        let r = read.receipt;
        let cost = r.total_chain_fee_wei();
        if !r.success {
            let class = self.revert_class(tx).await;
            self.record(tx, Settled::Reverted {
                block: r.block_number,
                gas_used: r.gas_used,
                effective_gas_price_wei: r.effective_gas_price_wei,
                l1_fee_wei: r.l1_fee_wei,
                cost_wei: cost,
                class: format!("{class:?}"),
                early: r.gas_used < EARLY_REVERT_GAS,
            });
            self.in_flight.remove(&tx);
            return Err(AdapterError::Reverted { tx, class, gas_used: r.gas_used, spent_wei: cost });
        }
        if read.realised_wei != Some(read.paid_wei) {
            let detail = format!("paid {} wei, and ProfitRealised says {:?}", read.paid_wei, read.realised_wei);
            self.record(tx, Settled::Unexplained { detail: detail.clone() });
            self.in_flight.remove(&tx);
            return Err(AdapterError::Uninterpretable { detail });
        }
        let inputs = ReconcileInputs {
            ticket: TicketId(sent.ticket),
            chain: sent.chain,
            strategy: sent.strategy,
            venues: sent.venues.clone(),
            route_hash: sent.route_hash,
            optimization_layers: vec![OptimizationLayer::SinglePath],
            profit_token: self.s.weth,
            gas_limit: GasLimit(sent.gas_limit),
            // §2.10: the token decides; no price is invented here.
            usd_bounds: UsdBounds { low: 0.0, high: 0.0 },
            dex_fees_wei: 0,
            flash_fee_wei: 0,
            calldata_bytes: 0,
            compressed_data_estimate: 0,
        };
        let delta = BalanceDelta { token: self.s.weth, delta: i128::try_from(read.paid_wei).unwrap_or(i128::MAX) };
        let pnl = reconcile(&r, &[delta], &inputs).map_err(|e| AdapterError::Uninterpretable { detail: e.to_string() })?;
        self.record(tx, Settled::Landed {
            block: r.block_number,
            gas_used: r.gas_used,
            effective_gas_price_wei: r.effective_gas_price_wei,
            l1_fee_wei: r.l1_fee_wei,
            gross_wei: pnl.gross_profit,
            cost_wei: cost,
            net_wei: pnl.net_profit_token,
        });
        self.in_flight.remove(&tx);
        Ok(pnl)
    }

    /// What a crash left open (spec §9): the receipt if there is one, and
    /// otherwise whether the nonce was consumed.
    pub async fn resolve_open(&self, sent: &Sent) -> Resolved {
        self.in_flight.insert(sent.clone());
        match self.receipt(sent.tx).await {
            Some(v) => match self.reconcile_tx(sent, v).await {
                Ok(pnl) => Resolved::Landed(Box::new(pnl)),
                Err(AdapterError::Reverted { class, .. }) => Resolved::Reverted(class),
                Err(_) => Resolved::NotFound { nonce_consumed: true },
            },
            None => Resolved::NotFound { nonce_consumed: self.latest_nonce().await.is_some_and(|n| n > sent.nonce) },
        }
    }
}

#[async_trait::async_trait]
impl SettlementFeed for CanaryChain {
    async fn observe(&self, tx: B256) -> Result<Vec<TransactionObservation>, Decline> {
        let included = |v: Value| -> Result<Vec<TransactionObservation>, Decline> {
            // A receipt exists, so the transaction landed: one that cannot be
            // read is money the canary cannot account for, and it halts.
            let read = read_receipt(&v, self.s.weth, self.s.executor, &self.s.recipients).map_err(|detail| {
                self.record(tx, Settled::Unexplained { detail: format!("its receipt did not read: {detail}") });
                Decline::ChainUnavailable { detail }
            })?;
            self.in_flight.set_receipt(&tx, v);
            Ok(vec![TransactionObservation::included(read.receipt, unix_nanos())])
        };
        let deadline = Instant::now() + self.s.timeout;
        loop {
            if let Some(v) = self.receipt(tx).await {
                return included(v);
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(self.s.poll).await;
        }
        // The timeout. If the nonce was consumed, look once more.
        let consumed = match self.in_flight.get(&tx).map(|p| p.sent.nonce) {
            Some(n) => self.latest_nonce().await.is_some_and(|latest| latest > n),
            None => false,
        };
        if consumed {
            if let Some(v) = self.receipt(tx).await {
                return included(v);
            }
        }
        self.record(tx, Settled::Unresolved {
            detail: format!("no receipt within {} ms; nonce consumed: {consumed}", self.s.timeout.as_millis()),
        });
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl Attribution for CanaryChain {
    async fn attribute(&self, tx: B256) -> AdapterResult<PnlAttribution> {
        let Some(p) = self.in_flight.get(&tx) else {
            return Err(AdapterError::Uninterpretable { detail: format!("{tx} was not sent by this canary") });
        };
        let receipt = match p.receipt {
            Some(v) => v,
            None => self.receipt(tx).await.ok_or_else(|| AdapterError::Unreachable { detail: format!("no receipt for {tx}") })?,
        };
        self.reconcile_tx(&p.sent, receipt).await
    }
}
