use crate::math::mul_div;
use anyhow::{anyhow, Result};
use ethers::{
    prelude::*,
    providers::{Http, JsonRpcClient, Ws},
    types::Address,
    types::U256,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, MathematicalOps};
use serde::Serialize;
use std::{
    convert::TryFrom,
    fs::{create_dir_all, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

abigen!(
    IERC20,
    r#"[
        function decimals() view returns (uint8)
        function balanceOf(address) view returns (uint256)
        function approve(address,uint256) returns (bool)
    ]"#,
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TradeSizing {
    pub base_amount: U256,
    pub slippage_tolerance_bps: u32,
}

impl TradeSizing {
    pub fn new(base_amount: U256, slippage_tolerance_bps: u32) -> Self {
        Self {
            base_amount,
            slippage_tolerance_bps,
        }
    }
}

#[allow(dead_code)]
pub async fn erc20_decimals<C>(provider: Arc<Provider<C>>, token: Address) -> Result<u8>
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let contract = IERC20::new(token, provider);
    let decimals = contract.decimals().call().await?;
    Ok(decimals)
}

/// Read an env var and parse it as `T`, returning `None` if unset or unparseable.
///
/// Exactly the `std::env::var(name).ok().and_then(|v| v.parse::<T>().ok())` idiom
/// that was hand-written across the startup config path. Callers apply their own
/// default (`.unwrap_or(..)` / `.unwrap_or_else(..)`) and any clamping, so the
/// per-call semantics (including "unparseable falls back to default") are
/// preserved. Does not trim — a value with surrounding whitespace falls back,
/// matching the original inline reads.
pub fn env_parse_opt<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|raw| raw.parse::<T>().ok())
}

/// Read an env var as a decimal `U256`, returning `None` if unset or unparseable.
/// Mirrors the `env::var(name).ok().and_then(|v| U256::from_dec_str(&v).ok())`
/// idiom (decimal, not hex).
pub fn env_u256_opt(name: &str) -> Option<U256> {
    std::env::var(name)
        .ok()
        .and_then(|raw| U256::from_dec_str(&raw).ok())
}

/// Boolean flag: `true` for `1`/`true`/`yes` (case-insensitive), `false` for any
/// other set value, and `default` when unset. Same semantics as the
/// `matches!(raw.to_ascii_lowercase().as_str(), "1"|"true"|"yes")` idiom.
pub fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
}

/// Parse a 4-byte function selector from a hex string (with or without `0x`).
/// Accepts inputs of at least 4 bytes and takes the leading 4. Shared by the
/// bridge and liquidation config parsers.
pub fn parse_selector(raw: &str) -> Result<[u8; 4]> {
    use anyhow::Context;
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(trimmed)
        .with_context(|| format!("selector `{raw}` is not valid hexadecimal"))?;
    anyhow::ensure!(
        bytes.len() >= 4,
        "selector `{raw}` must decode to at least 4 bytes (8 hex characters)"
    );
    let mut selector = [0u8; 4];
    selector.copy_from_slice(&bytes[..4]);
    Ok(selector)
}

