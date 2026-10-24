//! `apex shadow` (Task 8.5 R9): the whole capture path over Base, with the
//! null dispatcher as its only lane.
//!
//! # What runs
//!
//! Every stage is the production one — the feed, the pool book, Engine C over
//! the live frontier, `LiveEconomics`, Tier 2 simulation against the chain, the
//! risk gate, last-mile revalidation against a view read each block, and the
//! signature — and the transaction goes to §16.1's null dispatcher, which
//! records it and sends nothing (`DispatchLane::Shadow`). INV-17's route clause
//! is waived on this lane and named on every outcome (`tests/shadow.rs`).
//!
//! | Task | Does | Why |
//! |---|---|---|
//! | feed | `newHeads`, `pendingLogs` and `logs` over the universe's pools → the book, a flashblock's swaps at once → one `PendingSwap` per flashblock on the fast lane | R1, R3, R17 |
//! | capture | each event → `Plane::on_event`, **only while the book is `Verified`** (INV-08) | the run itself |
//! | reload | one reload at a time, at the newest head, requests coalesced; a pool still off its ladder is asked for again next block; a full reload re-admits the universe and refreshes the admissions | R3, R9 |
//! | heads | each head: the live reader's view, Slipstream's TWAPs, and the costs — the base fee each block, the L1 oracle every `l1_every_blocks` | R6, R8, R9 |
//! | capacity | a flashblock sample at boot, then one every `sample_every_s`, into the recorder behind the chain adapter | R5 |
//! | full reload | a full reload every `full_reload_every_s`: pools a failed read removed come back, and the admissions' evidence stays inside its bound | R9 |
//! | report | the funnel, capture assurance and every counter, logged and appended to `report`; the miss ledger drained into `misses` | R9 |
//! | race | one line per handled proposal, with every stage's time, to `race` (a file per UTC day), off the capture path | R25 |
//! | flashblocks | Base's public flashblock stream, as it reached this host, to `flashblocks.ledger`: the clock the race ledger is measured against | R25 |
//! | armed | pass after pass, the arming engine over the verified book: the best routes' gaps armed as validity transactions would be, recorded to `armed.ledger`, never sent | R26 |
//!
//! # What it proves, and what it cannot
//!
//! `SYSTEM_CAPTURE_ASSURANCE` — tickets the null dispatcher accepted before
//! their deadline over tickets the gate authorized — and the funnel that says
//! where everything else went. **Not** that a trade would land or pay: nothing
//! is sent, the dispatcher's answer is `TransportAccepted` and nothing more
//! (INV-34), and the risk gate's route clause is waived.
//!
//! Until the Phase 5 executor is deployed and configured, every Tier 2
//! simulation fails — the configured executor cannot run a `startV2` plan —
//! and is counted as such (`SIM_FAIL/…`); the run's fourteen days start when
//! the configuration names the new executor.

pub mod config;
pub mod funnel;
pub mod ledger;
pub mod nothing_sent;
pub mod queue;
pub mod race;
pub mod reload;

use crate::bus::{EventBus, Lane, Subscription as BusSubscription};
use crate::canary::chain::{CanaryChain, ChainSettings};
use crate::canary::limits::{Canary, CanaryReport, Limits};
use crate::canary::recover::CanaryRecovery;
use crate::canary::send::{CanaryDispatcher, InFlight};
use crate::econ::{ChainCosts, FlashTerms, LiveEconomics, RouteCosts, ScenarioPriors};
use crate::live::abi::{self, selector};
use crate::live::adapter::LiveAdapter;
use crate::live::admission::{self, LiveCommitments};
use crate::live::book::PoolBook;
use crate::live::calls::LiveCallBuilder;
use crate::live::costs::{self, Costs};
use crate::live::feed::{Effect, FeedHandler, BURN, MINT, PANCAKE_SWAP, SWAP, SYNC};
use crate::live::frontier::{self, BALANCER_FLASH, BALANCER_VAULT, WETH};
use crate::live::gas;
use crate::live::near_miss::{NearMissReport, NearMisses};
use crate::live::inventory::{self, PoolSpec, UniverseFilter, Venue};
use crate::live::pricing::LivePricer;
use crate::live::reader::{ChainReader, ReaderConfig};
use crate::live::reads::ChainReads;
use crate::live::sim::{BlockContext, LiveSimulator};
use crate::plane::{DispatchLane, Handled, Plane, Ports};
use crate::risk::LiveRiskGate;
use crate::search::FrontierSearch;
use crate::shutdown::Shutdown;
use crate::sign::{LaneKey, LocalSigner};
use crate::supervise::{spawn_supervised, SupervisedHandle};
use alloy_primitives::{Address, U256};
use apex_capture::clock::{Clock, SystemClock};
use apex_capture::dispatch::NullDispatcher;
use apex_capture::journal::FileJournal;
use apex_capture::recover::DispatchGate;
use apex_capture::registry::TicketRegistry;
use apex_capture::scheduler::CaptureAssurance;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_chain::adapter::ReplacementPolicy;
use apex_chain::base::flashblock::{sample, FlashblockRecorder};
use apex_chain::base::flashblock_stream::{FlashblockIndexer, FlashblockStream, FlashblockStreamStats};
use apex_chain::base::send::{FallbackTransport, HttpSubmitter, RawTransport};
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport, MethodUsage};
use apex_chain::rpc::ws::{Head, LogFilter, Notification, Subscription, WsFeed, WsSettings, WsStats};
use apex_chain::rpc::RpcTransport;
use apex_config::{Env, Secret};
use apex_econ::cost::failure::FailureProfile;
use apex_econ::cost::l1_data::{L1FeeModel, L1FeeParameters};
use apex_risk::breaker::CircuitBreaker;
use apex_risk::posture::PostureLadder;
use apex_search::engine_d::EventEngine;
use apex_state::feed::event::StateEvent;
use apex_state::Versioned;
use apex_types::cost::GasUsed;
use apex_types::flash::CallbackConstraints;
use apex_types::ids::{ChainId, SignerLaneId, StrategyId, SubmissionLaneId, TokenId};
use apex_types::state::ReconstructionStatus;
use apex_types::ticket::TicketOutcome;
use config::ShadowConfig;
use crate::armed::targets::Targets;
use crate::live::frontier::Cycle;
use apex_search::frontier::RouteId;
use std::collections::BTreeMap;
use funnel::{Counts, Funnel};
use nothing_sent::NothingSent;
use queue::{pools_of, PendingQueue};
use reload::{ReloadQueue, Work};
use serde::Serialize;
use serde_json::{json, Value};
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{error, info, warn};

/// Why the run did not start.
#[derive(Debug)]
pub enum ShadowError {
    Config(config::ShadowConfigError),
    Boot(String),
}

impl std::fmt::Display for ShadowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "{e}"),
            Self::Boot(e) => write!(f, "shadow boot: {e}"),
        }
    }
}

impl std::error::Error for ShadowError {}

fn boot_err(what: &str, e: impl std::fmt::Display) -> ShadowError {
    ShadowError::Boot(format!("{what}: {e}"))
}

