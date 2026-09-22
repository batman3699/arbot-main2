use crate::util::TradeSizing;
use ethers::types::Address;
use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub struct ProfitabilitySnapshot {
    profitable_pairs: HashSet<(Address, Address, u32)>,
    last_profitable: HashMap<(Address, Address), Instant>,
    recency_window: Duration,
}

impl ProfitabilitySnapshot {
    pub fn score(&self, from: Address, to: Address, fee: u32) -> f64 {
        let mut score: f64 = 0.0;
        if self.profitable_pairs.contains(&(from, to, fee)) {
            score = 1.0;
        }
        if let Some(ts) = self.last_profitable.get(&(from, to)) {
            if !self.recency_window.is_zero() {
                let elapsed = ts.elapsed().as_secs_f64();
                let window = self.recency_window.as_secs_f64();
                let recency_score = (1.0 - (elapsed / window)).clamp(0.0, 1.0);
                score = score.max(recency_score);
            }
        }
        score
    }
}

#[derive(Debug)]
pub struct HotPathCache {
    profitable_pairs: RwLock<HashSet<(Address, Address, u32)>>,
    last_profitable: RwLock<HashMap<(Address, Address), Instant>>,
    top_tokens: RwLock<HashSet<Address>>,
    failed_pairs: RwLock<HashMap<(Address, Address, u32), Instant>>,
    discovery_probability: f32,
    recency_window: Duration,
    hot_token_count: usize,
    failure_backoff: Duration,
}

impl HotPathCache {
    #[allow(dead_code)]
    pub fn new(
        discovery_probability: f32,
        recency_window: Duration,
        hot_token_count: usize,
    ) -> Arc<Self> {
        Self::with_failure_backoff(
            discovery_probability,
            recency_window,
            hot_token_count,
            Duration::from_secs(60),
        )
    }

    pub fn with_failure_backoff(
        discovery_probability: f32,
        recency_window: Duration,
        hot_token_count: usize,
        failure_backoff: Duration,
    ) -> Arc<Self> {
        let probability = discovery_probability.clamp(0.0, 1.0);
        Arc::new(Self {
            profitable_pairs: RwLock::new(HashSet::new()),
            last_profitable: RwLock::new(HashMap::new()),
            top_tokens: RwLock::new(HashSet::new()),
            failed_pairs: RwLock::new(HashMap::new()),
            discovery_probability: probability,
            recency_window,
            hot_token_count: hot_token_count.max(1),
            failure_backoff,
        })
    }

    pub async fn update_top_tokens(&self, base_profiles: &HashMap<Address, TradeSizing>) {
        let mut tokens: Vec<(Address, TradeSizing)> = base_profiles
            .iter()
            .map(|(addr, profile)| (*addr, *profile))
            .collect();
        tokens.sort_by_key(|t| std::cmp::Reverse(t.1.base_amount));
        let mut hot = HashSet::new();
        for (idx, (addr, profile)) in tokens.into_iter().enumerate() {
            if idx >= self.hot_token_count {
                break;
            }
            if profile.base_amount.is_zero() {
                break;
            }
            hot.insert(addr);
        }
        let mut guard = self.top_tokens.write().await;
        *guard = hot;
    }

    pub async fn should_quote(&self, from: Address, to: Address, fee: u32) -> bool {
        self.prune_stale().await;

        let pair_in_backoff = {
            let failures = self.failed_pairs.read().await;
            failures
                .get(&(from, to, fee))
                .is_some_and(|ts| ts.elapsed() < self.failure_backoff)
        };

        if pair_in_backoff {
            return false;
        }

        let baseline = {
            let pairs = self.profitable_pairs.read().await;
            if pairs.contains(&(from, to, fee)) {
                return true;
            }
            pairs.is_empty()
        };

        if baseline {
            return true;
        }

        {
            let recency = self.last_profitable.read().await;
            if let Some(ts) = recency.get(&(from, to)) {
                if ts.elapsed() <= self.recency_window {
                    return true;
                }
            }
        }

        let top_tokens = self.top_tokens.read().await;
        if !top_tokens.is_empty() {
            if top_tokens.contains(&from) || top_tokens.contains(&to) {
                return true;
            }
        } else if baseline {
            return true;
        }
        drop(top_tokens);

        rand::thread_rng().gen::<f32>() < self.discovery_probability
    }

    pub async fn mark_profitable_pair(&self, from: Address, to: Address, fee: u32) {
        self.prune_stale().await;
        {
            let mut pairs = self.profitable_pairs.write().await;
            pairs.insert((from, to, fee));
        }
        {
            let mut recency = self.last_profitable.write().await;
            recency.insert((from, to), Instant::now());
        }
    }

