use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Days, NaiveDate, NaiveTime, Utc};
use csv::{ReaderBuilder, WriterBuilder};
use ethers::types::{Address, U256};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::math::mul_div;

#[derive(Clone, Debug)]
pub struct AccountingConfig {
    pub trade_log_path: PathBuf,
    pub daily_summary_path: PathBuf,
    pub event_log_path: PathBuf,
    pub tax_wallet: Option<Address>,
    pub tax_reserve_bps: u32,
    pub stablecoin_symbol: String,
    pub exchange_endpoint: Option<String>,
    pub exchange_api_key: Option<String>,
    pub alert_webhook_url: Option<String>,
    pub daily_net_profit_target_wei: Option<U256>,
    pub revert_rate_threshold: Option<f64>,
    pub rpc_error_rate_threshold: Option<f64>,
    pub enabled: bool,
}

impl AccountingConfig {
    pub fn from_env() -> Result<Self> {
        let base_dir = std::env::var("ACCOUNTING_DIR").unwrap_or_else(|_| "accounting".into());
        let trade_log_path = std::env::var("ACCOUNTING_TRADE_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&base_dir).join("trades.csv"));
        let daily_summary_path = std::env::var("ACCOUNTING_DAILY_SUMMARY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&base_dir).join("daily_totals.csv"));
        let event_log_path = std::env::var("ACCOUNTING_EVENT_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&base_dir).join("events.csv"));

        let tax_reserve_bps = std::env::var("TAX_RESERVE_BPS")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(0);
        let tax_wallet = std::env::var("TAX_WALLET_ADDRESS")
            .ok()
            .and_then(|raw| raw.parse::<Address>().ok());
        let stablecoin_symbol =
            std::env::var("TAX_STABLECOIN_SYMBOL").unwrap_or_else(|_| "USDC".into());
        let exchange_endpoint = std::env::var("TAX_EXCHANGE_API_URL").ok();
        let exchange_api_key = std::env::var("TAX_EXCHANGE_API_KEY").ok();
        let alert_webhook_url = std::env::var("ALERT_WEBHOOK_URL").ok();
        let daily_net_profit_target_wei = std::env::var("ALERT_DAILY_NET_TARGET_WEI")
            .ok()
            .and_then(|raw| U256::from_dec_str(&raw).ok());
        let revert_rate_threshold = std::env::var("ALERT_REVERT_RATE_THRESHOLD")
            .ok()
            .and_then(|raw| raw.parse::<f64>().ok());
        let rpc_error_rate_threshold = std::env::var("ALERT_RPC_ERROR_RATE_THRESHOLD")
            .ok()
            .and_then(|raw| raw.parse::<f64>().ok());
        let enabled = std::env::var("ACCOUNTING_ENABLED")
            .map(|raw| !matches!(raw.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);

        Ok(Self {
            trade_log_path,
            daily_summary_path,
            event_log_path,
            tax_wallet,
            tax_reserve_bps,
            stablecoin_symbol,
            exchange_endpoint,
            exchange_api_key,
            alert_webhook_url,
            daily_net_profit_target_wei,
            revert_rate_threshold,
            rpc_error_rate_threshold,
            enabled,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TradeRecord {
    timestamp: String,
    chain: String,
    #[serde(default)]
    strategy: String,
    relay: String,
    #[serde(default)]
    venue_path: String,
    start_token: String,
    amount_in_wei: String,
    gross_profit_wei: String,
    net_profit_wei: String,
    gas_cost_wei: String,
    #[serde(default)]
    gas_price_wei: String,
    #[serde(default)]
    gas_limit: String,
    #[serde(default)]
    max_fee_per_gas_wei: String,
    #[serde(default)]
    max_priority_fee_per_gas_wei: String,
    tx_hash: String,
    hops: usize,
    edges_scanned: usize,
    max_slippage_bps: u32,
    latency_ms: u64,
    private_relay_rejected: bool,
    competition_pressure: f64,
    competition_buffer_wei: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DailySummaryRow {
    date: String,
    chain: String,
    #[serde(default)]
    strategy: String,
    trades: u64,
    gross_profit_wei: String,
    net_profit_wei: String,
    gas_cost_wei: String,
    tax_reserve_wei: String,
    stablecoin: String,
    tax_wallet: String,
    exchange_endpoint: String,
}

#[derive(Clone, Debug)]
struct RollingNetSummary {
    start_date: String,
    end_date: String,
    chain: String,
    strategy: String,
    days_in_window: u32,
    days_with_data: u32,
    gross_profit_wei: String,
    net_profit_wei: String,
    gas_cost_wei: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EventRecord {
    timestamp: String,
    chain: String,
    #[serde(default)]
    strategy: String,
    event: String,
}

#[derive(Clone, Debug)]
struct EventSummary {
    chain: String,
    strategy: String,
    sent: u64,
    confirmed: u64,
    reverted: u64,
    rpc_errors: u64,
}

#[derive(Clone, Debug)]
pub struct Accounting {
    config: AccountingConfig,
    write_lock: Arc<std::sync::Mutex<()>>,
}

impl Accounting {
    pub fn from_env() -> Result<Option<Self>> {
        let config = AccountingConfig::from_env()?;
        if !config.enabled {
            return Ok(None);
        }
        Self::new(config).map(Some)
    }

    pub fn new(config: AccountingConfig) -> Result<Self> {
        if config.tax_reserve_bps > 10_000 {
            return Err(anyhow!("TAX_RESERVE_BPS cannot exceed 10000"));
        }

        if let Some(parent) = config
            .trade_log_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create accounting directory {}", parent.display()))?;
        }

        if let Some(parent) = config
            .daily_summary_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create accounting directory {}", parent.display()))?;
        }
        if let Some(parent) = config
            .event_log_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create accounting directory {}", parent.display()))?;
        }

        Ok(Self {
            config,
            write_lock: Arc::new(std::sync::Mutex::new(())),
        })
    }

    pub async fn record_execution(
        &self,
        chain: &str,
        relay: &str,
        summary: &crate::ExecutionSummary,
    ) -> Result<()> {
        let record = TradeRecord {
            timestamp: Utc::now().to_rfc3339(),
            chain: chain.to_string(),
            strategy: summary.strategy.clone(),
            relay: relay.to_string(),
            venue_path: summary.venue_path.join("->"),
            start_token: format!("0x{}", hex::encode(summary.start_token)),
            amount_in_wei: summary.amount_in.to_string(),
            gross_profit_wei: summary.gross.to_string(),
            net_profit_wei: summary.net.to_string(),
            gas_cost_wei: summary.gas_cost.to_string(),
            gas_price_wei: summary.gas_price.to_string(),
            gas_limit: summary.gas_limit.to_string(),
            max_fee_per_gas_wei: summary
                .max_fee_per_gas
                .map(|value| value.to_string())
                .unwrap_or_default(),
            max_priority_fee_per_gas_wei: summary
                .max_priority_fee_per_gas
                .map(|value| value.to_string())
                .unwrap_or_default(),
            tx_hash: format!("{:#x}", summary.tx_hash),
            hops: summary.hops,
            edges_scanned: summary.edges_scanned,
            max_slippage_bps: summary.max_slippage_bps,
            latency_ms: summary.inclusion_latency_ms,
            private_relay_rejected: summary.private_relay_rejected,
            competition_pressure: summary.competition_pressure,
            competition_buffer_wei: summary.competition_buffer_wei.to_string(),
        };

        let path = self.config.trade_log_path.clone();
        let lock = self.write_lock.clone();
        spawn_blocking(move || {
            let _guard = lock.lock().map_err(|err| {
                warn!(error = %err, "Accounting lock poisoned while writing trade log");
                anyhow!("accounting write lock poisoned")
            })?;
            let file_exists = path.exists();
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open trade log at {}", path.display()))?;
            let metadata = file.metadata().ok();
            let is_empty = metadata.map(|m| m.len() == 0).unwrap_or(!file_exists);
            let mut writer = WriterBuilder::new().has_headers(is_empty).from_writer(file);
            writer
                .serialize(&record)
                .with_context(|| format!("write trade record to {}", path.display()))?;
            writer.flush()?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn record_event(&self, chain: &str, strategy: &str, event: &str) -> Result<()> {
        let record = EventRecord {
            timestamp: Utc::now().to_rfc3339(),
            chain: chain.to_string(),
            strategy: strategy.to_string(),
            event: event.to_string(),
        };

        let path = self.config.event_log_path.clone();
        let lock = self.write_lock.clone();
        spawn_blocking(move || {
            let _guard = lock.lock().map_err(|err| {
                warn!(error = %err, "Accounting lock poisoned while writing event log");
                anyhow!("accounting write lock poisoned")
            })?;
            let file_exists = path.exists();
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open event log at {}", path.display()))?;
            let metadata = file.metadata().ok();
            let is_empty = metadata.map(|m| m.len() == 0).unwrap_or(!file_exists);
            let mut writer = WriterBuilder::new().has_headers(is_empty).from_writer(file);
            writer
                .serialize(&record)
                .with_context(|| format!("write event record to {}", path.display()))?;
            writer.flush()?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub fn spawn_daily_rollups(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                let now = Utc::now();
                let next_midnight_date = now
                    .date_naive()
                    .succ_opt()
                    .unwrap_or_else(|| now.date_naive());
                let next_midnight_time = NaiveTime::from_hms_opt(0, 0, 15).unwrap_or_else(|| {
                    warn!(
                        "Invalid midnight time; falling back to current time for rollup scheduling"
                    );
                    now.time()
                });
                let next_midnight = next_midnight_date.and_time(next_midnight_time).and_utc();
                let wait = (next_midnight - now)
                    .to_std()
                    .unwrap_or_else(|_| std::time::Duration::from_secs(60));
                sleep(wait).await;

                if let Err(err) = self.rollup_previous_day().await {
                    warn!(error = %err, "Failed to roll up daily accounting totals");
                }
            }
        });
    }

    pub async fn rollup_previous_day(&self) -> Result<()> {
        let today = Utc::now().date_naive();
        let Some(target_date) = today.pred_opt() else {
            return Ok(());
        };
        let summaries = self.rollup_for_date(target_date).await?;
        if summaries.is_empty() {
            return Ok(());
        }
        self.persist_daily_summaries(summaries.clone()).await?;
        self.maybe_dispatch_tax_reserve(&summaries).await;
        let event_summaries = self.rollup_events_for_date(target_date).await?;
        self.emit_daily_rollup(&summaries, &event_summaries).await;
        let rolling = self.rollup_rolling_net(target_date, 30).await?;
        self.emit_rolling_net_rollup(&rolling).await;
        Ok(())
    }

    async fn maybe_dispatch_tax_reserve(&self, summaries: &[DailySummaryRow]) {
        let endpoint = match &self.config.exchange_endpoint {
            Some(url) if !url.is_empty() => url,
            _ => return,
        };
        if self.config.tax_reserve_bps == 0 {
            return;
        }

        let client = Client::new();
        for summary in summaries {
            if summary.tax_reserve_wei == "0" {
                continue;
            }
            let mut request = client.post(endpoint).json(&serde_json::json!({
                "chain": summary.chain,
                "date": summary.date,
                "stablecoin": self.config.stablecoin_symbol,
                "amount_wei": summary.tax_reserve_wei,
                "destination": summary.tax_wallet,
            }));
            if let Some(key) = &self.config.exchange_api_key {
                request = request.header("Authorization", key);
            }

            if let Err(err) = request.send().await {
                warn!(error = %err, endpoint = %crate::util::redact_endpoint(endpoint), "Failed to submit tax reserve swap request");
            }
        }
    }

    pub(crate) async fn rollup_for_date(&self, date: NaiveDate) -> Result<Vec<DailySummaryRow>> {
        let path = self.config.trade_log_path.clone();
        let tax_wallet = self
            .config
            .tax_wallet
            .map(|addr| format!("0x{}", hex::encode(addr)));
        let stablecoin = self.config.stablecoin_symbol.clone();
        let exchange_endpoint = self.config.exchange_endpoint.clone();
        let reserve_bps = self.config.tax_reserve_bps;

        let rows = spawn_blocking(move || {
            if !path.exists() {
                return Ok(Vec::new());
            }
            let file = File::open(&path)
                .with_context(|| format!("open trade log at {}", path.display()))?;
            let mut reader = ReaderBuilder::new().from_reader(file);
            let mut aggregates: HashMap<(String, String), (u64, U256, U256, U256)> = HashMap::new();
            for result in reader.deserialize::<TradeRecord>() {
                let record: TradeRecord = match result {
                    Ok(rec) => rec,
                    Err(err) => {
                        warn!(error = %err, "Skipping malformed trade record");
                        continue;
                    }
                };
                let timestamp = match DateTime::parse_from_rfc3339(&record.timestamp) {
                    Ok(ts) => ts.with_timezone(&Utc),
                    Err(err) => {
                        warn!(error = %err, "Skipping trade with invalid timestamp");
                        continue;
                    }
                };
                if timestamp.date_naive() != date {
                    continue;
                }
                let gross = match U256::from_dec_str(&record.gross_profit_wei) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping trade with invalid gross profit");
                        continue;
                    }
                };
                let net = match U256::from_dec_str(&record.net_profit_wei) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping trade with invalid net profit");
                        continue;
                    }
                };
                let gas = match U256::from_dec_str(&record.gas_cost_wei) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping trade with invalid gas cost");
                        continue;
                    }
                };

                let entry = aggregates
                    .entry((record.chain.clone(), record.strategy.clone()))
                    .or_insert((0, U256::zero(), U256::zero(), U256::zero()));
                entry.0 = entry.0.saturating_add(1);
                entry.1 = entry.1.saturating_add(gross);
                entry.2 = entry.2.saturating_add(net);
                entry.3 = entry.3.saturating_add(gas);
            }

            let summaries: Vec<DailySummaryRow> = aggregates
                .into_iter()
                .map(|((chain, strategy), (trades, gross, net, gas))| {
                    let tax_reserve =
                        mul_div(net, U256::from(reserve_bps as u64), U256::from(10_000u64));
                    DailySummaryRow {
                        date: date.to_string(),
                        chain,
                        strategy: if strategy.is_empty() {
                            "arb".to_string()
                        } else {
                            strategy
                        },
                        trades,
                        gross_profit_wei: gross.to_string(),
                        net_profit_wei: net.to_string(),
                        gas_cost_wei: gas.to_string(),
                        tax_reserve_wei: tax_reserve.to_string(),
                        stablecoin: stablecoin.clone(),
                        tax_wallet: tax_wallet.clone().unwrap_or_else(|| "".into()),
                        exchange_endpoint: exchange_endpoint.clone().unwrap_or_default(),
                    }
                })
                .collect();

            Ok::<Vec<DailySummaryRow>, anyhow::Error>(summaries)
        })
        .await??;

        Ok(rows)
    }

    pub(crate) async fn rolling_net_for_window(
        &self,
        end_date: NaiveDate,
        days: i64,
    ) -> Result<HashMap<(String, String), U256>> {
        let path = self.config.trade_log_path.clone();
        let start_date = end_date
            .checked_sub_days(chrono::Days::new(days.saturating_sub(1) as u64))
            .unwrap_or(end_date);

        let rows = spawn_blocking(move || {
            if !path.exists() {
                return Ok(HashMap::new());
            }
            let file = File::open(&path)
                .with_context(|| format!("open trade log at {}", path.display()))?;
            let mut reader = ReaderBuilder::new().from_reader(file);
            let mut aggregates: HashMap<(String, String), U256> = HashMap::new();
            for result in reader.deserialize::<TradeRecord>() {
                let record: TradeRecord = match result {
                    Ok(rec) => rec,
                    Err(err) => {
                        warn!(error = %err, "Skipping malformed trade record");
                        continue;
                    }
                };
                let timestamp = match DateTime::parse_from_rfc3339(&record.timestamp) {
                    Ok(ts) => ts.with_timezone(&Utc),
                    Err(err) => {
                        warn!(error = %err, "Skipping trade with invalid timestamp");
                        continue;
                    }
                };
                let date = timestamp.date_naive();
                if date < start_date || date > end_date {
                    continue;
                }
                let net = match U256::from_dec_str(&record.net_profit_wei) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping trade with invalid net profit");
                        continue;
                    }
                };
                let entry = aggregates
                    .entry((record.chain.clone(), record.strategy.clone()))
                    .or_insert(U256::zero());
                *entry = entry.saturating_add(net);
            }
            Ok::<HashMap<(String, String), U256>, anyhow::Error>(aggregates)
        })
        .await??;

        Ok(rows)
    }

    async fn rollup_events_for_date(&self, date: NaiveDate) -> Result<Vec<EventSummary>> {
        let path = self.config.event_log_path.clone();
        let rows = spawn_blocking(move || {
            if !path.exists() {
                return Ok(Vec::new());
            }
            let file = File::open(&path)
                .with_context(|| format!("open event log at {}", path.display()))?;
            let mut reader = ReaderBuilder::new().from_reader(file);
            let mut aggregates: HashMap<(String, String), EventSummary> = HashMap::new();
            for result in reader.deserialize::<EventRecord>() {
                let record: EventRecord = match result {
                    Ok(rec) => rec,
                    Err(err) => {
                        warn!(error = %err, "Skipping malformed event record");
                        continue;
                    }
                };
                let timestamp = match DateTime::parse_from_rfc3339(&record.timestamp) {
                    Ok(ts) => ts.with_timezone(&Utc),
                    Err(err) => {
                        warn!(error = %err, "Skipping event with invalid timestamp");
                        continue;
                    }
                };
                if timestamp.date_naive() != date {
                    continue;
                }
                let key = (record.chain.clone(), record.strategy.clone());
                let entry = aggregates.entry(key.clone()).or_insert(EventSummary {
                    chain: record.chain.clone(),
                    strategy: if record.strategy.is_empty() {
                        "arb".to_string()
                    } else {
                        record.strategy.clone()
                    },
                    sent: 0,
                    confirmed: 0,
                    reverted: 0,
                    rpc_errors: 0,
                });
                match record.event.as_str() {
                    "tx_sent" => entry.sent = entry.sent.saturating_add(1),
                    "tx_confirmed" => entry.confirmed = entry.confirmed.saturating_add(1),
                    "tx_reverted" => entry.reverted = entry.reverted.saturating_add(1),
                    "rpc_error" => entry.rpc_errors = entry.rpc_errors.saturating_add(1),
                    _ => {}
                }
            }
            Ok::<Vec<EventSummary>, anyhow::Error>(aggregates.into_values().collect())
        })
        .await??;
        Ok(rows)
    }

    async fn emit_daily_rollup(&self, summaries: &[DailySummaryRow], events: &[EventSummary]) {
        let mut rolling_net_by_key = HashMap::new();
        if let Some(first) = summaries.first() {
            if let Ok(date) = NaiveDate::parse_from_str(&first.date, "%Y-%m-%d") {
                if let Ok(rolling) = self.rolling_net_for_window(date, 30).await {
                    rolling_net_by_key = rolling;
                }
            }
        }
        for summary in summaries {
            let event = events
                .iter()
                .find(|entry| entry.chain == summary.chain && entry.strategy == summary.strategy);
            let (sent, reverted, rpc_errors) = event
                .map(|entry| (entry.sent, entry.reverted, entry.rpc_errors))
                .unwrap_or((0, 0, 0));
            let revert_rate = if sent > 0 {
                reverted as f64 / sent as f64
            } else {
                0.0
            };
            let rpc_error_rate = if sent > 0 {
                rpc_errors as f64 / sent as f64
            } else {
                0.0
            };
            let rolling_net = rolling_net_by_key
                .get(&(summary.chain.clone(), summary.strategy.clone()))
                .cloned()
                .unwrap_or_else(U256::zero);
            info!(
                chain = %summary.chain,
                strategy = %summary.strategy,
                date = %summary.date,
                trades = summary.trades,
                gross_profit_wei = %summary.gross_profit_wei,
                net_profit_wei = %summary.net_profit_wei,
                gas_cost_wei = %summary.gas_cost_wei,
                rolling_30d_net_wei = %rolling_net,
                revert_rate = revert_rate,
                rpc_error_rate = rpc_error_rate,
                "Daily rollup summary"
            );
            self.dispatch_alert_if_needed(summary, revert_rate, rpc_error_rate, rolling_net)
                .await;
        }
    }

    async fn emit_rolling_net_rollup(&self, summaries: &[RollingNetSummary]) {
        for summary in summaries {
            let net_profit = U256::from_dec_str(&summary.net_profit_wei).unwrap_or_default();
            let daily_target = match self.config.daily_net_profit_target_wei {
                Some(value) if value > U256::zero() => value,
                _ => continue,
            };
            let target = daily_target.saturating_mul(U256::from(summary.days_in_window));
            let below_target = net_profit < target;

            info!(
                chain = %summary.chain,
                strategy = %summary.strategy,
                window_start = %summary.start_date,
                window_end = %summary.end_date,
                days_in_window = summary.days_in_window,
                days_with_data = summary.days_with_data,
                gross_profit_wei = %summary.gross_profit_wei,
                net_profit_wei = %summary.net_profit_wei,
                gas_cost_wei = %summary.gas_cost_wei,
                net_target_wei = %target,
                "Rolling net summary"
            );

            if below_target {
                self.dispatch_rolling_alert(summary, target).await;
            }
        }
    }

    async fn dispatch_alert_if_needed(
        &self,
        summary: &DailySummaryRow,
        revert_rate: f64,
        rpc_error_rate: f64,
        rolling_net_30d: U256,
    ) {
        let webhook = match &self.config.alert_webhook_url {
            Some(url) if !url.is_empty() => url,
            _ => return,
        };
        let net_profit = U256::from_dec_str(&summary.net_profit_wei).unwrap_or_default();
        let net_target = self.config.daily_net_profit_target_wei.unwrap_or_default();
        let net_below_target = net_target > U256::zero() && net_profit < net_target;
        let rolling_target = net_target.saturating_mul(U256::from(30u64));
        let rolling_below_target =
            rolling_target > U256::zero() && rolling_net_30d < rolling_target;
        let revert_limit = self.config.revert_rate_threshold.unwrap_or(1.0);
        let rpc_limit = self.config.rpc_error_rate_threshold.unwrap_or(1.0);
        let revert_spike = revert_rate > revert_limit;
        let rpc_spike = rpc_error_rate > rpc_limit;

        if !(net_below_target || rolling_below_target || revert_spike || rpc_spike) {
            return;
        }

        let payload = serde_json::json!({
            "type": "daily_rollup_alert",
            "date": summary.date,
            "chain": summary.chain,
            "strategy": summary.strategy,
            "net_profit_wei": summary.net_profit_wei,
            "net_target_wei": net_target.to_string(),
            "rolling_30d_net_wei": rolling_net_30d.to_string(),
            "rolling_30d_target_wei": rolling_target.to_string(),
            "revert_rate": revert_rate,
            "revert_rate_threshold": revert_limit,
            "rpc_error_rate": rpc_error_rate,
            "rpc_error_rate_threshold": rpc_limit,
            "flags": {
                "net_below_target": net_below_target,
                "rolling_30d_below_target": rolling_below_target,
                "revert_rate_spike": revert_spike,
                "rpc_error_rate_spike": rpc_spike
            }
        });
        let client = Client::new();
        // A Slack/Discord webhook URL is itself a bearer credential.
        if let Err(err) = client.post(webhook).json(&payload).send().await {
            warn!(error = %err, endpoint = %crate::util::redact_endpoint(webhook), "Failed to send daily rollup alert");
        }
    }

    async fn dispatch_rolling_alert(&self, summary: &RollingNetSummary, target: U256) {
        let webhook = match &self.config.alert_webhook_url {
            Some(url) if !url.is_empty() => url,
            _ => return,
        };

        let payload = serde_json::json!({
            "type": "rolling_30d_alert",
            "window_start": summary.start_date,
            "window_end": summary.end_date,
            "days_in_window": summary.days_in_window,
            "days_with_data": summary.days_with_data,
            "chain": summary.chain,
            "strategy": summary.strategy,
            "net_profit_wei": summary.net_profit_wei,
            "net_target_wei": target.to_string(),
            "flags": {
                "net_below_target": true
            }
        });
        let client = Client::new();
        if let Err(err) = client.post(webhook).json(&payload).send().await {
            warn!(error = %err, endpoint = %crate::util::redact_endpoint(webhook), "Failed to send rolling net alert");
        }
    }

    async fn persist_daily_summaries(&self, summaries: Vec<DailySummaryRow>) -> Result<()> {
        let path = self.config.daily_summary_path.clone();
        let lock = self.write_lock.clone();
        spawn_blocking(move || {
            let _guard = lock.lock().map_err(|err| {
                warn!(error = %err, "Accounting lock poisoned while persisting summaries");
                anyhow!("accounting summary lock poisoned")
            })?;
            let mut existing: Vec<DailySummaryRow> = if path.exists() {
                let file = File::open(&path)
                    .with_context(|| format!("open daily summary at {}", path.display()))?;
                ReaderBuilder::new()
                    .from_reader(file)
                    .deserialize()
                    .filter_map(|row| row.ok())
                    .collect()
            } else {
                Vec::new()
            };

            for summary in summaries {
                existing.retain(|row| !(row.date == summary.date && row.chain == summary.chain));
                existing.push(summary);
            }

            let file = File::create(&path)
                .with_context(|| format!("create daily summary at {}", path.display()))?;
            let mut writer = WriterBuilder::new().has_headers(true).from_writer(file);
            for row in existing {
                writer.serialize(row)?;
            }
            writer.flush()?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    async fn rollup_rolling_net(
        &self,
        end_date: NaiveDate,
        window_days: u32,
    ) -> Result<Vec<RollingNetSummary>> {
        if window_days == 0 {
            return Ok(Vec::new());
        }
        let path = self.config.daily_summary_path.clone();
        let rows = spawn_blocking(move || {
            if !path.exists() {
                return Ok(Vec::new());
            }
            let file = File::open(&path)
                .with_context(|| format!("open daily summary at {}", path.display()))?;
            let mut reader = ReaderBuilder::new().from_reader(file);
            let window_start = end_date
                .checked_sub_days(Days::new((window_days - 1) as u64))
                .unwrap_or(end_date);
            type RollingNetAggregate = (U256, U256, U256, HashMap<NaiveDate, ()>);
            let mut aggregates: HashMap<(String, String), RollingNetAggregate> = HashMap::new();
            for result in reader.deserialize::<DailySummaryRow>() {
                let record: DailySummaryRow = match result {
                    Ok(rec) => rec,
                    Err(err) => {
                        warn!(error = %err, "Skipping malformed daily summary record");
                        continue;
                    }
                };
                let date = match NaiveDate::parse_from_str(&record.date, "%Y-%m-%d") {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping daily summary with invalid date");
                        continue;
                    }
                };
                if date < window_start || date > end_date {
                    continue;
                }
                let gross = match U256::from_dec_str(&record.gross_profit_wei) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping daily summary with invalid gross profit");
                        continue;
                    }
                };
                let net = match U256::from_dec_str(&record.net_profit_wei) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping daily summary with invalid net profit");
                        continue;
                    }
                };
                let gas = match U256::from_dec_str(&record.gas_cost_wei) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(error = %err, "Skipping daily summary with invalid gas cost");
                        continue;
                    }
                };
                let entry = aggregates
                    .entry((record.chain.clone(), record.strategy.clone()))
                    .or_insert((U256::zero(), U256::zero(), U256::zero(), HashMap::new()));
                entry.0 = entry.0.saturating_add(gross);
                entry.1 = entry.1.saturating_add(net);
                entry.2 = entry.2.saturating_add(gas);
                entry.3.insert(date, ());
            }

            let rows = aggregates
                .into_iter()
                .map(
                    |((chain, strategy), (gross, net, gas, days))| RollingNetSummary {
                        start_date: window_start.to_string(),
                        end_date: end_date.to_string(),
                        chain,
                        strategy: if strategy.is_empty() {
                            "arb".to_string()
                        } else {
                            strategy
                        },
                        days_in_window: window_days,
                        days_with_data: days.len() as u32,
                        gross_profit_wei: gross.to_string(),
                        net_profit_wei: net.to_string(),
                        gas_cost_wei: gas.to_string(),
                    },
                )
                .collect();

            Ok::<Vec<RollingNetSummary>, anyhow::Error>(rows)
        })
        .await??;

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::H256;
    use tempfile::tempdir;

    #[tokio::test]
    async fn records_trades_and_rolls_up() {
        let dir = tempdir().unwrap();
        let trade_log = dir.path().join("trades.csv");
        let daily = dir.path().join("daily.csv");
        let config = AccountingConfig {
            trade_log_path: trade_log.clone(),
            daily_summary_path: daily.clone(),
            event_log_path: dir.path().join("events.csv"),
            tax_wallet: None,
            tax_reserve_bps: 3000,
            stablecoin_symbol: "USDC".into(),
            exchange_endpoint: None,
            exchange_api_key: None,
            alert_webhook_url: None,
            daily_net_profit_target_wei: None,
            revert_rate_threshold: None,
            rpc_error_rate_threshold: None,
            enabled: true,
        };
        let accounting = Accounting::new(config).expect("accounting config");

        let summary = crate::ExecutionSummary {
            start_token: Address::zero(),
            amount_in: U256::from(1_000u64),
            hops: 2,
            gross: U256::from(400u64),
            net: U256::from(300u64),
            gross_native: U256::from(400u64),
            net_native: U256::from(300u64),
            gas_cost: U256::from(100u64),
            gas_cost_native: U256::from(100u64),
            gas_limit: U256::from(210_000u64),
            gas_price: U256::from(1u64),
            max_fee_per_gas: Some(U256::from(2u64)),
            max_priority_fee_per_gas: Some(U256::from(1u64)),
            tx_hash: H256::zero(),
            edges_scanned: 4,
            max_slippage_bps: 50,
            inclusion_latency_ms: 1_000,
            private_relay_rejected: false,
            competition_pressure: 1.1,
            competition_buffer_wei: U256::from(10u64),
            strategy: "arb".to_string(),
            venue_path: vec!["univ3".to_string()],
        };

        accounting
            .record_execution("arbitrum", "public", &summary)
            .await
            .expect("record execution");

        let today = Utc::now().date_naive();
        let summaries = accounting
            .rollup_for_date(today)
            .await
            .expect("daily rollup");
        assert_eq!(summaries.len(), 1);
        let row = summaries.first().unwrap();
        assert_eq!(row.chain, "arbitrum");
        assert_eq!(row.trades, 1);
        assert_eq!(row.net_profit_wei, "300");
        assert_eq!(row.tax_reserve_wei, "90");

        accounting
            .persist_daily_summaries(summaries)
            .await
            .expect("persist summaries");
        assert!(daily.exists());
    }

    #[tokio::test]
    async fn computes_rolling_net_over_window() {
        let dir = tempdir().unwrap();
        let trade_log = dir.path().join("trades.csv");
        let config = AccountingConfig {
            trade_log_path: trade_log.clone(),
            daily_summary_path: dir.path().join("daily.csv"),
            event_log_path: dir.path().join("events.csv"),
            tax_wallet: None,
            tax_reserve_bps: 0,
            stablecoin_symbol: "USDC".into(),
            exchange_endpoint: None,
            exchange_api_key: None,
            alert_webhook_url: None,
            daily_net_profit_target_wei: None,
            revert_rate_threshold: None,
            rpc_error_rate_threshold: None,
            enabled: true,
        };
        let accounting = Accounting::new(config).expect("accounting config");

        let today = Utc::now().date_naive();
        let day_one = today.checked_sub_days(Days::new(2)).unwrap_or(today);
        let day_two = today.checked_sub_days(Days::new(1)).unwrap_or(today);

        let summaries = vec![
            DailySummaryRow {
                date: day_one.to_string(),
                chain: "arbitrum".to_string(),
                strategy: "arb".to_string(),
                trades: 1,
                gross_profit_wei: "100".to_string(),
                net_profit_wei: "100".to_string(),
                gas_cost_wei: "0".to_string(),
                tax_reserve_wei: "0".to_string(),
                stablecoin: "USDC".to_string(),
                tax_wallet: "".to_string(),
                exchange_endpoint: "".to_string(),
            },
            DailySummaryRow {
                date: day_two.to_string(),
                chain: "arbitrum".to_string(),
                strategy: "arb".to_string(),
                trades: 1,
                gross_profit_wei: "200".to_string(),
                net_profit_wei: "200".to_string(),
                gas_cost_wei: "0".to_string(),
                tax_reserve_wei: "0".to_string(),
                stablecoin: "USDC".to_string(),
                tax_wallet: "".to_string(),
                exchange_endpoint: "".to_string(),
            },
            DailySummaryRow {
                date: today.to_string(),
                chain: "arbitrum".to_string(),
                strategy: "arb".to_string(),
                trades: 1,
                gross_profit_wei: "300".to_string(),
                net_profit_wei: "300".to_string(),
                gas_cost_wei: "0".to_string(),
                tax_reserve_wei: "0".to_string(),
                stablecoin: "USDC".to_string(),
                tax_wallet: "".to_string(),
                exchange_endpoint: "".to_string(),
            },
        ];

        accounting
            .persist_daily_summaries(summaries)
            .await
            .expect("persist summaries");

        let rolling = accounting
            .rollup_rolling_net(today, 3)
            .await
            .expect("rollup rolling net");
        assert_eq!(rolling.len(), 1);
        let summary = rolling.first().unwrap();
        assert_eq!(summary.days_in_window, 3);
        assert_eq!(summary.days_with_data, 3);
        assert_eq!(summary.net_profit_wei, "600");
        let records = vec![
            TradeRecord {
                timestamp: "2024-01-10T00:00:00Z".into(),
                chain: "arbitrum".into(),
                strategy: "arb".into(),
                relay: "private".into(),
                venue_path: "univ3".into(),
                start_token: "0x0000000000000000000000000000000000000000".into(),
                amount_in_wei: "100".into(),
                gross_profit_wei: "200".into(),
                net_profit_wei: "150".into(),
                gas_cost_wei: "50".into(),
                gas_price_wei: "1".into(),
                gas_limit: "210000".into(),
                max_fee_per_gas_wei: "2".into(),
                max_priority_fee_per_gas_wei: "1".into(),
                tx_hash: "0x0".into(),
                hops: 2,
                edges_scanned: 1,
                max_slippage_bps: 50,
                latency_ms: 1000,
                private_relay_rejected: false,
                competition_pressure: 1.0,
                competition_buffer_wei: "0".into(),
            },
            TradeRecord {
                timestamp: "2024-01-20T00:00:00Z".into(),
                chain: "arbitrum".into(),
                strategy: "arb".into(),
                relay: "private".into(),
                venue_path: "univ3".into(),
                start_token: "0x0000000000000000000000000000000000000000".into(),
                amount_in_wei: "100".into(),
                gross_profit_wei: "300".into(),
                net_profit_wei: "250".into(),
                gas_cost_wei: "50".into(),
                gas_price_wei: "1".into(),
                gas_limit: "210000".into(),
                max_fee_per_gas_wei: "2".into(),
                max_priority_fee_per_gas_wei: "1".into(),
                tx_hash: "0x1".into(),
                hops: 2,
                edges_scanned: 1,
                max_slippage_bps: 50,
                latency_ms: 1000,
                private_relay_rejected: false,
                competition_pressure: 1.0,
                competition_buffer_wei: "0".into(),
            },
            TradeRecord {
                timestamp: "2023-12-01T00:00:00Z".into(),
                chain: "arbitrum".into(),
                strategy: "arb".into(),
                relay: "private".into(),
                venue_path: "univ3".into(),
                start_token: "0x0000000000000000000000000000000000000000".into(),
                amount_in_wei: "100".into(),
                gross_profit_wei: "400".into(),
                net_profit_wei: "350".into(),
                gas_cost_wei: "50".into(),
                gas_price_wei: "1".into(),
                gas_limit: "210000".into(),
                max_fee_per_gas_wei: "2".into(),
                max_priority_fee_per_gas_wei: "1".into(),
                tx_hash: "0x2".into(),
                hops: 2,
                edges_scanned: 1,
                max_slippage_bps: 50,
                latency_ms: 1000,
                private_relay_rejected: false,
                competition_pressure: 1.0,
                competition_buffer_wei: "0".into(),
            },
        ];

        let file = File::create(&trade_log).expect("create trade log");
        let mut writer = WriterBuilder::new().has_headers(true).from_writer(file);
        for record in records {
            writer.serialize(record).expect("serialize trade record");
        }
        writer.flush().expect("flush trade log");

        let end_date = NaiveDate::from_ymd_opt(2024, 1, 20).unwrap();
        let rolling = accounting
            .rolling_net_for_window(end_date, 30)
            .await
            .expect("rolling net");
        let key = ("arbitrum".to_string(), "arb".to_string());
        assert_eq!(
            rolling.get(&key).cloned().unwrap_or_default(),
            U256::from(400u64)
        );
    }
}
