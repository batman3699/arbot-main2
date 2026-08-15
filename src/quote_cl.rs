//! Shared concentrated-liquidity quoting core for the UniV3-style venues
//! (Uniswap V3 / PancakeSwap V3 via [`crate::quote_univ3`] and Aerodrome
//! Slipstream via [`crate::quote_slipstream`]).
//!
//! Both venues expose an identical `quoteExactInput(bytes,uint256)` QuoterV2
//! entrypoint and were carrying byte-for-byte copies of the per-block quote
//! path (single quote, cached; and the Multicall3-batched grid quote). The only
//! real differences between the two quoters are the factory ABI used by
//! `pool_address`/`validate` (fee `uint24` vs tickSpacing `int24`) and their log
//! targets — none of which touch this hot path. Keeping the quote path in one
//! place means a fix (e.g. the grid retry-at-Latest below) lands for every CL
//! venue at once and the two cannot silently drift.
//!
//! The core is deliberately typed only over the provider + the quoter address,
//! not over the abigen contract wrapper: it builds the `quoteExactInput`
//! calldata directly. `quote_exact_input_calldata_matches_abigen` in
//! `quote_univ3` pins that hand-built calldata to what abigen emits.

use anyhow::{ensure, Result};
use ethers::abi::{ParamType, Token};
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;
use ethers::types::transaction::eip2718::TypedTransaction;
use lru::LruCache;
use once_cell::sync::Lazy;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

pub(crate) const QUOTE_CACHE_TTL: Duration = Duration::from_secs(30);
const QUOTE_CACHE_SIZE: usize = 2048;

/// Canonical Multicall3 address (same on every chain arbot targets).
const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// `quoteExactInput(bytes,uint256)` 4-byte selector, shared by the UniV3 and
/// Slipstream QuoterV2 contracts.
static QUOTE_EXACT_INPUT_SELECTOR: Lazy<[u8; 4]> = Lazy::new(|| {
    let hash = ethers::utils::keccak256(b"quoteExactInput(bytes,uint256)");
    [hash[0], hash[1], hash[2], hash[3]]
});

/// ABI-encoded calldata for `quoteExactInput(bytes path, uint256 amountIn)`.
/// Equivalent to the abigen contract call's `.calldata()` (asserted by test).
pub(crate) fn quote_exact_input_calldata(path: &Bytes, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(4 + 96);
    data.extend_from_slice(QUOTE_EXACT_INPUT_SELECTOR.as_slice());
    data.extend(ethers::abi::encode(&[
        Token::Bytes(path.to_vec()),
        Token::Uint(amount),
    ]));
    Bytes::from(data)
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct QuoteCacheKey {
    path: Bytes,
    amount_in: U256,
    block: U64,
}

struct CachedQuote {
    inserted: Instant,
    value: U256,
}

/// TTL'd LRU cache of single-path CL quotes, keyed by (path, amount, block).
pub(crate) struct ClQuoteCache {
    cache: Mutex<LruCache<QuoteCacheKey, CachedQuote>>,
}

impl ClQuoteCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(QUOTE_CACHE_SIZE).unwrap_or(NonZeroUsize::MIN),
            )),
        }
    }
}

fn block_id_for(block: U64) -> BlockId {
    if block.is_zero() {
        BlockId::Number(BlockNumber::Latest)
    } else {
        BlockId::Number(BlockNumber::Number(block))
    }
}

/// Quote a single exact-input path against a QuoterV2, cached with a TTL.
///
/// Pins the `eth_call` to `block` when non-zero and retries once at `Latest` if
/// the node reports the pinned block is out of range (a lagging/failover node
/// behind the requested head).
pub(crate) async fn cl_quote_path<C>(
    provider: &Arc<Provider<C>>,
    quoter_addr: Address,
    cache: &ClQuoteCache,
    path: Vec<(Address, Option<u32>)>,
    amount_in: U256,
    block: U64,
    quoter_label: &str,
) -> Result<U256>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let path_bytes = Bytes::from(crate::util::encode_univ3_path(&path)?);
    let cache_key = QuoteCacheKey {
        path: path_bytes.clone(),
        amount_in,
        block,
    };

    {
        let mut guard = cache.cache.lock().await;
        if let Some(cached) = guard.get(&cache_key) {
            if cached.inserted.elapsed() <= QUOTE_CACHE_TTL {
                return Ok(cached.value);
            }
            guard.pop(&cache_key);
        }
    }

    let calldata = quote_exact_input_calldata(&path_bytes, amount_in);
    let tx: TypedTransaction = TransactionRequest::new()
        .to(quoter_addr)
        .data(calldata)
        .into();

    let raw = match provider.call(&tx, Some(block_id_for(block))).await {
        Ok(value) => value,
        Err(err) if !block.is_zero() && crate::quote_common::is_block_out_of_range_error(&err) => {
            provider
                .call(&tx, Some(BlockId::Number(BlockNumber::Latest)))
                .await?
        }
        Err(err) => return Err(err.into()),
    };

    let raw_bytes = raw.as_ref();
    ensure!(
        raw_bytes.len() >= 32,
        "{quoter_label} returned insufficient data: expected at least 32 bytes got {}",
        raw_bytes.len()
    );
    let out = U256::from_big_endian(&raw_bytes[..32]);

    let mut guard = cache.cache.lock().await;
    guard.put(
        cache_key,
        CachedQuote {
            inserted: Instant::now(),
            value: out,
        },
    );
    Ok(out)
}

