use crate::quote_cl::{cl_quote_path, cl_quote_path_grid, ClQuoteCache};
use crate::util::encode_univ3_path;
use anyhow::{ensure, Result};
use ethers::{prelude::*, providers::JsonRpcClient};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::warn;

#[derive(Clone, Debug)]
pub struct UniV3ValidationConfig {
    pub token_in: Address,
    pub token_out: Address,
    pub fee: u32,
    pub amount_in: U256,
}

abigen!(
    IQuoterV2,
    r#"[
        function quoteExactInput(bytes path, uint256 amountIn) external returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate)
    ]"#,
);

abigen!(
    IUniswapV3Factory,
    r#"[
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool)
    ]"#,
);

abigen!(
    IUniswapV3Pool,
    r#"[
        function liquidity() external view returns (uint128)
    ]"#,
);

/// Fee tiers in hundredths of a bip: 500=0.05%, 3000=0.3%, 10000=1%, 100=0.01%
///
/// 100 is LAST, not first, and that ordering is load-bearing. Callers that probe
/// for a native price take the FIRST tier returning a non-zero quote, so leading
/// with the 1bp tier would let a thin correlated-pair pool outrank a deep 500
/// pool and set the price. Appended, it only answers when every other tier has
/// no route — strictly additive, no change to prices that already resolve.
///
/// It was missing entirely, which mattered: 63 pools in the Base inventory sit
/// at this tier (stable- and LST-correlated pairs — wstETH, cbETH, stable/stable),
/// and a token whose only WETH route is a 1bp pool resolved as `NoRoute`. That
/// verdict is cached, and an unpriceable start token is rejected as
/// `unreliable_native_price_for_start_token` BEFORE sizing runs, so every cycle
/// beginning at that token vanished silently and looked like "no opportunity".
pub const FEE_TIERS: [u32; 4] = [500, 3000, 10000, 100];

#[allow(dead_code)]
pub fn is_supported_univ3_fee(fee: u32) -> bool {
    FEE_TIERS.contains(&fee)
}

const UNIV3_POOL_CACHE_TTL: Duration = Duration::from_secs(300);

pub struct UniQuoter<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub quoter: IQuoterV2<Provider<C>>,
    factory: IUniswapV3Factory<Provider<C>>,
    provider: Arc<Provider<C>>,
    cache: ClQuoteCache,
    pool_cache: Mutex<HashMap<UniV3PoolKey, CachedPoolAddress>>,
    zero_liquidity_logged: Mutex<HashSet<UniV3PoolKey>>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct UniV3PoolKey {
    token0: Address,
    token1: Address,
    fee: u32,
}

#[derive(Clone, Copy)]
struct CachedPoolAddress {
    inserted: Instant,
    value: Option<Address>,
}

impl<C> UniQuoter<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn new(provider: Arc<Provider<C>>, quoter: Address, factory: Address) -> Self {
        Self {
            quoter: IQuoterV2::new(quoter, Arc::clone(&provider)),
            factory: IUniswapV3Factory::new(factory, Arc::clone(&provider)),
            provider,
            cache: ClQuoteCache::new(),
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
        cl_quote_path(
            &self.provider,
            self.quoter.address(),
            &self.cache,
            path,
            amount_in,
            block,
            "quoter",
        )
        .await
    }

    /// Quote a fixed set of input amounts for ONE path in a SINGLE eth_call by
    /// batching `quoteExactInput` through Multicall3.aggregate3. See
    /// [`crate::quote_cl::cl_quote_path_grid`].
    pub async fn quote_path_grid(
        &self,
        path: Vec<(Address, Option<u32>)>,
        amounts: &[U256],
        block: U64,
    ) -> Result<Vec<Option<U256>>> {
        cl_quote_path_grid(&self.provider, self.quoter.address(), path, amounts, block).await
    }

