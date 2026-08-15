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

            pair.truncate_tokens();
            let Some(depth) = pair
                .depth_tokens_for(&addr_lower)
                .and_then(Decimal::from_f64)
            else {
                continue;
            };
            if depth.is_sign_negative() || depth.is_zero() {
                continue;
            }
            total_tokens = total_tokens.checked_add(depth).unwrap_or(Decimal::MAX);
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

    /// Verbatim shape of a live `api.dexscreener.com/latest/dex/tokens/<addr>`
    /// pair: `priceUsd` is a STRING on the pair, and the token objects carry no
    /// price at all. Parsing it must yield real depth, not zero.
    const LIVE_PAIR: &str = r#"{
      "pairs": [{
        "chainId": "base",
        "priceUsd": "1915.79",
        "liquidity": { "usd": 95343.46, "base": 36.279, "quote": 25840 },
        "baseToken": { "address": "0x4200000000000000000000000000000000000006", "name": "Wrapped Ether", "symbol": "WETH" },
        "quoteToken": { "address": "0xbA9986D2381edf1DA03B0B9c1f8b00dc4AacC369", "name": "USDC.e", "symbol": "USDC.e" }
      }]
    }"#;

    fn only_pair() -> DexScreenerPair {
        let mut parsed: DexScreenerResponse = serde_json::from_str(LIVE_PAIR).expect("parses");
        let mut pair = parsed.pairs.remove(0);
        pair.truncate_tokens();
        pair
    }

    #[test]
    fn pair_level_price_and_token_depths_are_parsed() {
        let pair = only_pair();
        assert_eq!(pair.chain_id, "base");
        assert_eq!(pair.price_usd, Some(1915.79));
        assert_eq!(pair.liquidity.usd(), Some(95343.46));
        assert_eq!(pair.liquidity.base, Some(36.279));
        assert_eq!(pair.liquidity.quote, Some(25840.0));
    }

    #[test]
    fn depth_uses_the_side_the_token_sits_on() {
        let pair = only_pair();
        let weth = "0x4200000000000000000000000000000000000006";
        let usdce = "0xba9986d2381edf1da03b0b9c1f8b00dc4aacc369";

        assert_eq!(pair.depth_tokens_for(weth), Some(36.279));
        assert_eq!(pair.depth_tokens_for(usdce), Some(25840.0));
        // A token that is not in the pair contributes nothing.
        assert_eq!(pair.depth_tokens_for("0x00000000000000000000000000000000deadbeef"), None);
    }

    /// The regression: previously depth came from `baseToken.priceUsd`, which
    /// this payload does not contain, so every token cached zero.
    #[test]
    fn depth_is_non_zero_for_a_real_payload() {
        let pair = only_pair();
        let weth = "0x4200000000000000000000000000000000000006";
        assert!(pair.depth_tokens_for(weth).unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn base_side_falls_back_to_usd_over_pair_price() {
        // Same pair with the token-denominated amounts stripped.
        let json = r#"{"pairs":[{
            "chainId":"base","priceUsd":"2000",
            "liquidity":{"usd":100000},
            "baseToken":{"address":"0x4200000000000000000000000000000000000006"},
            "quoteToken":{"address":"0xbA9986D2381edf1DA03B0B9c1f8b00dc4AacC369"}
        }]}"#;
        let mut parsed: DexScreenerResponse = serde_json::from_str(json).expect("parses");
        let mut pair = parsed.pairs.remove(0);
        pair.truncate_tokens();

        // (100000 / 2) / 2000 = 25 WETH
        assert_eq!(
            pair.depth_tokens_for("0x4200000000000000000000000000000000000006"),
            Some(25.0)
        );
        // The quote side has no sound fallback — priceUsd prices the base token.
        assert_eq!(
            pair.depth_tokens_for("0xba9986d2381edf1da03b0b9c1f8b00dc4aacc369"),
            None
        );
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
    /// Pair-level USD price of the BASE token. DexScreener reports it here as a
    /// string, NOT inside `baseToken`/`quoteToken` — those objects carry only
    /// address/name/symbol. Reading `priceUsd` off the token objects therefore
    /// always yielded `None`, `price_for` always failed, and every token cached
    /// zero depth. That floored `compute_base_amounts` at `MIN_FLASH_LOAN_WEI`,
    /// so every trade was sized at 0.001 WETH.
    #[serde(default, rename = "priceUsd", deserialize_with = "deserialize_opt_f64")]
    price_usd: Option<f64>,
    #[serde(default)]
    liquidity: DexScreenerLiquidity,
    #[serde(rename = "baseToken")]
    base_token: DexScreenerToken,
    #[serde(rename = "quoteToken")]
    quote_token: DexScreenerToken,
}

