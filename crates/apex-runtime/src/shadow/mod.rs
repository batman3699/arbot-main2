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
//! | feed | `newHeads`, `pendingLogs` and `logs` over the universe's pools → the book → `PendingSwap` events on the fast lane | R1, R3 |
//! | capture | each event → `Plane::on_event`, **only while the book is `Verified`** (INV-08) | the run itself |
//! | reload | one reload at a time, at the newest head, requests coalesced; a pool still off its ladder is asked for again next block; a full reload re-admits the universe and refreshes the admissions | R3, R9 |
//! | heads | each head: the live reader's view, Slipstream's TWAPs, and the costs — the base fee each block, the L1 oracle every `l1_every_blocks` | R6, R8, R9 |
//! | capacity | a flashblock sample at boot, then one every `sample_every_s`, into the recorder behind the chain adapter | R5 |
//! | full reload | a full reload every `full_reload_every_s`: pools a failed read removed come back, and the admissions' evidence stays inside its bound | R9 |
//! | report | the funnel, capture assurance and every counter, logged and appended to `report`; the miss ledger drained into `misses` | R9 |
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
pub mod nothing_sent;
pub mod reload;

use crate::bus::{EventBus, Lane, Subscription as BusSubscription};
use crate::econ::{ChainCosts, FlashTerms, LiveEconomics, RouteCosts, ScenarioPriors};
use crate::live::abi::{self, selector};
use crate::live::adapter::LiveAdapter;
use crate::live::admission::{self, LiveCommitments};
use crate::live::book::PoolBook;
use crate::live::calls::{self, LiveCallBuilder};
use crate::live::costs::{self, Costs};
use crate::live::feed::{Effect, FeedHandler, BURN, MINT, PANCAKE_SWAP, SWAP};
use crate::live::frontier::{self, BALANCER_FLASH, BALANCER_VAULT, WETH};
use crate::live::gas;
use crate::live::near_miss::{NearMissReport, NearMisses};
use crate::live::inventory::{self, PoolSpec, UniverseFilter, Venue};
use crate::live::pricing::LivePricer;
use crate::live::reader::{ChainReader, ReaderConfig};
use crate::live::reads::ChainReads;
use crate::live::sim::LiveSimulator;
use crate::plane::{DispatchLane, Plane, Ports};
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
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::ws::{Head, LogFilter, Notification, Subscription, WsFeed, WsSettings, WsStats};
use apex_chain::rpc::RpcTransport;
use apex_config::{Env, Secret};
use apex_econ::cost::failure::FailureProfile;
use apex_econ::cost::l1_data::{L1FeeModel, L1FeeParameters};
use apex_econ::eligibility::EligibilityPolicy;
use apex_risk::breaker::CircuitBreaker;
use apex_risk::posture::PostureLadder;
use apex_search::engine_c::FiniteSizeEngine;
use apex_search::engine_d::EventEngine;
use apex_state::Versioned;
use apex_types::cost::GasUsed;
use apex_types::flash::CallbackConstraints;
use apex_types::ids::{ChainId, SignerLaneId, StrategyId, SubmissionLaneId, TokenId};
use apex_types::state::ReconstructionStatus;
use config::ShadowConfig;
use funnel::{Counts, Funnel};
use nothing_sent::NothingSent;
use reload::{ReloadQueue, Work};
use serde::Serialize;
use serde_json::{json, Value};
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
    l1_failures: AtomicU64,
    capacity_samples: AtomicU64,
    flashblocks_seen: AtomicU64,
    capacity_blocks: AtomicU64,
    capacity_refused: AtomicU64,
    misses_written: AtomicU64,
    base_fee_wei: AtomicU64,
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
    largest_window: AtomicU64,
    recorder: tokio::sync::Mutex<FlashblockRecorder>,
    feed_stats: Versioned<Option<Arc<WsStats>>>,
    stats: Stats,
    started: Instant,
}

