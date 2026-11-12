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
use crate::denim::{DENIM_BLOCK_MS, PRE_DENIM_BLOCK_MS};
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
    /// R36: absent once Denim ends flashblocks.
    #[serde(default)]
    flashblocks: Option<FlashblocksConfig>,
    #[serde(default)]
    feed: FeedConfig,
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
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UniverseConfig {
    /// The highest fee a pool may charge, in ppm.
    pub max_fee_ppm: u32,
    /// The least a pool must hold, in whole US dollars.
    pub min_depth_usd: u64,
    /// R37: the reviewed v4 hooks (`ops/v4-hooks.base.json`). None: no hooked
    /// v4 pool is admitted.
    #[serde(default)]
    pub v4_hooks: Option<std::path::PathBuf>,
}

impl UniverseConfig {
    pub fn filter(&self) -> UniverseFilter {
        UniverseFilter { max_fee_ppm: self.max_fee_ppm, min_depth_usd: self.min_depth_usd as f64 }
    }

    /// The v4 hooks allowed: the file's, or none.
    pub fn hooks(&self) -> Result<crate::live::v4::HookAllowlist, String> {
        match &self.v4_hooks {
            Some(path) => crate::live::v4::HookAllowlist::load(path),
            None => Ok(crate::live::v4::HookAllowlist::default()),
        }
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
    /// R41: the head loop's reads of slow values (the view, Slipstream's
    /// TWAPs, Aerodrome v2's fees) and armed-live's stops, at most once this
    /// often. 0, the default, is every head, as before Denim.
    #[serde(default)]
    pub head_reads_every_ms: u64,
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

/// R36: what the capture feed subscribes to. Denim ends `pendingLogs`: on
/// that day `pending_logs: false` leaves heads and canonical logs.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    #[serde(default = "yes")]
    pub pending_logs: bool,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self { pending_logs: true }
    }
}

const fn yes() -> bool {
    true
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
    /// 2000 before Denim, 200 after (R41's load check).
    pub block_time_ms: u64,
    /// R41: under Denim, the oldest the newest head's `timestampMs` may be at
    /// a send, in ms. 0 before Denim, whose heads carry no such stamp.
    #[serde(default)]
    pub max_head_lag_ms: u64,
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
    /// R37: the largest USDC arm sent, in USDC's six-decimal units; about
    /// `max_size_wei`'s 5 WETH.
    #[serde(default = "twelve_thousand_usdc")]
    pub max_size_usdc: u128,
    /// R36: a bound on one transaction's L1 data fee, in its worst case.
    #[serde(default = "fifty_microeth")]
    pub l1_fee_bound_wei: u128,
    /// R36: left in every sender after its worst case.
    #[serde(default = "two_hundred_microeth")]
    pub sender_reserve_wei: u128,
    /// R36: where armed-live's parts are. Its caps are the ones above.
    #[serde(default)]
    pub live: Option<ArmedLiveConfig>,
    /// R36: where the ranking's move statistics are saved and reloaded, so a
    /// restart does not begin them again. None: learned in memory alone.
    #[serde(default)]
    pub moves: Option<PathBuf>,
    /// R37: `LeanExecutor2`, which every arm is then built for. Unset, the
    /// arming engine builds for v1 and leaves USDC and v4 routes unarmed.
    #[serde(default)]
    pub executor2: Option<Executor2Config>,
    /// R37: the fires `apex armed-eval` simulates a run, a fixed even sample;
    /// every fire is still quoted exactly. Each simulation replays its fire's
    /// whole block: 3,261 in four hours spent BlockPI's prepaid units
    /// (2026-10-29). At a run every five minutes, 2 is at most 576 a day.
    #[serde(default = "two_simulations")]
    pub eval_simulations_per_run: usize,
    /// R38: the rule whose bid an arm sends; the other's rides along to be
    /// judged. `best_share` sends only on a contest sample the census passed,
    /// and falls back to `best_bid` otherwise (ops/host/ARMED.md).
    #[serde(default)]
    pub bid_model: crate::armed::bid::BidModel,
    /// R40.1: the sized lane, `LeanExecutor3`'s, beside this one: in shadow
    /// until it is flipped live, and then this one goes to shadow.
    #[serde(default)]
    pub sized: Option<SizedConfig>,
}

const fn two_simulations() -> usize {
    2
}

/// R40.1: the two arming lanes: R39's fixed-size one, and the sized one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Fixed,
    Sized,
}

