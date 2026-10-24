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
use apex_econ::eligibility::EligibilityPolicy;
use crate::live::inventory::UniverseFilter;
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
    race: PathBuf,
    flashblocks: FlashblocksConfig,
    armed: ArmedConfig,
    executor: ExecutorConfig,
    signer: SignerConfig,
    policy: Policy,
    costs: CostConfig,
    capacity: CapacityConfig,
    universe: UniverseConfig,
    /// R28: the live canary. Only `apex canary` accepts it.
    #[serde(default)]
    canary: Option<CanaryConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpc {
    http: Vec<String>,
    ws: String,
}

/// R28: the live canary. Present only in a canary's configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryConfig {
    pub ledger: PathBuf,
    /// `touch` it to stop new sends; remove it to resume. Not a halt.
    pub stop_file: PathBuf,
    /// The executor must pay here, or the canary refuses to start.
    pub profit_recipient: Address,
    /// R32.1: where sends go first: Base's sequencer, whose answer and
    /// inclusion took 18-38 ms and 70-254 ms against BlockPI's protected
    /// endpoint's 0.7-1.5 s and 0.6-1.7 s (`apex send-probe`, 2026-10-21).
    /// `rpc_http[0]` takes whatever this has not. Absent: `rpc_http[0]` alone.
    #[serde(default)]
    pub send_url: Option<String>,
    /// Losses on losing trades; wins do not offset them.
    pub loss_cap_wei: u128,
    pub max_consecutive_reverts: u32,
    pub max_consecutive_refusals: u32,
    pub max_landed: u32,
    pub receipt_poll_ms: u64,
    pub settle_timeout_ms: u64,
}

/// Which command is loading: each refuses the other's configuration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Shadow,
    Canary,
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

/// Which pools the run may price, by the inventory's measured fee and depth
/// (R20). The census's 500 ppm left out Base's deepest WETH/USDC and
/// WETH/cbBTC pools, where the large swaps behind every real gap land.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UniverseConfig {
    /// The highest fee a pool may charge, in ppm.
    pub max_fee_ppm: u32,
    /// The least a pool must hold, in whole US dollars.
    pub min_depth_usd: u64,
}

impl UniverseConfig {
    pub fn filter(&self) -> UniverseFilter {
        UniverseFilter { max_fee_ppm: self.max_fee_ppm, min_depth_usd: self.min_depth_usd as f64 }
    }
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
    /// The risk gate's cost-confidence cap for this run: how large a share of
    /// the expected profit the gas estimate's p50-to-p99 width may be, in bps.
    pub max_cost_confidence_bps: u32,
    /// R31: the file of rivals' bids per gas measured on our gaps
    /// (`scripts/data/rival_bids.py`), against which each candidate bids the
    /// priority fee that maximises its expected profit. In both the shadow's
    /// file and the canary's, so the two price the same trade the same way.
    pub rival_bids: std::path::PathBuf,
}

impl Policy {
    /// The risk gate's thresholds: the default's, with this run's
    /// cost-confidence cap. The default's 1,000 bps refused the first trade
    /// ever to pass Tier 2 inside the block being built (ticket 46,
    /// 2026-10-06): half a cent expected, a 1,629 bps width, still profitable
    /// at p99 gas. Operator decision: relax it for the shadow only.
    pub fn eligibility(&self) -> EligibilityPolicy {
        EligibilityPolicy { max_cost_confidence_bps: self.max_cost_confidence_bps, ..EligibilityPolicy::default() }
    }
}

/// How a settlement fails, and what failing costs: what the chain does not
/// say. What a success uses is each route's own gas at its size, measured per
/// venue (`live::gas::LEAN`), and its limit is built from that.
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

/// Base's public flashblock stream and where its ledger goes (R25).
/// Measurement only: nothing priced or signed reads it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashblocksConfig {
    pub stream: String,
    pub ledger: PathBuf,
}

/// R26: armed backruns. `shadow` arms and records; `live` (R36) also sends,
/// and runs only under `apex canary`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmedMode {
    Shadow,
    Live,
}