/// The run's own counters, beside the plane's and the search's.
#[derive(Debug, Default)]
struct Stats {
    reloads_full: AtomicU64,
    reloads_partial: AtomicU64,
    reload_failures: AtomicU64,
    /// Every pool a reload could not read.
    pools_refused: AtomicU64,
    /// Of those, the ones the book held — and so dropped.
    pools_removed: AtomicU64,
    view_failures: AtomicU64,
    twap_failures: AtomicU64,
    /// Aerodrome v2's per-pool fees, re-read each head (R24).
    fee_failures: AtomicU64,
    l1_failures: AtomicU64,
    capacity_samples: AtomicU64,
    flashblocks_seen: AtomicU64,
    capacity_blocks: AtomicU64,
    capacity_refused: AtomicU64,
    misses_written: AtomicU64,
    base_fee_wei: AtomicU64,
    /// R21's queue: proposals dropped as too old, or as sharing a pool with a
    /// ticket just dispatched, and the deepest it got.
    queue_stale: AtomicU64,
    queue_conflicts: AtomicU64,
    queue_max_depth: AtomicU64,
    /// R25: blocks the flashblock ledger could not see whole, and ledger lines
    /// (race or flashblock) that could not be written.
    incomplete_blocks: AtomicU64,
    ledger_failures: AtomicU64,
    /// R26: armed-ledger lines that could not be written.
    armed_write_failures: AtomicU64,
    /// R36: passes whose lines found armed-live's queue full: never sent.
    armed_live_dropped: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

/// A failing read, logged when it starts failing and when it recovers — not
/// every block while it stays down.
#[derive(Default)]
struct Health {
    failing: bool,
}

impl Health {
    fn note<E: std::fmt::Display>(&mut self, what: &str, r: Result<(), E>, count: &AtomicU64) {
        match r {
            Ok(()) if self.failing => {
                self.failing = false;
                info!(what, "recovered");
            }
            Ok(()) => {}
            Err(e) => {
                bump(count);
                if !self.failing {
                    self.failing = true;
                    warn!(what, error = %e, "failing; logged again when it recovers");
                }
            }
        }
    }
}

/// Everything the tasks share.
struct Shadow {
    config: ShadowConfig,
    reads: ChainReads,
    /// The HTTP transport every read goes through, kept for its usage (R28.1).
    http: Arc<FailoverTransport>,
    universe: Vec<Address>,
    /// The venues the executor could reach at boot.
    venues: Vec<Venue>,
    book: Arc<PoolBook>,
    plane: Arc<Plane>,
    search: Arc<FrontierSearch>,
    reader: Arc<ChainReader>,
    adapter: Arc<LiveAdapter>,
    commitments: Arc<LiveCommitments>,
    costs: Costs,
    /// How close the priced routes came to paying (R14).
    near_misses: Arc<NearMisses>,
    null: Arc<NullDispatcher>,
    nothing_sent: Arc<NothingSent>,
    funnel: Funnel,
    reloads: ReloadQueue,
    heads: watch::Sender<Option<Head>>,
    /// The block being built, from the feed: what Tier 2 simulates in (R18).
    building: watch::Sender<Option<BlockContext>>,
    /// What the lender holds, for sizing (R21); read with the reader's view.
    lender: watch::Sender<Option<crate::live::lenders::Lenders>>,
    largest_window: AtomicU64,
    recorder: tokio::sync::Mutex<FlashblockRecorder>,
    feed_stats: Versioned<Option<Arc<WsStats>>>,
    /// The race ledger's two ends (R25): capture records, a worker writes.
    race: race::RaceWriter,
    race_lines: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<race::RaceLine>>,
    flashblock_stats: Versioned<Option<Arc<FlashblockStreamStats>>>,
    /// The signer's pending nonce at the first view, and whether it has moved
    /// since: a shadow never sends, so a move means the key was used elsewhere.
    boot_nonce: OnceLock<u64>,
    nonce_moved: AtomicBool,
    /// R28: the canary, when this run is `apex canary`.
    canary: Option<Arc<Canary>>,
    /// R26: the arming engine (armed-shadow).
    armed: Arc<crate::armed::engine::Engine>,
    /// R36: armed-live's worker, in armed-live; and each pass's lines on their
    /// way to it, with the pass's base fee, in the order they were armed.
    armed_live: Option<Arc<crate::armed::live::ArmedLive>>,
    armed_lines: tokio::sync::mpsc::Sender<(Vec<crate::armed::lines::ArmedLine>, u128)>,
    armed_lines_rx: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<(Vec<crate::armed::lines::ArmedLine>, u128)>>,
    /// R36: the census's targeted pools, admitted below the floor on every
    /// reload too; whether targets were read, their routes, and their pools
    /// with no inventory record.
    target_pools: std::collections::BTreeSet<Address>,
    targets_loaded: bool,
    targeted_routes: u64,
    targets_missing: u64,
    stats: Stats,
    started: Instant,
}

/// R28, R29: the executor pays its profit to the canary's recipient, or the
/// canary does not start.
pub fn recipient_check(paid_to: Option<Address>, recipient: Address) -> Result<(), ShadowError> {
    if paid_to == Some(recipient) {
        return Ok(());
    }
    Err(ShadowError::Boot(format!(
        "the executor pays profit to {paid_to:?}, not canary.profit_recipient {recipient}: the owner sets the recipient first"
    )))
}

/// Whether the signer's nonce differs from its first reading. A shadow never
/// broadcasts, so any move means the key was used elsewhere (R25).
pub fn nonce_moved(boot: Option<u64>, now: u64) -> bool {
    boot.is_some_and(|b| b != now)
}

/// R28: the canary's tripwire. Its own sends move the nonce; a nonce beyond the
/// boot reading plus every send since means the key is in use elsewhere. A view
/// that lags our latest send reads below that, and is not a theft.
pub fn nonce_beyond_ours(boot: u64, now: u64, sent: u64) -> bool {
    now > boot.saturating_add(sent)
}

/// The census's pools that can form a cycle the live frontier prices: pairs
/// with WETH on one side and at least two pools, **counting only `venues`** —
/// the ones the executor can reach. Filtered before pairing, so a pool on an
/// unreachable venue cannot make up a pair's two.
///
/// R36: the census's targeted pools (`include`) are loaded below the depth
/// floor.
pub fn universe(
    dir: &Path,
    venues: &[Venue],
    filter: UniverseFilter,
    include: &std::collections::BTreeSet<Address>,
) -> Result<Vec<PoolSpec>, inventory::InventoryError> {
    let specs: Vec<PoolSpec> = inventory::load(dir, filter, include)?
        .into_iter()
        .filter(|s| venues.contains(&s.venue))
        .collect();
    Ok(inventory::pairs(&specs)
        .into_iter()
        .filter(|((a, b), _)| *a == WETH || *b == WETH)
        .flat_map(|(_, pools)| pools)
        .collect())
}

/// R36: the capture feed's subscriptions: heads, `pendingLogs` until Denim
/// ends it, and canonical logs.
pub fn subscriptions(feed: &config::FeedConfig, filter: LogFilter) -> Vec<Subscription> {
    if feed.pending_logs {
        vec![Subscription::NewHeads, Subscription::PendingLogs(filter.clone()), Subscription::Logs(filter)]
    } else {
        vec![Subscription::NewHeads, Subscription::Logs(filter)]
    }
}

/// R36: the census's targets the arming engine arms from. In live mode a
/// missing, stale or unfit file stops the boot; armed-shadow arms R26's
/// universe instead, and says why.
pub fn load_targets(armed: &config::ArmedConfig, now_unix: u64) -> Result<Option<Targets>, String> {
    let live = armed.mode == config::ArmedMode::Live;
    let refuse = |why: String| {
        if live {
            Err(why)
        } else {
            warn!(%why, "armed-shadow arms R26's universe: no targets");
            Ok(None)
        }
    };
    let Some(path) = &armed.targets else {
        return refuse("armed.targets is not configured".into());
    };
    match Targets::load(path, now_unix, armed.targets_max_age_s) {
        Ok(t) if t.live_ready() => Ok(Some(t)),
        Ok(t) => refuse(format!("{}: not fit to arm (ok {}, {} routes)", path.display(), t.ok, t.routes.len())),
        Err(e) => refuse(e),
    }
}

/// R36: the cycles the arming engine arms: the targeted routes', WETH-in pool
/// first; every cycle without targets (R26).
pub fn armed_cycles(targets: Option<&Targets>, all: &BTreeMap<RouteId, Cycle>) -> BTreeMap<RouteId, Cycle> {
    match targets {
        None => all.clone(),
        Some(t) => all.iter().filter(|(_, c)| t.has(c.legs[0].pool, c.legs[1].pool)).map(|(k, c)| (*k, c.clone())).collect(),
    }
}

/// R36: the targeted pools with no inventory record: never loaded, so never
/// armed. The census can see a pool the factory refresh has not recorded.
pub fn targets_missing(targets: &Targets, specs: &[PoolSpec]) -> usize {
    targets.pools().iter().filter(|p| !specs.iter().any(|s| s.pool == **p)).count()
}

async fn base_fee_at(rpc: &dyn RpcTransport, block: u64) -> Result<u128, ShadowError> {
    let v = rpc
        .call("eth_getBlockByNumber", json!([format!("{block:#x}"), false]))
        .await
        .map_err(|e| boot_err("the head block", e))?;
    v.get("baseFeePerGas")
        .and_then(Value::as_str)
        .and_then(|s| u128::from_str_radix(s.strip_prefix("0x")?, 16).ok())
        .ok_or_else(|| ShadowError::Boot(format!("block {block} has no base fee")))
}

/// Run until `stop` resolves, then drain. Returns the last report.
pub async fn run(config: ShadowConfig, env: &Env, stop: impl Future<Output = ()>) -> Result<Report, ShadowError> {
    let shadow = Arc::new(boot(config, env).await?);
    let shutdown = Shutdown::new();
    let (feed_stop, feed_stopped) = watch::channel(false);
    let mut bus = EventBus::with_capacity(4_096);
    let capture = Arc::new(tokio::sync::Mutex::new(bus.subscribe("capture", Lane::Fast)));
    let bus = Arc::new(bus);

    let mut workers: Vec<SupervisedHandle> = Vec::new();
    let mut spawn = |name: &'static str, f: Box<dyn FnMut() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send>| {
        workers.push(spawn_supervised(name, "base".to_string(), shutdown.subscribe(), f));
    };
    {
        let (s, bus, stopped) = (Arc::clone(&shadow), Arc::clone(&bus), feed_stopped.clone());
        spawn("feed", Box::new(move || {
            let (s, bus, stopped) = (Arc::clone(&s), Arc::clone(&bus), stopped.clone());
            Box::pin(async move { s.feed(&bus, stopped).await })
        }));
    }
    {
        let (s, capture) = (Arc::clone(&shadow), Arc::clone(&capture));
        spawn("capture", Box::new(move || {
            let (s, capture) = (Arc::clone(&s), Arc::clone(&capture));
            Box::pin(async move { s.capture(&mut *capture.lock().await).await })
        }));
    }
    // R36: no stream to open once Denim has ended flashblocks.
    if shadow.config.flashblocks.is_some() {
        let (s, stopped) = (Arc::clone(&shadow), feed_stopped.clone());
        spawn("flashblocks", Box::new(move || {
            let (s, stopped) = (Arc::clone(&s), stopped.clone());
            Box::pin(async move { s.flashblock_ledger(stopped).await })
        }));
    }
    for (name, task) in [
        ("reload", Task::Reload),
        ("heads", Task::Heads),
        ("capacity", Task::Capacity),
        ("full-reload", Task::FullReload),
        ("race", Task::Race),
        ("armed", Task::Armed),
    ] {
        let s = Arc::clone(&shadow);
        spawn(name, Box::new(move || {
            let s = Arc::clone(&s);
            Box::pin(async move { s.task(task).await })
        }));
    }
    // R36: armed-live's sender and its head hook, in armed-live only.
    if shadow.armed_live.is_some() {
        for (name, task) in [("armed-live", Task::ArmedLive), ("armed-heads", Task::ArmedHeads)] {
            let s = Arc::clone(&shadow);
            spawn(name, Box::new(move || {
                let s = Arc::clone(&s);
                Box::pin(async move { s.task(task).await })
            }));
        }
    }
    {
        let (s, bus) = (Arc::clone(&shadow), Arc::clone(&bus));
        spawn("report", Box::new(move || {
            let (s, bus) = (Arc::clone(&s), Arc::clone(&bus));
            Box::pin(async move {
                let mut every = tokio::time::interval(Duration::from_secs(s.config.policy.report_every_s));
                every.tick().await;
                loop {
                    every.tick().await;
                    s.report(&bus);
                }
            })
        }));
    }