    pub async fn record_failure(&self, from: Address, to: Address, fee: u32) {
        self.prune_stale().await;
        let mut failures = self.failed_pairs.write().await;
        failures.insert((from, to, fee), Instant::now());
    }

    async fn prune_stale(&self) {
        let now = Instant::now();
        let mut stale_pairs: HashSet<(Address, Address)> = HashSet::new();
        {
            let mut recency = self.last_profitable.write().await;
            recency.retain(|pair, ts| {
                if now.duration_since(*ts) > self.recency_window {
                    stale_pairs.insert(*pair);
                    false
                } else {
                    true
                }
            });
        }
        {
            let mut failures = self.failed_pairs.write().await;
            failures.retain(|(from, to, _), ts| {
                if now.duration_since(*ts) > self.failure_backoff {
                    return false;
                }
                !stale_pairs.contains(&(*from, *to))
            });
        }

        if stale_pairs.is_empty() {
            return;
        }
        let mut pairs = self.profitable_pairs.write().await;
        pairs.retain(|(from, to, _)| !stale_pairs.contains(&(*from, *to)));
    }

    pub async fn mark_cycle(&self, edges: &[(Address, Address, u32)]) {
        for &(from, to, fee) in edges {
            self.mark_profitable_pair(from, to, fee).await;
        }
    }

    pub async fn profitability_snapshot(&self) -> ProfitabilitySnapshot {
        self.prune_stale().await;
        let pairs = self.profitable_pairs.read().await;
        let recency = self.last_profitable.read().await;
        ProfitabilitySnapshot {
            profitable_pairs: pairs.clone(),
            last_profitable: recency.clone(),
            recency_window: self.recency_window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn startup_allows_quotes_even_with_top_token_filter() {
        let cache = HotPathCache::with_failure_backoff(
            0.0,
            Duration::from_secs(300),
            1,
            Duration::from_secs(60),
        );
        let from = Address::from_low_u64_be(1);
        let to = Address::from_low_u64_be(2);

        let mut profiles = HashMap::new();
        profiles.insert(from, TradeSizing::new(ethers::types::U256::from(10u64), 50));
        profiles.insert(to, TradeSizing::new(ethers::types::U256::from(1u64), 50));
        cache.update_top_tokens(&profiles).await;

        assert!(cache.should_quote(from, to, 500).await);
    }

    #[tokio::test]
    async fn top_token_anchor_keeps_exploration_live_after_profit_snapshot() {
        let cache = HotPathCache::with_failure_backoff(
            0.0,
            Duration::from_secs(300),
            1,
            Duration::from_secs(60),
        );

        let top = Address::from_low_u64_be(1);
        let tail = Address::from_low_u64_be(2);
        let seeded_a = Address::from_low_u64_be(10);
        let seeded_b = Address::from_low_u64_be(11);

        let mut profiles = HashMap::new();
        profiles.insert(top, TradeSizing::new(ethers::types::U256::from(10u64), 50));
        profiles.insert(tail, TradeSizing::new(ethers::types::U256::from(1u64), 50));
        cache.update_top_tokens(&profiles).await;

        cache.mark_profitable_pair(seeded_a, seeded_b, 500).await;

        assert!(cache.should_quote(top, tail, 500).await);
    }

    #[tokio::test]
    async fn failure_backoff_throttles_recently_failed_profitable_pair() {
        let cache = HotPathCache::with_failure_backoff(
            0.0,
            Duration::from_secs(300),
            1,
            Duration::from_secs(60),
        );
        let from = Address::from_low_u64_be(1);
        let to = Address::from_low_u64_be(2);

        cache.mark_profitable_pair(from, to, 500).await;
        cache.record_failure(from, to, 500).await;

        assert!(!cache.should_quote(from, to, 500).await);
    }

    #[tokio::test]
    async fn failure_backoff_throttles_recently_failed_top_token_pair() {
        let cache = HotPathCache::with_failure_backoff(
            0.0,
            Duration::from_secs(300),
            1,
            Duration::from_secs(60),
        );
        let from = Address::from_low_u64_be(1);
        let to = Address::from_low_u64_be(2);

        let mut profiles = HashMap::new();
        profiles.insert(from, TradeSizing::new(ethers::types::U256::from(10u64), 50));
        profiles.insert(to, TradeSizing::new(ethers::types::U256::from(1u64), 50));
        cache.update_top_tokens(&profiles).await;

        cache.record_failure(from, to, 500).await;

        assert!(!cache.should_quote(from, to, 500).await);
    }
}