/// R40.1: the sized lane. Its caps, budget and senders' keys are `armed:`'s;
/// what is its own is here.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizedConfig {
    pub mode: ArmedMode,
    pub ledger: PathBuf,
    pub verdicts: PathBuf,
    pub executor3: Executor2Config,
    /// 16, then 32, then 64 (the ramp).
    #[serde(default = "sixteen")]
    pub max_senders: usize,
    /// The widest band the engine may choose, of 5, 10 and 20 steps.
    #[serde(default = "twenty")]
    pub band_cap_bps: u32,
    #[serde(default = "ten")]
    pub window_s: u64,
    /// Tier 2 sits where the surplus reaches this many times tier 1's
    /// reference, and only with at least twice tier 1's bid.
    #[serde(default = "ten_u32")]
    pub tier2_multiple: u32,
    /// The executor's `minNet`: the least a trade may net, in the start token.
    #[serde(default = "one")]
    pub min_net_wei: u64,
    /// The plan's `maxSteps`: 1 to 16.
    #[serde(default = "eight")]
    pub max_steps: u8,
    /// The priors the expected net is charged with, in parts per million of
    /// fires: a `NoTrade`, and a revert. Live data replaces them.
    #[serde(default = "two_percent_ppm")]
    pub p_no_trade_ppm: u32,
    #[serde(default)]
    pub p_revert_ppm: u32,
    /// Sends a second at most, from Task 19; 0 is no limit.
    #[serde(default)]
    pub send_rate_per_s: u32,
    /// R43: the tiers for large moves, smallest first, at most
    /// [`MAX_SPIKE_TIERS`]. Each holds its own senders, the last of
    /// `max_senders`, in the tiers' order: with 16 senders and tiers of 6 and
    /// 2, slots 8 to 13 and 14 to 15. None is R40.1's lane.
    #[serde(default)]
    pub tiers: Vec<SpikeTierConfig>,
    /// R43: what a pending arm holds of the day's budget: `worst_case`, its
    /// gas limit at its fee, or `no_trade`, what an inclusion that does not
    /// trade costs.
    #[serde(default)]
    pub reserve: crate::armed::sized::Reserve,
    /// R43: the lane's own write-ahead live ledger, for when it sends beside
    /// R39's: two workers never share one.
    #[serde(default)]
    pub live_ledger: Option<PathBuf>,
    /// R43: the lane's own day's loss budget; `armed.daily_loss_budget_wei`
    /// when unset. Its engine fills against it, and its worker holds to it.
    #[serde(default)]
    pub daily_loss_budget_wei: Option<u128>,
}

/// R43: the most spike tiers a lane holds.
pub const MAX_SPIKE_TIERS: usize = 3;
/// R43: the most of its net a spike tier may tip: half.
pub const MAX_BID_SHARE_BPS: u32 = 5_000;

/// R43: one spike tier (`armed.sized.tiers`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpikeTierConfig {
    /// What a move must open before any tip, in wei, for the tier to trade it.
    pub least_net_wei: u128,
    /// The share of that net the tier tips, in basis points.
    pub bid_share_bps: u32,
    /// The tier's own priority cap: a sender is funded for the lane's gas
    /// limit at this, plus twice the base fee.
    pub max_priority_wei_per_gas: u128,
    /// The senders held for the tier.
    pub senders: usize,
    /// The most its plan may size to; `armed.max_size_wei` and
    /// `armed.max_size_usdc` when unset.
    #[serde(default)]
    pub max_size_wei: Option<u128>,
    #[serde(default)]
    pub max_size_usdc: Option<u128>,
}

impl SizedConfig {
    /// R43: the tiers as the engine and the live worker take them, each
    /// unset size cap `armed`'s.
    pub fn spike_tiers(&self, a: &ArmedConfig) -> Vec<crate::armed::threshold::SpikeTier> {
        self.tiers
            .iter()
            .map(|t| crate::armed::threshold::SpikeTier {
                least_net_wei: t.least_net_wei,
                bid_share_bps: t.bid_share_bps,
                cap_wei_per_gas: t.max_priority_wei_per_gas,
                senders: t.senders,
                max_size_wei: t.max_size_wei.unwrap_or(a.max_size_wei),
                max_size_usdc: t.max_size_usdc.unwrap_or(a.max_size_usdc),
            })
            .collect()
    }
}

const fn sixteen() -> usize {
    16
}

const fn twenty() -> u32 {
    20
}

