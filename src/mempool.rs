//! Live mempool ingestion: decode pending DEX swaps and feed backrun hints.
//!
//! Prior `spawn_pending_tx_monitor` only counted txs — it never parsed calldata or
//! triggered rescans. Production arb on Base requires sub-block visibility into
//! large swaps that move prices before the next canonical head.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ethers::abi::{decode, ParamType, Token};
use ethers::providers::{JsonRpcClient, Middleware, Provider, Ws};
use ethers::types::{Address, BlockId, BlockNumber, Transaction, U256};
use futures_util::StreamExt;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::ingestion::poll_pending_block;
use crate::math::mul_div;
use crate::metrics::Metrics;
use crate::token_refresh::TokenList;
use crate::util::connect_ws_provider_with_fallbacks;

const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_DECODED_HINTS: usize = 64;

#[derive(Clone, Debug)]
pub struct BackrunHint {
    pub from: Address,
    pub to: Address,
    pub amount_in: U256,
    pub price_impact_bps: u32,
    pub source: String,
    pub observed_at: Instant,
}

#[derive(Debug)]
struct PendingSwap {
    amount_in: U256,
    last_seen: Instant,
}

struct HintParams<'a> {
    from: Address,
    to: Address,
    amount_in: U256,
    estimated_impact: u32,
    source: &'a str,
    now: Instant,
}

#[derive(Debug)]
pub struct BackrunMonitor {
    swaps: Arc<Mutex<HashMap<(Address, Address), PendingSwap>>>,
    hints: Arc<Mutex<VecDeque<BackrunHint>>>,
    min_amount: U256,
    min_price_impact_bps: u32,
    tokens: TokenList,
    decoded_total: std::sync::atomic::AtomicU64,
}

