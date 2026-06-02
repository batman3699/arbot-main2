use anyhow::{Context, Result};
use ethers::{prelude::*, providers::JsonRpcClient};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use crate::quote_univ2::{load_pair_state, UniV2PairState};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, MathematicalOps};

const PAIR_CREATED_TOPIC: H256 = H256([
    0x0d, 0x36, 0x48, 0xbd, 0x0f, 0x6b, 0xa8, 0x01, 0x34, 0xa3, 0x3b, 0xa9, 0x27, 0x5a, 0xc5, 0x85,
    0xd9, 0xd3, 0x15, 0xf0, 0xad, 0x83, 0x55, 0xcd, 0xde, 0xfd, 0xe3, 0x1a, 0xfa, 0x28, 0xd0, 0xe9,
]);

#[derive(Clone, Debug)]
pub struct LowLiquidityPool {
    pub pair: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee_bps: u32,
    pub state: UniV2PairState,
    pub price_deviation_bps: u32,
}

pub struct LowLiquidityScanner<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    provider: Arc<Provider<C>>,
    factories: Vec<Address>,
    last_block: HashMap<Address, U64>,
    lookback: u64,
    max_total_tokens: Decimal,
    deviation_threshold_bps: u32,
    tracked_pairs: HashSet<Address>,
}

impl<C> LowLiquidityScanner<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn from_env(provider: Arc<Provider<C>>) -> Result<Option<Self>> {
        let raw = match std::env::var("LOW_LIQUIDITY_FACTORIES") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(anyhow::anyhow!(
                    "environment variable LOW_LIQUIDITY_FACTORIES contains invalid UTF-8"
                ))
            }
        };

        let mut factories = Vec::new();
        for part in raw.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            let addr = Address::from_str(trimmed).with_context(|| {
                format!("invalid factory address `{trimmed}` in LOW_LIQUIDITY_FACTORIES")
            })?;
            factories.push(addr);
        }

        if factories.is_empty() {
            return Ok(None);
        }

        let lookback = std::env::var("LOW_LIQUIDITY_LOOKBACK_BLOCKS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(5000);

        let max_total_tokens = std::env::var("LOW_LIQUIDITY_MAX_TOTAL_TOKENS")
            .ok()
            .and_then(|raw| raw.parse::<f64>().ok())
            .and_then(Decimal::from_f64)
            .unwrap_or_else(|| Decimal::from(1_000u64));

        let deviation_threshold_bps = std::env::var("LOW_LIQUIDITY_PRICE_DEVIATION_BPS")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(2_000);

        Ok(Some(Self {
            provider,
            factories,
            last_block: HashMap::new(),
            lookback,
            max_total_tokens,
            deviation_threshold_bps,
            tracked_pairs: HashSet::new(),
        }))
    }

    pub async fn poll(
        &mut self,
        token_decimals: &HashMap<Address, u8>,
    ) -> Result<Vec<LowLiquidityPool>> {
        let mut pools = Vec::new();
        let latest_block = self.provider.get_block_number().await?;

        for &factory in &self.factories {
            let from_block = self
                .last_block
                .get(&factory)
                .map(|block| block.saturating_add(U64::one()))
                .unwrap_or_else(|| latest_block.saturating_sub(U64::from(self.lookback)));
            let filter = Filter::new()
                .address(factory)
                .from_block(from_block)
                .to_block(latest_block)
                .topic0(PAIR_CREATED_TOPIC);
            let logs = self.provider.get_logs(&filter).await?;
            self.last_block.insert(factory, latest_block);

            for log in logs {
                if log.topics.len() < 3 {
                    continue;
                }
                let token0 = Address::from_slice(&log.topics[1].as_bytes()[12..]);
                let token1 = Address::from_slice(&log.topics[2].as_bytes()[12..]);
                let data = log.data.0;
                if data.len() < 32 {
                    continue;
                }
                let pair = Address::from_slice(&data[12..32]);
                if !self.tracked_pairs.insert(pair) {
                    continue;
                }

                let state = match load_pair_state(self.provider.clone(), pair).await {
                    Ok(Some(state)) => state,
                    Ok(None) => continue,
                    Err(_) => continue,
                };

                let (reserve0_tokens, reserve1_tokens) = (
                    scale_reserve(state.reserve0, token_decimals.get(&state.token0).copied()),
                    scale_reserve(state.reserve1, token_decimals.get(&state.token1).copied()),
                );

                let total_tokens = reserve0_tokens
                    .checked_add(reserve1_tokens)
                    .unwrap_or(Decimal::MAX);
                if total_tokens > self.max_total_tokens {
                    continue;
                }

                if reserve0_tokens.is_zero()
                    || reserve1_tokens.is_zero()
                    || reserve0_tokens.is_sign_negative()
                    || reserve1_tokens.is_sign_negative()
                {
                    continue;
                }

                let ratio = reserve0_tokens
                    .checked_div(reserve1_tokens)
                    .unwrap_or(Decimal::ZERO);
                if ratio.is_zero() || ratio.is_sign_negative() {
                    continue;
                }
                let deviation = if ratio >= Decimal::ONE {
                    ratio - Decimal::ONE
                } else {
                    Decimal::ONE.checked_div(ratio).unwrap_or(Decimal::MAX) - Decimal::ONE
                };
                let deviation_bps = deviation
                    .checked_mul(Decimal::from(10_000u32))
                    .and_then(|v| v.round().to_u32())
                    .unwrap_or(u32::MAX);
                if deviation_bps < self.deviation_threshold_bps {
                    continue;
                }

                pools.push(LowLiquidityPool {
                    pair,
                    token0,
                    token1,
                    fee_bps: 30,
                    state,
                    price_deviation_bps: deviation_bps,
                });
            }
        }

        Ok(pools)
    }
}

fn scale_reserve(reserve: U256, decimals: Option<u8>) -> Decimal {
    if reserve.is_zero() {
        return Decimal::ZERO;
    }
    let mut value = crate::util::u256_to_decimal(reserve);
    if let Some(decimals) = decimals {
        let divisor = Decimal::from(10u64)
            .checked_powu(decimals as u64)
            .unwrap_or(Decimal::MAX);
        value = value.checked_div(divisor).unwrap_or(Decimal::ZERO);
    }
    value
}
