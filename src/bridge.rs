use anyhow::{anyhow, Context, Result};
use ethers::types::{Address, U256};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::graph::{Edge, Graph, VenueEdge};
use crate::util::{compute_edge_weight, parse_selector, NativePrice, TradeSizing, WEIGHT_SCALE};
use tracing::info;

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct BridgeRouteCfg {
    pub name: Option<String>,
    #[serde(rename = "router")]
    pub router: String,
    #[serde(rename = "tokenIn")]
    pub token_in: String,
    #[serde(rename = "tokenOut")]
    pub token_out: String,
    #[serde(rename = "dstChainId")]
    pub dst_chain_id: u64,
    #[serde(default, rename = "feeBps")]
    pub fee_bps: u32,
    #[serde(rename = "selector")]
    pub selector: String,
    #[serde(default, rename = "estimatedGas")]
    pub estimated_gas: u64,
    #[serde(default, rename = "estimatedTimeSecs")]
    pub estimated_time_secs: u64,
    #[serde(default, rename = "maxTimeSecs")]
    pub max_time_secs: Option<u64>,
    #[serde(default, rename = "maxNotionalWei")]
    pub max_notional_wei: Option<String>,
}

#[derive(Clone, Debug)]
struct BridgeRoute {
    pub name: String,
    pub router: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub dst_chain_id: u64,
    pub fee_bps: u32,
    pub selector: [u8; 4],
    pub estimated_gas: u64,
    pub estimated_time: Duration,
    pub max_time: Duration,
    pub max_notional: Option<U256>,
}

#[derive(Clone, Debug)]
pub struct BridgePlanner {
    routes: Vec<BridgeRoute>,
    max_duration: Duration,
}

fn parse_address(raw: &str, field: &str) -> Result<Address> {
    Address::from_str(raw).with_context(|| format!("invalid address `{raw}` for {field}"))
}

fn parse_u256(raw: &str, field: &str) -> Result<U256> {
    let trimmed = raw.trim();
    if let Some(stripped) = trimmed.strip_prefix("0x") {
        U256::from_str_radix(stripped, 16)
            .with_context(|| format!("failed to parse hex value `{raw}` for {field}"))
    } else {
        U256::from_dec_str(trimmed)
            .with_context(|| format!("failed to parse decimal value `{raw}` for {field}"))
    }
}

impl BridgePlanner {
    pub fn from_env(default_max_time_secs: u64) -> Result<Option<Self>> {
        let Some((raw, source_desc)) = load_bridge_routes_config()? else {
            return Ok(None);
        };

        let routes_cfg: Vec<BridgeRouteCfg> = json5::from_str(&raw)
            .with_context(|| format!("failed to parse bridge routes from {source_desc}"))?;

        let max_duration = Duration::from_secs(default_max_time_secs.max(1));
        let mut routes = Vec::new();
        for cfg in routes_cfg {
            anyhow::ensure!(
                cfg.fee_bps <= 10_000,
                "bridge fee basis points must be <= 10_000 (got {})",
                cfg.fee_bps
            );
            let router = parse_address(&cfg.router, "router")?;
            let token_in = parse_address(&cfg.token_in, "tokenIn")?;
            let token_out = parse_address(&cfg.token_out, "tokenOut")?;
            let selector = parse_selector(&cfg.selector)?;
            let estimated_time = Duration::from_secs(cfg.estimated_time_secs);
            let max_time =
                Duration::from_secs(cfg.max_time_secs.unwrap_or(default_max_time_secs).max(1));
            if estimated_time > max_time || estimated_time > max_duration {
                continue;
            }
            let max_notional = cfg
                .max_notional_wei
                .as_deref()
                .map(|raw| parse_u256(raw, "maxNotionalWei"))
                .transpose()?;
            routes.push(BridgeRoute {
                name: cfg.name.unwrap_or_else(|| "bridge".to_string()),
                router,
                token_in,
                token_out,
                dst_chain_id: cfg.dst_chain_id,
                fee_bps: cfg.fee_bps,
                selector,
                estimated_gas: cfg.estimated_gas,
                estimated_time,
                max_time,
                max_notional,
            });
        }

        if routes.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self {
            routes,
            max_duration,
        }))
    }

    pub fn add_edges(
        &self,
        graph: &mut Graph,
        base_profiles: &HashMap<Address, TradeSizing>,
        default_base: U256,
        _gas_price: U256,
        _native_prices: &HashMap<Address, NativePrice>,
    ) -> Result<Vec<Edge>> {
        let mut edges = Vec::new();
        for route in &self.routes {
            let base_amount = base_profiles
                .get(&route.token_in)
                .map(|profile| profile.base_amount)
                .unwrap_or(default_base);
            let liquidity_cap = route.max_notional.unwrap_or(base_amount);
            if liquidity_cap.is_zero() {
                continue;
            }
            let trade_cap = liquidity_cap.min(base_amount);
            if trade_cap.is_zero() {
                continue;
            }
            let fee_den = U256::from(10_000u64);
            let fee_num = U256::from(10_000u64.saturating_sub(route.fee_bps as u64));
            if fee_num.is_zero() {
                continue;
            }
            let rate_num = fee_num;
            let rate_den = fee_den;
            let mut weight = compute_edge_weight(rate_num, rate_den);
            let latency_penalty = (route.estimated_time.as_secs_f64()
                / self.max_duration.as_secs_f64().max(1.0))
            .min(10.0)
                * WEIGHT_SCALE as f64;
            let latency_penalty = latency_penalty
                .clamp(i64::MIN as f64, i64::MAX as f64)
                .round() as i64;
            weight = weight.saturating_add(latency_penalty);

            let edge = Edge {
                from: route.token_in,
                to: route.token_out,
                rate_num,
                rate_den,
                venue: VenueEdge::Bridge {
                    router: route.router,
                    token_in: route.token_in,
                    token_out: route.token_out,
                    dst_chain_id: route.dst_chain_id,
                    selector: route.selector,
                    bridge_name: route.name.clone(),
                    max_bridge_time_secs: route.max_time.as_secs(),
                    estimated_time_secs: route.estimated_time.as_secs(),
                    fee_bps: route.fee_bps,
                    liquidity_limit: liquidity_cap,
                },
                estimated_gas: route.estimated_gas,
                weight,
                max_input: trade_cap,
                tolerance_bps: route.fee_bps,
                observed_slippage_bps: route.fee_bps,
                quote_block: None,
                active: true,
                tick_ladder: None,
            };
            graph.add_edge(edge.clone());
            edges.push(edge);
            info!(
                bridge = %route.name,
                dst_chain = route.dst_chain_id,
                fee_bps = route.fee_bps,
                eta_secs = route.estimated_time.as_secs(),
                liquidity_limit_wei = %liquidity_cap,
                "Added bridge edge"
            );
        }
        Ok(edges)
    }
}

