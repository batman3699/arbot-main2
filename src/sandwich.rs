use std::{
    collections::{HashMap, HashSet, VecDeque},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use ethers::{
    abi::{decode, ParamType, Token},
    providers::{JsonRpcClient, Middleware, Provider},
    types::{Address, BlockNumber, Transaction, TxHash, U256},
};
use serde::Deserialize;
use tokio::{sync::Mutex, time::sleep};
use tracing::{info, warn};

use crate::quote_univ2::{load_pair_state, UniV2PairState};
use crate::venues::{env_var_with_fallback, parse_pool_configs};

const SANDWICH_ESTIMATED_GAS: u64 = 320_000;
const SEEN_TX_TTL: Duration = Duration::from_secs(300);

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct SandwichOpportunity {
    pub victim_hash: TxHash,
    pub victim_sender: Option<Address>,
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub victim_amount_in: U256,
    pub frontrun_amount_in: U256,
    pub frontrun_expected_out: U256,
    pub backrun_expected_out: U256,
    pub expected_profit: U256,
    pub fee_bps: u32,
    pub estimated_gas: u64,
    pub discovered_at: Instant,
}

#[derive(Clone)]
struct ResolvedPool {
    pair: Address,
    token_in: Address,
    token_out: Address,
    fee_bps: u32,
}

#[derive(Deserialize)]
struct UniV2PoolCfg {
    pair: String,
    #[serde(rename = "tokenIn")]
    token_in: String,
    #[serde(rename = "tokenOut")]
    token_out: String,
    #[serde(default = "UniV2PoolCfg::default_fee_bps", rename = "feeBps")]
    fee_bps: u32,
}

impl UniV2PoolCfg {
    fn default_fee_bps() -> u32 {
        30
    }
}

pub struct SandwichMonitor<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    provider: Arc<Provider<C>>,
    min_profit_wei: U256,
    pools: HashMap<(Address, Address), ResolvedPool>,
    opportunities: Arc<Mutex<VecDeque<SandwichOpportunity>>>,
    seen: Arc<Mutex<SeenTxs>>,
}

impl<C> SandwichMonitor<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn new(
        provider: Arc<Provider<C>>,
        chain_env_prefix: &str,
        min_profit_wei: U256,
    ) -> Result<Self> {
        let pools = load_univ2_pools(chain_env_prefix)?;
        Ok(Self {
            provider,
            min_profit_wei,
            pools,
            opportunities: Arc::new(Mutex::new(VecDeque::new())),
            seen: Arc::new(Mutex::new(SeenTxs::new(SEEN_TX_TTL))),
        })
    }

    pub async fn run(self: Arc<Self>, interval: Duration) {
        loop {
            if let Err(err) = self.scan_once().await {
                warn!(error = %err, "Sandwich monitor scan failed");
            }
            sleep(interval).await;
        }
    }

    async fn scan_once(&self) -> Result<()> {
        let pending = self
            .provider
            .get_block_with_txs(BlockNumber::Pending)
            .await?;

        let Some(block) = pending else {
            return Ok(());
        };

        for tx in block.transactions {
            let hash = tx.hash;
            {
                let mut seen = self.seen.lock().await;
                if !seen.insert(hash, Instant::now()) {
                    continue;
                }
            }

            if let Some((amount_in, path)) = decode_univ2_swap(&tx) {
                if path.len() != 2 {
                    continue;
                }
                let token_in = path[0];
                let token_out = path[1];
                let Some(pool) = self.pools.get(&(token_in, token_out)) else {
                    continue;
                };
                if amount_in.is_zero() {
                    continue;
                }
                let Some(state) = load_pair_state(self.provider.clone(), pool.pair).await? else {
                    continue;
                };
                if let Some(opportunity) = simulate_sandwich(
                    &state,
                    pool,
                    amount_in,
                    hash,
                    Some(tx.from),
                    self.min_profit_wei,
                ) {
                    let mut queue = self.opportunities.lock().await;
                    queue.push_back(opportunity);
                }
            }
        }

        Ok(())
    }

    pub async fn next_opportunity(&self) -> Option<SandwichOpportunity> {
        let mut queue = self.opportunities.lock().await;
        queue.pop_front()
    }
}

