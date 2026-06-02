use crate::{metrics::Metrics, token_refresh::TokenList};
use anyhow::{Context, Result};
use ethers::types::{Address, U256};
use lru::LruCache;
use reqwest::Client;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::{Decimal, MathematicalOps};
use serde::Deserialize;
use serde_json::Value;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, warn};

const DEX_SCREENER_BASE: &str = "https://api.dexscreener.com/latest/dex/tokens";
const LIQUIDITY_CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
struct LiquiditySnapshot {
    tokens: Decimal,
    updated_at: Instant,
}

impl LiquiditySnapshot {
    fn to_amount_wei(&self, decimals: u8) -> U256 {
        if self.tokens.is_sign_negative() || self.tokens.is_zero() {
            return U256::zero();
        }
        let scale = Decimal::from(10u64)
            .checked_powu(decimals as u64)
            .unwrap_or(Decimal::ZERO);
        if scale.is_zero() {
            return U256::zero();
        }
        let scaled = match self.tokens.checked_mul(scale) {
            Some(value) => value,
            None => return U256::zero(),
        };
        if scaled.is_sign_negative() || scaled.is_zero() {
            return U256::zero();
        }
        let floored = scaled.floor();
        let text = floored.to_string();
        U256::from_dec_str(&text).unwrap_or_else(|_| U256::zero())
    }

    fn is_stale(&self, stale_after: Duration) -> bool {
        self.updated_at.elapsed() > stale_after
    }
}

#[derive(Clone)]
pub struct PoolDepthCache {
    client: Client,
    chain_name: String,
    chain_filter: Option<String>,
    tokens: TokenList,
    stale_after: Duration,
    inner: Arc<RwLock<LruCache<Address, LiquiditySnapshot>>>,
    metrics: Option<Arc<Metrics>>,
}

impl PoolDepthCache {
    pub fn new(chain: String, tokens: TokenList, refresh_interval: Duration) -> Result<Self> {
        Self::with_metrics(chain, tokens, refresh_interval, None)
    }

    pub fn with_metrics(
        chain: String,
        tokens: TokenList,
        refresh_interval: Duration,
        metrics: Option<Arc<Metrics>>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("failed to construct http client for pool depth cache")?;

        let cache_capacity =
            NonZeroUsize::new(tokens.current().len().max(1)).unwrap_or(NonZeroUsize::MIN);
        let chain_name = if chain.is_empty() {
            "unknown".to_string()
        } else {
            chain.clone()
        };

        Ok(Self {
            client,
            chain_name,
            chain_filter: if chain.is_empty() {
                None
            } else {
                Some(chain.to_ascii_lowercase())
            },
            tokens,
            stale_after: LIQUIDITY_CACHE_TTL.max(refresh_interval),
            inner: Arc::new(RwLock::new(LruCache::new(cache_capacity))),
            metrics,
        })
    }