    stop.await;
    info!("stopping: the feed first, then every worker");
    let _ = feed_stop.send(true);
    shutdown.stop_workers(&mut workers).await;
    // The race writer was stopped between lines: what it had not reached
    // still goes to the ledger.
    shadow.drain_race_lines();
    Ok(shadow.report(&bus))
}

#[derive(Clone, Copy)]
enum Task {
    Reload,
    Heads,
    Capacity,
    FullReload,
    Race,
    Armed,
    ArmedLive,
    ArmedHeads,
}

async fn boot(config: ShadowConfig, env: &Env) -> Result<Shadow, ShadowError> {
    let started = Instant::now();
    let base = ChainId::BASE;

    // The key: read once, straight into a `Secret`, and held to the address
    // the executor authorizes. A key signing as anyone else would sign tickets
    // the executor refuses.
    let s = &config.signer;
    let key = env
        .get(&s.key_env)
        .ok_or_else(|| ShadowError::Boot(format!("{} is not set", s.key_env)))
        .and_then(|k| LaneKey::from_hex(&Secret::new(k)).map_err(|e| boot_err(&s.key_env, e)))?;
    if key.address() != s.address {
        return Err(ShadowError::Boot(format!(
            "the key in {} signs as {}, and signer.address is {}",
            s.key_env,
            key.address(),
            s.address
        )));
    }

    for path in [&config.journal, &config.report, &config.misses] {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| boot_err(&dir.display().to_string(), e))?;
        }
    }
    let journal = FileJournal::open(&config.journal).map_err(|e| boot_err("the journal", e))?;

    // Every endpoint answers Base's chain id before anything reads.
    let urls: Vec<String> = config.rpc_http.iter().map(|u| u.expose().clone()).collect();
    let http = Arc::new(
        FailoverTransport::connect(&urls, config.chain_id, FailoverSettings::default())
            .await
            .map_err(|e| boot_err("the RPC transport", e))?,
    );
    let rpc: Arc<dyn RpcTransport> = http.clone();
    drop(urls);
    let reads = ChainReads::new(Arc::clone(&rpc));
    let head = reads.head().await.map_err(|e| boot_err("the head", e))?;

    // R29: the lean executor swaps on every venue's pools by its own code; there
    // is no adapter registry to read.
    let venues: Vec<Venue> = Venue::ALL.to_vec();
    // R36: the census's targets, before the inventory: their pools load below
    // the floor.
    let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let targets = load_targets(&config.armed, now_unix).map_err(ShadowError::Boot)?;
    let target_pools = targets.as_ref().map(Targets::pools).unwrap_or_default();
    let specs = universe(&config.inventory, &venues, config.universe.filter(), &target_pools)
        .map_err(|e| boot_err("the inventory", e))?;
    let missing = targets.as_ref().map_or(0, |t| targets_missing(t, &specs));
    if let Some(t) = &targets {
        info!(routes = t.routes.len(), pools = target_pools.len(), missing, generated_at = %t.generated_at, "the census's targets read");
    }
    let (book, unloaded) = PoolBook::load(&reads, &specs, head).await.map_err(|e| boot_err("the pool book", e))?;
    for u in &unloaded {
        warn!(pool = %u.pool, why = ?u.why, "not loaded at boot; the first full reload reads it again");
    }
    let book = Arc::new(book);
    // R26: the armed predicates' storage words, read back at the boot block.
    let unarmable = crate::armed::engine::verify_layouts(&reads, &book.snapshot(), head).await;
    for pool in &unarmable {
        warn!(%pool, "its watched storage words did not read back as loaded: never armed");
    }
    // The reactive lane prices the pools over the floor, as before R36; the
    // targeted pools below it are the arming engine's.
    let everything = book.snapshot();
    let floor = config.universe.min_depth_usd as f64;
    let over_floor: BTreeMap<Address, Arc<crate::live::book::PoolSnapshot>> =
        everything.iter().filter(|(_, p)| p.spec.depth_usd >= floor).map(|(a, p)| (*a, Arc::clone(p))).collect();
    let (frontier, cycles) = frontier::build(base, WETH, &over_floor);
    let armed_set = armed_cycles(targets.as_ref(), &frontier::build(base, WETH, &everything).1);
    if targets.is_some() {
        info!(cycles = armed_set.len(), "the arming engine arms the targeted routes' cycles");
    }
    if cycles.is_empty() {
        return Err(ShadowError::Boot("the book holds no WETH cycle; nothing to search".into()));
    }

    let executor_code = reads.code_hash(config.executor.address, head).await.map_err(|e| boot_err("the executor's code", e))?;
    if executor_code == config.executor.code_hash {
        info!(executor = %config.executor.address, "the executor's code is the configured code");
    } else {
        warn!(
            executor = %config.executor.address,
            found = %executor_code,
            "the executor's code is not the configured code: last-mile check 2 refuses every ticket until it is"
        );
    }
    // R28: a canary trades only once the executor pays its profit where the
    // configuration says. R29: the lean executor has one recipient.
    if let Some(k) = &config.canary {
        let e = config.executor.address;
        let read = reads
            .multicall(&[(e, abi::call0(selector::PROFIT_RECIPIENT))], head)
            .await
            .map_err(|e| boot_err("the executor's recipient", e))?;
        recipient_check(read.first().cloned().flatten().and_then(|d| abi::word_address(&d, 0)), k.profit_recipient)?;
    }

    // Costs, from the chain now; the head task keeps them current.
    let c = &config.costs;
    let chain_costs = ChainCosts {
        gas_price_wei: U256::from(base_fee_at(&*rpc, head).await?),
        l1: costs::read_l1(&reads, head).await.map_err(|e| boot_err("the L1 fee oracle", e))?,
        l1_model: L1FeeModel::unvalidated(),
        failure: FailureProfile { gas_on_failure: GasUsed(c.gas_on_failure), failure_ppm: c.failure_ppm },
    };

    // The lender: Balancer's vault, fee-free on Base, repaid by transfer and
    // not re-entrant during the loan. Its holding as read now; last-mile
    // check 9 reads it again before every signature.
    let holding = reads
        .multicall(&[(WETH, abi::call_address(selector::BALANCE_OF, BALANCER_VAULT))], head)
        .await
        .map_err(|e| boot_err("the lender's holding", e))?
        .first()
        .cloned()
        .flatten()
        .and_then(|d| abi::word_uint(&d, 0, 128))
        .ok_or_else(|| ShadowError::Boot("the lender's holding did not decode".into()))?;
    let terms = FlashTerms {
        provider: BALANCER_FLASH,
        asset: TokenId { chain: base, address: WETH },
        available: U256::from(holding),
        callback: CallbackConstraints { repay_by_transfer: true, reentrancy_permitted: false, max_callback_gas: u64::MAX },
    };
    // R29: Morpho Blue lends directly, repaid by its own pull after the
    // callback; what it can lend is its WETH holding.
    let morpho_held = reads
        .multicall(&[(WETH, abi::call_address(selector::BALANCE_OF, crate::live::lenders::MORPHO_BLUE))], head)
        .await
        .map_err(|e| boot_err("Morpho's holding", e))?
        .first()
        .cloned()
        .flatten()
        .and_then(|d| abi::word_uint(&d, 0, 128))
        .ok_or_else(|| ShadowError::Boot("Morpho's holding did not decode".into()))?;
    info!(morpho_weth = morpho_held, balancer_weth = holding, "the lenders' holdings read");
    let morpho_terms = FlashTerms {
        provider: crate::live::lenders::MORPHO_FLASH,
        available: U256::from(morpho_held),
        callback: CallbackConstraints { repay_by_transfer: false, ..terms.callback },
        ..terms
    };

    // Built before the economics it serves, so it starts at costs no route
    // clears; `Costs::new` aligns it with the economics' before anything prices.
    let unpriced = RouteCosts { other_wei: u128::MAX, wei_per_gas: u128::MAX };
    // R21: what the lender holds, for sizing. Seeded with the boot read; the
    // head loop replaces it after every view the reader takes.
    let (lender, lender_now) = watch::channel(Some(crate::live::lenders::Lenders {
        balancer: ethers_core::types::U256::from(holding),
        morpho: Some(ethers_core::types::U256::from(morpho_held)),
    }));
    let engine_settings = crate::armed::engine::EngineSettings {
            max_arms: config.armed.max_arms,
            rungs: config.armed.rungs,
            window_s: config.armed.window_s,
            block_time_ms: config.armed.block_time_ms,
            band_cap_bps: config.armed.band_cap_bps,
            liquidity_floor_bps: config.armed.liquidity_floor_bps,
            executor: config.executor.address,
            chain_id: base.0,
            reprice_budget: crate::armed::engine::REPRICE_BUDGET,
            reprice_for: crate::armed::engine::REPRICE_FOR,
            workers: crate::armed::engine::workers(),
            max_priority_wei_per_gas: config.armed.max_priority_wei_per_gas,
    };
    let window_blocks = engine_settings.window_blocks();
    let armed = Arc::new(crate::armed::engine::Engine::new(armed_set.clone(), engine_settings, unarmable)
    .with_bids({
        let t = targets.clone();
        Arc::new(move |weth_in, other| t.as_ref().map_or_else(Vec::new, |t| t.bids_for(weth_in, other).to_vec()))
    }));
    // R31: the rivals' bids each candidate's priority fee is chosen against.
    let rival_bids = crate::econ::RivalBids::load(&config.policy.rival_bids)
        .map_err(|e| ShadowError::Boot(format!("policy.rival_bids: {e}")))?;
    info!(
        bids = rival_bids.len(),
        contested_share = rival_bids.contested_share(),
        p25 = %rival_bids.quantile(0.25),
        p50 = %rival_bids.quantile(0.5),
        p75 = %rival_bids.quantile(0.75),
        p90 = %rival_bids.quantile(0.9),
        "the rivals' bids read, wei per gas"
    );
    let pricer =
        Arc::new(LivePricer::new(Arc::clone(&book), cycles.clone(), unpriced, gas::LEAN).with_lender(lender_now));
    let pricer_near_misses = pricer.near_misses();
    let econ = Arc::new(
        {
            let e = LiveEconomics::new(pricer.clone(), ScenarioPriors::default(), chain_costs, StrategyId(1))
                .with_rival_bids(rival_bids)
                .with_flash(terms);
            // The terms' `available` is the boot read, a pricing hint for the
            // risk gate; last-mile check 9 reads the chosen lender live.
            e.with_flash(morpho_terms)
        },
    );
    let costs = Costs::new(Arc::clone(&econ), Arc::clone(&pricer));
    let search = Arc::new(FrontierSearch::new(
        frontier,
        // Every template is over pools the book read and held to their
        // factory and tokens at load.
        ReconstructionStatus::Verified,
        pricer.clone(),
        EventEngine::measured(),
        // Every template a flashblock touches: see `flashblock_engine`.
        crate::search::flashblock_engine(cycles.len()),
    ));

    let (admitted, refused) = admission::admit_book(&book, base, head, &config.universe.filter(), &target_pools);
    for (pool, why) in &refused {
        warn!(%pool, ?why, "not admitted: no route through it can be committed");
    }
    let p = &config.policy;
    let commitments =
        Arc::new(LiveCommitments::new(admitted, p.slippage_bps_per_hop, p.max_evidence_age_blocks));

    let e = &config.executor;
    let reader = Arc::new(ChainReader::new(
        Arc::clone(&rpc),
        Arc::clone(&book),
        ReaderConfig {
            executor: e.address,
            executor_code_hash: e.code_hash,
            plan_version: e.plan_version,
            signer: key.address(),
            lender: BALANCER_VAULT,
            morpho: Some(crate::live::lenders::MORPHO_BLUE),
            loan_token: WETH,
            fee_ceiling_wei: p.fee_ceiling_wei,
            min_profit: p.min_profit_wei,
            max_age: Duration::from_millis(p.max_view_age_ms),
        },
    ));
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&e.plan_version.to_be_bytes());
    let lane = SignerLaneId(s.lane);
    let pool = Arc::new(SignerPool::new(
        ExecutorAuth { chain: base, executor: e.address.into_array(), executor_version: version },
        vec![LaneConfig { id: lane, address: key.address().into_array(), gas_reserve_wei: s.gas_reserve_wei }],
    ));
    let trader = key.address();
    let signer = Arc::new(LocalSigner::new(base).with_lane(lane, key));
    let null = Arc::new(NullDispatcher::new());
    let nothing_sent = Arc::new(NothingSent::default());
    // R28: the canary's pieces, when this is `apex canary`.
    let canary = match &config.canary {
        None => None,
        Some(k) => {
            let c = Arc::new(
                Canary::open(
                    &k.ledger,
                    &k.stop_file,
                    Limits {
                        loss_cap_wei: k.loss_cap_wei,
                        max_consecutive_reverts: k.max_consecutive_reverts,
                        max_consecutive_refusals: k.max_consecutive_refusals,
                        max_landed: k.max_landed,
                    },
                )
                .map_err(|e| boot_err("the canary ledger", e))?,
            );
            let in_flight = Arc::new(InFlight::default());
            let chain = Arc::new(CanaryChain::new(
                Arc::clone(&rpc),
                Arc::clone(&c),
                Arc::clone(&in_flight),
                ChainSettings {
                    weth: WETH,
                    executor: config.executor.address,
                    trader,
                    recipients: vec![k.profit_recipient],
                    poll: Duration::from_millis(k.receipt_poll_ms),
                    timeout: Duration::from_millis(k.settle_timeout_ms),
                },
            ));
            let submitter = |url: &str| {
                HttpSubmitter::new(url, Duration::from_secs(3)).map_err(|e| ShadowError::Boot(format!("the canary's submitter: {e}")))
            };
            let blockpi = submitter(config.rpc_http[0].expose())?;
            // R32.1: the sequencer first, BlockPI whenever it has not taken it.
            let transport: Arc<dyn RawTransport> = match &k.send_url {
                Some(url) => {
                    let first = submitter(url)?;
                    info!(first = first.label(), then = blockpi.label(), "the canary sends through these endpoints");
                    Arc::new(FallbackTransport::new(Arc::new(first), Arc::new(blockpi)))
                }
                None => {
                    info!(endpoint = blockpi.label(), "the canary sends through this endpoint");
                    Arc::new(blockpi)
                }
            };
            let dispatcher = Arc::new(CanaryDispatcher::new(Arc::clone(&c), transport, in_flight));
            Some((c, chain, dispatcher))
        }
    };
    let adapter = Arc::new({
        let a = LiveAdapter::new(
            vec![SubmissionLaneId(1)],
            ReplacementPolicy { supported: true, min_fee_bump_bps: 1_000, max_attempts: 2 },
        );
        match &canary {
            Some((_, chain, _)) => a.with_attribution(Arc::clone(chain) as Arc<dyn crate::live::adapter::Attribution>),
            None => a,
        }
    });
    // The block being built, as the feed has seen it: Tier 2 simulates in it.
    let (building, building_now) = watch::channel(None);
    let plane = Arc::new(Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(Box::new(journal), Box::new(SystemClock))),
        pool,
        gate: Arc::new(DispatchGate::shut()),
        dispatch: match &canary {
            Some((_, _, d)) => DispatchLane::Canary(Arc::clone(d) as Arc<dyn crate::plane::Sender>),
            None => DispatchLane::Shadow(Arc::clone(&null)),
        },
        chain: adapter.clone(),
        search: search.clone(),
        econ,
        sim: Arc::new(LiveSimulator::new(Arc::clone(&rpc), base, building_now, Arc::new(SystemClock))),
        // Nothing records a loss into the breaker, so it never trips here; a
        // canary's limits are its own (R28), and they hold every candidate.
        risk: Arc::new({
            let g = LiveRiskGate::new(
                PostureLadder::new(),
                CircuitBreaker::new(U256::MAX, U256::MAX, u32::MAX),
                config.policy.eligibility(),
                Box::new(SystemClock),
            );
            match &canary {
                Some((c, _, _)) => g.with_hold(Arc::clone(c) as Arc<dyn crate::risk::Hold>),
                None => g,
            }
        }),
        commitments: commitments.clone(),
        calls: Arc::new(LiveCallBuilder::new(Arc::clone(&book), cycles.into_values())),
        signer,
        live: reader.clone(),
        settlement: match &canary {
            Some((_, chain, _)) => Arc::clone(chain) as Arc<dyn crate::plane::SettlementFeed>,
            None => nothing_sent.clone(),
        },
    }));
    let reconciled = match &canary {
        Some((c, chain, _)) => {
            let recovery = CanaryRecovery::build(c, chain).await;
            plane.boot(&recovery, SystemClock.now())
        }
        None => plane.boot(&*nothing_sent, SystemClock.now()),
    }
    .map_err(|e| boot_err("recovery", e))?;
    if let Some((c, _, _)) = &canary {
        info!(halted = ?c.halted(), open = c.open_sends().len(), "the canary's ledger is read");
    }
    // R36: armed-live's worker. The configuration allows the mode only under
    // `apex canary`, with its section; the executor pays the canary's recipient.
    let armed_live = match (config.armed.mode, &config.armed.live, &config.canary) {
        (config::ArmedMode::Live, Some(live), Some(k)) => {
            Some(Arc::new(boot_armed_live(&config, live, k.profit_recipient, &rpc, window_blocks).await?))
        }
        _ => None,
    };
    let (armed_lines, armed_lines_rx) = tokio::sync::mpsc::channel(1_024);

    info!(
        config = %config.version,
        head,
        ?venues,
        pools = book.len(),
        universe = specs.len(),
        cycles = search.resident(),
        admitted = commitments.admitted(),
        reconciled,
        route_other_wei = costs.route_costs().other_wei,
        lender_weth = holding,
        "{} booted",
        if config.canary.is_some() { "canary" } else { "shadow" }
    );
    let (heads, _) = watch::channel(None);
    let (race, race_lines) = race::RaceWriter::new(4_096);
    Ok(Shadow {
        universe: specs.iter().map(|p| p.pool).collect(),
        venues,
        reads,
        http,
        book,
        plane,
        search,
        reader,
        adapter,
        commitments,
        costs,
        near_misses: pricer_near_misses,
        null,
        nothing_sent,
        funnel: Funnel::default(),
        reloads: ReloadQueue::default(),
        heads,
        building,
        lender,
        largest_window: AtomicU64::new(0),
        recorder: tokio::sync::Mutex::new(FlashblockRecorder::new(FlashblockRecorder::DEFAULT_WINDOW)),
        feed_stats: Versioned::new(None, ReconstructionStatus::Rebuilding),
        race,
        race_lines: tokio::sync::Mutex::new(race_lines),
        flashblock_stats: Versioned::new(None, ReconstructionStatus::Rebuilding),
        boot_nonce: OnceLock::new(),
        nonce_moved: AtomicBool::new(false),
        canary: canary.map(|(c, _, _)| c),
        armed,
        armed_live,
        armed_lines,
        armed_lines_rx: tokio::sync::Mutex::new(armed_lines_rx),
        targets_loaded: targets.is_some(),
        targeted_routes: targets.as_ref().map_or(0, |t| t.routes.len() as u64),
        targets_missing: missing as u64,
        target_pools,
        stats: Stats::default(),
        started,
        config,
    })
}