struct SeenTxs {
    entries: HashSet<TxHash>,
    order: VecDeque<(Instant, TxHash)>,
    ttl: Duration,
}

impl SeenTxs {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: HashSet::new(),
            order: VecDeque::new(),
            ttl,
        }
    }

    fn insert(&mut self, hash: TxHash, now: Instant) -> bool {
        self.evict_expired(now);
        if !self.entries.insert(hash) {
            return false;
        }
        self.order.push_back((now, hash));
        true
    }

    fn evict_expired(&mut self, now: Instant) {
        while let Some((seen_at, _)) = self.order.front() {
            if now.saturating_duration_since(*seen_at) <= self.ttl {
                break;
            }
            if let Some((_, stale_hash)) = self.order.pop_front() {
                self.entries.remove(&stale_hash);
            }
        }
    }
}

fn decode_univ2_swap(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0.get(0..4)?.try_into().ok()?;
    const SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4] = [0x38, 0xed, 0x17, 0x39];
    const SWAP_EXACT_TOKENS_FOR_TOKENS_SUPPORTING_FEE_ON_TRANSFER: [u8; 4] =
        [0x5c, 0x11, 0xd7, 0x95];
    if selector != SWAP_EXACT_TOKENS_FOR_TOKENS
        && selector != SWAP_EXACT_TOKENS_FOR_TOKENS_SUPPORTING_FEE_ON_TRANSFER
    {
        return None;
    }
    let params = vec![
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Array(Box::new(ParamType::Address)),
        ParamType::Address,
        ParamType::Uint(256),
    ];
    let decoded = decode(&params, &tx.input.0[4..]).ok()?;
    let amount_in = decoded.first()?.clone().into_uint()?;
    if amount_in.is_zero() {
        return None;
    }
    let path_tokens = match decoded.get(2)?.clone() {
        Token::Array(tokens) => tokens,
        _ => return None,
    };
    if path_tokens.len() < 2 {
        return None;
    }
    let mut path = Vec::with_capacity(path_tokens.len());
    for token in path_tokens {
        if let Some(addr) = token.into_address() {
            path.push(addr);
        }
    }
    Some((amount_in, path))
}

fn simulate_sandwich(
    state: &UniV2PairState,
    pool: &ResolvedPool,
    victim_amount_in: U256,
    victim_hash: TxHash,
    victim_sender: Option<Address>,
    min_profit_wei: U256,
) -> Option<SandwichOpportunity> {
    let (reserve_in, reserve_out) = state.reserves_for(pool.token_in)?;
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return None;
    }

    let max_front = reserve_in / U256::from(3u64);
    let candidates = [
        victim_amount_in / U256::from(10u64),
        victim_amount_in / U256::from(5u64),
        victim_amount_in / U256::from(2u64),
        victim_amount_in,
    ];

    let mut best: Option<SandwichOpportunity> = None;
    for mut front_amount in candidates {
        if front_amount.is_zero() {
            continue;
        }
        if front_amount > max_front && !max_front.is_zero() {
            front_amount = max_front;
        }
        if front_amount.is_zero() {
            continue;
        }
        let Some(sim) = evaluate_candidate(
            reserve_in,
            reserve_out,
            front_amount,
            victim_amount_in,
            pool.fee_bps,
        ) else {
            continue;
        };
        if sim.expected_profit <= min_profit_wei {
            continue;
        }
        let candidate = SandwichOpportunity {
            victim_hash,
            victim_sender,
            pool: pool.pair,
            token_in: pool.token_in,
            token_out: pool.token_out,
            victim_amount_in,
            frontrun_amount_in: front_amount,
            frontrun_expected_out: sim.front_out,
            backrun_expected_out: sim.back_out,
            expected_profit: sim.expected_profit,
            fee_bps: pool.fee_bps,
            estimated_gas: SANDWICH_ESTIMATED_GAS,
            discovered_at: Instant::now(),
        };
        match &best {
            Some(existing) if existing.expected_profit >= candidate.expected_profit => {}
            _ => best = Some(candidate),
        }
    }
    best
}