impl BackrunMonitor {
    pub fn new(min_amount: U256, min_price_impact_bps: u32, tokens: TokenList) -> Self {
        Self {
            swaps: Arc::new(Mutex::new(HashMap::new())),
            hints: Arc::new(Mutex::new(VecDeque::new())),
            min_amount,
            min_price_impact_bps,
            tokens,
            decoded_total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[allow(dead_code)]
    pub fn decoded_swap_count(&self) -> u64 {
        self.decoded_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn record(&self, path: &[Address], amount_in: U256, source: &str) {
        if amount_in < self.min_amount || path.len() < 2 {
            return;
        }
        let tokens = self.tokens.current_set();
        if !path.iter().all(|token| tokens.contains(token)) {
            return;
        }
        let mut swaps = self.swaps.lock().await;
        let now = Instant::now();
        let from = path[0];
        let to = path[1];
        let estimated_impact = self.estimate_price_impact(amount_in);
        if estimated_impact < self.min_price_impact_bps {
            return;
        }
        self.push_hint(
            &mut swaps,
            HintParams {
                from,
                to,
                amount_in,
                estimated_impact,
                source,
                now,
            },
        )
        .await;
        self.decoded_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub async fn record_liquidation(
        &self,
        debt_token: Address,
        collateral_token: Address,
        repay_amount: U256,
        protocol: &str,
    ) {
        let mut swaps = self.swaps.lock().await;
        let now = Instant::now();
        let estimated_impact = self.estimate_price_impact(repay_amount);
        if repay_amount < self.min_amount || estimated_impact < self.min_price_impact_bps {
            return;
        }
        self.push_hint(
            &mut swaps,
            HintParams {
                from: debt_token,
                to: collateral_token,
                amount_in: repay_amount,
                estimated_impact,
                source: protocol,
                now,
            },
        )
        .await;
    }

    async fn push_hint(&self, swaps: &mut HashMap<(Address, Address), PendingSwap>, params: HintParams<'_>) {
        swaps.insert(
            (params.from, params.to),
            PendingSwap {
                amount_in: params.amount_in,
                last_seen: params.now,
            },
        );

        let mut hints = self.hints.lock().await;
        hints.push_back(BackrunHint {
            from: params.from,
            to: params.to,
            amount_in: params.amount_in,
            price_impact_bps: params.estimated_impact,
            source: params.source.to_string(),
            observed_at: params.now,
        });
        while hints.len() > MAX_DECODED_HINTS {
            hints.pop_front();
        }
    }

    fn estimate_price_impact(&self, amount_in: U256) -> u32 {
        if amount_in.is_zero() {
            return 0;
        }
        let rough = mul_div(
            amount_in,
            U256::from(10_000u64),
            amount_in.saturating_add(self.min_amount),
        );
        u32::try_from(rough.as_u64()).unwrap_or(u32::MAX)
    }

    async fn prune(&self, ttl: Duration) {
        let mut swaps = self.swaps.lock().await;
        let now = Instant::now();
        swaps.retain(|_, swap| now.duration_since(swap.last_seen) <= ttl);
        let mut hints = self.hints.lock().await;
        hints.retain(|hint| now.duration_since(hint.observed_at) <= ttl);
    }

    pub async fn active_hints(&self, ttl: Duration) -> Vec<BackrunHint> {
        self.prune(ttl).await;
        let now = Instant::now();
        let hints = self.hints.lock().await;
        hints
            .iter()
            .filter(|hint| now.duration_since(hint.observed_at) <= ttl)
            .cloned()
            .collect()
    }

    pub async fn best_amount_for(&self, from: Address, to: Address, ttl: Duration) -> Option<U256> {
        let swaps = self.swaps.lock().await;
        if let Some(swap) = swaps.get(&(from, to)) {
            if Instant::now().duration_since(swap.last_seen) <= ttl {
                return Some(swap.amount_in);
            }
        }
        None
    }

    pub async fn hint_for(&self, from: Address, to: Address, ttl: Duration) -> Option<BackrunHint> {
        let now = Instant::now();
        let mut hints = self.hints.lock().await;
        let mut found_idx = None;
        for (idx, hint) in hints.iter().enumerate() {
            if hint.from == from && hint.to == to && now.duration_since(hint.observed_at) <= ttl {
                found_idx = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_idx {
            if let Some(hint) = hints.remove(idx) {
                return Some(hint);
            }
        }
        None
    }

    async fn ingest_transaction(&self, tx: &Transaction, source: &str) {
        if let Some((amount_in, path)) = decode_swap_transaction(tx) {
            self.record(&path, amount_in, source).await;
        }
    }

    /// Poll pending block (HTTP) — used when WS is down or as a supplement.
    pub async fn poll_pending_block<C>(&self, provider: &Provider<C>)
    where
        C: JsonRpcClient + 'static,
    {
        if let Ok(Some(block)) = provider
            .get_block_with_txs(BlockId::Number(BlockNumber::Pending))
            .await
        {
            for tx in block.transactions {
                self.ingest_transaction(&tx, "pending_block").await;
            }
        }
    }

    #[allow(dead_code)]
    pub async fn run<C>(self: Arc<Self>, provider: Arc<Provider<C>>, interval: Duration)
    where
        C: JsonRpcClient + Clone + Send + Sync + 'static,
    {
        loop {
            self.poll_pending_block(provider.as_ref()).await;
            if let Ok(Some(block)) = provider
                .get_block_with_txs(BlockNumber::Latest)
                .await
            {
                for tx in block.transactions {
                    self.ingest_transaction(&tx, "latest_block").await;
                }
            }
            self.prune(Duration::from_secs(45)).await;
            sleep(interval).await;
        }
    }
}

/// Decode swap intent from a pending/mined transaction calldata.
pub fn decode_swap_transaction(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    decode_univ2_style_router_swap(tx)
        .or_else(|| decode_solidly_route_swap(tx))
        .or_else(|| decode_solidly_simple_swap(tx))
        .or_else(|| decode_univ3_exact_input_single(tx))
        .or_else(|| decode_univ3_swap_router02_exact_input_single(tx))
        .or_else(|| decode_univ3_exact_input(tx))
}

/// Solidly-style `Route` tuple used by Aerodrome / Velodrome routers.
fn solidly_route_param() -> ParamType {
    ParamType::Tuple(vec![
        ParamType::Address,
        ParamType::Address,
        ParamType::Bool,
        ParamType::Address,
    ])
}

fn solidly_route_path(routes: Vec<Token>) -> Option<Vec<Address>> {
    if routes.is_empty() {
        return None;
    }
    let mut path = Vec::with_capacity(routes.len().saturating_add(1));
    for (idx, route) in routes.into_iter().enumerate() {
        let tuple = route.into_tuple()?;
        if tuple.len() < 2 {
            return None;
        }
        let from = tuple[0].clone().into_address()?;
        let to = tuple[1].clone().into_address()?;
        if idx == 0 {
            path.push(from);
        }
        path.push(to);
    }
    if path.len() >= 2 {
        Some(path)
    } else {
        None
    }
}

/// Aerodrome / Velodrome `swapExactTokensForTokens` with `Route[]` calldata.
fn decode_solidly_route_swap(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0[0..4].try_into().ok()?;
    const SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4] = [0xca, 0xc8, 0x8e, 0xa9];
    const SWAP_EXACT_ETH_FOR_TOKENS: [u8; 4] = [0x90, 0x36, 0x38, 0xa4];
    const SWAP_EXACT_TOKENS_FOR_ETH: [u8; 4] = [0xc6, 0xb7, 0xf1, 0xb6];
    if selector != SWAP_EXACT_TOKENS_FOR_TOKENS
        && selector != SWAP_EXACT_ETH_FOR_TOKENS
        && selector != SWAP_EXACT_TOKENS_FOR_ETH
    {
        return None;
    }

    let route_array = ParamType::Array(Box::new(solidly_route_param()));
    let params = if selector == SWAP_EXACT_ETH_FOR_TOKENS {
        vec![
            ParamType::Uint(256),
            route_array,
            ParamType::Address,
            ParamType::Uint(256),
        ]
    } else {
        vec![
            ParamType::Uint(256),
            ParamType::Uint(256),
            route_array,
            ParamType::Address,
            ParamType::Uint(256),
        ]
    };
    let decoded = decode(&params, &tx.input.0[4..]).ok()?;
    let (amount_in, routes_token) = if selector == SWAP_EXACT_ETH_FOR_TOKENS {
        (tx.value, decoded[1].clone())
    } else {
        (
            decoded[0].clone().into_uint()?,
            decoded[2].clone(),
        )
    };
    if amount_in.is_zero() {
        return None;
    }
    let routes = routes_token.into_array()?;
    let path = solidly_route_path(routes)?;
    Some((amount_in, path))
}

/// Aerodrome `swapExactTokensForTokensSimple` — single-pool volatile/stable hop.
fn decode_solidly_simple_swap(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0[0..4].try_into().ok()?;
    const SWAP_EXACT_TOKENS_FOR_TOKENS_SIMPLE: [u8; 4] = [0x13, 0xdc, 0xfc, 0x59];
    if selector != SWAP_EXACT_TOKENS_FOR_TOKENS_SIMPLE {
        return None;
    }
    let params = vec![
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Address,
        ParamType::Address,
        ParamType::Bool,
        ParamType::Address,
        ParamType::Uint(256),
    ];
    let decoded = decode(&params, &tx.input.0[4..]).ok()?;
    let amount_in = decoded[0].clone().into_uint()?;
    let token_from = decoded[2].clone().into_address()?;
    let token_to = decoded[3].clone().into_address()?;
    if amount_in.is_zero() || token_from == token_to {
        return None;
    }
    Some((amount_in, vec![token_from, token_to]))
}

/// Uniswap SwapRouter02 `exactInputSingle` — no deadline field in the params tuple.
fn decode_univ3_swap_router02_exact_input_single(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0[0..4].try_into().ok()?;
    const EXACT_INPUT_SINGLE_V2: [u8; 4] = [0x04, 0xe4, 0x5a, 0xaf];
    if selector != EXACT_INPUT_SINGLE_V2 {
        return None;
    }
    let params = vec![ParamType::Tuple(vec![
        ParamType::Address,
        ParamType::Address,
        ParamType::Uint(24),
        ParamType::Address,
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(160),
    ])];
    let decoded = decode(&params, &tx.input.0[4..]).ok()?;
    let tuple = decoded[0].clone().into_tuple()?;
    let token_in = tuple[0].clone().into_address()?;
    let token_out = tuple[1].clone().into_address()?;
    let amount_in = tuple[4].clone().into_uint()?;
    if amount_in.is_zero() {
        return None;
    }
    Some((amount_in, vec![token_in, token_out]))
}

fn decode_univ2_style_router_swap(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0[0..4].try_into().ok()?;
    const SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4] = [0x38, 0xed, 0x17, 0x39];
    const SWAP_EXACT_TOKENS_FOR_TOKENS_FOT: [u8; 4] = [0x5c, 0x11, 0xd7, 0x95];
    const SWAP_EXACT_ETH_FOR_TOKENS: [u8; 4] = [0x7f, 0xf3, 0x6a, 0xb5];
    const SWAP_EXACT_TOKENS_FOR_ETH: [u8; 4] = [0x18, 0xcb, 0xaf, 0xe5];
    if selector != SWAP_EXACT_TOKENS_FOR_TOKENS
        && selector != SWAP_EXACT_TOKENS_FOR_TOKENS_FOT
        && selector != SWAP_EXACT_ETH_FOR_TOKENS
        && selector != SWAP_EXACT_TOKENS_FOR_ETH
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
    let amount_in = if selector == SWAP_EXACT_ETH_FOR_TOKENS {
        tx.value
    } else {
        decoded[0].clone().into_uint()?
    };
    if amount_in.is_zero() {
        return None;
    }
    let path_tokens = decoded[2].clone().into_array()?;
    tokens_from_abi_array(path_tokens)
        .map(|path| (amount_in, path))
}

fn decode_univ3_exact_input_single(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0[0..4].try_into().ok()?;
    const EXACT_INPUT_SINGLE: [u8; 4] = [0x41, 0x4b, 0xf3, 0x89];
    if selector != EXACT_INPUT_SINGLE {
        return None;
    }
    let params = vec![ParamType::Tuple(vec![
        ParamType::Address,
        ParamType::Address,
        ParamType::Uint(24),
        ParamType::Address,
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(160),
    ])];
    let decoded = decode(&params, &tx.input.0[4..]).ok()?;
    let tuple = decoded[0].clone().into_tuple()?;
    let token_in = tuple[0].clone().into_address()?;
    let token_out = tuple[1].clone().into_address()?;
    let amount_in = tuple[4].clone().into_uint()?;
    if amount_in.is_zero() {
        return None;
    }
    Some((amount_in, vec![token_in, token_out]))
}

fn decode_univ3_exact_input(tx: &Transaction) -> Option<(U256, Vec<Address>)> {
    if tx.input.0.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = tx.input.0[0..4].try_into().ok()?;
    const EXACT_INPUT: [u8; 4] = [0xc0, 0x4b, 0x8d, 0x70];
    if selector != EXACT_INPUT {
        return None;
    }
    let params = vec![
        ParamType::Bytes,
        ParamType::Address,
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
    ];
    let decoded = decode(&params, &tx.input.0[4..]).ok()?;
    let path_bytes = decoded[0].clone().into_bytes()?;
    let amount_in = decoded[2].clone().into_uint()?;
    if amount_in.is_zero() {
        return None;
    }
    let path = decode_univ3_path(&path_bytes)?;
    Some((amount_in, path))
}

fn decode_univ3_path(bytes: &[u8]) -> Option<Vec<Address>> {
    if bytes.len() < 20 {
        return None;
    }
    let mut tokens = Vec::new();
    let mut offset = 0usize;
    while offset + 20 <= bytes.len() {
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&bytes[offset..offset + 20]);
        tokens.push(Address::from(addr));
        offset += 20;
        if offset >= bytes.len() {
            break;
        }
        if offset + 3 > bytes.len() {
            break;
        }
        offset += 3; // fee tier
    }
    if tokens.len() >= 2 {
        Some(tokens)
    } else {
        None
    }
}

fn tokens_from_abi_array(tokens: Vec<Token>) -> Option<Vec<Address>> {
    if tokens.len() < 2 {
        return None;
    }
    let mut path = Vec::with_capacity(tokens.len());
    for token in tokens {
        path.push(token.into_address()?);
    }
    Some(path)
}

/// Subscribe to `newPendingTransactions`, fetch each tx, decode swaps, feed backrun hints.
pub async fn spawn_live_mempool_monitor<C>(
    http_provider: Arc<Provider<C>>,
    initial_ws_provider: Option<Arc<Provider<Ws>>>,
    ws_endpoints: Vec<String>,
    ws_backoff: Duration,
    backrun: Option<Arc<BackrunMonitor>>,
    metrics: Option<Arc<Metrics>>,
) where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let mut ws_provider = initial_ws_provider;

    loop {
        if let Some(monitor) = backrun.as_ref() {
            monitor.poll_pending_block(http_provider.as_ref()).await;
        } else if let Err(err) = poll_pending_block(&http_provider, &metrics).await {
            warn!(error = %err, "pending block RPC poll failed");
        }

        if ws_provider.is_none() && ws_endpoints.is_empty() {
            sleep(FALLBACK_POLL_INTERVAL).await;
            continue;
        }

        let provider = if let Some(provider) = ws_provider.take() {
            provider
        } else {
            let connect =
                connect_ws_provider_with_fallbacks("mempool-ws", &ws_endpoints, ws_backoff);
            match timeout(WS_CONNECT_TIMEOUT, connect).await {
                Ok(Ok(provider)) => Arc::new(provider),
                Ok(Err(err)) => {
                    warn!(error = %err, "mempool websocket connect failed");
                    sleep(FALLBACK_POLL_INTERVAL).await;
                    continue;
                }
                Err(_) => {
                    warn!("mempool websocket connect timed out");
                    sleep(FALLBACK_POLL_INTERVAL).await;
                    continue;
                }
            }
        };

        match provider.subscribe_pending_txs().await {
            Ok(sub) => {
                info!("live mempool monitor connected (pending tx subscription)");
                let mut sub = sub.transactions_unordered(8);
                while let Some(result) = sub.next().await {
                    if let Some(metrics) = &metrics {
                        metrics.mempool_txs_observed.inc();
                    }
                    if let Some(monitor) = backrun.as_ref() {
                        match result {
                            Ok(tx) => {
                                monitor.ingest_transaction(&tx, "mempool_ws").await;
                            }
                            Err(err) => {
                                debug!(error = %err, "mempool tx fetch failed");
                            }
                        }
                    }
                }
                warn!("mempool pending subscription ended; reconnecting");
            }
            Err(err) => {
                warn!(error = %err, "mempool pending subscription failed");
            }
        }
        sleep(FALLBACK_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Bytes;

    #[test]
    fn decodes_aerodrome_simple_swap() {
        let mut input = vec![0x13, 0xdc, 0xfc, 0x59];
        let amount_in = U256::from(5_000_000u64);
        let token_a = Address::from_low_u64_be(10);
        let token_b = Address::from_low_u64_be(11);
        let encoded = ethers::abi::encode(&[
            Token::Uint(amount_in),
            Token::Uint(U256::from(1u64)),
            Token::Address(token_a),
            Token::Address(token_b),
            Token::Bool(false),
            Token::Address(Address::from_low_u64_be(3)),
            Token::Uint(U256::from(999_999_999u64)),
        ]);
        input.extend_from_slice(&encoded);
        let tx = Transaction {
            input: Bytes::from(input),
            ..Default::default()
        };
        let (amt, path) = decode_swap_transaction(&tx).expect("decode aerodrome simple");
        assert_eq!(amt, amount_in);
        assert_eq!(path, vec![token_a, token_b]);
    }

    #[test]
    fn decodes_aerodrome_route_swap() {
        let mut input = vec![0xca, 0xc8, 0x8e, 0xa9];
        let amount_in = U256::from(2_000_000u64);
        let token_a = Address::from_low_u64_be(20);
        let token_b = Address::from_low_u64_be(21);
        let token_c = Address::from_low_u64_be(22);
        let factory = Address::from_low_u64_be(99);
        let route = |from: Address, to: Address| {
            Token::Tuple(vec![
                Token::Address(from),
                Token::Address(to),
                Token::Bool(false),
                Token::Address(factory),
            ])
        };
        let encoded = ethers::abi::encode(&[
            Token::Uint(amount_in),
            Token::Uint(U256::from(1u64)),
            Token::Array(vec![route(token_a, token_b), route(token_b, token_c)]),
            Token::Address(Address::from_low_u64_be(3)),
            Token::Uint(U256::from(999_999_999u64)),
        ]);
        input.extend_from_slice(&encoded);
        let tx = Transaction {
            input: Bytes::from(input),
            ..Default::default()
        };
        let (amt, path) = decode_swap_transaction(&tx).expect("decode aerodrome route");
        assert_eq!(amt, amount_in);
        assert_eq!(path, vec![token_a, token_b, token_c]);
    }

    #[test]
    fn decodes_swap_router02_exact_input_single() {
        let mut input = vec![0x04, 0xe4, 0x5a, 0xaf];
        let amount_in = U256::from(3_000_000u64);
        let token_a = Address::from_low_u64_be(30);
        let token_b = Address::from_low_u64_be(31);
        let encoded = ethers::abi::encode(&[Token::Tuple(vec![
            Token::Address(token_a),
            Token::Address(token_b),
            Token::Uint(U256::from(500u64)),
            Token::Address(Address::from_low_u64_be(3)),
            Token::Uint(amount_in),
            Token::Uint(U256::from(1u64)),
            Token::Uint(U256::zero()),
        ])]);
        input.extend_from_slice(&encoded);
        let tx = Transaction {
            input: Bytes::from(input),
            ..Default::default()
        };
        let (amt, path) = decode_swap_transaction(&tx).expect("decode swaprouter02");
        assert_eq!(amt, amount_in);
        assert_eq!(path, vec![token_a, token_b]);
    }

    #[test]
    fn decodes_univ2_router_swap() {
        let mut input = vec![0x38, 0xed, 0x17, 0x39];
        let amount_in = U256::from(1_000_000u64);
        let token_a = Address::from_low_u64_be(1);
        let token_b = Address::from_low_u64_be(2);
        let encoded = ethers::abi::encode(&[
            Token::Uint(amount_in),
            Token::Uint(U256::from(1u64)),
            Token::Array(vec![Token::Address(token_a), Token::Address(token_b)]),
            Token::Address(Address::from_low_u64_be(3)),
            Token::Uint(U256::from(999_999_999u64)),
        ]);
        input.extend_from_slice(&encoded);
        let tx = Transaction {
            input: Bytes::from(input),
            ..Default::default()
        };
        let (amt, path) = decode_swap_transaction(&tx).expect("decode");
        assert_eq!(amt, amount_in);
        assert_eq!(path, vec![token_a, token_b]);
    }
}