const fn ten() -> u64 {
    10
}

const fn ten_u32() -> u32 {
    10
}

const fn one() -> u64 {
    1
}

const fn eight() -> u8 {
    8
}

const fn two_percent_ppm() -> u32 {
    20_000
}

/// R37: whether `found`, the hash of `LeanExecutor2`'s code at boot (the
/// chain's, or `runtime_code`'s before the deploy), is the configured one.
pub fn check_executor2_hash(e: &Executor2Config, found: B256) -> Result<(), String> {
    check_executor_hash("armed.executor2", e, found)
}

/// R40.1: the same, for `LeanExecutor3` under `armed.sized.executor3`.
pub fn check_executor3_hash(e: &Executor2Config, found: B256) -> Result<(), String> {
    check_executor_hash("armed.sized.executor3", e, found)
}

fn check_executor_hash(name: &str, e: &Executor2Config, found: B256) -> Result<(), String> {
    if found == keccak256([]) {
        return Err(format!("{name} {:#x} has no code", e.address));
    }
    if found != e.code_hash {
        return Err(format!("{name} {:#x}'s code hashes to {found:#x}, not its code_hash {:#x}", e.address, e.code_hash));
    }
    Ok(())
}

/// R37: `LeanExecutor2`'s deployment.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executor2Config {
    pub address: Address,
    /// The deployed runtime code's keccak.
    pub code_hash: B256,
    /// The runtime code's hex (`forge inspect LeanExecutor2
    /// deployedBytecode`), for simulating against it before it is deployed
    /// (Task 14). Never sent.
    #[serde(default)]
    pub runtime_code: Option<PathBuf>,
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

const fn twelve_thousand_usdc() -> u128 {
    12_000_000_000
}

/// R37: `max_size_usdc`'s ceiling, as `ARMED_CAPS` holds the wei caps.
pub const MAX_SIZE_USDC_CEILING: u128 = 100_000_000_000;

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
    /// Base's flashblock stream and its ledger (R25); none after Denim.
    pub flashblocks: Option<FlashblocksConfig>,
    /// R36: the capture feed's subscriptions.
    pub feed: FeedConfig,
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

/// R40.1: the sized lane's section. Only one lane sends; its ranges are the
/// plan's; its executor is not R39's.
fn sized(a: &ArmedConfig, sz: &SizedConfig, command: Command) -> Result<(), ShadowConfigError> {
    if sz.mode == ArmedMode::Live {
        // R43: both send only with the sized lane on its spike tiers alone,
        // from senders that are all a tier's, into a ledger of its own.
        let spike_only = !sz.tiers.is_empty() && sz.tiers.iter().map(|t| t.senders).sum::<usize>() == sz.max_senders;
        if a.mode == ArmedMode::Live && !(spike_only && sz.live_ledger.is_some()) {
            return Err(invalid(
                "armed.mode and armed.sized.mode are both live: that needs armed.sized.live_ledger, and armed.sized.tiers holding every one of armed.sized.max_senders",
            ));
        }
        if a.mode == ArmedMode::Live && sz.live_ledger.as_ref() == a.live.as_ref().map(|l| &l.ledger) {
            return Err(invalid("armed.sized.live_ledger is armed.live.ledger: each lane's worker writes its own"));
        }
        if command == Command::Shadow {
            return Err(invalid("armed.sized.mode live sends: it runs under `apex canary`, never `apex shadow`"));
        }
        if a.live.is_none() {
            return Err(invalid("armed.sized.mode live needs armed.live: its keys, sequencer, ledger and kill file"));
        }
        if sz.executor3.runtime_code.is_some() {
            return Err(invalid("armed.sized.executor3.runtime_code is the pre-deploy shadow's: live sends go to a deployed LeanExecutor3"));
        }
    }
    if a.executor2.as_ref().is_some_and(|e| e.address == sz.executor3.address) {
        return Err(invalid("armed.sized.executor3 is armed.executor2's address: LeanExecutor3 is its own contract"));
    }
    if !(1..=64).contains(&sz.max_senders)
        || !(1..=20).contains(&sz.band_cap_bps)
        || !(2..=50).contains(&sz.window_s)
        || !(1..=apex_exec::lean3::MAX_STEPS_LIMIT).contains(&sz.max_steps)
        || sz.tier2_multiple < 2
        || sz.p_no_trade_ppm > 1_000_000
        || sz.p_revert_ppm > 1_000_000
    {
        return Err(invalid(
            "armed.sized: max_senders 1-64, band_cap_bps 1-20, window_s 2-50, max_steps 1-16, tier2_multiple >= 2, priors <= 1000000 ppm",
        ));
    }
    if let Some(budget) = sz.daily_loss_budget_wei {
        let most = ARMED_CAPS.iter().find(|(n, _)| *n == "daily_loss_budget_wei").map_or(0, |(_, most)| *most);
        if budget == 0 || budget > most {
            return Err(invalid(format!("armed.sized.daily_loss_budget_wei {budget} is outside 1..={most} wei")));
        }
    }
    spike_tiers(a, sz)
}

