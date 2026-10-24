//! Armed-live's chain (Task 8.5 R36): the live worker's four reads over the
//! run's read-only transport, and the boot's check that the executor lets
//! every sender trade.

use super::live::{LiveChain, Stops};
use crate::live::abi::{self, selector};
use crate::live::reads::ChainReads;
use alloy_primitives::{keccak256, Address, B256, U256};
use apex_chain::rpc::RpcTransport;
use serde_json::{json, Value};
use std::sync::Arc;

/// The live worker's reads, at `latest`: a validity transaction waits in the
/// builder, so only a landing moves a sender's nonce.
pub struct RpcChain {
    rpc: Arc<dyn RpcTransport>,
    reads: ChainReads,
}

impl RpcChain {
    pub fn new(rpc: Arc<dyn RpcTransport>) -> Self {
        Self { reads: ChainReads::new(Arc::clone(&rpc)), rpc }
    }

    async fn quantity(&self, method: &str, params: Value) -> Result<U256, String> {
        let v = self.rpc.call(method, params).await.map_err(|e| e.to_string())?;
        v.as_str().and_then(|s| U256::from_str_radix(s.strip_prefix("0x")?, 16).ok()).ok_or_else(|| format!("{method} answered {v}"))
    }
}

#[async_trait::async_trait]
impl LiveChain for RpcChain {
    async fn head(&self) -> Result<u64, String> {
        let n = self.quantity("eth_blockNumber", json!([])).await?;
        u64::try_from(n).map_err(|e| e.to_string())
    }

    async fn nonce(&self, a: Address) -> Result<u64, String> {
        let n = self.quantity("eth_getTransactionCount", json!([format!("{a:#x}"), "latest"])).await?;
        u64::try_from(n).map_err(|e| e.to_string())
    }

    async fn nonce_at(&self, a: Address, block: u64) -> Result<u64, String> {
        let n = self.quantity("eth_getTransactionCount", json!([format!("{a:#x}"), format!("{block:#x}")])).await?;
        u64::try_from(n).map_err(|e| e.to_string())
    }

    /// One aggregated `eth_call`: `isCircuitOpen()`, then `executors(s)` for
    /// each sender. An answer that does not decode reads as a stop.
    async fn stops(&self, executor: Address, senders: &[Address], block: u64) -> Result<Stops, String> {
        let mut calls = vec![(executor, abi::call0(selector::IS_CIRCUIT_OPEN))];
        calls.extend(senders.iter().map(|s| (executor, abi::call_address(selector::EXECUTORS, *s))));
        let answers = self.reads.multicall(&calls, block).await.map_err(|e| e.to_string())?;
        let yes = |i: usize| answers.get(i).and_then(Option::as_deref).and_then(|d| abi::word_u256(d, 0)).map(|w| w == U256::from(1));
        Ok(Stops { circuit_open: yes(0) != Some(false), allowed: (1..=senders.len()).map(|i| yes(i) == Some(true)).collect() })
    }

    async fn balance(&self, a: Address) -> Result<u128, String> {
        let wei = self.quantity("eth_getBalance", json!([format!("{a:#x}"), "latest"])).await?;
        u128::try_from(wei).map_err(|e| e.to_string())
    }

    async fn receipt(&self, tx: B256) -> Result<Option<Value>, String> {
        let v = self.rpc.call("eth_getTransactionReceipt", json!([format!("{tx:#x}")])).await.map_err(|e| e.to_string())?;
        Ok((!v.is_null()).then_some(v))
    }
}

/// `eth_call`'s params for the executor's `executors(account)`.
pub fn executors_call(executor: Address, account: Address) -> Value {
    let mut data = keccak256(b"executors(address)")[..4].to_vec();
    data.extend_from_slice(B256::left_padding_from(account.as_slice()).as_slice());
    json!([{ "to": format!("{executor:#x}"), "data": format!("0x{}", alloy_primitives::hex::encode(data)) }, "latest"])
}

/// Every sender is one of the executor's executors, or the boot stops,
/// naming those that are not: the owner authorizes them first.
pub async fn check_executors(rpc: &dyn RpcTransport, executor: Address, senders: &[Address]) -> Result<(), String> {
    let mut refused = Vec::new();
    for sender in senders {
        let v = rpc.call("eth_call", executors_call(executor, *sender)).await.map_err(|e| format!("executors({sender:#x}): {e}"))?;
        let allowed = v.as_str().and_then(|s| U256::from_str_radix(s.strip_prefix("0x")?, 16).ok());
        if allowed != Some(U256::from(1)) {
            refused.push(format!("{sender:#x}"));
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the executor {executor:#x} does not let {} trade: the owner authorizes them first (script/AuthorizeArmedSenders.s.sol)",
        refused.join(", ")
    ))
}
