use crate::quote_common::is_block_out_of_range_error;
use crate::quote_univ3::UniV3ValidationConfig;
use crate::util::encode_univ3_path;
use anyhow::{ensure, Result};
use ethers::abi::{ParamType, Token};
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;
use ethers::types::transaction::eip2718::TypedTransaction;
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::warn;

/// Aerodrome Slipstream tick spacings on Base (stored in `PoolRecord.fee`).
pub const TICK_SPACINGS: [u32; 6] = [1, 10, 50, 100, 200, 2000];

abigen!(
    ISlipstreamQuoterV2,
    r#"[
        function quoteExactInput(bytes path, uint256 amountIn) external returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate)
    ]"#,
);

abigen!(
    ISlipstreamClFactory,
    r#"[
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool)
    ]"#,
);

abigen!(
    ISlipstreamClPool,
    r#"[
        function liquidity() external view returns (uint128)
    ]"#,
);

const SLIPSTREAM_QUOTE_CACHE_TTL: Duration = Duration::from_secs(30);
const SLIPSTREAM_POOL_CACHE_TTL: Duration = Duration::from_secs(300);
const SLIPSTREAM_QUOTE_CACHE_SIZE: usize = 2048;

pub struct SlipstreamQuoter<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub quoter: ISlipstreamQuoterV2<Provider<C>>,
    factory: ISlipstreamClFactory<Provider<C>>,
    provider: Arc<Provider<C>>,
    cache: Mutex<LruCache<SlipstreamCacheKey, CachedQuote>>,
    pool_cache: Mutex<HashMap<SlipstreamPoolKey, CachedPoolAddress>>,
    zero_liquidity_logged: Mutex<HashSet<SlipstreamPoolKey>>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct SlipstreamCacheKey {
    path: Bytes,
    amount_in: U256,
    block: U64,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct SlipstreamPoolKey {
    token0: Address,
    token1: Address,
    tick_spacing: u32,
}

struct CachedQuote {
    inserted: Instant,
    value: U256,
}

#[derive(Clone, Copy)]
struct CachedPoolAddress {
    inserted: Instant,
    value: Option<Address>,
}

impl<C> SlipstreamQuoter<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn new(provider: Arc<Provider<C>>, quoter: Address, factory: Address) -> Self {
        Self {
            quoter: ISlipstreamQuoterV2::new(quoter, Arc::clone(&provider)),
            factory: ISlipstreamClFactory::new(factory, Arc::clone(&provider)),
            provider,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(SLIPSTREAM_QUOTE_CACHE_SIZE).unwrap_or(NonZeroUsize::MIN),
            )),
            pool_cache: Mutex::new(HashMap::new()),
            zero_liquidity_logged: Mutex::new(HashSet::new()),
        }
    }

    pub async fn quote_path(
        &self,
        path: Vec<(Address, Option<u32>)>,
        amount_in: U256,
        block: U64,
    ) -> Result<U256> {
        let path_bytes = Bytes::from(encode_univ3_path(&path)?);
        let cache_key = SlipstreamCacheKey {
            path: path_bytes.clone(),
            amount_in,
            block,
        };

        let block_id = if block.is_zero() {
            BlockId::Number(BlockNumber::Latest)
        } else {
            BlockId::Number(BlockNumber::Number(block))
        };

        {
            let mut cache = self.cache.lock().await;
            if let Some(cached) = cache.get(&cache_key) {
                if cached.inserted.elapsed() <= SLIPSTREAM_QUOTE_CACHE_TTL {
                    return Ok(cached.value);
                }
                cache.pop(&cache_key);
            }
        }

        let raw = match self
            .quoter
            .quote_exact_input(path_bytes.clone(), amount_in)
            .call_raw_bytes()
            .block(block_id)
            .await
        {
            Ok(value) => value,
            Err(err) if !block.is_zero() && is_block_out_of_range_error(&err) => {
                self.quoter
                    .quote_exact_input(path_bytes.clone(), amount_in)
                    .call_raw_bytes()
                    .await?
            }
            Err(err) => return Err(err.into()),
        };
        let raw_bytes = raw.as_ref();
        ensure!(
            raw_bytes.len() >= 32,
            "slipstream quoter returned insufficient data: expected at least 32 bytes got {}",
            raw_bytes.len()
        );
        let out = U256::from_big_endian(&raw_bytes[..32]);

        let mut cache = self.cache.lock().await;
        cache.put(
            cache_key,
            CachedQuote {
                inserted: Instant::now(),
                value: out,
            },
        );
        Ok(out)
    }

    pub async fn quote_path_grid(
        &self,
        path: Vec<(Address, Option<u32>)>,
        amounts: &[U256],
        block: U64,
    ) -> Result<Vec<Option<U256>>> {
        if amounts.is_empty() {
            return Ok(Vec::new());
        }
        let path_bytes = Bytes::from(encode_univ3_path(&path)?);
        let quoter_addr = self.quoter.address();
        let multicall3 = Address::from_str("0xcA11bde05977b3631167028862bE2a173976CA11")
            .map_err(|err| anyhow::anyhow!("invalid multicall3 address: {err}"))?;

        let mut call_tokens = Vec::with_capacity(amounts.len());
        for &amount in amounts {
            let inner = self
                .quoter
                .quote_exact_input(path_bytes.clone(), amount)
                .calldata()
                .ok_or_else(|| anyhow::anyhow!("failed to encode quoteExactInput calldata"))?;
            call_tokens.push(Token::Tuple(vec![
                Token::Address(quoter_addr),
                Token::Bool(true),
                Token::Bytes(inner.to_vec()),
            ]));
        }
        let mut data = vec![0x82u8, 0xad, 0x56, 0xcb];
        data.extend(ethers::abi::encode(&[Token::Array(call_tokens)]));

        let block_id = if block.is_zero() {
            BlockId::Number(BlockNumber::Latest)
        } else {
            BlockId::Number(BlockNumber::Number(block))
        };
        let tx: TypedTransaction = TransactionRequest::new()
            .to(multicall3)
            .data(Bytes::from(data))
            .into();
        let raw = self.provider.call(&tx, Some(block_id)).await?;

        let decoded = ethers::abi::decode(
            &[ParamType::Array(Box::new(ParamType::Tuple(vec![
                ParamType::Bool,
                ParamType::Bytes,
            ])))],
            raw.as_ref(),
        )?;

        let mut out = vec![None; amounts.len()];
        if let Some(Token::Array(results)) = decoded.into_iter().next() {
            for (i, result) in results.into_iter().enumerate() {
                if i >= out.len() {
                    break;
                }
                if let Token::Tuple(fields) = result {
                    let success = matches!(fields.first(), Some(Token::Bool(true)));
                    if success {
                        if let Some(Token::Bytes(return_data)) = fields.get(1) {
                            if return_data.len() >= 32 {
                                out[i] = Some(U256::from_big_endian(&return_data[..32]));
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn pool_address(
        &self,
        token_in: Address,
        token_out: Address,
        tick_spacing: u32,
    ) -> Result<Option<Address>> {
        let (token0, token1) = if token_in.as_bytes() <= token_out.as_bytes() {
            (token_in, token_out)
        } else {
            (token_out, token_in)
        };
        let key = SlipstreamPoolKey {
            token0,
            token1,
            tick_spacing,
        };

        {
            let mut cache = self.pool_cache.lock().await;
            if let Some(cached) = cache.get(&key) {
                if cached.inserted.elapsed() <= SLIPSTREAM_POOL_CACHE_TTL {
                    return Ok(cached.value);
                }
                cache.remove(&key);
            }
            cache.retain(|_, entry| entry.inserted.elapsed() <= SLIPSTREAM_POOL_CACHE_TTL);
        }

        let spacing_i24 = i32::try_from(tick_spacing)
            .map_err(|_| anyhow::anyhow!("tick spacing {tick_spacing} exceeds int24 range"))?;
        let pool = self
            .factory
            .get_pool(token0, token1, spacing_i24)
            .call()
            .await?;
        let normalized = if pool == Address::zero() {
            None
        } else {
            let pool_contract = ISlipstreamClPool::new(pool, Arc::clone(&self.provider));
            match pool_contract.liquidity().call().await {
                Ok(liquidity) if liquidity > 0 => Some(pool),
                Ok(_) => {
                    let mut logged = self.zero_liquidity_logged.lock().await;
                    if logged.insert(key) {
                        warn!(
                            target: "venue::slipstream",
                            token0 = %format!("0x{}", hex::encode(token0)),
                            token1 = %format!("0x{}", hex::encode(token1)),
                            tick_spacing,
                            pool = %format!("0x{}", hex::encode(pool)),
                            "Slipstream pool has zero liquidity"
                        );
                    }
                    None
                }
                Err(err) => {
                    warn!(
                        target: "venue::slipstream",
                        error = %err,
                        token0 = %format!("0x{}", hex::encode(token0)),
                        token1 = %format!("0x{}", hex::encode(token1)),
                        tick_spacing,
                        pool = %format!("0x{}", hex::encode(pool)),
                        "Slipstream pool liquidity check failed"
                    );
                    None
                }
            }
        };

        let mut cache = self.pool_cache.lock().await;
        cache.insert(
            key,
            CachedPoolAddress {
                inserted: Instant::now(),
                value: normalized,
            },
        );
        Ok(normalized)
    }

    pub async fn validate(&self, config: &UniV3ValidationConfig) -> Result<U256> {
        let quoter_addr = self.quoter.address();
        let code = self.provider.get_code(quoter_addr, None).await?;
        ensure!(
            !code.0.is_empty(),
            "slipstream_quoter has no code at {quoter_addr} (wrong address or wrong chain RPC)"
        );

        let pool = self
            .pool_address(config.token_in, config.token_out, config.fee)
            .await?;
        ensure!(
            pool.is_some(),
            "slipstream validation pool missing/illiquid (token_in={}, token_out={}, tick_spacing={})",
            config.token_in,
            config.token_out,
            config.fee
        );

        let path = encode_univ3_path(&[
            (config.token_in, None),
            (config.token_out, Some(config.fee)),
        ])?;

        let (amount_out, _, _, _) = self
            .quoter
            .quote_exact_input(Bytes::from(path), config.amount_in)
            .call()
            .await?;
        ensure!(
            !amount_out.is_zero(),
            "slipstream validation returned zero output"
        );
        Ok(amount_out)
    }
}

pub fn default_slipstream_validation_base() -> UniV3ValidationConfig {
    use hex_literal::hex;
    UniV3ValidationConfig {
        token_in: Address::from_slice(&hex!("4200000000000000000000000000000000000006")),
        token_out: Address::from_slice(&hex!("833589fcd6edb6e08f4c7c32d4f71b54bda02913")),
        fee: 1,
        amount_in: U256::from(10u64).pow(U256::from(16u64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::encode_univ3_path;

    #[test]
    fn tick_spacing_path_encoding_matches_univ3_layout() {
        use hex_literal::hex;

        let weth = Address::from_slice(&hex!("4200000000000000000000000000000000000006"));
        let usdc = Address::from_slice(&hex!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"));
        let encoded = encode_univ3_path(&[(weth, None), (usdc, Some(1))]).expect("encode path");
        assert_eq!(encoded.len(), 43);
        assert_eq!(&encoded[20..23], &[0, 0, 1]);
    }

    /// Live Base mainnet quote smoke — run with:
    /// `ARBOT_SLIPSTREAM_LIVE_QUOTE=1 cargo test slipstream_live_weth_usdc_quote -- --nocapture`
    #[tokio::test]
    async fn slipstream_live_weth_usdc_quote() {
        if std::env::var("ARBOT_SLIPSTREAM_LIVE_QUOTE").ok().as_deref() != Some("1") {
            return;
        }
        let rpc = std::env::var("BASE_RPC_URL")
            .or_else(|_| std::env::var("RPC_URL"))
            .expect("BASE_RPC_URL or RPC_URL required for live quote test");
        let provider = Arc::new(Provider::<Http>::try_from(rpc.as_str()).expect("rpc url"));
        let quoter = SlipstreamQuoter::new(
            provider,
            "0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0"
                .parse()
                .unwrap(),
            "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A"
                .parse()
                .unwrap(),
        );
        let weth: Address = "0x4200000000000000000000000000000000000006"
            .parse()
            .unwrap();
        let usdc: Address = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
            .parse()
            .unwrap();
        let amount_in = U256::from(10u64).pow(U256::from(16u64));
        let out = quoter
            .quote_path(vec![(weth, None), (usdc, Some(1))], amount_in, U64::zero())
            .await
            .expect("slipstream quote");
        assert!(out > U256::zero(), "expected positive USDC out for WETH in");
        eprintln!("slipstream WETH->USDC ts=1 quote: {out}");
    }
}