/// R43: the spike tiers: each within the ceilings `armed`'s own caps are held
/// to, rising in what they wait for and never falling in what they may bid,
/// on senders the lane has.
fn spike_tiers(a: &ArmedConfig, sz: &SizedConfig) -> Result<(), ShadowConfigError> {
    let ceiling = |name: &str| ARMED_CAPS.iter().find(|(n, _)| *n == name).map_or(0, |(_, most)| *most);
    if sz.tiers.len() > MAX_SPIKE_TIERS {
        return Err(invalid(format!("armed.sized.tiers: {} tiers, at most {MAX_SPIKE_TIERS}", sz.tiers.len())));
    }
    for (i, t) in sz.spike_tiers(a).iter().enumerate() {
        let at = format!("armed.sized.tiers[{i}]");
        if t.least_net_wei == 0 || t.senders == 0 || !(1..=MAX_BID_SHARE_BPS).contains(&t.bid_share_bps) {
            return Err(invalid(format!("{at}: least_net_wei > 0, senders >= 1, bid_share_bps 1-{MAX_BID_SHARE_BPS}")));
        }
        let most = ceiling("max_priority_wei_per_gas");
        if t.cap_wei_per_gas == 0 || t.cap_wei_per_gas > most {
            return Err(invalid(format!("{at}.max_priority_wei_per_gas {} is outside 1..={most} wei", t.cap_wei_per_gas)));
        }
        let most = ceiling("max_size_wei");
        if t.max_size_wei == 0 || t.max_size_wei > most {
            return Err(invalid(format!("{at}.max_size_wei {} is outside 1..={most} wei", t.max_size_wei)));
        }
        if t.max_size_usdc == 0 || t.max_size_usdc > MAX_SIZE_USDC_CEILING {
            return Err(invalid(format!("{at}.max_size_usdc {} is outside 1..={MAX_SIZE_USDC_CEILING} USDC units", t.max_size_usdc)));
        }
    }
    let tiers = sz.spike_tiers(a);
    if tiers.windows(2).any(|w| w[1].least_net_wei <= w[0].least_net_wei || w[1].cap_wei_per_gas < w[0].cap_wei_per_gas) {
        return Err(invalid("armed.sized.tiers: each tier's least_net_wei is above the one before, and its cap is no lower"));
    }
    let held: usize = tiers.iter().map(|t| t.senders).sum();
    if held > sz.max_senders {
        return Err(invalid(format!("armed.sized.tiers hold {held} senders, and armed.sized.max_senders is {}", sz.max_senders)));
    }
    Ok(())
}

/// R41: one regime or the other, never half of each. Base's 2 s blocks keep
/// `pendingLogs` and have no stamp to measure lag by; Denim's 200 ms blocks
/// have neither `pendingLogs` nor flashblocks, and a bounded head lag.
fn regime(raw: &Raw) -> Result<(), ShadowConfigError> {
    let a = &raw.armed;
    match a.block_time_ms {
        DENIM_BLOCK_MS => {
            if raw.feed.pending_logs {
                return Err(invalid("feed.pending_logs must be false at 200 ms: Denim ends pendingLogs"));
            }
            if raw.flashblocks.is_some() {
                return Err(invalid("the flashblocks section must be absent at 200 ms: Denim ends flashblocks"));
            }
            if !(1..=5_000).contains(&a.max_head_lag_ms) {
                return Err(invalid(format!("armed.max_head_lag_ms {} is outside 1-5000 at 200 ms", a.max_head_lag_ms)));
            }
        }
        PRE_DENIM_BLOCK_MS => {
            if !raw.feed.pending_logs {
                return Err(invalid("feed.pending_logs must be true at 2000 ms"));
            }
            if a.max_head_lag_ms != 0 {
                return Err(invalid("armed.max_head_lag_ms must be 0 at 2000 ms: pre-fork heads carry no timestampMs"));
            }
        }
        other => return Err(invalid(format!("armed.block_time_ms {other}: 2000 before Denim, 200 after"))),
    }
    let reads = raw.policy.head_reads_every_ms;
    if reads != 0 && !(1_000..=10_000).contains(&reads) {
        return Err(invalid(format!("policy.head_reads_every_ms {reads}: 0, or 1000-10000")));
    }
    Ok(())
}

