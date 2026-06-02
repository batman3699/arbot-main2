use anyhow::Result;
use ethers::{prelude::*, providers::JsonRpcClient};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

fn is_block_out_of_range_error(err: &impl std::fmt::Display) -> bool {
    let message = err.to_string();
    let lower = message.to_ascii_lowercase();
    lower.contains("blockoutofrangeerror")
        || lower.contains("block out of range")
        || lower.contains("header not found")
        || lower.contains("requested was")
}

fn should_cache_quote(block: U64, used_latest_fallback: bool) -> bool {
    block.is_zero() || !used_latest_fallback
}

abigen!(
    ICurvePoolGeneric,
    r#"[
        function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) external returns (uint256)
        function exchange_underlying(int128 i, int128 j, uint256 dx, uint256 min_dy) external returns (uint256)
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256)
    ]"#,
);

#[allow(dead_code)]
pub async fn quote_curve_getdy<C>(
    provider: Arc<Provider<C>>,
    pool: Address,
    i: i128,
    j: i128,
    dx: U256,
) -> Result<U256>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = ICurvePoolGeneric::new(pool, provider);
    let out = contract.get_dy(i, j, dx).call().await?;
    Ok(out)
}

pub struct CurveQuote<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    provider: Arc<Provider<C>>,
    cache: Mutex<HashMap<CurveQuoteKey, U256>>,
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct CurveQuoteKey {
    pool: Address,
    i: i128,
    j: i128,
    amount_in: U256,
    block: U64,
}

impl<C> CurveQuote<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn new(provider: Arc<Provider<C>>) -> Self {
        Self {
            provider,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn quote_get_dy(
        &self,
        pool: Address,
        i: i128,
        j: i128,
        amount_in: U256,
        block: U64,
    ) -> Result<U256> {
        let cache_key = CurveQuoteKey {
            pool,
            i,
            j,
            amount_in,
            block,
        };

        {
            let cache = self.cache.lock().await;
            if let Some(cached) = cache.get(&cache_key) {
                return Ok(*cached);
            }
        }

        let block_id = if block.is_zero() {
            BlockId::Number(BlockNumber::Latest)
        } else {
            BlockId::Number(BlockNumber::Number(block))
        };
        let contract = ICurvePoolGeneric::new(pool, Arc::clone(&self.provider));
        let (out, used_latest_fallback) = match contract
            .get_dy(i, j, amount_in)
            .block(block_id)
            .call()
            .await
        {
            Ok(value) => (value, false),
            Err(err) if !block.is_zero() && is_block_out_of_range_error(&err) => {
                (contract.get_dy(i, j, amount_in).call().await?, true)
            }
            Err(err) => return Err(err.into()),
        };

        if should_cache_quote(block, used_latest_fallback) {
            let mut cache = self.cache.lock().await;
            cache.insert(cache_key, out);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{is_block_out_of_range_error, should_cache_quote};
    use ethers::types::U64;

    #[test]
    fn detects_block_out_of_range_messages() {
        let err = anyhow::anyhow!("(code: -32602, message: BlockOutOfRangeError: block height is 19623357 but requested was 19623356)");
        assert!(is_block_out_of_range_error(&err));
    }

    #[test]
    fn ignores_unrelated_messages() {
        let err = anyhow::anyhow!("execution reverted");
        assert!(!is_block_out_of_range_error(&err));
    }

    #[test]
    fn skips_cache_for_latest_fallback_on_pinned_blocks() {
        assert!(!should_cache_quote(U64::from(123_u64), true));
        assert!(should_cache_quote(U64::zero(), true));
        assert!(should_cache_quote(U64::from(123_u64), false));
    }
}
