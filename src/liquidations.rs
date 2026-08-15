use anyhow::{anyhow, Context, Result};
use ethers::{
    abi::{ParamType, Token},
    prelude::*,
    providers::{JsonRpcClient, PubsubClient},
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::graph::{Edge, VenueEdge};
use crate::math::mul_div;
use crate::util::parse_selector;
use crate::ops_inputs::{LiquidationMarketKind, LiquidationMarketSpec, OpsInputs};
use crate::util::{compute_edge_weight, NativePrice};

const HEALTH_FACTOR_THRESHOLD_RAW: u128 = 1_020_000_000_000_000_000u128;
const DEFAULT_BONUS_BPS: u32 = 800;
const DEFAULT_EXCHANGE_RATE_BPS: u32 = 10_000;
const DEFAULT_SELECTOR: &str = "0x6f307dc3";
const DEFAULT_ESTIMATED_GAS: u64 = 350_000;
const DEFAULT_CANDIDATE_TTL_SECS: u64 = 120;
const DEFAULT_CANDIDATE_MIN_SCORE: u32 = 2;
const DEFAULT_CANDIDATE_MAX: usize = 50;
const DEFAULT_REVERT_THRESHOLD: u32 = 3;
const DEFAULT_REVERT_COOLDOWN_SECS: u64 = 600;

const AAVE_USER_ACCOUNT_DATA_UPDATED_EVENT: &str =
    "UserAccountDataUpdated(address,uint256,uint256,uint256,uint256,uint256,uint256)";
const COMPOUND_BORROW_EVENT: &str = "Borrow(address,address,uint256,uint256)";
const COMPOUND_WITHDRAW_EVENT: &str = "Withdraw(address,address,uint256)";

#[derive(Clone, Debug)]
pub struct LiquidationOpportunity {
    pub debt_token: Address,
    pub collateral_token: Address,
    pub repay_amount: U256,
    pub expected_collateral: U256,
    pub call: LiquidationCall,
    pub estimated_gas: u64,
    pub protocol: String,
}

#[derive(Clone, Debug)]
pub struct LiquidationCall {
    pub adapter: Address,
    pub selector: [u8; 4],
    pub flash_loan_pool: Address,
    pub debt_token: Address,
    pub collateral_token: Address,
    pub user: Address,
    pub receive_atoken: bool,
}

impl LiquidationOpportunity {
    pub fn to_edge(&self, _gas_price: U256, _base_amount: U256, _native_price: NativePrice) -> Edge {
        let rate_num = self.expected_collateral;
        let rate_den = self.repay_amount;
        let weight = compute_edge_weight(rate_num, rate_den);
        info!(
            protocol = %self.protocol,
            debt = %format!("{:#x}", self.debt_token),
            collateral = %format!("{:#x}", self.collateral_token),
            "Added liquidation edge"
        );
        Edge {
            from: self.debt_token,
            to: self.collateral_token,
            rate_num,
            rate_den,
            venue: VenueEdge::Liquidation {
                adapter: self.call.adapter,
                selector: self.call.selector,
                flash_loan_pool: self.call.flash_loan_pool,
                debt_token: self.call.debt_token,
                collateral_token: self.call.collateral_token,
                user: self.call.user,
                receive_atoken: self.call.receive_atoken,
                protocol: self.protocol.clone(),
            },
            estimated_gas: self.estimated_gas,
            weight,
            max_input: self.repay_amount,
            tolerance_bps: 0,
            observed_slippage_bps: 0,
            quote_block: None,
            active: true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum MarketKind {
    Aave,
    Compound,
}

#[derive(Clone, Debug)]
struct AaveV3Config {
    pool: Address,
    data_provider: Address,
    price_oracle: Address,
}

#[derive(Clone, Debug)]
struct CompoundV3Config {
    comet: Address,
    #[allow(dead_code)]
    rewards: Address,
    #[allow(dead_code)]
    configurator: Address,
    oracle: Address,
}

#[derive(Clone, Debug)]
struct LiquidationMarket {
    name: String,
    kind: MarketKind,
    adapter: Address,
    flash_loan_pool: Address,
    debt_token: Address,
    collateral_token: Address,
    bonus_bps: u32,
    exchange_rate_bps: u32,
    estimated_gas: u64,
    selector: [u8; 4],
    receive_atoken: bool,
    max_repay: Option<U256>,
    candidates: Arc<Mutex<CandidateTracker>>,
    breaker: Arc<Mutex<RevertBreaker>>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct LiquidationChainConfig {
    chain_name: String,
    aave_v3: Option<AaveV3Config>,
    compound_v3: Option<CompoundV3Config>,
    markets: Vec<LiquidationMarket>,
    candidate_ttl: Duration,
    candidate_min_score: u32,
    candidate_max: usize,
    revert_threshold: u32,
    revert_cooldown: Duration,
}

#[derive(Clone, Debug)]
struct CandidateTracker {
    ttl: Duration,
    min_score: u32,
    max_candidates: usize,
    entries: HashMap<Address, CandidateEntry>,
}

#[derive(Clone, Debug)]
struct CandidateEntry {
    score: u32,
    expires_at: Instant,
    last_seen: Instant,
}

impl CandidateTracker {
    fn new(ttl: Duration, min_score: u32, max_candidates: usize) -> Self {
        Self {
            ttl,
            min_score,
            max_candidates,
            entries: HashMap::new(),
        }
    }

    fn record(&mut self, borrower: Address, score_delta: u32) {
        let now = Instant::now();
        let entry = self.entries.entry(borrower).or_insert(CandidateEntry {
            score: 0,
            expires_at: now + self.ttl,
            last_seen: now,
        });
        entry.score = entry.score.saturating_add(score_delta);
        entry.expires_at = now + self.ttl;
        entry.last_seen = now;
    }

    fn remove(&mut self, borrower: &Address) {
        self.entries.remove(borrower);
    }

    fn snapshot(&mut self) -> Vec<Address> {
        let now = Instant::now();
        self.entries.retain(|_, entry| entry.expires_at > now);
        let mut scored: Vec<(Address, u32)> = self
            .entries
            .iter()
            .filter_map(|(addr, entry)| {
                if entry.score >= self.min_score {
                    Some((*addr, entry.score))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored
            .into_iter()
            .take(self.max_candidates)
            .map(|(addr, _)| addr)
            .collect()
    }
}

#[derive(Clone, Debug)]
struct RevertBreaker {
    max_reverts: u32,
    cooldown: Duration,
    consecutive: u32,
    paused_until: Option<Instant>,
}

impl RevertBreaker {
    fn new(max_reverts: u32, cooldown: Duration) -> Self {
        Self {
            max_reverts: max_reverts.max(1),
            cooldown,
            consecutive: 0,
            paused_until: None,
        }
    }

    fn is_active(&self) -> bool {
        match self.paused_until {
            Some(until) => until > Instant::now(),
            None => false,
        }
    }

    fn record_success(&mut self) {
        self.consecutive = 0;
        self.paused_until = None;
    }

    fn record_revert(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= self.max_reverts {
            self.consecutive = 0;
            self.paused_until = Some(Instant::now() + self.cooldown);
            return true;
        }
        false
    }
}

pub struct LiquidationMonitor<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    call_provider: Arc<Provider<C>>,
    markets: Vec<LiquidationMarket>,
    aave: Option<AaveV3Config>,
    compound: Option<CompoundV3Config>,
    market_index: HashMap<String, usize>,
}

impl LiquidationChainConfig {
    fn from_ops_inputs(chain_name: &str, ops_inputs: &OpsInputs) -> Result<Option<Self>> {
        let Some(entry) = ops_inputs.liquidation_markets_for(chain_name) else {
            return Ok(None);
        };

        let aave_v3 = entry
            .aave_v3
            .as_ref()
            .map(|cfg| parse_aave_config(chain_name, cfg))
            .transpose()?;
        let compound_v3 = entry
            .compound_v3
            .as_ref()
            .map(|cfg| parse_compound_config(chain_name, cfg))
            .transpose()?;

        let candidate_ttl = Duration::from_secs(
            entry
                .candidate_ttl_secs
                .unwrap_or(DEFAULT_CANDIDATE_TTL_SECS),
        );
        let candidate_min_score = entry
            .candidate_min_score
            .unwrap_or(DEFAULT_CANDIDATE_MIN_SCORE);
        let candidate_max = entry.candidate_max.unwrap_or(DEFAULT_CANDIDATE_MAX);
        let revert_threshold = entry.revert_threshold.unwrap_or(DEFAULT_REVERT_THRESHOLD);
        let revert_cooldown = Duration::from_secs(
            entry
                .revert_cooldown_secs
                .unwrap_or(DEFAULT_REVERT_COOLDOWN_SECS),
        );

        let mut markets = Vec::new();
        for market in &entry.markets {
            markets.push(parse_market_config(
                chain_name,
                market,
                candidate_ttl,
                candidate_min_score,
                candidate_max,
                revert_threshold,
                revert_cooldown,
            )?);
        }

        Ok(Some(Self {
            chain_name: chain_name.to_string(),
            aave_v3,
            compound_v3,
            markets,
            candidate_ttl,
            candidate_min_score,
            candidate_max,
            revert_threshold,
            revert_cooldown,
        }))
    }
}

impl<C> LiquidationMonitor<C>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    pub fn from_ops_inputs<S>(
        call_provider: Arc<Provider<C>>,
        sub_provider: Arc<Provider<S>>,
        chain_name: &str,
        ops_inputs: &OpsInputs,
    ) -> Result<Option<Self>>
    where
        S: JsonRpcClient + PubsubClient + Clone + Send + Sync + 'static,
    {
        let Some(config) = LiquidationChainConfig::from_ops_inputs(chain_name, ops_inputs)? else {
            return Ok(None);
        };

        if config.markets.is_empty() {
            return Ok(None);
        }

        let mut monitor = Self {
            call_provider,
            markets: config.markets,
            aave: config.aave_v3,
            compound: config.compound_v3,
            market_index: HashMap::new(),
        };

        for (idx, market) in monitor.markets.iter().enumerate() {
            monitor.market_index.insert(market.name.clone(), idx);
        }

        monitor.spawn_listeners(sub_provider);

        Ok(Some(monitor))
    }

    pub async fn record_revert(&self, market: &str) {
        let Some(index) = self.market_index.get(market) else {
            return;
        };
        let breaker = &self.markets[*index].breaker;
        let mut guard = breaker.lock().await;
        if guard.record_revert() {
            warn!(market = %market, "Liquidation circuit breaker active after repeated reverts");
        }
    }

    pub async fn record_success(&self, market: &str) {
        let Some(index) = self.market_index.get(market) else {
            return;
        };
        let breaker = &self.markets[*index].breaker;
        breaker.lock().await.record_success();
    }

    pub async fn poll(&self) -> Result<Vec<LiquidationOpportunity>> {
        let mut opportunities = Vec::new();

        for market in &self.markets {
            if market.breaker.lock().await.is_active() {
                continue;
            }
            let borrowers = market.candidates.lock().await.snapshot();
            if borrowers.is_empty() {
                continue;
            }
            match market.kind {
                MarketKind::Aave => {
                    opportunities.extend(self.poll_aave_like(market, &borrowers).await?);
                }
                MarketKind::Compound => {
                    opportunities.extend(self.poll_compound_like(market, &borrowers).await?);
                }
            }
        }

        Ok(opportunities)
    }

    async fn poll_aave_like(
        &self,
        market: &LiquidationMarket,
        borrowers: &[Address],
    ) -> Result<Vec<LiquidationOpportunity>> {
        let Some(aave) = self.aave.as_ref() else {
            return Ok(Vec::new());
        };
        let selector = ethers::utils::id("getUserAccountData(address)");
        let outputs = vec![
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
        ];
        let reserve_outputs = vec![
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(40),
            ParamType::Bool,
        ];

        let mut opportunities = Vec::new();
        for borrower in borrowers {
            let mut data = selector.to_vec();
            data.extend(ethers::abi::encode(&[Token::Address(*borrower)]));
            let call = TransactionRequest {
                to: Some(NameOrAddress::Address(aave.pool)),
                data: Some(Bytes::from(data)),
                ..Default::default()
            };
            let raw = self.call_provider.call(&call.into(), None).await?;
            if raw.0.is_empty() {
                continue;
            }
            let decoded = ethers::abi::decode(&outputs, raw.0.as_ref())?;
            if decoded.len() < 6 {
                continue;
            }
            let total_debt = decoded[1]
                .clone()
                .into_uint()
                .ok_or_else(|| anyhow!("expected uint for total debt"))?;
            let health_factor = decoded[5]
                .clone()
                .into_uint()
                .ok_or_else(|| anyhow!("expected uint for health factor"))?;
            let threshold = U256::from(HEALTH_FACTOR_THRESHOLD_RAW);
            if health_factor >= threshold {
                market.candidates.lock().await.remove(borrower);
                continue;
            }
            if total_debt.is_zero() {
                market.candidates.lock().await.remove(borrower);
                continue;
            }

            let debt_amount = self
                .fetch_aave_reserve_balance(aave, market.debt_token, *borrower, &reserve_outputs)
                .await?;
            let collateral_amount = self
                .fetch_aave_reserve_balance(
                    aave,
                    market.collateral_token,
                    *borrower,
                    &reserve_outputs,
                )
                .await?;
            if collateral_amount.is_zero() {
                market.candidates.lock().await.remove(borrower);
                continue;
            }
            if debt_amount.is_zero() {
                market.candidates.lock().await.remove(borrower);
                continue;
            }

            let mut repay_amount = debt_amount;
            if let Some(max_repay) = market.max_repay {
                repay_amount = repay_amount.min(max_repay);
            }
            if repay_amount.is_zero() {
                continue;
            }
            let expected_collateral = self
                .estimate_collateral_out(
                    aave.price_oracle,
                    market.debt_token,
                    market.collateral_token,
                    repay_amount,
                    market.bonus_bps,
                    market.exchange_rate_bps,
                )
                .await?;
            if expected_collateral <= repay_amount {
                continue;
            }

            opportunities.push(LiquidationOpportunity {
                debt_token: market.debt_token,
                collateral_token: market.collateral_token,
                repay_amount,
                expected_collateral,
                call: LiquidationCall {
                    adapter: market.adapter,
                    selector: market.selector,
                    flash_loan_pool: market.flash_loan_pool,
                    debt_token: market.debt_token,
                    collateral_token: market.collateral_token,
                    user: *borrower,
                    receive_atoken: market.receive_atoken,
                },
                estimated_gas: market.estimated_gas,
                protocol: market.name.clone(),
            });
        }

        Ok(opportunities)
    }

    async fn poll_compound_like(
        &self,
        market: &LiquidationMarket,
        borrowers: &[Address],
    ) -> Result<Vec<LiquidationOpportunity>> {
        let Some(compound) = self.compound.as_ref() else {
            return Ok(Vec::new());
        };

        let mut opportunities = Vec::new();
        for borrower in borrowers {
            if !self
                .is_compound_liquidatable(compound.comet, *borrower)
                .await?
            {
                market.candidates.lock().await.remove(borrower);
                continue;
            }
            let debt_amount = self
                .fetch_compound_borrow(compound.comet, *borrower)
                .await?;
            let collateral_amount = self
                .fetch_compound_collateral(compound.comet, *borrower, market.collateral_token)
                .await?;
            if debt_amount.is_zero() || collateral_amount.is_zero() {
                market.candidates.lock().await.remove(borrower);
                continue;
            }
            let mut repay_amount = debt_amount;
            if let Some(max_repay) = market.max_repay {
                repay_amount = repay_amount.min(max_repay);
            }
            if repay_amount.is_zero() {
                continue;
            }
            let expected_collateral = self
                .estimate_collateral_out(
                    compound.oracle,
                    market.debt_token,
                    market.collateral_token,
                    repay_amount,
                    market.bonus_bps,
                    market.exchange_rate_bps,
                )
                .await?;
            if expected_collateral <= repay_amount {
                continue;
            }
            opportunities.push(LiquidationOpportunity {
                debt_token: market.debt_token,
                collateral_token: market.collateral_token,
                repay_amount,
                expected_collateral,
                call: LiquidationCall {
                    adapter: market.adapter,
                    selector: market.selector,
                    flash_loan_pool: market.flash_loan_pool,
                    debt_token: market.debt_token,
                    collateral_token: market.collateral_token,
                    user: *borrower,
                    receive_atoken: market.receive_atoken,
                },
                estimated_gas: market.estimated_gas,
                protocol: market.name.clone(),
            });
        }

        Ok(opportunities)
    }

    async fn fetch_aave_reserve_balance(
        &self,
        aave: &AaveV3Config,
        asset: Address,
        borrower: Address,
        outputs: &[ParamType],
    ) -> Result<U256> {
        let selector = ethers::utils::id("getUserReserveData(address,address)");
        let mut data = selector.to_vec();
        data.extend(ethers::abi::encode(&[
            Token::Address(asset),
            Token::Address(borrower),
        ]));
        let call = TransactionRequest {
            to: Some(NameOrAddress::Address(aave.data_provider)),
            data: Some(Bytes::from(data)),
            ..Default::default()
        };
        let raw = self.call_provider.call(&call.into(), None).await?;
        if raw.0.is_empty() {
            return Ok(U256::zero());
        }
        let decoded = ethers::abi::decode(outputs, raw.0.as_ref())?;
        if decoded.len() < 3 {
            return Ok(U256::zero());
        }
        let a_token_balance = decoded[0]
            .clone()
            .into_uint()
            .ok_or_else(|| anyhow!("expected uint for aToken balance"))?;
        let stable_debt = decoded[1]
            .clone()
            .into_uint()
            .ok_or_else(|| anyhow!("expected uint for stable debt"))?;
        let variable_debt = decoded[2]
            .clone()
            .into_uint()
            .ok_or_else(|| anyhow!("expected uint for variable debt"))?;
        if !stable_debt.is_zero() || !variable_debt.is_zero() {
            return Ok(stable_debt.saturating_add(variable_debt));
        }
        Ok(a_token_balance)
    }

    async fn is_compound_liquidatable(&self, comet: Address, borrower: Address) -> Result<bool> {
        let selector = ethers::utils::id("isLiquidatable(address)");
        let mut data = selector.to_vec();
        data.extend(ethers::abi::encode(&[Token::Address(borrower)]));
        let call = TransactionRequest {
            to: Some(NameOrAddress::Address(comet)),
            data: Some(Bytes::from(data)),
            ..Default::default()
        };
        let raw = self.call_provider.call(&call.into(), None).await;
        let Ok(raw) = raw else {
            return Ok(false);
        };
        if raw.0.is_empty() {
            return Ok(false);
        }
        let decoded = ethers::abi::decode(&[ParamType::Bool], raw.0.as_ref())?;
        Ok(decoded
            .first()
            .and_then(|token| token.clone().into_bool())
            .unwrap_or(false))
    }

    async fn fetch_compound_borrow(&self, comet: Address, borrower: Address) -> Result<U256> {
        let selector = ethers::utils::id("borrowBalanceOf(address)");
        let mut data = selector.to_vec();
        data.extend(ethers::abi::encode(&[Token::Address(borrower)]));
        let call = TransactionRequest {
            to: Some(NameOrAddress::Address(comet)),
            data: Some(Bytes::from(data)),
            ..Default::default()
        };
        let raw = self.call_provider.call(&call.into(), None).await?;
        if raw.0.is_empty() {
            return Ok(U256::zero());
        }
        let decoded = ethers::abi::decode(&[ParamType::Uint(256)], raw.0.as_ref())?;
        Ok(decoded
            .first()
            .and_then(|token| token.clone().into_uint())
            .unwrap_or_default())
    }

    async fn fetch_compound_collateral(
        &self,
        comet: Address,
        borrower: Address,
        asset: Address,
    ) -> Result<U256> {
        let selector = ethers::utils::id("collateralBalanceOf(address,address)");
        let mut data = selector.to_vec();
        data.extend(ethers::abi::encode(&[
            Token::Address(borrower),
            Token::Address(asset),
        ]));
        let call = TransactionRequest {
            to: Some(NameOrAddress::Address(comet)),
            data: Some(Bytes::from(data)),
            ..Default::default()
        };
        let raw = self.call_provider.call(&call.into(), None).await?;
        if raw.0.is_empty() {
            return Ok(U256::zero());
        }
        let decoded = ethers::abi::decode(&[ParamType::Uint(256)], raw.0.as_ref())?;
        Ok(decoded
            .first()
            .and_then(|token| token.clone().into_uint())
            .unwrap_or_default())
    }

    async fn estimate_collateral_out(
        &self,
        oracle: Address,
        debt_token: Address,
        collateral_token: Address,
        repay_amount: U256,
        bonus_bps: u32,
        fallback_exchange_bps: u32,
    ) -> Result<U256> {
        let debt_price = self.fetch_oracle_price(oracle, debt_token).await;
        let collateral_price = self.fetch_oracle_price(oracle, collateral_token).await;
        if let (Ok(debt_price), Ok(collateral_price)) = (debt_price, collateral_price) {
            if collateral_price.is_zero() {
                return Ok(U256::zero());
            }
            let repay_value = repay_amount.saturating_mul(debt_price);
            let base_out = repay_value
                .checked_div(collateral_price)
                .unwrap_or_default();
            let bonus_out = mul_div(
                base_out,
                U256::from(bonus_bps as u64),
                U256::from(10_000u64),
            );
            return Ok(base_out.saturating_add(bonus_out));
        }
        let base_out = mul_div(
            repay_amount,
            U256::from(fallback_exchange_bps as u64),
            U256::from(10_000u64),
        );
        let bonus_out = mul_div(
            base_out,
            U256::from(bonus_bps as u64),
            U256::from(10_000u64),
        );
        Ok(base_out.saturating_add(bonus_out))
    }

    async fn fetch_oracle_price(&self, oracle: Address, asset: Address) -> Result<U256> {
        match self
            .call_oracle_price(oracle, "getAssetPrice(address)", asset)
            .await
        {
            Ok(price) => Ok(price),
            Err(_) => {
                self.call_oracle_price(oracle, "getPrice(address)", asset)
                    .await
            }
        }
    }

    async fn call_oracle_price(
        &self,
        oracle: Address,
        selector: &str,
        asset: Address,
    ) -> Result<U256> {
        let selector = ethers::utils::id(selector);
        let mut data = selector.to_vec();
        data.extend(ethers::abi::encode(&[Token::Address(asset)]));
        let call = TransactionRequest {
            to: Some(NameOrAddress::Address(oracle)),
            data: Some(Bytes::from(data)),
            ..Default::default()
        };
        let raw = self.call_provider.call(&call.into(), None).await?;
        if raw.0.is_empty() {
            return Ok(U256::zero());
        }
        let decoded = ethers::abi::decode(&[ParamType::Uint(256)], raw.0.as_ref())?;
        Ok(decoded
            .first()
            .and_then(|token| token.clone().into_uint())
            .unwrap_or_default())
    }

    fn spawn_listeners<S>(&self, provider: Arc<Provider<S>>)
    where
        S: JsonRpcClient + PubsubClient + Clone + Send + Sync + 'static,
    {
        if let Some(aave) = self.aave.as_ref() {
            let trackers: Vec<_> = self
                .markets
                .iter()
                .filter(|market| matches!(market.kind, MarketKind::Aave))
                .map(|market| market.candidates.clone())
                .collect();
            if !trackers.is_empty() {
                Self::spawn_aave_listener(provider.clone(), aave.pool, trackers);
            }
        }

        if let Some(compound) = self.compound.as_ref() {
            let trackers: Vec<_> = self
                .markets
                .iter()
                .filter(|market| matches!(market.kind, MarketKind::Compound))
                .map(|market| market.candidates.clone())
                .collect();
            if !trackers.is_empty() {
                Self::spawn_compound_listener(provider, compound.comet, trackers);
            }
        }
    }

    fn spawn_aave_listener<S>(
        provider: Arc<Provider<S>>,
        pool: Address,
        trackers: Vec<Arc<Mutex<CandidateTracker>>>,
    ) where
        S: JsonRpcClient + PubsubClient + Clone + Send + Sync + 'static,
    {
        tokio::spawn(async move {
            let filter = Filter::new()
                .address(pool)
                .topic0(H256::from(ethers::utils::keccak256(
                    AAVE_USER_ACCOUNT_DATA_UPDATED_EVENT,
                )));

            loop {
                match provider.subscribe_logs(&filter).await {
                    Ok(mut stream) => {
                        debug!("Subscribed to Aave v3 health factor updates");
                        while let Some(log) = stream.next().await {
                            if log.topics.len() < 2 {
                                continue;
                            }
                            let raw = log.topics[1].as_bytes();
                            if raw.len() < 32 {
                                continue;
                            }
                            let borrower = Address::from_slice(&raw[12..]);
                            let threshold = U256::from(HEALTH_FACTOR_THRESHOLD_RAW);
                            match decode_health_factor(&log.data.0) {
                                Some(health_factor) if health_factor < threshold => {
                                    debug!(
                                        borrower = %format!("{:#x}", borrower),
                                        "Borrower below health factor threshold"
                                    );
                                    for tracker in &trackers {
                                        tracker.lock().await.record(borrower, 3);
                                    }
                                }
                                Some(_) => {
                                    for tracker in &trackers {
                                        tracker.lock().await.remove(&borrower);
                                    }
                                }
                                None => {
                                    warn!("Failed to decode health factor from Aave event");
                                }
                            }
                        }
                        warn!("Aave v3 health factor subscription ended, retrying");
                    }
                    Err(err) => {
                        error!(error = %err, "Failed to subscribe to Aave v3 events");
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }

    fn spawn_compound_listener<S>(
        provider: Arc<Provider<S>>,
        comet: Address,
        trackers: Vec<Arc<Mutex<CandidateTracker>>>,
    ) where
        S: JsonRpcClient + PubsubClient + Clone + Send + Sync + 'static,
    {
        tokio::spawn(async move {
            let borrow_topic = H256::from(ethers::utils::keccak256(COMPOUND_BORROW_EVENT));
            let withdraw_topic = H256::from(ethers::utils::keccak256(COMPOUND_WITHDRAW_EVENT));
            let filter = Filter::new()
                .address(comet)
                .topic0(vec![borrow_topic, withdraw_topic]);

            loop {
                match provider.subscribe_logs(&filter).await {
                    Ok(mut stream) => {
                        debug!("Subscribed to Compound v3 borrow/withdraw events");
                        while let Some(log) = stream.next().await {
                            if log.topics.len() < 2 {
                                continue;
                            }
                            let raw = log.topics[1].as_bytes();
                            if raw.len() < 32 {
                                continue;
                            }
                            let borrower = Address::from_slice(&raw[12..]);
                            for tracker in &trackers {
                                tracker.lock().await.record(borrower, 1);
                            }
                        }
                        warn!("Compound v3 subscription ended, retrying");
                    }
                    Err(err) => {
                        error!(error = %err, "Failed to subscribe to Compound v3 events");
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }
}

fn parse_aave_config(
    chain_name: &str,
    cfg: &crate::ops_inputs::AaveV3Addresses,
) -> Result<AaveV3Config> {
    Ok(AaveV3Config {
        pool: parse_address(chain_name, "aave_v3.pool", &cfg.pool)?,
        data_provider: parse_address(chain_name, "aave_v3.data_provider", &cfg.data_provider)?,
        price_oracle: parse_address(chain_name, "aave_v3.price_oracle", &cfg.price_oracle)?,
    })
}

fn parse_compound_config(
    chain_name: &str,
    cfg: &crate::ops_inputs::CompoundV3Addresses,
) -> Result<CompoundV3Config> {
    Ok(CompoundV3Config {
        comet: parse_address(chain_name, "compound_v3.comet", &cfg.comet)?,
        rewards: parse_address(chain_name, "compound_v3.rewards", &cfg.rewards)?,
        configurator: parse_address(chain_name, "compound_v3.configurator", &cfg.configurator)?,
        oracle: parse_address(chain_name, "compound_v3.oracle", &cfg.oracle)?,
    })
}

fn parse_market_config(
    chain_name: &str,
    market: &LiquidationMarketSpec,
    candidate_ttl: Duration,
    candidate_min_score: u32,
    candidate_max: usize,
    revert_threshold: u32,
    revert_cooldown: Duration,
) -> Result<LiquidationMarket> {
    let kind = match market.kind {
        Some(LiquidationMarketKind::Aave) => MarketKind::Aave,
        Some(LiquidationMarketKind::Compound) => MarketKind::Compound,
        None => return Err(anyhow!("liquidation market kind missing for {chain_name}")),
    };
    let name = market
        .name
        .clone()
        .unwrap_or_else(|| format!("{chain_name}-liquidation"));
    let selector_raw = market
        .selector
        .clone()
        .unwrap_or_else(|| DEFAULT_SELECTOR.to_string());
    let max_repay = match &market.max_repay_wei {
        Some(raw) if !raw.trim().is_empty() => {
            Some(U256::from_dec_str(raw).context("invalid max_repay_wei")?)
        }
        _ => None,
    };

    let bonus_bps = market.bonus_bps.unwrap_or(DEFAULT_BONUS_BPS);
    anyhow::ensure!(
        bonus_bps <= 5_000,
        "liquidation bonus must be <= 5_000bps (got {})",
        bonus_bps
    );
    let estimated_gas = market
        .estimated_gas
        .unwrap_or(DEFAULT_ESTIMATED_GAS)
        .max(DEFAULT_ESTIMATED_GAS);

    Ok(LiquidationMarket {
        name,
        kind,
        adapter: parse_address(chain_name, "market.adapter", &market.adapter)?,
        flash_loan_pool: parse_address(
            chain_name,
            "market.flash_loan_pool",
            &market.flash_loan_pool,
        )?,
        debt_token: parse_address(chain_name, "market.debt_token", &market.debt_token)?,
        collateral_token: parse_address(
            chain_name,
            "market.collateral_token",
            &market.collateral_token,
        )?,
        bonus_bps,
        exchange_rate_bps: market
            .collateral_exchange_rate_bps
            .unwrap_or(DEFAULT_EXCHANGE_RATE_BPS),
        estimated_gas,
        selector: parse_selector(&selector_raw)?,
        receive_atoken: market.receive_atoken.unwrap_or(false),
        max_repay,
        candidates: Arc::new(Mutex::new(CandidateTracker::new(
            candidate_ttl,
            candidate_min_score,
            candidate_max,
        ))),
        breaker: Arc::new(Mutex::new(RevertBreaker::new(
            revert_threshold,
            revert_cooldown,
        ))),
    })
}

fn parse_address(chain_name: &str, field: &str, raw: &str) -> Result<Address> {
    Address::from_str(raw)
        .with_context(|| format!("invalid address for {chain_name} {field}: {raw}"))
}

fn decode_health_factor(data: &[u8]) -> Option<U256> {
    if data.len() < 32 * 6 {
        return None;
    }
    let decoded = ethers::abi::decode(
        &[
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
        ],
        data,
    )
    .ok()?;
    decoded.last()?.clone().into_uint()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    #[test]
    fn candidate_tracker_caps_and_orders() {
        let mut tracker = CandidateTracker::new(Duration::from_secs(60), 1, 2);
        tracker.record(addr(1), 1);
        tracker.record(addr(2), 5);
        tracker.record(addr(3), 2);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0], addr(2));
        assert_eq!(snapshot[1], addr(3));
    }

    #[test]
    fn revert_breaker_trips_after_threshold() {
        let mut breaker = RevertBreaker::new(2, Duration::from_secs(60));
        assert!(!breaker.is_active());
        assert!(!breaker.record_revert());
        assert!(breaker.record_revert());
        assert!(breaker.is_active());
        breaker.record_success();
        assert!(!breaker.is_active());
    }
}