impl ShadowConfig {
    /// R41: a Denim configuration: 200 ms blocks. Every path whose behaviour
    /// depends on the regime asks this.
    pub const fn denim(&self) -> bool {
        self.armed.block_time_ms == DENIM_BLOCK_MS
    }

    /// R37: the executor the arming engine builds for, and armed-live sends
    /// to: `LeanExecutor2` when `armed.executor2` is set, else v1.
    pub fn armed_executor(&self) -> Address {
        match (self.live_lane(), &self.armed.sized) {
            (Some(Lane::Sized), Some(s)) => s.executor3.address,
            _ => self.armed.executor2.as_ref().map_or(self.executor.address, |e| e.address),
        }
    }

    /// R40.1: which lane's lines go to armed-live: R39's, the sized lane's,
    /// or neither. R43: with both live it is R39's, and the sized lane has a
    /// worker of its own ([`Self::sized_beside`]).
    pub fn live_lane(&self) -> Option<Lane> {
        if self.armed.mode == ArmedMode::Live {
            Some(Lane::Fixed)
        } else if self.armed.sized.as_ref().is_some_and(|s| s.mode == ArmedMode::Live) {
            Some(Lane::Sized)
        } else {
            None
        }
    }

    /// R43: the sized lane, when it sends beside R39's: its spike tiers, from
    /// the keys after R39's, to its own executor and ledger.
    pub fn sized_beside(&self) -> Option<&SizedConfig> {
        self.armed.sized.as_ref().filter(|s| s.mode == ArmedMode::Live && self.armed.mode == ArmedMode::Live)
    }

    /// R43: the day's loss budget the sized lane fills against and is held
    /// to: its own, or `armed`'s.
    pub fn sized_budget_wei(&self) -> u128 {
        self.armed.sized.as_ref().and_then(|s| s.daily_loss_budget_wei).unwrap_or(self.armed.daily_loss_budget_wei)
    }

    /// R40.1: the senders the live lane arms with: one key each.
    pub fn live_senders(&self) -> usize {
        match (self.live_lane(), &self.armed.sized) {
            (Some(Lane::Sized), Some(s)) => s.max_senders,
            _ => self.armed.max_arms,
        }
    }

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
        if let Some(sz) = &a.sized {
            sized(a, sz, command)?;
        }
        if a.mode == ArmedMode::Live {
            if command == Command::Shadow {
                return Err(invalid("armed.mode live sends: it runs under `apex canary`, never `apex shadow`"));
            }
            if a.live.is_none() {
                return Err(invalid("armed.mode live needs armed.live: its keys, sequencer, ledger and kill file"));
            }
            if a.executor2.as_ref().is_some_and(|e| e.runtime_code.is_some()) {
                return Err(invalid(
                    "armed.executor2.runtime_code is the pre-deploy shadow's: live sends go to a deployed LeanExecutor2 (ops/host/ARMED.md §3c)",
                ));
            }
        }
        for ((name, most), value) in ARMED_CAPS.iter().zip(a.caps()) {
            if value == 0 || value > *most {
                return Err(invalid(format!("armed.{name} {value} is outside 1..={most} wei")));
            }
        }
        if a.max_size_usdc == 0 || a.max_size_usdc > MAX_SIZE_USDC_CEILING {
            return Err(invalid(format!("armed.max_size_usdc {} is outside 1..={MAX_SIZE_USDC_CEILING} USDC units", a.max_size_usdc)));
        }
        if !(1..=64).contains(&a.max_arms)
            || !(1..=crate::armed::threshold::RUNG_OFFSETS.len()).contains(&usize::from(a.rungs))
            || !(2..=50).contains(&a.window_s)
            || a.band_cap_bps == 0
            || a.liquidity_floor_bps > 10_000
        {
            return Err(invalid(
                "armed: max_arms 1-64, rungs 1-6, window_s 2-50, band_cap_bps > 0, liquidity_floor_bps <= 10000",
            ));
        }
        regime(&raw)?;
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
            feed: raw.feed,
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