/// R26: the arming engine's settings and its two ledgers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmedConfig {
    pub mode: ArmedMode,
    pub ledger: PathBuf,
    pub verdicts: PathBuf,
    pub max_arms: usize,
    pub rungs: u8,
    pub window_s: u64,
    pub block_time_ms: u64,
    pub margin_wei: u64,
    pub band_cap_bps: u32,
    pub liquidity_floor_bps: u32,
    /// R36: the census's `targets.json`. Without it (or with one not fit to
    /// arm) armed-shadow arms R26's universe and live mode does not boot.
    #[serde(default)]
    pub targets: Option<PathBuf>,
    /// R36: the oldest `targets.json` armed from, in seconds.
    #[serde(default = "two_days_s")]
    pub targets_max_age_s: u64,
    /// R36: the most an arm bids, in wei per gas over the base fee.
    #[serde(default = "twenty_gwei")]
    pub max_priority_wei_per_gas: u128,
    /// R36: the day's loss budget: realised losses and every pending
    /// transaction's worst case share it.
    #[serde(default = "a_hundredth_eth")]
    pub daily_loss_budget_wei: u128,
    /// R36: the largest size an arm is sent at.
    #[serde(default = "five_weth")]
    pub max_size_wei: u128,
    /// R36: a bound on one transaction's L1 data fee, in its worst case.
    #[serde(default = "fifty_microeth")]
    pub l1_fee_bound_wei: u128,
    /// R36: left in every sender after its worst case.
    #[serde(default = "two_hundred_microeth")]
    pub sender_reserve_wei: u128,
    /// R36: where armed-live's parts are. Its caps are the ones above.
    #[serde(default)]
    pub live: Option<ArmedLiveConfig>,
}

/// R36: where armed-live's parts are, and nothing else: every cap is
/// `armed:`'s, read by the engine and the live worker alike.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmedLiveConfig {
    /// The senders' keys, one per slot (`apex keys new`), mode 0400.
    pub keys_file: PathBuf,
    /// Where `base_sendRawTransactionValidity` goes.
    pub sequencer: String,
    /// The write-ahead live ledger.
    pub ledger: PathBuf,
    /// `touch` it to stop new sends; remove it to resume. Not a halt.
    pub kill_file: PathBuf,
}

const fn twenty_gwei() -> u128 {
    20_000_000_000
}

const fn a_hundredth_eth() -> u128 {
    10_000_000_000_000_000
}

const fn five_weth() -> u128 {
    5_000_000_000_000_000_000
}

const fn fifty_microeth() -> u128 {
    50_000_000_000_000
}

const fn two_hundred_microeth() -> u128 {
    200_000_000_000_000
}

/// R36: each money cap's range, in wei: none may be zero, and none may be a
/// slip of the decimal point beyond what the pilot was approved at.
const ARMED_CAPS: [(&str, u128); 5] = [
    ("daily_loss_budget_wei", 100_000_000_000_000_000),   // 0.1 ETH
    ("max_priority_wei_per_gas", 1_000_000_000_000),      // 1,000 gwei
    ("max_size_wei", 100_000_000_000_000_000_000),        // 100 WETH
    ("l1_fee_bound_wei", 10_000_000_000_000_000),         // 0.01 ETH
    ("sender_reserve_wei", 100_000_000_000_000_000),      // 0.1 ETH
];

impl ArmedConfig {
    fn caps(&self) -> [u128; 5] {
        [self.daily_loss_budget_wei, self.max_priority_wei_per_gas, self.max_size_wei, self.l1_fee_bound_wei, self.sender_reserve_wei]
    }
}