/// Expand `${VAR}` references in a config string from the process environment.
/// Unset or empty-named references are left verbatim (`${VAR}` / `${}`) so a
/// missing variable surfaces downstream instead of silently becoming empty.
/// Shared by the registry and ops-inputs config loaders so their substitution
/// behaviour stays identical.
pub fn expand_env_vars(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && matches!(chars.peek(), Some('{')) {
            chars.next();
            let mut key = String::new();
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
                key.push(next);
            }

            if key.is_empty() {
                result.push_str("${}");
                continue;
            }

            match std::env::var(&key) {
                Ok(value) => result.push_str(&value),
                Err(_) => {
                    result.push_str("${");
                    result.push_str(&key);
                    result.push('}');
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

pub fn u256_to_f64(x: U256) -> f64 {
    u256_to_decimal(x)
        .to_f64()
        .unwrap_or_else(|| if x.is_zero() { 0.0 } else { f64::INFINITY })
}

pub const WEIGHT_SCALE: i64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativePrice {
    pub token_amount: U256,
    pub native_amount: U256,
    pub reliable: bool,
}

impl NativePrice {
    pub fn new(token_amount: U256, native_amount: U256, reliable: bool) -> Self {
        Self {
            token_amount,
            native_amount,
            reliable,
        }
    }

    pub fn unit() -> Self {
        let amount = U256::exp10(18);
        Self::new(amount, amount, false)
    }

    pub fn is_reliable(&self) -> bool {
        self.reliable && !self.token_amount.is_zero() && !self.native_amount.is_zero()
    }

    pub fn tokens_for_native(&self, native_cost: U256) -> U256 {
        if self.native_amount.is_zero() {
            return native_cost;
        }

        mul_div(
            native_cost,
            self.token_amount.max(U256::one()),
            self.native_amount,
        )
    }

    pub fn tokens_for_native_if_reliable(&self, native_cost: U256) -> Option<U256> {
        if !self.is_reliable() {
            return None;
        }

        if self.native_amount.is_zero() {
            return None;
        }

        Some(self.tokens_for_native(native_cost))
    }

    pub fn tokens_for_native_strict(&self, native_cost: U256) -> Option<U256> {
        if !self.is_reliable() || self.native_amount.is_zero() {
            return None;
        }
        Some(mul_div(
            native_cost,
            self.token_amount.max(U256::one()),
            self.native_amount,
        ))
    }

    /// Convert a start-token amount into native (wei). Inverse of
    /// `tokens_for_native_strict`; used to denominate token-denominated profit
    /// in native so it can be priced against per-gas priority fees. Returns
    /// `None` when the price is unreliable (fail closed — never bid on a guess).
    pub fn native_for_tokens_strict(&self, token_amount: U256) -> Option<U256> {
        if !self.is_reliable() || self.token_amount.is_zero() {
            return None;
        }
        Some(mul_div(
            token_amount,
            self.native_amount.max(U256::one()),
            self.token_amount,
        ))
    }

    #[allow(dead_code)]
    pub fn native_per_token(&self) -> Decimal {
        if self.token_amount.is_zero() {
            return Decimal::ONE;
        }

        decimal_ratio(self.native_amount, self.token_amount).unwrap_or(Decimal::ONE)
    }
}

pub fn compute_edge_weight(
    rate_num: U256,
    rate_den: U256,
    estimated_gas: u64,
    gas_price: U256,
    base_amount_in: U256,
    native_price: NativePrice,
) -> i64 {
    let base = if base_amount_in.is_zero() {
        U256::one()
    } else {
        base_amount_in
    };

    let rate = decimal_ratio(rate_num, rate_den).unwrap_or(Decimal::MAX);
    let rate_ln = if rate.is_zero() || rate.is_sign_negative() {
        Decimal::MAX
    } else {
        rate.ln()
    };

    let gas_cost_native = U256::from(estimated_gas).saturating_mul(gas_price);
    let base_dec = u256_to_decimal(base);
    let Some(gas_cost_tokens) = native_price.tokens_for_native_if_reliable(gas_cost_native) else {
        // Unreliable native price: exclude edge from BF (prohibitive weight).
        return i64::MAX;
    };
    let gas_ratio = u256_to_decimal(gas_cost_tokens)
        .checked_div(base_dec)
        .unwrap_or(Decimal::MAX);

    let composite = gas_ratio.checked_sub(rate_ln).unwrap_or_else(|| {
        if gas_ratio > rate_ln {
            Decimal::MAX
        } else {
            Decimal::MIN
        }
    });
    let scaled = composite
        .checked_mul(Decimal::from_i128_with_scale(WEIGHT_SCALE as i128, 0))
        .unwrap_or_else(|| {
            if composite.is_sign_negative() {
                Decimal::MIN
            } else {
                Decimal::MAX
            }
        })
        .round();

    scaled
        .to_i128()
        .map(|v| v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
        .unwrap_or_else(|| {
            if scaled.is_sign_negative() {
                i64::MIN
            } else {
                i64::MAX
            }
        })
}

pub fn u256_to_decimal(value: U256) -> Decimal {
    Decimal::from_str_exact(&value.to_string()).unwrap_or(Decimal::MAX)
}

pub fn decimal_ratio(num: U256, den: U256) -> Option<Decimal> {
    if den.is_zero() {
        return None;
    }
    let numerator = u256_to_decimal(num);
    let denominator = u256_to_decimal(den);
    numerator.checked_div(denominator)
}

pub fn encode_univ3_path(path: &[(Address, Option<u32>)]) -> Result<Vec<u8>> {
    let hops = path.len();
    if hops == 0 {
        return Ok(Vec::new());
    }

    let mut capacity = hops.saturating_mul(20);
    if hops > 1 {
        capacity += (hops - 1) * 3;
    }

    let mut bytes = Vec::with_capacity(capacity);
    for (i, (token, fee)) in path.iter().enumerate() {
        if i > 0 {
            let Some(fee) = fee else {
                warn!("Skipping UniV3 path encoding due to missing fee");
                return Err(anyhow!("missing fee for hop {i}"));
            };
            bytes.extend_from_slice(&fee.to_be_bytes()[1..4]);
        }
        bytes.extend_from_slice(token.as_bytes());
    }
    Ok(bytes)
}

pub fn apply_slippage(amount: U256, slippage_bps: u32) -> U256 {
    if amount.is_zero() || slippage_bps == 0 {
        return amount;
    }
    if slippage_bps >= 10_000 {
        return U256::zero();
    }

    let numerator = U256::from(10_000u64 - slippage_bps as u64);
    let denominator = U256::from(10_000u64);
    mul_div(amount, numerator, denominator)
}

#[derive(Clone, Debug, Serialize)]
pub struct CandidateDecisionRecord {
    pub timestamp_ms: u128,
    pub stage: String,
    pub chain: String,
    pub candidate_id: Option<String>,
    pub cycle_start_token: Option<String>,
    pub hops: Option<usize>,
    pub edges_scanned: usize,
    pub venue_path: Option<Vec<String>>,
    pub gross_after_fee: Option<String>,
    pub gas_cost_native: Option<String>,
    pub gas_cost_start_token: Option<String>,
    pub pricing_reliable: bool,
    pub min_profit_threshold: Option<String>,
    pub rejection_reason: Option<String>,
    pub simulation_status: Option<String>,
    pub bridge: bool,
    pub liquidation: bool,
    pub block_number: Option<u64>,
    pub path_tokens: Option<Vec<String>>,
    pub fee_tiers: Option<Vec<u32>>,
    pub quote_block_number: Option<u64>,
    pub quote_block_lag: Option<u64>,
    pub gas_estimate_wei: Option<String>,
    pub profit_after_gas_wei: Option<String>,
    pub pricing_source: Option<String>,
    pub error: Option<String>,
}

impl CandidateDecisionRecord {
    pub fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    }
}

pub struct CandidateDecisionLogger {
    path: PathBuf,
    write_guard: Arc<Mutex<()>>,
    sender: mpsc::SyncSender<CandidateDecisionRecord>,
}

impl CandidateDecisionLogger {
    pub fn from_env() -> Self {
        let path = std::env::var("CANDIDATE_LOG_PATH")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("logs/candidates.jsonl"));
        let buffer = std::env::var("CANDIDATE_LOG_BUFFER")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2048)
            .max(128);
        Self::new(path, buffer)
    }

    pub fn new(path: PathBuf, buffer: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<CandidateDecisionRecord>(buffer);
        let write_guard = Arc::new(Mutex::new(()));
        let worker_path = path.clone();
        let worker_guard = Arc::clone(&write_guard);
        thread::Builder::new()
            .name("candidate-log-writer".to_string())
            .spawn(move || {
                while let Ok(record) = receiver.recv() {
                    if let Err(err) = Self::write_direct(&worker_path, &worker_guard, &record) {
                        warn!(
                            error = %err,
                            path = %worker_path.display(),
                            "candidate decision background write failed"
                        );
                    }
                }
            })
            .ok();
        Self {
            path,
            write_guard,
            sender,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_direct(
        path: &Path,
        write_guard: &Arc<Mutex<()>>,
        record: &CandidateDecisionRecord,
    ) -> Result<()> {
        let _guard = write_guard
            .lock()
            .map_err(|_| anyhow!("candidate decision logger mutex poisoned"))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string(record)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{json}")?;
        Ok(())
    }

    pub fn write(&self, record: CandidateDecisionRecord) -> Result<()> {
        match self.sender.try_send(record.clone()) {
            Ok(_) => Ok(()),
            Err(mpsc::TrySendError::Full(rec)) | Err(mpsc::TrySendError::Disconnected(rec)) => {
                Self::write_direct(&self.path, &self.write_guard, &rec)
            }
        }
    }
}

/// Expands `${VAR}` placeholders using the process environment (used for RPC URLs in .env).
pub fn expand_env_placeholders(raw: &str) -> String {
    let mut result = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            while let Some(next) = chars.next() {
                if next == '}' {
                    break;
                }
                var.push(next);
            }
            if var.is_empty() {
                result.push_str("${}");
                continue;
            }
            match std::env::var(&var) {
                Ok(value) => result.push_str(&value),
                Err(_) => {
                    result.push_str("${");
                    result.push_str(&var);
                    result.push('}');
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

pub fn parse_endpoint_list(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| expand_env_placeholders(s))
        .collect()
}

pub fn coerce_http_url(endpoint: &str) -> Option<String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        Some(endpoint.to_string())
    } else {
        None
    }
}

pub fn coerce_ws_url(endpoint: &str) -> Option<String> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        Some(endpoint.to_string())
    } else if endpoint.starts_with("http://") {
        Some(endpoint.replacen("http://", "ws://", 1))
    } else if endpoint.starts_with("https://") {
        Some(endpoint.replacen("https://", "wss://", 1))
    } else {
        None
    }
}

#[allow(dead_code)]
pub async fn connect_http_provider_with_fallbacks(
    label: &str,
    endpoints: &[String],
    max_backoff: Duration,
) -> Result<Provider<Http>> {
    if endpoints.is_empty() {
        return Err(anyhow!("no http endpoints configured for {label}"));
    }

    let mut attempt: u32 = 0;
    loop {
        for endpoint in endpoints {
            info!(target: "rpc", %label, endpoint = %endpoint, "connecting http endpoint");
            match Provider::<Http>::try_from(endpoint.as_str()) {
                Ok(provider) => {
                    info!(target: "rpc", %label, endpoint = %endpoint, "http endpoint connected");
                    return Ok(provider);
                }
                Err(err) => {
                    warn!(
                        target: "rpc",
                        %label,
                        endpoint = %endpoint,
                        error = ?err,
                        "http endpoint connection failed"
                    );
                }
            }
        }

        attempt = attempt.saturating_add(1);
        let capped = attempt.min(5);
        let backoff_secs = 1u64 << capped;
        let delay = Duration::from_secs(backoff_secs).min(max_backoff);
        error!(
            target: "rpc",
            %label,
            ?delay,
            "all http endpoints failed, backing off before retry"
        );
        sleep(delay).await;
    }
}

pub async fn connect_ws_provider_with_fallbacks(
    label: &str,
    endpoints: &[String],
    max_backoff: Duration,
) -> Result<Provider<Ws>> {
    if endpoints.is_empty() {
        return Err(anyhow!("no websocket endpoints configured for {label}"));
    }

    let mut attempt: u32 = 0;
    loop {
        for endpoint in endpoints {
            info!(target: "rpc", %label, endpoint = %endpoint, "connecting websocket endpoint");
            match Provider::<Ws>::connect(endpoint).await {
                Ok(provider) => {
                    info!(target: "rpc", %label, endpoint = %endpoint, "websocket endpoint connected");
                    return Ok(provider);
                }
                Err(err) => {
                    warn!(
                        target: "rpc",
                        %label,
                        endpoint = %endpoint,
                        error = ?err,
                        "websocket endpoint connection failed"
                    );
                }
            }
        }

        attempt = attempt.saturating_add(1);
        let capped = attempt.min(5);
        let backoff_secs = 1u64 << capped;
        let delay = Duration::from_secs(backoff_secs).min(max_backoff);
        error!(
            target: "rpc",
            %label,
            ?delay,
            "all websocket endpoints failed, backing off before retry"
        );
        sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    #[test]
    fn env_helpers_match_inline_idioms() {
        // Unique names so these don't race other tests over shared process env.
        let pk = "ARBOT_TEST_ENV_PARSE_9f3a";
        std::env::remove_var(pk);
        assert_eq!(env_parse_opt::<u32>(pk), None);
        std::env::set_var(pk, "42");
        assert_eq!(env_parse_opt::<u32>(pk), Some(42));
        std::env::set_var(pk, "notanumber");
        assert_eq!(env_parse_opt::<u32>(pk), None);
        std::env::set_var(pk, " 42 "); // no trimming: whitespace -> None (falls back)
        assert_eq!(env_parse_opt::<u32>(pk), None);
        std::env::remove_var(pk);

        let uk = "ARBOT_TEST_ENV_U256_9f3a";
        std::env::remove_var(uk);
        assert_eq!(env_u256_opt(uk), None);
        std::env::set_var(uk, "1000000000000000000");
        assert_eq!(env_u256_opt(uk), Some(U256::from(1_000_000_000_000_000_000u64)));
        std::env::set_var(uk, "0x10"); // from_dec_str rejects hex
        assert_eq!(env_u256_opt(uk), None);
        std::env::remove_var(uk);

        let fk = "ARBOT_TEST_ENV_FLAG_9f3a";
        std::env::remove_var(fk);
        assert!(env_flag(fk, true));
        assert!(!env_flag(fk, false));
        for truthy in ["1", "true", "YES", "Yes"] {
            std::env::set_var(fk, truthy);
            assert!(env_flag(fk, false), "{truthy} should be truthy");
        }
        for falsy in ["0", "false", "no", "banana"] {
            std::env::set_var(fk, falsy);
            assert!(!env_flag(fk, true), "{falsy} should be falsy");
        }
        std::env::remove_var(fk);
    }

    #[test]
    fn native_for_tokens_strict_is_inverse_of_tokens_for_native() {
        // 1 native = 2000 tokens (e.g. 1 ETH = 2000 USDC).
        let price = NativePrice::new(U256::from(2_000u64), U256::from(1u64), true);
        // 4000 tokens of profit -> 2 native.
        assert_eq!(
            price.native_for_tokens_strict(U256::from(4_000u64)),
            Some(U256::from(2u64))
        );
        // Round-trips back to tokens.
        let native = price.native_for_tokens_strict(U256::from(4_000u64)).unwrap();
        assert_eq!(price.tokens_for_native_strict(native), Some(U256::from(4_000u64)));
    }

    #[test]
    fn native_for_tokens_strict_fails_closed_when_unreliable() {
        let price = NativePrice::new(U256::from(2_000u64), U256::from(1u64), false);
        assert!(price.native_for_tokens_strict(U256::from(4_000u64)).is_none());
    }

    #[test]
    fn encodes_univ3_path_with_expected_layout() {
        let path = vec![(addr(1), None), (addr(2), Some(500))];
        let encoded = encode_univ3_path(&path).expect("path should encode");

        assert_eq!(encoded.len(), 43);
        assert_eq!(&encoded[..20], addr(1).as_bytes());
        assert_eq!(&encoded[20..23], &500u32.to_be_bytes()[1..4]);
        assert_eq!(&encoded[23..], addr(2).as_bytes());
    }

    #[test]
    fn rejects_univ3_path_without_fee_on_second_hop() {
        let path = vec![(addr(1), Some(500)), (addr(2), None)];
        let err = encode_univ3_path(&path).expect_err("path should be rejected");
        assert!(err.to_string().contains("missing fee for hop 1"));
    }

    #[test]
    fn coerce_http_url_converts_known_schemes() {
        assert_eq!(
            coerce_http_url("https://alchemy.io/v2/key"),
            Some("https://alchemy.io/v2/key".into())
        );
        assert_eq!(
            coerce_http_url("http://alchemy.io/v2/key"),
            Some("http://alchemy.io/v2/key".into())
        );
        assert_eq!(coerce_http_url("wss://alchemy.io/v2/key"), None);
        assert_eq!(coerce_http_url("ws://alchemy.io/v2/key"), None);
        assert_eq!(coerce_http_url("mev://example"), None);
    }

    #[test]
    fn coerce_ws_url_converts_known_schemes() {
        assert_eq!(
            coerce_ws_url("wss://alchemy.io/v2/key"),
            Some("wss://alchemy.io/v2/key".into())
        );
        assert_eq!(
            coerce_ws_url("ws://alchemy.io/v2/key"),
            Some("ws://alchemy.io/v2/key".into())
        );
        assert_eq!(
            coerce_ws_url("https://alchemy.io/v2/key"),
            Some("wss://alchemy.io/v2/key".into())
        );
        assert_eq!(
            coerce_ws_url("http://alchemy.io/v2/key"),
            Some("ws://alchemy.io/v2/key".into())
        );
        assert_eq!(coerce_ws_url("mev://example"), None);
    }

    #[test]
    fn apply_slippage_reduces_amount() {
        let amount = U256::from(1_000_000u64);
        let adjusted = apply_slippage(amount, 250);
        assert_eq!(adjusted, U256::from(975_000u64));
    }

    #[test]
    fn apply_slippage_caps_full_drawdown() {
        let amount = U256::from(42u64);
        assert_eq!(apply_slippage(amount, 10_000), U256::zero());
        assert_eq!(apply_slippage(amount, 0), amount);
    }

    #[test]
    fn compute_edge_weight_accounts_for_gas_costs() {
        let rate_num = U256::from(105u64);
        let rate_den = U256::from(100u64);
        let estimated_gas = 30_000u64;
        let base_amount_in = U256::from(1_000_000u64);
        let reliable_price = NativePrice::new(U256::exp10(18), U256::exp10(18), true);

        let cheap_gas = compute_edge_weight(
            rate_num,
            rate_den,
            estimated_gas,
            U256::from(1u64),
            base_amount_in,
            reliable_price,
        );
        let expensive_gas = compute_edge_weight(
            rate_num,
            rate_den,
            estimated_gas,
            U256::from(200u64),
            base_amount_in,
            reliable_price,
        );

        assert!(
            cheap_gas < 0,
            "profitable trade with low gas should be negative weight"
        );
        assert!(
            expensive_gas > 0,
            "high gas should flip edge weight positive"
        );
    }

    #[test]
    fn compute_edge_weight_prohibitive_when_price_unreliable() {
        let rate_num = U256::from(105u64);
        let rate_den = U256::from(100u64);
        let weight = compute_edge_weight(
            rate_num,
            rate_den,
            30_000,
            U256::from(200u64),
            U256::from(1_000_000u64),
            NativePrice::unit(),
        );
        assert_eq!(weight, i64::MAX);
    }

    #[test]
    fn u256_to_f64_scales_large_values() {
        let value = U256::from_dec_str("340282366920938463463374607431768211455").unwrap();
        let as_f64 = u256_to_f64(value);
        assert!(as_f64.is_finite());
        assert!(as_f64 > 0.0);
    }

    fn temp_candidate_log_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "arbot_candidate_log_{}_{}_{}.jsonl",
            name,
            std::process::id(),
            CandidateDecisionRecord::now_ms()
        ));
        path
    }

    #[test]
    fn rejected_candidate_writes_jsonl_record() {
        let path = temp_candidate_log_path("rejected");
        let logger = CandidateDecisionLogger::new(path.clone(), 64);
        let record = CandidateDecisionRecord {
            timestamp_ms: CandidateDecisionRecord::now_ms(),
            stage: "candidate_rejected_pre_sim".to_string(),
            chain: "test".to_string(),
            candidate_id: Some("cand-1".to_string()),
            cycle_start_token: Some("0x01".to_string()),
            hops: Some(3),
            edges_scanned: 16,
            venue_path: Some(vec!["univ3".to_string()]),
            gross_after_fee: Some("1000".to_string()),
            gas_cost_native: Some("250".to_string()),
            gas_cost_start_token: None,
            pricing_reliable: false,
            min_profit_threshold: Some("900".to_string()),
            rejection_reason: Some("unreliable_native_price_for_start_token".to_string()),
            simulation_status: None,
            bridge: false,
            liquidation: false,
            block_number: Some(100),
            path_tokens: Some(vec!["0x01".into(), "0x02".into()]),
            fee_tiers: Some(vec![500]),
            quote_block_number: Some(98),
            quote_block_lag: Some(2),
            gas_estimate_wei: Some("210000".into()),
            profit_after_gas_wei: Some("750".into()),
            pricing_source: Some("unreliable".into()),
            error: Some("forced".into()),
        };
        logger.write(record).expect("write candidate record");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let contents = fs::read_to_string(path).expect("read jsonl");
        assert!(contents.contains("candidate_rejected_pre_sim"));
        assert!(contents.contains("unreliable_native_price_for_start_token"));
    }

    #[test]
    fn pre_dispatch_candidate_logging_works_without_dispatch() {
        let path = temp_candidate_log_path("predispatch");
        let logger = CandidateDecisionLogger::new(path.clone(), 64);
        let selected = CandidateDecisionRecord {
            timestamp_ms: CandidateDecisionRecord::now_ms(),
            stage: "candidate_selected".to_string(),
            chain: "test".to_string(),
            candidate_id: Some("cand-2".to_string()),
            cycle_start_token: Some("0x02".to_string()),
            hops: Some(2),
            edges_scanned: 8,
            venue_path: Some(vec!["univ2".to_string(), "univ3".to_string()]),
            gross_after_fee: Some("2000".to_string()),
            gas_cost_native: Some("300".to_string()),
            gas_cost_start_token: Some("300".to_string()),
            pricing_reliable: true,
            min_profit_threshold: Some("1200".to_string()),
            rejection_reason: None,
            simulation_status: None,
            bridge: false,
            liquidation: false,
            block_number: Some(100),
            path_tokens: Some(vec!["0x01".into(), "0x02".into()]),
            fee_tiers: Some(vec![500]),
            quote_block_number: Some(98),
            quote_block_lag: Some(2),
            gas_estimate_wei: Some("210000".into()),
            profit_after_gas_wei: Some("1700".into()),
            pricing_source: Some("native".into()),
            error: None,
        };
        logger.write(selected).expect("write selected");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let contents = fs::read_to_string(path).expect("read jsonl");
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("candidate_selected"));
    }

    #[test]
    fn candidate_log_schema_contains_required_fields() {
        let path = temp_candidate_log_path("schema");
        let logger = CandidateDecisionLogger::new(path.clone(), 64);
        let record = CandidateDecisionRecord {
            timestamp_ms: CandidateDecisionRecord::now_ms(),
            stage: "candidate_dispatch_eligible".to_string(),
            chain: "test".to_string(),
            candidate_id: Some("cand-3".to_string()),
            cycle_start_token: Some("0x03".to_string()),
            hops: Some(4),
            edges_scanned: 21,
            venue_path: Some(vec!["curve".to_string()]),
            gross_after_fee: Some("3000".to_string()),
            gas_cost_native: Some("400".to_string()),
            gas_cost_start_token: Some("1200".to_string()),
            pricing_reliable: true,
            min_profit_threshold: Some("1400".to_string()),
            rejection_reason: None,
            simulation_status: Some("ok".to_string()),
            bridge: true,
            liquidation: true,
            block_number: Some(100),
            path_tokens: Some(vec!["0x01".into(), "0x02".into()]),
            fee_tiers: Some(vec![500]),
            quote_block_number: Some(98),
            quote_block_lag: Some(2),
            gas_estimate_wei: Some("210000".into()),
            profit_after_gas_wei: Some("2600".into()),
            pricing_source: Some("native".into()),
            error: None,
        };
        logger.write(record).expect("write schema record");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let raw = fs::read_to_string(path).expect("read schema");
        let line = raw.lines().next().expect("one line");
        let parsed: Value = serde_json::from_str(line).expect("valid json");
        for key in [
            "timestamp_ms",
            "stage",
            "chain",
            "candidate_id",
            "cycle_start_token",
            "hops",
            "edges_scanned",
            "venue_path",
            "gross_after_fee",
            "gas_cost_native",
            "gas_cost_start_token",
            "pricing_reliable",
            "min_profit_threshold",
            "rejection_reason",
            "simulation_status",
            "bridge",
            "liquidation",
            "block_number",
            "path_tokens",
            "fee_tiers",
            "quote_block_number",
            "quote_block_lag",
            "gas_estimate_wei",
            "profit_after_gas_wei",
            "pricing_source",
            "error",
        ] {
            assert!(parsed.get(key).is_some(), "missing key {key}");
        }
    }
}