/// The census's pools that can form a cycle the live frontier prices: pairs
/// with WETH on one side and at least two pools, **counting only `venues`** —
/// the ones the executor can reach. Filtered before pairing, so a pool on an
/// unreachable venue cannot make up a pair's two.
pub fn universe(dir: &Path, venues: &[Venue]) -> Result<Vec<PoolSpec>, inventory::InventoryError> {
    let specs: Vec<PoolSpec> = inventory::load(dir, UniverseFilter::default())?
        .into_iter()
        .filter(|s| venues.contains(&s.venue))
        .collect();
    Ok(inventory::pairs(&specs)
        .into_iter()
        .filter(|((a, b), _)| *a == WETH || *b == WETH)
        .flat_map(|(_, pools)| pools)
        .collect())
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
    for (name, task) in [("reload", Task::Reload), ("heads", Task::Heads), ("capacity", Task::Capacity), ("full-reload", Task::FullReload)] {
        let s = Arc::clone(&shadow);
        spawn(name, Box::new(move || {
            let s = Arc::clone(&s);
            Box::pin(async move { s.task(task).await })
        }));
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
    Ok(shadow.report(&bus))
}

#[derive(Clone, Copy)]
enum Task {
    Reload,
    Heads,
    Capacity,
    FullReload,
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
    let rpc: Arc<dyn RpcTransport> = Arc::new(
        FailoverTransport::connect(&urls, config.chain_id, FailoverSettings::default())
            .await
            .map_err(|e| boot_err("the RPC transport", e))?,
    );
    drop(urls);
    let reads = ChainReads::new(Arc::clone(&rpc));
    let head = reads.head().await.map_err(|e| boot_err("the head", e))?;

    // Only what the executor can reach: an adapter venue's routes would revert
    // without its registration, and pricing them would fill the funnel with
    // trades nothing could make.
    let venues = calls::reachable_venues(&reads, config.executor.address, head)
        .await
        .map_err(|e| boot_err("the executor's adapters", e))?;
    for v in Venue::ALL.iter().filter(|v| !venues.contains(v)) {
        warn!(venue = ?v, "the executor has no adapter for it: its pools are left out of the universe");
    }
    let specs = universe(&config.inventory, &venues).map_err(|e| boot_err("the inventory", e))?;
    let (book, unloaded) = PoolBook::load(&reads, &specs, head).await.map_err(|e| boot_err("the pool book", e))?;
    for u in &unloaded {
        warn!(pool = %u.pool, why = ?u.why, "not loaded at boot; the first full reload reads it again");
    }
    let book = Arc::new(book);
    let (frontier, cycles) = frontier::build(base, WETH, &book.snapshot());
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

    // Built before the economics it serves, so it starts at costs no route
    // clears; `Costs::new` aligns it with the economics' before anything prices.
    let unpriced = RouteCosts { other_wei: u128::MAX, wei_per_gas: u128::MAX };
    let pricer = Arc::new(LivePricer::new(Arc::clone(&book), cycles.clone(), unpriced, gas::MEASURED));
    let pricer_near_misses = pricer.near_misses();
    let econ = Arc::new(
        LiveEconomics::new(pricer.clone(), ScenarioPriors::default(), chain_costs, StrategyId(1)).with_flash(terms),
    );
    let costs = Costs::new(Arc::clone(&econ), Arc::clone(&pricer));
    let search = Arc::new(FrontierSearch::new(
        frontier,
        // Every template is over pools the book read and held to their
        // factory and tokens at load.
        ReconstructionStatus::Verified,
        pricer.clone(),
        EventEngine::measured(),
        FiniteSizeEngine::measured(),
    ));

    let (admitted, refused) = admission::admit_book(&book, base, head);
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
    let signer = Arc::new(LocalSigner::new(base).with_lane(lane, key));
    let adapter = Arc::new(LiveAdapter::new(
        vec![SubmissionLaneId(1)],
        ReplacementPolicy { supported: true, min_fee_bump_bps: 1_000, max_attempts: 2 },
    ));
    let null = Arc::new(NullDispatcher::new());
    let nothing_sent = Arc::new(NothingSent::default());
    let plane = Arc::new(Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(Box::new(journal), Box::new(SystemClock))),
        pool,
        gate: Arc::new(DispatchGate::shut()),
        dispatch: DispatchLane::Shadow(Arc::clone(&null)),
        chain: adapter.clone(),
        search: search.clone(),
        econ,
        sim: Arc::new(LiveSimulator::new(Arc::clone(&rpc), base)),
        // Nothing records a loss on a lane that sends nothing, so the breaker
        // never trips here; its limits are the live run's to set.
        risk: Arc::new(LiveRiskGate::new(
            PostureLadder::new(),
            CircuitBreaker::new(U256::MAX, U256::MAX, u32::MAX),
            EligibilityPolicy::default(),
            Box::new(SystemClock),
        )),
        commitments: commitments.clone(),
        calls: Arc::new(LiveCallBuilder::new(Arc::clone(&book), cycles.into_values())),
        signer,
        live: reader.clone(),
        settlement: nothing_sent.clone(),
    }));
    let reconciled = plane.boot(&*nothing_sent, SystemClock.now()).map_err(|e| boot_err("recovery", e))?;

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
        "shadow booted"
    );
    let (heads, _) = watch::channel(None);
    Ok(Shadow {
        universe: specs.iter().map(|p| p.pool).collect(),
        venues,
        reads,
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
        largest_window: AtomicU64::new(0),
        recorder: tokio::sync::Mutex::new(FlashblockRecorder::new(FlashblockRecorder::DEFAULT_WINDOW)),
        feed_stats: Versioned::new(None, ReconstructionStatus::Rebuilding),
        stats: Stats::default(),
        started,
        config,
    })
}