struct SandwichSimulation {
    front_out: U256,
    back_out: U256,
    expected_profit: U256,
}

fn evaluate_candidate(
    reserve_in: U256,
    reserve_out: U256,
    front_amount: U256,
    victim_amount: U256,
    fee_bps: u32,
) -> Option<SandwichSimulation> {
    let (front_out, res_in_after, res_out_after) =
        univ2_swap(reserve_in, reserve_out, front_amount, fee_bps)?;
    if front_out.is_zero() {
        return None;
    }
    let (victim_out, res_in_post_victim, res_out_post_victim) =
        univ2_swap(res_in_after, res_out_after, victim_amount, fee_bps)?;
    if victim_out.is_zero() {
        return None;
    }
    let (back_out, _, _) = univ2_swap(res_out_post_victim, res_in_post_victim, front_out, fee_bps)?;
    if back_out <= front_amount {
        return None;
    }
    Some(SandwichSimulation {
        front_out,
        back_out,
        expected_profit: back_out.saturating_sub(front_amount),
    })
}

fn univ2_swap(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    fee_bps: u32,
) -> Option<(U256, U256, U256)> {
    if reserve_in.is_zero() || reserve_out.is_zero() || amount_in.is_zero() {
        return None;
    }
    let fee_den = U256::from(10_000u64);
    let fee_num = fee_den.saturating_sub(U256::from(fee_bps as u64));
    if fee_num.is_zero() {
        return None;
    }
    let amount_in_with_fee = amount_in * fee_num / fee_den;
    if amount_in_with_fee.is_zero() {
        return None;
    }
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in + amount_in_with_fee;
    if denominator.is_zero() {
        return None;
    }
    let amount_out = numerator / denominator;
    if amount_out.is_zero() || amount_out >= reserve_out {
        return None;
    }
    let new_reserve_in = reserve_in + amount_in;
    let new_reserve_out = reserve_out - amount_out;
    Some((amount_out, new_reserve_in, new_reserve_out))
}

fn load_univ2_pools(chain_env_prefix: &str) -> Result<HashMap<(Address, Address), ResolvedPool>> {
    let env_key = format!("{}_UNIV2_POOLS", chain_env_prefix);
    let Some((raw, source)) = env_var_with_fallback(&env_key, "UNIV2_POOLS") else {
        info!(prefix = %chain_env_prefix, "No UniV2 pool config for sandwich monitor");
        return Ok(HashMap::new());
    };
    let configs: Vec<UniV2PoolCfg> = parse_pool_configs(&raw, &source)?;
    let mut map = HashMap::new();
    for cfg in configs {
        let pair = Address::from_str(&cfg.pair)
            .with_context(|| format!("invalid pair `{}` in {source}", cfg.pair))?;
        let token_in = Address::from_str(&cfg.token_in)
            .with_context(|| format!("invalid tokenIn `{}` in {source}", cfg.token_in))?;
        let token_out = Address::from_str(&cfg.token_out)
            .with_context(|| format!("invalid tokenOut `{}` in {source}", cfg.token_out))?;
        let forward = ResolvedPool {
            pair,
            token_in,
            token_out,
            fee_bps: cfg.fee_bps,
        };
        let reverse = ResolvedPool {
            pair,
            token_in: token_out,
            token_out: token_in,
            fee_bps: cfg.fee_bps,
        };
        map.insert((forward.token_in, forward.token_out), forward);
        map.insert((reverse.token_in, reverse.token_out), reverse);
    }
    Ok(map)
}

// Config loading (env-with-fallback + JSON5/file parsing) is shared with the
// venue config path — see crate::venues. Reused here rather than duplicated so
// the two cannot drift.