/// R36: armed-live's worker: one sender a slot, each one the executor lets
/// trade, its ledger replayed and every nonce read before the first send.
async fn boot_armed_live(
    config: &ShadowConfig,
    live: &config::ArmedLiveConfig,
    recipient: Address,
    rpc: &Arc<dyn RpcTransport>,
    window_blocks: u64,
) -> Result<crate::armed::live::ArmedLive, ShadowError> {
    use crate::armed::live::{ArmedLive, LivePolicy};
    let keys = crate::armed::keys::load(&live.keys_file).map_err(ShadowError::Boot)?;
    let a = &config.armed;
    if keys.len() != a.max_arms {
        return Err(ShadowError::Boot(format!(
            "{} holds {} keys for {} slots (armed.max_arms): one sender a slot",
            live.keys_file.display(),
            keys.len(),
            a.max_arms
        )));
    }
    let senders: Vec<Address> = keys.iter().map(LaneKey::address).collect();
    crate::armed::live_chain::check_executors(&**rpc, config.executor.address, &senders).await.map_err(ShadowError::Boot)?;
    // The daily restart compacts a long ledger first: the boot replays all of
    // it, and renewals add ~200 records a minute.
    match crate::armed::live_ledger::LiveLedger::compact_file(&live.ledger, now_ms(), a.daily_loss_budget_wei, ARMED_LEDGER_COMPACT_ABOVE) {
        Ok(Some(archive)) => info!(archive = %archive.display(), "armed-live's ledger compacted; the original is kept beside it"),
        Ok(None) => {}
        Err(e) => warn!(error = %e, "armed-live's ledger was not compacted; it is read whole"),
    }
    let (ledger, records) = crate::armed::live_ledger::LiveLedger::open(&live.ledger).map_err(|e| boot_err("the armed live ledger", e))?;
    let submit = apex_chain::base::validity::ValiditySubmitter::new(&live.sequencer, Duration::from_secs(3))
        .map_err(|e| ShadowError::Boot(format!("armed-live's submitter: {e}")))?;
    info!(endpoint = submit.label(), senders = senders.len(), records = records.len(), "armed-live sends validity transactions through this endpoint");
    let policy = LivePolicy {
        budget_wei: a.daily_loss_budget_wei,
        max_priority_wei_per_gas: a.max_priority_wei_per_gas,
        max_size_wei: ethers_core::types::U256::from(a.max_size_wei),
        l1_fee_bound_wei: a.l1_fee_bound_wei,
        sender_reserve_wei: a.sender_reserve_wei,
        window_blocks,
        block_time_ms: a.block_time_ms,
        kill_file: live.kill_file.clone(),
    };
    let chain = Arc::new(crate::armed::live_chain::RpcChain::new(Arc::clone(rpc)));
    let worker = ArmedLive::new(keys, chain, Arc::new(submit), ledger, records, policy, config.executor.address, recipient, config.chain_id);
    worker.begin(now_ms()).await.map_err(|e| ShadowError::Boot(format!("armed-live: {e}")))?;
    info!(ready = worker.ready(), halted = ?worker.state(now_ms()).halted, "armed-live's ledger is read");
    Ok(worker)
}