impl Shadow {
    /// The capture feed: notifications → the book → effects.
    async fn feed(&self, bus: &EventBus, stopped: watch::Receiver<bool>) {
        let filter = LogFilter { addresses: self.universe.clone(), topics0: vec![SWAP, PANCAKE_SWAP, MINT, BURN] };
        let subs = vec![Subscription::NewHeads, Subscription::PendingLogs(filter.clone()), Subscription::Logs(filter)];
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
        while let Some(n) = rx.recv().await {
            if let Notification::Head(h) = &n {
                self.heads.send_replace(Some(h.clone()));
            }
            for effect in handler.handle(&self.book, n, SystemClock.now()) {
                match effect {
                    Effect::Event(e) => bus.publish(*e),
                    Effect::Reload(pools) => self.reloads.request(pools),
                    Effect::FullReload => self.reloads.request_full(),
                }
            }
        }
        let _ = task.await;
    }

    /// Events into the plane, while the book can be priced from.
    async fn capture(&self, sub: &mut BusSubscription) {
        while let Some(ev) = sub.recv().await {
            self.funnel.event();
            if self.book.status() != ReconstructionStatus::Verified {
                self.funnel.skipped_unverified();
                continue;
            }
            let handled = self.plane.on_event(&ev).await;
            self.funnel.record(&handled);
        }
    }

    async fn task(&self, t: Task) {
        match t {
            Task::Reload => self.reload_loop().await,
            Task::Heads => self.head_loop().await,
            Task::Capacity => self.capacity_loop().await,
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
                        let (admitted, refused) = admission::admit_book(&self.book, ChainId::BASE, block);
                        for (pool, why) in &refused {
                            warn!(%pool, ?why, block, "not admitted on reload");
                        }
                        self.commitments.set_admissions(admitted);
                    }
                    // Read before a swap it already holds: off its fresh ladder
                    // until a later block is read.
                    let off: Vec<Address> = self
                        .book
                        .snapshot()
                        .values()
                        .filter(|p| !p.ladder_covers_price())
                        .map(|p| p.spec.pool)
                        .collect();
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
        let mut l1: Option<(u64, L1FeeParameters)> = None;
        let every = self.config.policy.l1_every_blocks;
        loop {
            if heads.changed().await.is_err() {
                return;
            }
            let Some(h) = heads.borrow_and_update().clone() else { continue };
            let window = self.largest_window.load(Ordering::Relaxed);
            view.note("the live reader's view", self.reader.refresh(&h, window).await, &self.stats.view_failures);
            twap.note("Slipstream's TWAPs", self.book.refresh_twaps(&self.reads, h.number).await.map(|_| ()), &self.stats.twap_failures);
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

    async fn capacity_loop(&self) {
        let c = &self.config.capacity;
        let mut duration = Duration::from_secs(c.boot_sample_s);
        loop {
            {
                let mut rec = self.recorder.lock().await;
                match sample(self.config.rpc_ws.expose(), WsSettings::default(), duration, &mut rec).await {
                    Ok(seen) => {
                        bump(&self.stats.capacity_samples);
                        self.stats.flashblocks_seen.fetch_add(seen, Ordering::Relaxed);
                    }
                    Err(e) => warn!(error = %e, "flashblock sample failed"),
                }
                match rec.model() {
                    Ok(model) => {
                        self.largest_window.store(model.largest_measured_window(), Ordering::Relaxed);
                        self.adapter.set_capacity(model);
                    }
                    Err(e) => info!(error = ?e, "no capacity model yet; every submission is refused until one"),
                }
                self.stats.capacity_blocks.store(rec.blocks() as u64, Ordering::Relaxed);
                self.stats.capacity_refused.store(rec.refused(), Ordering::Relaxed);
            }
            duration = Duration::from_secs(c.sample_for_s);
            tokio::time::sleep(Duration::from_secs(c.sample_every_s)).await;
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
        let r = Report {
            unix_s: SystemClock.now().0 / 1_000_000_000,
            uptime_s: self.started.elapsed().as_secs(),
            config: self.config.version.to_string(),
            head: self.heads.borrow().as_ref().map(|h| h.number),
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
            reloads: ReloadReport {
                full: get(&self.stats.reloads_full),
                partial: get(&self.stats.reloads_partial),
                failed: get(&self.stats.reload_failures),
                pools_refused: get(&self.stats.pools_refused),
                pools_removed: get(&self.stats.pools_removed),
            },
            read_failures: ReadFailures {
                view: get(&self.stats.view_failures),
                twap: get(&self.stats.twap_failures),
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
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadFailures {
    pub view: u64,
    pub twap: u64,
    pub l1: u64,
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
