//! Chain reads for the pool book, over the read-only transport.
//!
//! Everything is batched through Multicall3 and **pinned to a block**: a pool
//! state assembled from reads at different blocks is a state that never existed,
//! and the pricing done on it would be pricing a chimera.

use crate::live::abi::{self, selector, MULTICALL3};
use alloy_primitives::{keccak256, Address, B256};
use apex_chain::rpc::{parse_quantity, RpcError, RpcTransport};
use apex_types::compat::{address_to_alloy, u256_to_ethers};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Sub-calls per `aggregate3`. The event census found one multicall carrying
/// 1,440 sub-calls fails outright, and the failure is total; a hundred is well
/// inside what providers accept.
pub const CHUNK: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum ReadError {
    Rpc(RpcError),
    /// The node answered something that does not decode.
    Malformed(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "{e}"),
            Self::Malformed(d) => write!(f, "malformed answer: {d}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<RpcError> for ReadError {
    fn from(e: RpcError) -> Self {
        Self::Rpc(e)
    }
}

#[derive(Clone)]
pub struct ChainReads {
    rpc: Arc<dyn RpcTransport>,
}

impl ChainReads {
    pub fn new(rpc: Arc<dyn RpcTransport>) -> Self {
        Self { rpc }
    }

    pub async fn head(&self) -> Result<u64, ReadError> {
        let v = self.rpc.call("eth_blockNumber", json!([])).await?;
        parse_quantity(&v).ok_or_else(|| ReadError::Malformed(format!("eth_blockNumber: {v}")))
    }

    /// Every call, at `block`. `None` for a call that failed — reverted, or a
    /// selector the target does not have — which is an answer about that call
    /// and not a reason to lose the others.
    pub async fn multicall(
        &self,
        calls: &[(Address, Vec<u8>)],
        block: u64,
    ) -> Result<Vec<Option<Vec<u8>>>, ReadError> {
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(CHUNK) {
            let data = abi::encode_aggregate3(chunk);
            let answer = self
                .rpc
                .call(
                    "eth_call",
                    json!([
                        { "to": MULTICALL3, "data": format!("0x{}", alloy_primitives::hex::encode(&data)) },
                        format!("0x{block:x}")
                    ]),
                )
                .await?;
            let bytes = answer
                .as_str()
                .and_then(|s| alloy_primitives::hex::decode(s).ok())
                .ok_or_else(|| ReadError::Malformed("aggregate3 answer is not hex".into()))?;
            let results = abi::decode_aggregate3(&bytes)
                .ok_or_else(|| ReadError::Malformed("aggregate3 answer does not decode".into()))?;
            if results.len() != chunk.len() {
                // A short list would shift every later answer onto the wrong call.
                return Err(ReadError::Malformed(format!(
                    "aggregate3 answered {} of {} calls",
                    results.len(),
                    chunk.len()
                )));
            }
            out.extend(results.into_iter().map(|(ok, data)| ok.then_some(data)));
        }
        Ok(out)
    }

    /// `extcodehash`, computed from the code itself at `block`. An address with
    /// no code hashes to the empty-code hash, which admission refuses.
    pub async fn code_hash(&self, a: Address, block: u64) -> Result<B256, ReadError> {
        let v = self.rpc.call("eth_getCode", json!([a, format!("0x{block:x}")])).await?;
        let code = v
            .as_str()
            .and_then(|s| alloy_primitives::hex::decode(s).ok())
            .ok_or_else(|| ReadError::Malformed(format!("eth_getCode for {a}")))?;
        Ok(keccak256(&code))
    }
}

/// `apex-venues`' ladder builder, reading through this transport. Two batched
/// round trips per pool whatever the tick count — the reason the trait is shaped
/// the way it is.
#[async_trait]
impl apex_venues::cl_ticks::TickDataSource for ChainReads {
    async fn tick_words(
        &self,
        pool: ethers_core::types::Address,
        word_positions: &[i16],
        block: ethers_core::types::U64,
    ) -> anyhow::Result<Vec<Option<ethers_core::types::U256>>> {
        let pool = address_to_alloy(pool);
        let calls: Vec<(Address, Vec<u8>)> = word_positions
            .iter()
            .map(|w| (pool, abi::call_signed(selector::TICK_BITMAP, i64::from(*w))))
            .collect();
        let answers = self.multicall(&calls, block.as_u64()).await?;
        Ok(answers
            .into_iter()
            .map(|a| a.and_then(|d| abi::word_u256(&d, 0)).map(u256_to_ethers))
            .collect())
    }

    async fn liquidity_net(
        &self,
        pool: ethers_core::types::Address,
        ticks: &[i32],
        block: ethers_core::types::U64,
    ) -> anyhow::Result<Vec<Option<i128>>> {
        let pool = address_to_alloy(pool);
        let calls: Vec<(Address, Vec<u8>)> = ticks
            .iter()
            .map(|t| (pool, abi::call_signed(selector::TICKS, i64::from(*t))))
            .collect();
        let answers = self.multicall(&calls, block.as_u64()).await?;
        Ok(answers
            .into_iter()
            .map(|a| a.and_then(|d| apex_venues::cl_ticks::decode_liquidity_net(&d)))
            .collect())
    }
}