/// R36: the live ledger's length past which the boot compacts it.
const ARMED_LEDGER_COMPACT_ABOVE: usize = 10_000;

fn now_ms() -> u64 {
    SystemClock.now().0 / 1_000_000
}

impl Shadow {
    /// The capture feed: notifications → the book → effects.
    async fn feed(&self, bus: &EventBus, stopped: watch::Receiver<bool>) {
        let filter = LogFilter { addresses: self.universe.clone(), topics0: vec![SWAP, PANCAKE_SWAP, MINT, BURN, SYNC] };
        let subs = subscriptions(&self.config.feed, filter);
        let feed = match WsFeed::new(self.config.rpc_ws.expose(), subs, WsSettings::default()) {
            Ok(f) => f,
            Err(e) => {
                error!(error = %e, "the capture feed cannot start");
                tokio::time::sleep(Duration::from_secs(10)).await;
                return;
            }
        };
        self.feed_stats.store(Some(feed.stats()), ReconstructionStatus::Verified);
        // Whatever landed between the book's last read and this subscription
        // was not seen: rebuild before anything is priced from it.
        self.book.mark_rebuilding();
        self.reloads.request_full();

        let (task, mut rx) = feed.spawn(stopped);
        let mut handler = FeedHandler::new(ChainId::BASE);
        loop {
            // A burst of swaps is held until the feed has been quiet for
            // `BURST_QUIET`, then applied and published as one state (R17).
            // `timeout` polls the receiver before its timer, so a notification
            // already waiting joins the burst rather than being split from it.
            let next = match handler.quiet_deadline() {
                Some(deadline) => {
                    let wait = Duration::from_nanos(deadline.0.saturating_sub(SystemClock.now().0));
                    match tokio::time::timeout(wait, rx.recv()).await {
                        Ok(n) => n,
                        Err(_) => {
                            self.apply(bus, handler.flush(&self.book));
                            continue;
                        }
                    }
                }
                None => rx.recv().await,
            };
            let Some(n) = next else { break };
            if let Notification::Head(h) = &n {
                self.heads.send_replace(Some(h.clone()));
            }
            self.apply(bus, handler.handle(&self.book, n, SystemClock.now()));
            // Tier 2 simulates in the block being built: published as the feed
            // learns it, so a simulation reads it rather than asking (R18).
            let building = handler.block_being_built();
            self.building.send_if_modified(|now| {
                let moved = *now != building;
                *now = building;
                moved
            });
        }
        // The feed is gone; what it held still happened.
        self.apply(bus, handler.flush(&self.book));
        let _ = task.await;
    }