/// Which side of a pair a queried token sits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PairSide {
    Base,
    Quote,
}

impl DexScreenerPair {
    fn truncate_tokens(&mut self) {
        self.base_token.normalize();
        self.quote_token.normalize();
    }

    fn side_of(&self, addr_lower: &str) -> Option<PairSide> {
        if self.base_token.address.as_deref() == Some(addr_lower) {
            return Some(PairSide::Base);
        }
        if self.quote_token.address.as_deref() == Some(addr_lower) {
            return Some(PairSide::Quote);
        }
        None
    }

    /// Depth attributable to `addr_lower` in that token's own units.
    ///
    /// `liquidity.base` / `liquidity.quote` are already token-denominated, so
    /// they are used directly. Only when they are absent do we fall back to
    /// converting half the USD liquidity through the pair price — and that
    /// fallback is only sound on the base side, since `priceUsd` prices the
    /// base token.
    fn depth_tokens_for(&self, addr_lower: &str) -> Option<f64> {
        let side = self.side_of(addr_lower)?;
        let reported = match side {
            PairSide::Base => self.liquidity.base,
            PairSide::Quote => self.liquidity.quote,
        };
        if let Some(amount) = reported.filter(|v| v.is_finite() && *v > 0.0) {
            return Some(amount);
        }
        if side == PairSide::Base {
            let usd = self.liquidity.usd()?;
            let price = self.price_usd.filter(|p| p.is_finite() && *p > 0.0)?;
            let derived = (usd / 2.0) / price;
            if derived.is_finite() && derived > 0.0 {
                return Some(derived);
            }
        }
        None
    }
}

#[derive(Debug, Default)]
struct DexScreenerLiquidity {
    usd: Option<f64>,
    /// Token-denominated depth of each side, as reported by DexScreener. These
    /// are what we actually want — no price conversion needed.
    base: Option<f64>,
    quote: Option<f64>,
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
        fn as_f64(val: Value) -> Option<f64> {
            match val {
                Value::Number(num) => num.as_f64(),
                Value::String(s) => s.parse::<f64>().ok(),
                _ => None,
            }
        }

        let value = Value::deserialize(deserializer)?;

        // A bare scalar is treated as the USD figure, preserving the previous
        // tolerance for non-object payloads.
        let (usd, base, quote) = match value {
            Value::Object(mut map) => (
                map.remove("usd").and_then(as_f64),
                map.remove("base").and_then(as_f64),
                map.remove("quote").and_then(as_f64),
            ),
            other @ (Value::Number(_) | Value::String(_)) => (as_f64(other), None, None),
            Value::Null | Value::Bool(_) | Value::Array(_) => (None, None, None),
        };

        Ok(Self { usd, base, quote })
    }
}

#[derive(Debug, Deserialize, Default)]
struct DexScreenerToken {
    #[serde(default, deserialize_with = "deserialize_opt_string")]
    address: Option<String>,
    // No `priceUsd` here on purpose: DexScreener does not put one inside the
    // token objects. It lives on the pair (`DexScreenerPair::price_usd`).
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