const fn two_days_s() -> u64 {
    172_800
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
    /// The race ledger: one line per handled proposal, a file per UTC day (R25).
    pub race: PathBuf,
    pub flashblocks: FlashblocksConfig,
    /// Armed backruns, R26.
    pub armed: ArmedConfig,
    pub executor: ExecutorConfig,
    pub signer: SignerConfig,
    pub policy: Policy,
    pub costs: CostConfig,
    pub capacity: CapacityConfig,
    /// Which pools the run prices.
    pub universe: UniverseConfig,
    /// R28: the live canary, when this is `apex canary`'s configuration.
    pub canary: Option<CanaryConfig>,
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
        Self::parse(text, env, Command::Shadow)
    }

    /// `ops/canary.base.yaml` (R28).
    pub fn canary_from_yaml_str(text: &str, env: &Env) -> Result<Self, ShadowConfigError> {
        Self::parse(text, env, Command::Canary)
    }

    pub fn load_canary(path: &str, env: &Env) -> Result<Self, ShadowConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ShadowConfigError::Io { path: path.to_string(), detail: e.to_string() })?;
        Self::canary_from_yaml_str(&text, env)
    }

    fn parse(text: &str, env: &Env, command: Command) -> Result<Self, ShadowConfigError> {
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
        // R28: each command refuses the other's configuration. `apex shadow`
        // never sends, whatever its file says; a canary's journal is reconciled
        // against the chain and must never be a shadow's.
        let named = |p: &PathBuf, what: &str| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(what));
        match (command, &raw.canary) {
            (Command::Shadow, Some(_)) => {
                return Err(invalid("a canary section in a shadow's configuration: `apex shadow` never sends; run `apex canary`"))
            }
            (Command::Shadow, None) if !named(&raw.journal, "shadow") => {
                return Err(invalid(
                    "the journal must be named as a shadow's: recovery closes everything in it as never sent",
                ))
            }
            (Command::Canary, None) => return Err(invalid("`apex canary` needs a canary section")),
            (Command::Canary, Some(_)) if !named(&raw.journal, "canary") || named(&raw.journal, "shadow") => {
                return Err(invalid("the journal must be named as a canary's, never a shadow's"))
            }
            (Command::Canary, Some(k))
                if k.loss_cap_wei == 0
                    || k.max_consecutive_reverts == 0
                    || k.max_consecutive_refusals == 0
                    || k.max_landed == 0
                    || k.receipt_poll_ms == 0
                    || k.settle_timeout_ms == 0 =>
            {
                return Err(invalid("canary: every limit and period must be above zero"))
            }
            _ => {}
        }
        let p = &raw.policy;
        if p.report_every_s == 0 || p.full_reload_every_s == 0 || p.l1_every_blocks == 0 {
            return Err(invalid("a period of zero"));
        }
        if p.rival_bids.as_os_str().is_empty() {
            return Err(invalid("policy.rival_bids names no file: a run without it would bid nothing"));
        }
        if raw.capacity.sample_every_s == 0 || raw.capacity.boot_sample_s == 0 || raw.capacity.sample_for_s == 0 {
            return Err(invalid("a capacity sample of zero"));
        }
        if raw.universe.max_fee_ppm == 0 {
            return Err(invalid("a universe fee ceiling of zero admits no pool"));
        }
        let a = &raw.armed;
        if a.mode == ArmedMode::Live {
            if command == Command::Shadow {
                return Err(invalid("armed.mode live sends: it runs under `apex canary`, never `apex shadow`"));
            }
            if a.live.is_none() {
                return Err(invalid("armed.mode live needs armed.live: its keys, sequencer, ledger and kill file"));
            }
        }
        for ((name, most), value) in ARMED_CAPS.iter().zip(a.caps()) {
            if value == 0 || value > *most {
                return Err(invalid(format!("armed.{name} {value} is outside 1..={most} wei")));
            }
        }
        if !(1..=64).contains(&a.max_arms)
            || !(1..=crate::armed::threshold::RUNG_OFFSETS.len()).contains(&usize::from(a.rungs))
            || !(2..=50).contains(&a.window_s)
            || a.block_time_ms == 0
            || a.band_cap_bps == 0
            || a.liquidity_floor_bps > 10_000
        {
            return Err(invalid(
                "armed: max_arms 1-64, rungs 1-6, window_s 2-50, block_time_ms > 0, band_cap_bps > 0, liquidity_floor_bps <= 10000",
            ));
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
            race: raw.race,
            flashblocks: raw.flashblocks,
            armed: raw.armed,
            executor: raw.executor,
            signer: raw.signer,
            policy: raw.policy,
            costs: raw.costs,
            capacity: raw.capacity,
            universe: raw.universe,
            canary: raw.canary,
        })
    }
}