    fn apply(&self, bus: &EventBus, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Event(e) => bus.publish(*e),
                Effect::Reload(pools) => self.reloads.request(pools),
                Effect::FullReload => self.reloads.request_full(),
            }
        }
    }

    /// Events into the plane, while the book can be priced from — through one
    /// queue, best first (R21; see `queue`).
    async fn capture(&self, sub: &mut BusSubscription) {
        let mut queue = PendingQueue::default();
        loop {
            // Wait only when there is nothing to do.
            if queue.is_empty() {
                let Some(ev) = sub.recv().await else { return };
                self.take(&mut queue, ev).await;
            }
            // Everything already waiting, before choosing.
            while let Some(ev) = sub.try_recv() {
                self.take(&mut queue, ev).await;
            }
            // Priced against a book being rebuilt: dropped, not handled. A route
            // that still pays is proposed again once the book is verified.
            if self.book.status() != ReconstructionStatus::Verified {
                queue.clear();
                continue;
            }
            let now = SystemClock.now();
            let popped = queue.pop_best(now);
            let stale: Vec<Handled> =
                popped.stale.iter().map(|(p, age)| self.plane.decline_stale(&p.proposal, *age)).collect();
            for (p, _) in &popped.stale {
                self.race.record(race::race_line(p, now, now, (race::RaceOutcome::Stale, None)));
            }
            self.stats.queue_stale.fetch_add(stale.len() as u64, Ordering::Relaxed);
            self.funnel.record(&stale);
            self.stats.queue_max_depth.fetch_max(queue.max_depth() as u64, Ordering::Relaxed);
            let Some(best) = popped.best else { continue };
            let handled = self.plane.handle_proposal(&best.event, &best.proposal).await;
            self.race.record(race::race_line(&best, now, SystemClock.now(), race::outcome_of(&handled)));
            if dispatched(&handled) {
                let n = queue.drop_conflicting(&pools_of(&best.proposal));
                self.stats.queue_conflicts.fetch_add(n as u64, Ordering::Relaxed);
            }
            self.funnel.record(std::slice::from_ref(&handled));
        }
    }

    /// One event: counted, then proposed and queued while the book is verified.
    async fn take(&self, queue: &mut PendingQueue, ev: Arc<StateEvent>) {
        let received = SystemClock.now();
        self.funnel.event();
        if self.book.status() != ReconstructionStatus::Verified {
            self.funnel.skipped_unverified();
            return;
        }
        match self.plane.propose(&ev).await {
            Ok(proposals) => queue.merge(&ev, proposals, received, SystemClock.now()),
            Err(handled) => self.funnel.record(std::slice::from_ref(&handled)),
        }
    }

    async fn task(&self, t: Task) {
        match t {
            Task::Reload => self.reload_loop().await,
            Task::Heads => self.head_loop().await,
            Task::Capacity => self.capacity_loop().await,
            Task::Race => self.race_writer().await,
            Task::Armed => self.armed_loop().await,
            Task::ArmedLive => self.armed_live_loop().await,
            Task::ArmedHeads => self.armed_head_loop().await,
            Task::FullReload => {
                let mut every = tokio::time::interval(Duration::from_secs(self.config.policy.full_reload_every_s));
                every.tick().await;
                loop {
                    every.tick().await;
                    self.reloads.request_full();
                }
            }
        }
    }

    /// The newest head above `after`, waiting for one if need be. `None` once
    /// the feed is gone for good.
    async fn head_after(heads: &mut watch::Receiver<Option<Head>>, after: u64) -> Option<u64> {
        loop {
            let newest = heads.borrow_and_update().as_ref().map(|h| h.number);
            if let Some(n) = newest.filter(|n| *n > after) {
                return Some(n);
            }
            heads.changed().await.ok()?;
        }
    }

    async fn reload_loop(&self) {
        let mut heads = self.heads.subscribe();
        let mut last = 0u64;
        loop {
            let work = self.reloads.next().await;
            let Some(block) = Self::head_after(&mut heads, last).await else { return };
            last = block;
            let pools = match &work {
                Work::Full => Vec::new(),
                Work::Pools(p) => p.clone(),
            };
            let held = self.book.snapshot();
            match self.book.reload(&self.reads, &pools, block).await {
                Ok(refused) => {
                    bump(if work == Work::Full { &self.stats.reloads_full } else { &self.stats.reloads_partial });
                    for u in &refused {
                        bump(&self.stats.pools_refused);
                        if held.contains_key(&u.pool) {
                            bump(&self.stats.pools_removed);
                            warn!(pool = %u.pool, why = ?u.why, block, "removed on reload; the next full reload reads it again");
                        } else {
                            info!(pool = %u.pool, why = ?u.why, block, "still unreadable");
                        }
                    }
                    if work == Work::Full {
                        let (admitted, refused) =
                            admission::admit_book(&self.book, ChainId::BASE, block, &self.config.universe.filter(), &self.target_pools);
                        for (pool, why) in &refused {
                            warn!(%pool, ?why, block, "not admitted on reload");
                        }
                        self.commitments.set_admissions(admitted);
                    }
                    // Read before a swap it already holds: off its fresh ladder
                    // until a later block is read. And read before a position
                    // change it could not apply exactly (R28.1): not the
                    // chain's until a read at or after it.
                    let mut off: Vec<Address> = self
                        .book
                        .snapshot()
                        .values()
                        .filter(|p| !p.ladder_covers_price())
                        .map(|p| p.spec.pool)
                        .collect();
                    off.extend(self.book.positions_unconfirmed());
                    self.reloads.request(off);
                }
                Err(e) => {
                    bump(&self.stats.reload_failures);
                    warn!(error = %e, block, "reload failed; asked for again at the next head");
                    match work {
                        Work::Full => self.reloads.request_full(),
                        Work::Pools(p) => self.reloads.request(p),
                    }
                }
            }
        }
    }

    async fn head_loop(&self) {
        let mut heads = self.heads.subscribe();
        let (mut view, mut twap, mut l1_health) = (Health::default(), Health::default(), Health::default());
        let mut fees = Health::default();
        let mut l1: Option<(u64, L1FeeParameters)> = None;
        let every = self.config.policy.l1_every_blocks;
        loop {
            if heads.changed().await.is_err() {
                return;
            }
            let Some(h) = heads.borrow_and_update().clone() else { continue };
            let window = self.largest_window.load(Ordering::Relaxed);
            view.note("the live reader's view", self.reader.refresh(&h, window).await, &self.stats.view_failures);
            if let Some(v) = self.reader.view() {
                let boot = *self.boot_nonce.get_or_init(|| v.signer_pending_nonce);
                if let Some(c) = &self.canary {
                    // R28: the canary's own sends move the nonce; only a move
                    // beyond them means the key is in use elsewhere.
                    let sent = c.sent_since_boot();
                    if nonce_beyond_ours(boot, v.signer_pending_nonce, sent) {
                        self.nonce_moved.store(true, Ordering::Relaxed);
                        let _ = c.halt(crate::canary::Halt::Tripwire { boot, now: v.signer_pending_nonce, sent });
                    }
                } else if nonce_moved(Some(boot), v.signer_pending_nonce) {
                    self.nonce_moved.store(true, Ordering::Relaxed);
                    error!(
                        boot,
                        now = v.signer_pending_nonce,
                        "the signer's nonce moved: a shadow never sends, so the trader key was used elsewhere; revoke it (ops/host/README.md)"
                    );
                }
                let held = Some(crate::live::lenders::Lenders {
                    balancer: ethers_core::types::U256::from(v.lender_holding),
                    morpho: Some(ethers_core::types::U256::from(v.morpho_holding)),
                });
                self.lender.send_if_modified(|now| {
                    let moved = *now != held;
                    *now = held;
                    moved
                });
            }
            twap.note("Slipstream's TWAPs", self.book.refresh_twaps(&self.reads, h.number).await.map(|_| ()), &self.stats.twap_failures);
            fees.note(
                "Aerodrome v2's fees",
                self.book.refresh_cp_fees(&self.reads, h.number).await.map(|_| ()),
                &self.stats.fee_failures,
            );
            if l1.is_none_or(|(at, _)| h.number >= at.saturating_add(every)) {
                let read = costs::read_l1(&self.reads, h.number).await;
                if let Ok(p) = &read {
                    l1 = Some((h.number, *p));
                }
                l1_health.note("the L1 fee oracle", read.map(|_| ()), &self.stats.l1_failures);
            }
            if let (Some(fee), Some((_, p))) = (h.base_fee_per_gas, l1) {
                self.costs.set(fee, p);
                self.stats.base_fee_wei.store(u64::try_from(fee).unwrap_or(u64::MAX), Ordering::Relaxed);
            }
        }
    }

    /// R26: armed-shadow. The engine's pass over the verified book, off the
    /// async threads; its lines to the armed ledger. A pass that left slots
    /// waiting is followed at once by the next (R27.1).
    async fn armed_loop(&self) {
        let mut waiting = 0;
        loop {
            // Nothing is priced on the paths that skip a pass: they pause.
            tokio::time::sleep(crate::armed::engine::pause_after(std::mem::take(&mut waiting))).await;
            if self.book.status() != ReconstructionStatus::Verified {
                continue;
            }
            let Some(head) = self.heads.borrow().clone() else { continue };
            let px = crate::armed::threshold::Pricing {
                costs: self.costs.route_costs(),
                gas: gas::LEAN,
                lenders: *self.lender.borrow(),
                margin_wei: ethers_core::types::U256::from(self.config.armed.margin_wei),
            };
            let (engine, snapshot) = (Arc::clone(&self.armed), self.book.snapshot());
            let base_fee = head.base_fee_per_gas.unwrap_or(0);
            let now = SystemClock.now().0;
            let (lines, left) = tokio::task::spawn_blocking(move || {
                let lines = engine.tick(&snapshot, head.number, base_fee, &px, now);
                (lines, engine.stats().stale_waiting)
            })
            .await
            .unwrap_or_default();
            waiting = left;
            for line in &lines {
                if ledger::append_line(&self.config.armed.ledger, now, line).is_err() {
                    bump(&self.stats.armed_write_failures);
                }
            }
            if self.armed_live.is_some() && !lines.is_empty() && self.armed_lines.try_send((lines, base_fee)).is_err() {
                bump(&self.stats.armed_live_dropped);
            }
        }
    }

    /// R36: armed-live's sender: each pass's lines, in order, to the worker,
    /// off the engine's loop so a pass never waits on a send.
    async fn armed_live_loop(&self) {
        let Some(live) = &self.armed_live else { return };
        let mut rx = self.armed_lines_rx.lock().await;
        while let Some((lines, base_fee)) = rx.recv().await {
            live.on_lines(&lines, base_fee, now_ms()).await;
        }
    }

    /// R36: armed-live's head hook: the cadence, and what came of every
    /// pending transaction whose bound a head has passed.
    async fn armed_head_loop(&self) {
        let Some(live) = &self.armed_live else { return };
        let mut heads = self.heads.subscribe();
        while heads.changed().await.is_ok() {
            let number = heads.borrow_and_update().as_ref().map(|h| h.number);
            if let Some(number) = number {
                let now = now_ms();
                live.on_head(number, now, now).await;
            }
        }
    }

    /// The race ledger's writer: off the capture path, a failed write counted.
    async fn race_writer(&self) {
        let mut rx = self.race_lines.lock().await;
        while let Some(line) = rx.recv().await {
            self.write_race_line(&line);
        }
    }

    fn write_race_line(&self, line: &race::RaceLine) {
        if ledger::append_line(&self.config.race, line.handled_ns, line).is_err() {
            bump(&self.stats.ledger_failures);
        }
    }

    /// Whatever the race writer had not reached when it was stopped.
    fn drain_race_lines(&self) {
        if let Ok(mut rx) = self.race_lines.try_lock() {
            while let Ok(line) = rx.try_recv() {
                self.write_race_line(&line);
            }
        }
    }

    /// Base's public flashblock stream, as it reached this host, to its ledger:
    /// the clock the race ledger is measured against (R25). Measurement only.
    async fn flashblock_ledger(&self, stop: watch::Receiver<bool>) {
        let Some(fb) = &self.config.flashblocks else { return };
        let stream = FlashblockStream::new(&fb.stream);
        self.flashblock_stats.store(Some(stream.stats()), ReconstructionStatus::Verified);
        let (task, mut rx) = stream.spawn(stop);
        let mut ix = FlashblockIndexer::new();
        while let Some((f, at)) = rx.recv().await {
            if let Some(seen) = ix.observe(&f, at) {
                if ledger::append_line(&fb.ledger, at, &seen).is_err() {
                    bump(&self.stats.ledger_failures);
                }
            }
            self.stats.incomplete_blocks.store(ix.incomplete_blocks(), Ordering::Relaxed);
        }
        let _ = task.await;
    }

    async fn capacity_loop(&self) {
        let c = &self.config.capacity;
        let mut duration = Duration::from_secs(c.boot_sample_s);
        loop {
            let modelled;
            {
                let mut rec = self.recorder.lock().await;
                match sample(self.config.rpc_ws.expose(), WsSettings::default(), duration, &mut rec).await {
                    Ok(seen) => {
                        bump(&self.stats.capacity_samples);
                        self.stats.flashblocks_seen.fetch_add(seen, Ordering::Relaxed);
                    }
                    Err(e) => warn!(error = %e, "flashblock sample failed"),
                }
                modelled = match rec.model() {
                    Ok(model) => {
                        self.largest_window.store(model.largest_measured_window(), Ordering::Relaxed);
                        self.adapter.set_capacity(model);
                        true
                    }
                    Err(e) => {
                        info!(error = ?e, "no capacity model yet; every submission is refused until one");
                        false
                    }
                };
                self.stats.capacity_blocks.store(rec.blocks() as u64, Ordering::Relaxed);
                self.stats.capacity_refused.store(rec.refused(), Ordering::Relaxed);
            }
            duration = Duration::from_secs(c.sample_for_s);
            // Without a model every submission is refused: sample again within
            // a minute, not at the next hour (R27.3).
            let wait = if modelled { c.sample_every_s } else { c.sample_every_s.min(60) };
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    }

    /// Drain the miss ledger to `misses`, then build, log and append the report.
    fn report(&self, bus: &EventBus) -> Report {
        let drained = self.plane.drain_misses();
        let lines = drained.misses().iter().map(|m| {
            json!({
                "candidate": m.record.candidate_id.0,
                "reason": m.record.reason.label(),
                "ev_wei": m.record.simulated_ev.to_string(),
                "block": m.record.state_fingerprint.confirmed_block_number,
                "detail": m.detail,
            })
            .to_string()
        });
        match append(&self.config.misses, lines) {
            Ok(n) => {
                self.stats.misses_written.fetch_add(n, Ordering::Relaxed);
            }
            Err(e) => warn!(error = %e, "the misses file could not be written"),
        }

        let (skipped, unpriced, declined, invalidated) = self.search.counters().snapshot();
        let feed = self.feed_stats.load().value.as_ref().clone();
        let flashblocks = self.flashblock_stats.load().value.as_ref().clone();
        let r = Report {
            unix_s: SystemClock.now().0 / 1_000_000_000,
            uptime_s: self.started.elapsed().as_secs(),
            config: self.config.version.to_string(),
            head: self.heads.borrow().as_ref().map(|h| h.number),
            building: (*self.building.borrow()).map(|b| b.at(SystemClock.now()).number),
            capture_assurance: match self.plane.capture_assurance() {
                CaptureAssurance::Measured(v) => Some(v),
                CaptureAssurance::Undefined => None,
            },
            funnel: self.funnel.counts(),
            search: SearchReport { resident: self.search.resident(), skipped, unpriced, declined, invalidated },
            null_dispatched: self.null.count(),
            settlement_asked: self.nothing_sent.asked(),
            book: BookReport {
                venues: self.venues.iter().map(|v| format!("{v:?}")).collect(),
                held: self.book.len(),
                universe: self.universe.len(),
                status: format!("{:?}", self.book.status()),
                admitted: self.commitments.admitted(),
            },
            reloads: {
                let (positions_in_place, positions_read_again) = self.book.position_counts();
                ReloadReport {
                    full: get(&self.stats.reloads_full),
                    partial: get(&self.stats.reloads_partial),
                    failed: get(&self.stats.reload_failures),
                    pools_refused: get(&self.stats.pools_refused),
                    pools_removed: get(&self.stats.pools_removed),
                    positions_in_place,
                    positions_read_again,
                }
            },
            read_failures: ReadFailures {
                view: get(&self.stats.view_failures),
                twap: get(&self.stats.twap_failures),
                fees: get(&self.stats.fee_failures),
                l1: get(&self.stats.l1_failures),
            },
            costs: CostReport { base_fee_wei: get(&self.stats.base_fee_wei), route_other_wei: self.costs.route_costs().other_wei },
            capacity: CapacityReport {
                samples: get(&self.stats.capacity_samples),
                flashblocks_seen: get(&self.stats.flashblocks_seen),
                blocks_held: get(&self.stats.capacity_blocks),
                blocks_refused: get(&self.stats.capacity_refused),
                largest_window: self.largest_window.load(Ordering::Relaxed),
            },
            feed: FeedReport {
                sessions: feed.as_ref().map_or(0, |f| f.sessions()),
                refusals: feed.as_ref().map_or(0, |f| f.refusals()),
                dropped: feed.as_ref().map_or(0, |f| f.dropped()),
                published: bus.published(),
                fast_lane_lossless: bus.fast_lane_is_lossless(),
            },
            misses_written: get(&self.stats.misses_written),
            near_miss: self.near_misses.report(),
            queue: QueueReport {
                stale: get(&self.stats.queue_stale),
                conflicts: get(&self.stats.queue_conflicts),
                max_depth: get(&self.stats.queue_max_depth),
            },
            flashblocks: FlashblockReport {
                sessions: flashblocks.as_ref().map_or(0, |f| f.sessions()),
                frames: flashblocks.as_ref().map_or(0, |f| f.frames()),
                decode_failures: flashblocks.as_ref().map_or(0, |f| f.decode_failures()),
                incomplete_blocks: get(&self.stats.incomplete_blocks),
                ledger_failures: get(&self.stats.ledger_failures),
            },
            race: RaceReport { written: self.race.written(), dropped: self.race.dropped() },
            signer: SignerReport {
                boot_nonce: self.boot_nonce.get().copied(),
                nonce_moved: self.nonce_moved.load(Ordering::Relaxed),
            },
            armed: {
                let (move_checks, move_tick_mismatches, move_liquidity_changes) = self.book.move_checks();
                ArmedReport {
                    mode: if self.armed_live.is_some() { "live" } else { "shadow" },
                    engine: self.armed.stats(),
                    write_failures: get(&self.stats.armed_write_failures),
                    live: self.armed_live.as_ref().map(|l| l.report(now_ms())),
                    live_dropped: get(&self.stats.armed_live_dropped),
                    move_checks,
                    move_tick_mismatches,
                    move_liquidity_changes,
                    targets_loaded: self.targets_loaded,
                    targeted_routes: self.targeted_routes,
                    targets_missing: self.targets_missing,
                }
            },
            lenders: {
                let l = *self.lender.borrow();
                LendersReport {
                    balancer_wei: l.map(|l| l.balancer.to_string()),
                    morpho_wei: l.and_then(|l| l.morpho).map(|h| h.to_string()),
                }
            },
            canary: self.canary.as_ref().map(|c| c.report()),
            rpc: {
                let mut usage = self.http.usage();
                usage.sort_by_key(|u| std::cmp::Reverse(u.bytes_in));
                usage
            },
        };
        info!(
            head = ?r.head,
            events = r.funnel.events,
            skipped_unverified = r.funnel.skipped_unverified,
            declined = r.funnel.declined.values().sum::<u64>(),
            closed = r.funnel.closed.values().sum::<u64>(),
            null_dispatched = r.null_dispatched,
            capture_assurance = ?r.capture_assurance,
            book = %r.book.status,
            pools = r.book.held,
            largest_window = r.capacity.largest_window,
            closest_bps = ?r.near_miss.best_bps,
            "shadow report"
        );
        match serde_json::to_string(&r) {
            Ok(line) => {
                if let Err(e) = append(&self.config.report, [line]) {
                    warn!(error = %e, "the report file could not be written");
                }
            }
            Err(e) => warn!(error = %e, "the report did not serialize"),
        }
        r
    }
}