/// Quote a fixed set of input amounts for ONE path in a SINGLE `eth_call` by
/// batching `quoteExactInput` through Multicall3.aggregate3. Returns one
/// optional `amountOut` per input amount (None when that sub-call failed or
/// returned empty). This collapses N sequential quote round-trips into one RTT —
/// the dominant scan-latency cost is per-quote network RTT, so this is the
/// decisive latency lever.
///
/// Like [`cl_quote_path`], the batched call is pinned to `block` and retried
/// once at `Latest` on a block-out-of-range error. Without that fallback a
/// websocket head that leads the quoting node (e.g. during RPC failover) would
/// fail the whole batch and force the slow per-amount path.
pub(crate) async fn cl_quote_path_grid<C>(
    provider: &Arc<Provider<C>>,
    quoter_addr: Address,
    path: Vec<(Address, Option<u32>)>,
    amounts: &[U256],
    block: U64,
) -> Result<Vec<Option<U256>>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    if amounts.is_empty() {
        return Ok(Vec::new());
    }
    let path_bytes = Bytes::from(crate::util::encode_univ3_path(&path)?);
    let calls: Vec<(Address, Vec<u8>)> = amounts
        .iter()
        .map(|&amount| {
            (
                quoter_addr,
                quote_exact_input_calldata(&path_bytes, amount).to_vec(),
            )
        })
        .collect();

    let results = multicall3_aggregate3(provider, &calls, block).await?;
    Ok(results
        .into_iter()
        .map(|ret| match ret {
            Some(bytes) if bytes.len() >= 32 => Some(U256::from_big_endian(&bytes[..32])),
            _ => None,
        })
        .collect())
}

/// Execute many independent `eth_call`s in ONE round-trip via
/// `Multicall3.aggregate3`, returning each sub-call's raw return data (`None`
/// when that sub-call reverted or returned nothing).
///
/// Per-sub-call failure is tolerated (`allowFailure: true`), so one dead pool
/// cannot poison the batch. This is the primitive that spec §3.4 requires —
/// "All pool-state reads via Multicall3. Never N sequential RPC round-trips per
/// scan" — and it is the decisive latency lever, because scan cost is dominated
/// by per-call network RTT rather than by node execution time.
///
/// The batch is pinned to `block` and retried once at `Latest` on a
/// block-out-of-range error, matching [`cl_quote_path`]; without that a
/// websocket head leading the quoting node would fail the whole batch.
pub(crate) async fn multicall3_aggregate3<C>(
    provider: &Arc<Provider<C>>,
    calls: &[(Address, Vec<u8>)],
    block: U64,
) -> Result<Vec<Option<Vec<u8>>>>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let multicall3 = Address::from_str(MULTICALL3)
        .map_err(|err| anyhow::anyhow!("invalid multicall3 address: {err}"))?;

    let call_tokens: Vec<Token> = calls
        .iter()
        .map(|(target, data)| {
            Token::Tuple(vec![
                Token::Address(*target),
                Token::Bool(true), // allowFailure: one bad sub-call must not fail the batch
                Token::Bytes(data.clone()),
            ])
        })
        .collect();

    // aggregate3((address,bool,bytes)[]) selector = 0x82ad56cb
    let mut data = vec![0x82u8, 0xad, 0x56, 0xcb];
    data.extend(ethers::abi::encode(&[Token::Array(call_tokens)]));

    let tx: TypedTransaction = TransactionRequest::new()
        .to(multicall3)
        .data(Bytes::from(data))
        .into();
    let raw = match provider.call(&tx, Some(block_id_for(block))).await {
        Ok(value) => value,
        Err(err) if !block.is_zero() && crate::quote_common::is_block_out_of_range_error(&err) => {
            provider
                .call(&tx, Some(BlockId::Number(BlockNumber::Latest)))
                .await?
        }
        Err(err) => return Err(err.into()),
    };

    let decoded = ethers::abi::decode(
        &[ParamType::Array(Box::new(ParamType::Tuple(vec![
            ParamType::Bool,
            ParamType::Bytes,
        ])))],
        raw.as_ref(),
    )?;

    let mut out = vec![None; calls.len()];
    if let Some(Token::Array(results)) = decoded.into_iter().next() {
        for (i, result) in results.into_iter().enumerate() {
            if i >= out.len() {
                break;
            }
            if let Token::Tuple(fields) = result {
                let success = matches!(fields.first(), Some(Token::Bool(true)));
                if success {
                    if let Some(Token::Bytes(return_data)) = fields.get(1) {
                        if !return_data.is_empty() {
                            out[i] = Some(return_data.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_matches_signature() {
        // keccak256("quoteExactInput(bytes,uint256)")[..4]
        assert_eq!(
            QUOTE_EXACT_INPUT_SELECTOR.as_slice(),
            &ethers::utils::id("quoteExactInput(bytes,uint256)")[..4]
        );
    }
}
