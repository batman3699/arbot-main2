//! `ops/shadow.base.yaml`: what `apex shadow` reads.
//!
//! # Secrets
//!
//! Endpoint URLs carry the provider key through `${APEX_SECRET_*}`
//! placeholders, resolved by `apex_config::interpolate` under
//! `Env::secrets_only` — an unset one refuses to start — and held as `Secret`s
//! from the moment they are parsed, so `Debug` prints none of them. A parse
//! error is scrubbed of every value the environment supplied before it is
//! reported: `serde_yaml` quotes a mistyped value, and a key interpolated into
//! the wrong field would otherwise be quoted with it.
//!
//! The signer's key is not in the file at all, not even as a placeholder: the
//! file names the variable ([`SignerConfig::key_env`]) and the assembly reads it
//! once, straight into a `Secret`, never through a YAML parser.
//!
//! # Fail closed
//!
//! Unknown fields are refused — a misspelt policy field that silently took a
//! default is how a run comes up on the wrong settings and looks healthy — and
//! so is a journal not named as a shadow's. Recovery closes whatever a shadow
//! journal holds as never sent, which is true only of a journal no sending run
//! has written.

use alloy_primitives::{keccak256, Address, B256};
use apex_config::{ConfigError, Env, Secret};
use serde::Deserialize;
use std::path::PathBuf;

/// Why a shadow configuration was refused.
#[derive(Debug)]
pub enum ShadowConfigError {
    Io { path: String, detail: String },
    /// A placeholder with no value. Names the variable, never a value.
    Unresolved(ConfigError),
    /// The document did not parse. Scrubbed of every resolved secret.
    Parse(String),
    /// It parsed, and says something this run will not do.
    Invalid(String),
}

impl std::fmt::Display for ShadowConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, detail } => write!(f, "cannot read {path}: {detail}"),
            Self::Unresolved(e) => write!(f, "{e}"),
            Self::Parse(e) => write!(f, "shadow config: {e}"),
            Self::Invalid(e) => write!(f, "shadow config: {e}"),
        }
    }
}

impl std::error::Error for ShadowConfigError {}

/// Base, and only Base: the book, the frontier and the call builder all are.
pub const BASE_CHAIN_ID: u64 = 8453;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    chain_id: u64,
    rpc: RawRpc,
    inventory: PathBuf,
    journal: PathBuf,
    report: PathBuf,
    misses: PathBuf,
    executor: ExecutorConfig,
    signer: SignerConfig,
    policy: Policy,
    costs: CostConfig,
    capacity: CapacityConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpc {
    http: Vec<String>,
    ws: String,
}

/// The executor every ticket is committed to.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    pub address: Address,
    /// The code it must be running to be that executor (the live reader's check
    /// 2): after the Phase 5 deploy, the clone's.
    pub code_hash: B256,
    /// The plan version it speaks (`PLAN_VERSION_V2`).
    pub plan_version: u32,
}

/// The one signer lane.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerConfig {
    pub lane: u16,
    /// The environment variable holding the key. `APEX_SECRET_*`, or
    /// `Env::secrets_only` will not read it.
    pub key_env: String,
    /// The address the executor authorizes. The key must sign as it.
    pub address: Address,
    pub gas_reserve_wei: u128,
}

/// What the run decides, stated once.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub fee_ceiling_wei: u128,
    pub min_profit_wei: i128,
    pub slippage_bps_per_hop: u32,
    pub max_evidence_age_blocks: u64,
    pub max_view_age_ms: u64,
    pub full_reload_every_s: u64,
    pub report_every_s: u64,
    /// How often, in blocks, the L1 fee oracle is read again.
    pub l1_every_blocks: u64,
}

/// How a settlement fails, and what failing costs: what the chain does not
/// say. What a success uses is each route's own gas at its size, measured per
/// venue (`live::gas::MEASURED`), and its limit is built from that.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostConfig {
    pub gas_on_failure: u64,
    pub failure_ppm: u32,
}

/// The flashblock sampler's schedule (R5).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityConfig {
    pub boot_sample_s: u64,
    pub sample_every_s: u64,
    pub sample_for_s: u64,
}

/// A resolved shadow configuration. `Debug` shows no secret.
#[derive(Clone, Debug)]
pub struct ShadowConfig {
    /// `keccak256` of the file as written — before interpolation, so it names
    /// the declared configuration and not the machine it resolved on.
    pub version: B256,
    pub chain_id: u64,
    pub rpc_http: Vec<Secret<String>>,
    pub rpc_ws: Secret<String>,
    pub inventory: PathBuf,
    pub journal: PathBuf,
    pub report: PathBuf,
    pub misses: PathBuf,
    pub executor: ExecutorConfig,
    pub signer: SignerConfig,
    pub policy: Policy,
    pub costs: CostConfig,
    pub capacity: CapacityConfig,
}

fn scrub(mut message: String, secrets: &[String]) -> String {
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        message = message.replace(s.as_str(), "***");
    }
    message
}

fn invalid(detail: impl Into<String>) -> ShadowConfigError {
    ShadowConfigError::Invalid(detail.into())
}

impl ShadowConfig {
    pub fn load(path: &str, env: &Env) -> Result<Self, ShadowConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ShadowConfigError::Io { path: path.to_string(), detail: e.to_string() })?;
        Self::from_yaml_str(&text, env)
    }

    pub fn from_yaml_str(text: &str, env: &Env) -> Result<Self, ShadowConfigError> {
        let version = keccak256(text.as_bytes());
        let resolved = apex_config::interpolate(text, env).map_err(ShadowConfigError::Unresolved)?;
        let raw: Raw = serde_yaml::from_str(&resolved).map_err(|e| {
            let secrets: Vec<String> = env.consulted().iter().filter_map(|k| env.get(k)).collect();
            ShadowConfigError::Parse(scrub(e.to_string(), &secrets))
        })?;
        drop(resolved);

        if raw.chain_id != BASE_CHAIN_ID {
            return Err(invalid(format!("chain {} is not Base; the shadow run is Base's", raw.chain_id)));
        }
        if raw.rpc.http.iter().all(|u| u.trim().is_empty()) {
            return Err(invalid("rpc.http names no endpoint"));
        }
        if !raw.signer.key_env.starts_with(apex_config::SECRET_PREFIX) {
            return Err(invalid(format!(
                "signer.key_env must name an {}* variable",
                apex_config::SECRET_PREFIX
            )));
        }
        let named_shadow = |p: &PathBuf| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("shadow"));
        if !named_shadow(&raw.journal) {
            return Err(invalid(
                "the journal must be named as a shadow's: recovery closes everything in it as never sent",
            ));
        }
        let p = &raw.policy;
        if p.report_every_s == 0 || p.full_reload_every_s == 0 || p.l1_every_blocks == 0 {
            return Err(invalid("a period of zero"));
        }
        if raw.capacity.sample_every_s == 0 || raw.capacity.boot_sample_s == 0 || raw.capacity.sample_for_s == 0 {
            return Err(invalid("a capacity sample of zero"));
        }
        Ok(Self {
            version,
            chain_id: raw.chain_id,
            rpc_http: raw.rpc.http.into_iter().map(Secret::new).collect(),
            rpc_ws: Secret::new(raw.rpc.ws),
            inventory: raw.inventory,
            journal: raw.journal,
            report: raw.report,
            misses: raw.misses,
            executor: raw.executor,
            signer: raw.signer,
            policy: raw.policy,
            costs: raw.costs,
            capacity: raw.capacity,
        })
    }
}