/// Append lines to `path`; how many.
fn append(path: &Path, lines: impl IntoIterator<Item = String>) -> std::io::Result<u64> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let mut n = 0;
    for line in lines {
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        n += 1;
    }
    Ok(n)
}

/// One period's report: logged, and appended as a JSON line to `report`.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub unix_s: u64,
    pub uptime_s: u64,
    pub config: String,
    pub head: Option<u64>,
    /// The block Tier 2 would simulate in now, from the feed (R18). `None`
    /// until the feed has seen a head — and every simulation refused until then.
    pub building: Option<u64>,
    /// `None` while no ticket has been authorized: nothing to be a share of.
    pub capture_assurance: Option<f64>,
    pub funnel: Counts,
    pub search: SearchReport,
    pub null_dispatched: usize,
    /// Settlement questions about a transaction. Zero, or a defect.
    pub settlement_asked: u64,
    pub book: BookReport,
    pub reloads: ReloadReport,
    pub read_failures: ReadFailures,
    pub costs: CostReport,
    pub capacity: CapacityReport,
    pub feed: FeedReport,
    pub misses_written: u64,
    /// How close the priced routes came to paying: the best net over a ladder
    /// of sizes, as basis points of the size (R14).
    pub near_miss: NearMissReport,
    /// R21's queue: what it dropped, and how deep it got.
    pub queue: QueueReport,
    /// R25: Base's public flashblock stream and both ledgers.
    pub flashblocks: FlashblockReport,
    pub race: RaceReport,
    /// R25's tripwire: the signer's nonce at boot, and whether it has moved.
    pub signer: SignerReport,
    /// R26: the arming engine, and the move model's check on every Swap.
    pub armed: ArmedReport,
    /// R27: what each lender holds of WETH, as last read.
    pub lenders: LendersReport,
    /// R28: the canary, when this run is one.
    pub canary: Option<CanaryReport>,
    /// R28.1: the HTTP traffic since boot, by endpoint and method, the most
    /// bytes received first: what the provider bills.
    pub rpc: Vec<MethodUsage>,
}