fn load_bridge_routes_config() -> Result<Option<(String, String)>> {
    if let Ok(path) = std::env::var("BRIDGE_ROUTES_FILE") {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read bridge routes file `{}`", path.display()))?;
        return Ok(Some((raw, format!("file `{}`", path.display()))));
    }

    let raw = match std::env::var("BRIDGE_ROUTES") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(anyhow!(
                "environment variable BRIDGE_ROUTES contains invalid UTF-8"
            ))
        }
    };

    let mut source_desc = "environment variable BRIDGE_ROUTES".to_string();
    let mut raw = raw;
    if needs_multiline_expansion(&raw) {
        if let Some((expanded, desc)) = try_expand_multiline_env("BRIDGE_ROUTES")? {
            raw = expanded;
            source_desc = desc;
        }
    }

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    Ok(Some((raw, source_desc)))
}

fn needs_multiline_expansion(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty() && !is_brace_balanced(trimmed)
}

fn is_brace_balanced(raw: &str) -> bool {
    let mut square = 0i32;
    let mut curly = 0i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;

    for ch in raw.chars() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_single || in_double => {
                escape = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            _ if in_single || in_double => {}
            '[' => square += 1,
            ']' => {
                if square == 0 {
                    return false;
                }
                square -= 1;
            }
            '{' => curly += 1,
            '}' => {
                if curly == 0 {
                    return false;
                }
                curly -= 1;
            }
            _ => {}
        }
    }

    !in_single && !in_double && square == 0 && curly == 0
}

fn try_expand_multiline_env(key: &str) -> Result<Option<(String, String)>> {
    let Some(path) = find_dotenv_file() else {
        return Ok(None);
    };
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read dotenv file `{}`", path.display()))?;

    let mut lines = contents.lines().enumerate().peekable();
    while let Some((idx, raw_line)) = lines.next() {
        let mut line = raw_line.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with("export ") {
            line = &line["export ".len()..];
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }

        let mut combined = value
            .trim_start_matches(|c: char| c.is_ascii_whitespace())
            .to_string();
        while !is_brace_balanced(&combined) {
            if let Some((_, next_line)) = lines.next() {
                combined.push('\n');
                combined.push_str(next_line.trim_end_matches('\r'));
            } else {
                break;
            }
        }

        let combined = combined.trim().to_string();
        let description = format!(
            "dotenv file `{}` entry `{}` starting at line {}",
            path.display(),
            key,
            idx + 1
        );
        return Ok(Some((combined, description)));
    }

    Ok(None)
}

fn find_dotenv_file() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DOTENV_FILE") {
        if !path.is_empty() {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
    }

    let mut dir = std::env::current_dir().ok()?;
    loop {
        for candidate in [".env", ".env.local"] {
            let path = dir.join(candidate);
            if path.exists() {
                return Some(path);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TradeSizing;
    use std::env;

    #[test]
    fn planner_adds_bridge_edges_with_latency_control() {
        let raw = r#"[
            {
                router: "0x4200000000000000000000000000000000000006",
                tokenIn: "0x0000000000000000000000000000000000000001",
                tokenOut: "0x0000000000000000000000000000000000000002",
                dstChainId: 10,
                selector: "0x12345678",
                estimatedGas: 450000,
                estimatedTimeSecs: 45,
                feeBps: 15,
            }
        ]"#;

        env::set_var("BRIDGE_ROUTES", raw);
        let planner = BridgePlanner::from_env(120)
            .expect("planner load should succeed")
            .expect("planner should be present");
        env::remove_var("BRIDGE_ROUTES");

        let token_in = Address::from_low_u64_be(1);
        let token_out = Address::from_low_u64_be(2);
        let mut graph = Graph::default();
        let mut base_profiles = HashMap::new();
        let base = U256::from(1_000_000u64);
        base_profiles.insert(token_in, TradeSizing::new(base, 30));

        let edges = planner
            .add_edges(
                &mut graph,
                &base_profiles,
                base,
                U256::from(1_000_000_000u64),
                &HashMap::new(),
            )
            .expect("edges added");

        assert_eq!(edges.len(), 1);
        let edge = &edges[0];
        assert!(matches!(
            edge.venue,
            VenueEdge::Bridge {
                dst_chain_id: 10,
                ..
            }
        ));
        assert_eq!(edge.from, token_in);
        assert_eq!(edge.to, token_out);
        assert_eq!(edge.max_input, base);
    }
}