    pub async fn pool_address(
        &self,
        token_in: Address,
        token_out: Address,
        fee: u32,
    ) -> Result<Option<Address>> {
        let (token0, token1) = if token_in.as_bytes() <= token_out.as_bytes() {
            (token_in, token_out)
        } else {
            (token_out, token_in)
        };
        let key = UniV3PoolKey {
            token0,
            token1,
            fee,
        };

        {
            let mut cache = self.pool_cache.lock().await;
            if let Some(cached) = cache.get(&key) {
                if cached.inserted.elapsed() <= UNIV3_POOL_CACHE_TTL {
                    return Ok(cached.value);
                }
                cache.remove(&key);
            }
            cache.retain(|_, entry| entry.inserted.elapsed() <= UNIV3_POOL_CACHE_TTL);
        }

        let pool = self.factory.get_pool(token0, token1, fee).call().await?;
        let normalized = if pool == Address::zero() {
            None
        } else {
            let pool_contract = IUniswapV3Pool::new(pool, Arc::clone(&self.provider));
            match pool_contract.liquidity().call().await {
                Ok(liquidity) if liquidity > 0 => Some(pool),
                Ok(_) => {
                    let mut logged = self.zero_liquidity_logged.lock().await;
                    if logged.insert(key) {
                        warn!(
                            target: "venue::univ3",
                            token0 = %format!("0x{}", hex::encode(token0)),
                            token1 = %format!("0x{}", hex::encode(token1)),
                            fee,
                            pool = %format!("0x{}", hex::encode(pool)),
                            "UniV3 pool has zero liquidity"
                        );
                    }
                    None
                }
                Err(err) => {
                    warn!(
                        target: "venue::univ3",
                        error = %err,
                        token0 = %format!("0x{}", hex::encode(token0)),
                        token1 = %format!("0x{}", hex::encode(token1)),
                        fee,
                        pool = %format!("0x{}", hex::encode(pool)),
                        "UniV3 pool liquidity check failed"
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
        // If the Quoter points at the wrong address (e.g. UniversalRouter) or the RPC is on the wrong
        // chain, QuoterV2 will typically "revert 0x" during the first swap simulation.
        let quoter_addr = self.quoter.address();
        let code = self.provider.get_code(quoter_addr, None).await?;
        ensure!(
            !code.0.is_empty(),
            "univ3_quoter has no code at {quoter_addr} (wrong address or wrong chain RPC)"
        );

        // Avoid QuoterV2's opaque empty-revert by checking pool existence/liquidity first.
        let pool = self
            .pool_address(config.token_in, config.token_out, config.fee)
            .await?;
        ensure!(
            pool.is_some(),
            "univ3 validation pool missing/illiquid (token_in={}, token_out={}, fee={})",
            config.token_in,
            config.token_out,
            config.fee
        );

        // IMPORTANT: fee belongs to the *next* token in the path representation.
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
            "univ3 validation returned zero output"
        );
        Ok(amount_out)
    }
}

/// Startup validation path for PancakeSwap V3 on Base (WETH/USDC 0.05%).
pub fn default_pancakeswap_validation_base() -> UniV3ValidationConfig {
    use hex_literal::hex;
    UniV3ValidationConfig {
        token_in: Address::from_slice(&hex!("4200000000000000000000000000000000000006")),
        token_out: Address::from_slice(&hex!("833589fcd6edb6e08f4c7c32d4f71b54bda02913")),
        fee: 500,
        amount_in: U256::from(10u64).pow(U256::from(15u64)),
    }
}

#[cfg(test)]
mod block_range_tests {
    use crate::quote_common::is_block_out_of_range_error;

    #[test]
    fn detects_block_out_of_range_messages() {
        let err = anyhow::anyhow!(
            "(code: -32602, message: BlockOutOfRangeError: block height is 24613432 but requested was 24613431, data: None)"
        );
        assert!(is_block_out_of_range_error(&err));
    }

    #[test]
    fn ignores_non_block_range_messages() {
        let err = anyhow::anyhow!("execution reverted");
        assert!(!is_block_out_of_range_error(&err));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::encode_univ3_path;
    use tokio::time::{advance, pause};

    fn amount_bytes(amount: U256) -> Bytes {
        let mut buf = [0u8; 32];
        amount.to_big_endian(&mut buf);
        Bytes::from(buf.to_vec())
    }

    #[tokio::test]
    async fn quote_cache_respects_block_and_ttl() {
        pause();
        let (provider, mock) = Provider::mocked();
        let provider = Arc::new(provider);
        let quoter = UniQuoter::new(provider, Address::zero(), Address::zero());
        let path = vec![
            (Address::repeat_byte(1), None),
            (Address::repeat_byte(2), Some(500)),
        ];
        let amount_in = U256::from(100u64);

        mock.push::<Bytes, _>(amount_bytes(U256::from(1_000u64)))
            .unwrap();
        let first = quoter
            .quote_path(path.clone(), amount_in, U64::from(1))
            .await
            .unwrap();
        assert_eq!(first, U256::from(1_000u64));

        let cached = quoter
            .quote_path(path.clone(), amount_in, U64::from(1))
            .await
            .unwrap();
        assert_eq!(cached, first);

        advance(crate::quote_cl::QUOTE_CACHE_TTL + Duration::from_secs(1)).await;

        mock.push::<Bytes, _>(amount_bytes(U256::from(2_000u64)))
            .unwrap();
        let refreshed = quoter
            .quote_path(path.clone(), amount_in, U64::from(1))
            .await
            .unwrap();
        assert_eq!(refreshed, U256::from(2_000u64));

        let refreshed_cached = quoter
            .quote_path(path.clone(), amount_in, U64::from(1))
            .await
            .unwrap();
        assert_eq!(refreshed_cached, refreshed);

        mock.push::<Bytes, _>(amount_bytes(U256::from(3_000u64)))
            .unwrap();
        let next_block = quoter
            .quote_path(path, amount_in, U64::from(2))
            .await
            .unwrap();
        assert_eq!(next_block, U256::from(3_000u64));
    }

    #[tokio::test]
    async fn pool_cache_honors_ttl_and_none_entries() {
        pause();
        let (provider, _mock) = Provider::mocked();
        let provider = Arc::new(provider);
        let quoter = UniQuoter::new(provider, Address::zero(), Address::zero());
        let token_a = Address::repeat_byte(10);
        let token_b = Address::repeat_byte(11);
        let fee = 500u32;

        let (token0, token1) = if token_a.as_bytes() <= token_b.as_bytes() {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };
        let key = UniV3PoolKey {
            token0,
            token1,
            fee,
        };
        {
            let mut cache = quoter.pool_cache.lock().await;
            cache.insert(
                key,
                CachedPoolAddress {
                    inserted: Instant::now(),
                    value: None,
                },
            );
        }

        let initial = quoter.pool_address(token_a, token_b, fee).await.unwrap();
        assert!(initial.is_none());

        let cached = quoter.pool_address(token_a, token_b, fee).await.unwrap();
        assert!(cached.is_none());

        advance(UNIV3_POOL_CACHE_TTL + Duration::from_secs(1)).await;

        {
            let mut cache = quoter.pool_cache.lock().await;
            cache.insert(
                key,
                CachedPoolAddress {
                    inserted: Instant::now() - UNIV3_POOL_CACHE_TTL - Duration::from_secs(1),
                    value: None,
                },
            );
        }

        let refreshed = quoter.pool_address(token_a, token_b, fee).await;
        assert!(refreshed.is_err());
    }

    #[test]
    fn shared_core_calldata_matches_abigen() {
        // The shared CL quote core builds quoteExactInput calldata by hand
        // instead of via the abigen contract wrapper. Pin the two together so a
        // future ABI/selector drift is caught here rather than on-chain.
        let provider = Arc::new(Provider::<Http>::try_from("http://localhost:8545").unwrap());
        let quoter = IQuoterV2::new(Address::zero(), provider);
        let path = encode_univ3_path(&[
            (Address::repeat_byte(1), None),
            (Address::repeat_byte(2), Some(500)),
        ])
        .unwrap();
        let path_bytes = Bytes::from(path);
        let amount = U256::from(123_456_789u64);

        let abigen_calldata = quoter
            .quote_exact_input(path_bytes.clone(), amount)
            .calldata()
            .expect("abigen calldata");
        let core_calldata = crate::quote_cl::quote_exact_input_calldata(&path_bytes, amount);
        assert_eq!(abigen_calldata, core_calldata);
    }

    #[test]
    fn single_hop_path_encoding_remains_valid() {
        let token_in = Address::repeat_byte(1);
        let token_out = Address::repeat_byte(2);
        let encoded = encode_univ3_path(&[(token_in, None), (token_out, Some(500))])
            .expect("single-hop path should encode");
        assert_eq!(
            encoded.len(),
            43,
            "single-hop path should be token(20)+fee(3)+token(20)"
        );
    }

    #[test]
    fn missing_fee_error_regression_guard() {
        let token_in = Address::repeat_byte(1);
        let token_out = Address::repeat_byte(2);
        let err = encode_univ3_path(&[(token_in, None), (token_out, None)])
            .expect_err("hop fee must be present");
        assert!(err.to_string().contains("missing fee for hop 1"));
    }
}
