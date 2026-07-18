use crate::quote_cl::{cl_quote_path, cl_quote_path_grid, ClQuoteCache};
use crate::quote_univ3::UniV3ValidationConfig;
use crate::util::encode_univ3_path;
use anyhow::{ensure, Result};
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;
use std::collections::{HashMap, HashSet};
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

const SLIPSTREAM_POOL_CACHE_TTL: Duration = Duration::from_secs(300);

pub struct SlipstreamQuoter<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub quoter: ISlipstreamQuoterV2<Provider<C>>,
    factory: ISlipstreamClFactory<Provider<C>>,
    provider: Arc<Provider<C>>,
    cache: ClQuoteCache,
    pool_cache: Mutex<HashMap<SlipstreamPoolKey, CachedPoolAddress>>,
    zero_liquidity_logged: Mutex<HashSet<SlipstreamPoolKey>>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct SlipstreamPoolKey {
    token0: Address,
    token1: Address,
    tick_spacing: u32,
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
            "slipstream quoter",
        )
        .await
    }

    /// See [`crate::quote_cl::cl_quote_path_grid`].
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