    pub async fn refresh_all(&self) {
        let tokens = self.tokens.current();
        for &token in tokens.iter() {
            match self.fetch_token_depth(token).await {
                Ok(Some(tokens_depth)) => {
                    self.store_depth(token, tokens_depth).await;
                }
                Ok(None) => {
                    debug!(
                        token = %format!("0x{}", hex::encode(token)),
                        "no liquidity data returned for token"
                    );
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        token = %format!("0x{}", hex::encode(token)),
                        "failed to refresh liquidity depth"
                    );
                }
            }
        }
    }

    pub async fn estimate_liquidity(&self, token: Address, decimals: u8) -> U256 {
        let mut guard = self.inner.write().await;
        if let Some(entry) = guard.get(&token).cloned() {
            if entry.is_stale(self.stale_after) {
                let _ = guard.pop(&token);
                if let Some(metrics) = &self.metrics {
                    metrics.record_liquidity_cache_miss(&self.chain_name);
                    metrics.record_liquidity_cache_evictions(&self.chain_name, 1);
                }
                U256::zero()
            } else {
                if let Some(metrics) = &self.metrics {
                    metrics.record_liquidity_cache_hit(&self.chain_name);
                }
                entry.to_amount_wei(decimals)
            }
        } else {
            if let Some(metrics) = &self.metrics {
                metrics.record_liquidity_cache_miss(&self.chain_name);
            }
            U256::zero()
        }
    }

    async fn store_depth(&self, token: Address, tokens_depth: Decimal) {
        let mut guard = self.inner.write().await;
        let existed = guard.peek(&token).is_some();
        let at_capacity = guard.len() >= guard.cap().get();
        guard.put(
            token,
            LiquiditySnapshot {
                tokens: tokens_depth,
                updated_at: Instant::now(),
            },
        );
        if !existed && at_capacity {
            if let Some(metrics) = &self.metrics {
                metrics.record_liquidity_cache_evictions(&self.chain_name, 1);
            }
        }
    }

    async fn fetch_token_depth(&self, token: Address) -> Result<Option<Decimal>> {
        let url = format!("{DEX_SCREENER_BASE}/0x{}", hex::encode(token));
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("failed to query DexScreener for liquidity depth")?;

        if !response.status().is_success() {
            warn!(
                status = %response.status(),
                token = %format!("0x{}", hex::encode(token)),
                "DexScreener liquidity query returned non-success status"
            );
            return Ok(None);
        }

        let payload: DexScreenerResponse = response
            .json()
            .await
            .context("failed to decode DexScreener liquidity payload")?;

        let mut total_tokens = Decimal::ZERO;
        let addr_lower = format!("0x{}", hex::encode(token));
        let addr_lower = addr_lower.to_ascii_lowercase();
        for mut pair in payload.pairs.into_iter() {
            if let Some(chain) = &self.chain_filter {
                if pair.chain_id.to_ascii_lowercase() != *chain {
                    continue;
                }
            }

            if let Some(order) = pair.liquidity.usd().and_then(Decimal::from_f64) {
                if order.is_sign_negative() || order.is_zero() {
                    continue;
                }
                pair.truncate_tokens();
                if let Some(price) = pair.price_for(&addr_lower).and_then(Decimal::from_f64) {
                    if price.is_sign_negative() || price.is_zero() {
                        continue;
                    }
                    if let Some(value) = order
                        .checked_div(Decimal::from(2u64))
                        .and_then(|half| half.checked_div(price))
                    {
                        total_tokens = total_tokens.checked_add(value).unwrap_or(Decimal::MAX);
                    }
                }
            }
        }

        if total_tokens.is_zero() || total_tokens.is_sign_negative() {
            Ok(None)
        } else {
            Ok(Some(total_tokens))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(value: u64) -> Address {
        Address::from_low_u64_be(value)
    }

    #[tokio::test]
    async fn evicts_least_recently_used_entry() {
        let cache = PoolDepthCache::new(
            "ethereum".to_string(),
            TokenList::new(vec![addr(1), addr(2)]),
            Duration::from_secs(1),
        )
        .expect("cache should initialize");

        cache.store_depth(addr(1), Decimal::from(100u64)).await;
        cache.store_depth(addr(2), Decimal::from(200u64)).await;
        let _ = cache.estimate_liquidity(addr(2), 18).await;
        cache.store_depth(addr(3), Decimal::from(300u64)).await;

        assert_eq!(cache.estimate_liquidity(addr(1), 18).await, U256::zero());
        assert!(cache.estimate_liquidity(addr(2), 18).await > U256::zero());
        assert!(cache.estimate_liquidity(addr(3), 18).await > U256::zero());
    }

    #[tokio::test]
    async fn returns_zero_for_stale_entries() {
        let cache = PoolDepthCache::new(
            "ethereum".to_string(),
            TokenList::new(vec![addr(1)]),
            Duration::from_secs(1),
        )
        .expect("cache should initialize");

        cache.store_depth(addr(1), Decimal::from(100u64)).await;

        {
            let mut guard = cache.inner.write().await;
            let entry = guard.get_mut(&addr(1)).expect("entry should exist");
            entry.updated_at = Instant::now() - Duration::from_secs(31);
        }

        assert_eq!(cache.estimate_liquidity(addr(1), 18).await, U256::zero());
        let guard = cache.inner.read().await;
        assert!(guard.peek(&addr(1)).is_none());
    }
}

#[derive(Debug, Deserialize)]
struct DexScreenerResponse {
    #[serde(default, deserialize_with = "deserialize_pairs")]
    pairs: Vec<DexScreenerPair>,
}

#[derive(Debug, Deserialize)]
struct DexScreenerPair {
    #[serde(rename = "chainId")]
    chain_id: String,
    #[serde(default)]
    liquidity: DexScreenerLiquidity,
    #[serde(rename = "baseToken")]
    base_token: DexScreenerToken,
    #[serde(rename = "quoteToken")]
    quote_token: DexScreenerToken,
}

impl DexScreenerPair {
    fn truncate_tokens(&mut self) {
        self.base_token.normalize();
        self.quote_token.normalize();
    }

    fn price_for(&self, addr_lower: &str) -> Option<f64> {
        if self
            .base_token
            .address
            .as_deref()
            .map(|addr| addr == addr_lower)
            .unwrap_or(false)
        {
            return self.base_token.price_usd;
        }
        if self
            .quote_token
            .address
            .as_deref()
            .map(|addr| addr == addr_lower)
            .unwrap_or(false)
        {
            return self.quote_token.price_usd;
        }
        None
    }
}

#[derive(Debug, Default)]
struct DexScreenerLiquidity {
    usd: Option<f64>,
}

impl DexScreenerLiquidity {
    fn usd(&self) -> Option<f64> {
        self.usd
    }
}

impl<'de> Deserialize<'de> for DexScreenerLiquidity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;

        let usd = match value {
            Value::Object(mut map) => map.remove("usd").and_then(|val| match val {
                Value::Number(num) => num.as_f64(),
                Value::String(s) => s.parse::<f64>().ok(),
                Value::Null => None,
                _ => None,
            }),
            Value::Number(num) => num.as_f64(),
            Value::String(s) => s.parse::<f64>().ok(),
            Value::Null | Value::Bool(_) | Value::Array(_) => None,
        };

        Ok(Self { usd })
    }
}

#[derive(Debug, Deserialize, Default)]
struct DexScreenerToken {
    #[serde(default, deserialize_with = "deserialize_opt_string")]
    address: Option<String>,
    #[serde(default, rename = "priceUsd", deserialize_with = "deserialize_opt_f64")]
    price_usd: Option<f64>,
}

impl DexScreenerToken {
    fn normalize(&mut self) {
        if let Some(addr) = self.address.as_mut() {
            if let Ok(parsed) = Address::from_str(addr) {
                *addr = format!("0x{}", hex::encode(parsed));
            } else {
                *addr = addr.to_ascii_lowercase();
            }
        }
    }
}

fn deserialize_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(|val| match val {
        Value::String(s) => Some(s),
        Value::Null => None,
        other => Some(other.to_string()),
    }))
}

fn deserialize_opt_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(|val| match val {
        Value::Number(num) => num.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Null => None,
        _ => None,
    }))
}

fn deserialize_pairs<'de, D>(deserializer: D) -> Result<Vec<DexScreenerPair>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<Vec<DexScreenerPair>> = Option::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}