/// What each flash lender holds, in wei (R27).
#[derive(Clone, Debug, Serialize)]
pub struct LendersReport {
    pub balancer_wei: Option<String>,
    /// Morpho Blue's holding (R29: it lends directly).
    pub morpho_wei: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ArmedReport {
    pub mode: &'static str,
    pub engine: crate::armed::engine::EngineStats,
    pub write_failures: u64,
    /// Applied Swaps checked against `move_to_price`.
    pub move_checks: u64,
    /// The model's tick was wrong: its error. Should stay near zero.
    pub move_tick_mismatches: u64,
    /// The tick agreed, the liquidity did not: an unapplied mint or burn.
    pub move_liquidity_changes: u64,
    /// R36: whether the census's targets were read; their routes; their pools
    /// with no inventory record.
    pub targets_loaded: bool,
    pub targeted_routes: u64,
    pub targets_missing: u64,
    /// R36: armed-live, when this run sends; passes its queue had no room for.
    pub live: Option<crate::armed::live::LiveReport>,
    pub live_dropped: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FlashblockReport {
    pub sessions: u64,
    pub frames: u64,
    pub decode_failures: u64,
    /// Blocks joined mid-way or with a missing index: their ranges unknown.
    pub incomplete_blocks: u64,
    /// Race or flashblock ledger lines that could not be written.
    pub ledger_failures: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RaceReport {
    pub written: u64,
    /// Lines dropped because the writer had fallen 4,096 behind.
    pub dropped: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SignerReport {
    pub boot_nonce: Option<u64>,
    pub nonce_moved: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct QueueReport {
    /// Proposals older than `queue::MAX_PROPOSAL_AGE` when picked, filed as
    /// stale-state misses.
    pub stale: u64,
    /// Proposals sharing a pool with a ticket just dispatched.
    pub conflicts: u64,
    pub max_depth: u64,
}

/// A ticket that went to a dispatcher: its pools are about to move.
fn dispatched(h: &Handled) -> bool {
    matches!(h, Handled::Closed { outcome, .. }
        if matches!(**outcome, TicketOutcome::Success { .. } | TicketOutcome::ShadowDispatched { .. }))
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchReport {
    pub resident: usize,
    pub skipped: u64,
    pub unpriced: u64,
    pub declined: u64,
    pub invalidated: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BookReport {
    /// The venues the executor could reach at boot, and so the ones priced.
    pub venues: Vec<String>,
    pub held: usize,
    pub universe: usize,
    pub status: String,
    pub admitted: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReloadReport {
    pub full: u64,
    pub partial: u64,
    pub failed: u64,
    /// Pools a reload could not read, held or not.
    pub pools_refused: u64,
    /// Of those, the ones the book held and dropped.
    pub pools_removed: u64,
    /// R28.1: `Mint` and `Burn` applied to the book exactly, each a partial
    /// reload saved.
    pub positions_in_place: u64,
    /// Those that could not be, and read the pool again.
    pub positions_read_again: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadFailures {
    pub view: u64,
    pub twap: u64,
    pub l1: u64,
    /// Aerodrome v2's fee re-reads (R24).
    pub fees: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CostReport {
    pub base_fee_wei: u64,
    /// What a two-hop route costs besides its gas: the L1 data fee and the
    /// failure branch. Its gas is its own, at its size.
    pub route_other_wei: u128,
}

#[derive(Clone, Debug, Serialize)]
pub struct CapacityReport {
    pub samples: u64,
    pub flashblocks_seen: u64,
    pub blocks_held: u64,
    pub blocks_refused: u64,
    pub largest_window: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeedReport {
    pub sessions: u64,
    pub refusals: u64,
    pub dropped: u64,
    pub published: u64,
    pub fast_lane_lossless: bool,
}
