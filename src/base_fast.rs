//! Base flashblock fast path: preconfirmed logs into local state.
//!
//! Base seals an L2 block every ~2s but publishes *flashblocks* every ~200ms.
//! The canonical scan loop measured 4.20s median end to end, which is 2x a
//! block and 21x a flashblock — so an opportunity is gone long before the
//! scanner looks at it. This module is the first half of closing that: it
//! consumes preconfirmed logs and marks pools dirty, and does nothing else.
//!
//! # Why `pendingLogs` and not the flashblocks stream
//!
//! `wss://mainnet.flashblocks.base.org/ws` is real, but it is node-operator
//! infrastructure: it pushes Brotli-compressed binary frames with no
//! subscription handshake, so consuming it means a new decompression
//! dependency and a bespoke framing layer.
//!
//! `pendingLogs` is ordinary `eth_subscribe` over the provider websocket we
//! already use. Verified against the configured provider 2026-09-02: **215
//! notifications in 30s across 4 pools, first at 576ms**, carrying the same
//! `topic0` values `log_decode::monitored_topics()` already decodes. It reuses
//! the entire decode and `LiveState` path, which is field-proven, and needs no
//! new crate.
//!
//! Base documents an eventual `Denim` upgrade replacing flashblocks with native
//! 200ms blocks, so the feed sits behind [`FlashFeed`] — the state and search
//! engine above it must survive that migration untouched.
//!
//! # What this module deliberately does NOT do
//!
//! No quoting, no graph work, no RPC, no simulation. Those belong downstream of
//! the dirty set. A websocket task that blocks on an `eth_call` stops draining
//! its socket, and a feed that falls behind is worse than no feed — it reports
//! stale state as fresh.

// The feed, the drain and the candidate handoff are all wired now. What is
// still unused are the individual accessors a caller would reach for at a
// console -- `feed`, `is_monitored`, `mean_apply_micros` -- and the
// simulation helpers, which have no call site until a real candidate's
// calldata is simulated instead of an empty probe.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ethers::types::{Address, Log, H256, U256};
use serde_json::{json, Value};

use ethers::providers::Middleware;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, info, warn};

use crate::ingestion::{
    idle_action, next_before_stall, IdleAction, StreamStep, SUBSCRIPTION_IDLE_TICK,
    SUBSCRIPTION_STALL_LIMIT, WS_MAX_CONNECTION_AGE,
};
use crate::live_state::{ApplyOutcome, LiveState, UnknownReason};
use crate::metrics::Metrics;
use crate::util::connect_ws_provider_with_fallbacks;

/// How the fast path receives preconfirmed logs.
///
/// An enum rather than a bare URL so the Denim migration — native 200ms blocks
/// replacing flashblocks — is a variant here and not a rewrite of everything
/// downstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlashFeed {
    /// `eth_subscribe("pendingLogs", ...)` over a flashblocks-capable provider.
    PendingLogs { ws_url: String },
}

impl FlashFeed {
    pub fn ws_url(&self) -> &str {
        match self {
            FlashFeed::PendingLogs { ws_url } => ws_url,
        }
    }
}

/// Subscription parameters for `eth_subscribe`.
///
/// Split out from the task so the filter is testable without a socket. The
/// original pool monitor shipped a filter whose `topic0` values were fabricated:
/// it connected cleanly, logged success, and matched nothing for the life of the
/// process. The topics here come from `monitored_topics()`, which derives every
/// one from its event signature, and the address list is asserted non-empty —
/// an address-less `pendingLogs` subscribes to the entire chain.
pub fn pending_logs_params(pools: &[Address]) -> Value {
    json!([
        "pendingLogs",
        {
            "address": pools,
            "topics": [crate::log_decode::monitored_topics()],
        }
    ])
}

/// True when a subscription would be worth opening.
///
/// An empty pool list must never subscribe: `pendingLogs` with no address
/// filter is a firehose of every log on Base, which this task would then try to
/// decode one at a time.
pub fn worth_subscribing(pools: &[Address]) -> bool {
    !pools.is_empty()
}

/// Counters for the one thing this module exists to prove: that preconfirmed
/// state arrives fast enough to matter.
#[derive(Default)]
pub struct FastPathStats {
    /// Logs accepted into `LiveState`.
    pub applied: AtomicU64,
    /// Logs the decoders recognised but state rejected (duplicate, superseded,
    /// untrusted base). Expected traffic, not a fault.
    pub declined: AtomicU64,
    /// Logs no decoder recognised. A real signal: some venue emits something we
    /// do not understand.
    pub undecodable: AtomicU64,
    /// Sum of receive→applied microseconds, and the count, so the mean is
    /// derivable without a histogram.
    pub apply_micros_total: AtomicU64,
    pub apply_samples: AtomicU64,
}

impl FastPathStats {
    /// Mean receive→applied latency in microseconds, or `None` before the first
    /// sample. This is the number that says whether the feed is keeping up.
    pub fn mean_apply_micros(&self) -> Option<u64> {
        let n = self.apply_samples.load(Ordering::Relaxed);
        if n == 0 {
            return None;
        }
        Some(self.apply_micros_total.load(Ordering::Relaxed) / n)
    }
}

/// Consumes preconfirmed logs and marks pools dirty. Nothing else.
pub struct BaseFastPath {
    feed: FlashFeed,
    pools: Vec<Address>,
    live: Arc<LiveState>,
    /// Shared with the scan loop, which drains it via `drain_and_resolve`. A `Mutex<HashSet>` rather
    /// than a `DashMap` because the drain must be a single atomic swap — the
    /// collect-then-clear form loses any insert landing between the two.
    touched: Arc<StdMutex<HashSet<Address>>>,
    metrics: Option<Arc<Metrics>>,
    ws_endpoints: Vec<String>,
    ws_backoff: Duration,
    /// pool -> (token0, token1), ORDERED. Required to price a hop in the
    /// direction asked for; without it `sqrtPriceX96` would be applied blind.
    pool_tokens: Arc<dashmap::DashMap<Address, PoolMeta>>,
    /// HTTP endpoint for `eth_simulateV1`. `None` disables the probe.
    sim_http: Option<Arc<ethers::providers::Provider<ethers::providers::Http>>>,
    /// How long a pool may go unchecked against chain before it stops being
    /// priced. A backstop, not the mechanism: the rolling verification pass is
    /// what keeps pools inside it.
    verify_ttl: Duration,
    /// Pools re-read per verification pass.
    verify_batch: usize,
    pub stats: Arc<FastPathStats>,
}

impl BaseFastPath {
    /// `live` MUST have no other writer.
    ///
    /// `LiveState` holds one global cursor and requires monotonic ordinals.
    /// Preconfirmed and sealed delivery are two orderings of the same log
    /// stream, so pointing this at the pool monitor's state makes every
    /// interleaving look like disorder. Measured 2026-09-02 when they shared
    /// one: 27,085 events, 37 applies, 584 continuity breaks, each invalidating
    /// all 683 pools -- the canonical path's candidate count fell to zero.
    pub fn new(
        feed: FlashFeed,
        pools: Vec<Address>,
        live: Arc<LiveState>,
        touched: Arc<StdMutex<HashSet<Address>>>,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self {
            feed,
            pools,
            live,
            touched,
            metrics,
            ws_endpoints: Vec::new(),
            ws_backoff: Duration::from_secs(5),
            pool_tokens: Arc::new(dashmap::DashMap::new()),
            sim_http: None,
            verify_ttl: Duration::from_secs(
                crate::util::env_parse_opt::<u64>("ARBOT_BASE_FAST_VERIFY_TTL_SECS")
                    .filter(|v| *v > 0)
                    .unwrap_or(120),
            ),
            verify_batch: crate::util::env_parse_opt::<usize>("ARBOT_BASE_FAST_VERIFY_BATCH")
                .filter(|v| *v > 0)
                .unwrap_or(64),
            stats: Arc::new(FastPathStats::default()),
        }
    }

    pub fn feed(&self) -> &FlashFeed {
        &self.feed
    }

    pub fn pools(&self) -> &[Address] {
        &self.pools
    }

    /// The dirty set, for the scan loop to drain.
    pub fn touched(&self) -> Arc<StdMutex<HashSet<Address>>> {
        Arc::clone(&self.touched)
    }

    /// Apply one preconfirmed log.
    ///
    /// Separated from the socket so the hot path is testable without a network:
    /// the pool monitor's equivalent logic could not be tested for months
    /// because it was welded inside the subscription loop.
    ///
    /// The pool is marked dirty on every *state-bearing* outcome, including the
    /// declines. A `Duplicate` or `Superseded` still means someone traded that
    /// pool, and the scan loop wants to look at it regardless of whether our
    /// snapshot moved.
    pub fn apply(&self, log: &Log, received_at: Instant) -> ApplyOutcome {
        let pool = log.address;
        let outcome = self.live.apply_log(log);
        match outcome {
            ApplyOutcome::Applied { .. } => {
                self.stats.applied.fetch_add(1, Ordering::Relaxed);
                self.mark(pool);
            }
            ApplyOutcome::Undecodable => {
                self.stats.undecodable.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.stats.declined.fetch_add(1, Ordering::Relaxed);
                self.mark(pool);
            }
        }
        let micros = received_at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.stats
            .apply_micros_total
            .fetch_add(micros, Ordering::Relaxed);
        self.stats.apply_samples.fetch_add(1, Ordering::Relaxed);
        if let Some(m) = &self.metrics {
            m.ingestion_ws_events.inc();
        }
        outcome
    }

    fn mark(&self, pool: Address) {
        if let Ok(mut g) = self.touched.lock() {
            g.insert(pool);
        }
    }

    /// What to do when the feed goes quiet.
    ///
    /// Delegates to the same decision function the pool monitor uses, so the
    /// distinction it encodes is not re-derived here and cannot drift: silence
    /// from birth means the filter matches nothing and a new socket would be
    /// just as deaf, while silence *after* delivery means a half-open socket
    /// that a reconnect does fix.
    pub(crate) fn idle_verdict(
        &self,
        events_seen: u64,
        idle_for: Duration,
        connected_for: Duration,
    ) -> IdleAction {
        idle_action(events_seen, idle_for, connected_for)
    }

    /// The idle budget for this feed.
    ///
    /// `pendingLogs` on a monitored pool set is high traffic — measured 215
    /// notifications in 30s across 4 pools — so prolonged silence is a dead
    /// socket, not a quiet market, and the 90s idle budget applies.
    pub fn stall_limit(&self) -> Duration {
        SUBSCRIPTION_STALL_LIMIT
    }
}

/// What the fast path knows about a pool, for directional and net pricing.
///
/// `fee_ppm`, not `fee_bps`. `MonitoredPool.fee_bps` is populated from the raw
/// pool fee, which for UniV3-style venues is PARTS PER MILLION -- 3_000 means
/// 0.30%, not 30%. The existing field name is a 10x error waiting to be made,
/// so this one states its unit and converts explicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolMeta {
    pub token0: Address,
    pub token1: Address,
    pub fee_ppm: u32,
    pub kind: PoolKind,
    /// Whether `token0`/`token1` came from the CHAIN rather than from config.
    ///
    /// CL records carry the pair straight out of the factory's `PoolCreated`
    /// event, so they are verified on construction. V2 metadata is not:
    /// `config/base_aerodrome_pools.json` lists every pair in both directions
    /// and `or_insert` keeps whichever line came first, so the recorded
    /// `token0` need not be the token `reserve0` counts. The seed reads
    /// `token0()`/`token1()` and flips this.
    ///
    /// Unverified V2 pools are REFUSED, not guessed. An inverted pair does not
    /// fail loudly — it returns the reciprocal rate, and a reciprocal around a
    /// loop manufactures a phantom edge.
    pub verified: bool,
    /// When this pool's state was last READ FROM CHAIN. `None` means never.
    ///
    /// Deliberately not `Provenance.anchored_at`, which a preconfirmed log also
    /// refreshes -- a pool whose every update arrived over `pendingLogs` has
    /// never been checked against a sealed block. That matters because the
    /// continuity cursor cannot see a MISSING log on a filtered subscription:
    /// a preconfirmed log that is dropped, or one that never lands in the
    /// sealed block, leaves state wrong with no signal at all.
    ///
    /// Age alone is not evidence of wrongness. A pool that has not traded has
    /// correct state however old it is, because reserves and `sqrtPriceX96`
    /// only move on events this feed subscribes to. What this bounds is how
    /// long an UNCHECKED assumption is allowed to stand.
    pub confirmed_at: Option<Instant>,
    /// A CL pool's real `balanceOf` holdings, `(token0, token1)`, as of the
    /// last chain read. `None` means unknown, and unknown must bound nothing.
    ///
    /// Balances and not `liquidity`. `liquidity` with `sqrt_price_x96` gives
    /// the VIRTUAL reserves of the constant-product curve the pool is tangent
    /// to, which runs 0..infinity far outside the ticks actually holding
    /// anything -- measured on Base it overstates real holdings by 16-56x on
    /// deep WETH/USDC and by orders of magnitude on thin pools. Using it as
    /// depth would re-create the exact bias this ranking exists to remove, and
    /// would do it worst on precisely the dust pools.
    ///
    /// V2 pools carry no balances here: their reserves ARE their holdings and
    /// come off the live snapshot, always fresher than a rotation pass.
    pub balances: Option<(f64, f64)>,
}

/// Which curve a pool trades on.
///
/// Needed for two separate reasons, and both are load-bearing:
/// - the SEED must read each pool with the loader for its storage layout;
/// - the PRICER must use each pool's actual invariant.
///
/// `StableSwap` is split out because `r1/r0` is not the marginal price of the
/// Solidly stable curve. Pricing one as constant-product produced a 163%
/// phantom edge in the 2026-09-01 spread census. This enum exists so that
/// mistake is unrepresentable rather than merely discouraged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    /// UniV3 / Aerodrome Slipstream. Priced from `sqrtPriceX96`.
    ConcentratedLiquidity,
    /// UniV2 and Solidly *volatile*. `x * y = k`, so `r1/r0` is the price.
    ConstantProduct,
    /// Solidly *stable*. `x^3*y + x*y^3 = k`.
    StableSwap,
}

/// Which unit a venue's declared fee is expressed in.
///
/// Both sources call the field `fee_bps` and they mean different things.
/// Verified against the Base inventories on 2026-09-03:
/// - `data/base/uniswap_v3` (1,881,808 pools): fees 100 / 500 / 3000 / 10000
///   — the UniV3 ppm tiers. `aerodrome_slipstream` and `pancakeswap_v3` match.
/// - `data/base/uniswap_v2` (141,364 pools): fee 30 for **every one of them**,
///   and `config/base_aerodrome_pools.json` carries `feeBps: 30`. Basis points.
///
/// Reading bps as ppm undercharges by 100x: `keep` becomes 0.99997 instead of
/// 0.997, crediting 29.1 bps of profit per hop that does not exist — about
/// 87 bps around a triangle. That is indistinguishable from a fat tail of
/// genuine opportunities, which is what makes it dangerous rather than merely
/// wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeUnit {
    /// UniV3 / Slipstream / PancakeSwap V3 records: raw pool fee, already ppm.
    Ppm,
    /// UniV2 records and Aerodrome/Solidly config: real basis points.
    Bps,
}

pub fn fee_to_ppm(unit: FeeUnit, declared: u32) -> u32 {
    match unit {
        FeeUnit::Ppm => declared,
        FeeUnit::Bps => declared.saturating_mul(100),
    }
}

/// How much of the fast path's universe can be priced right now.
///
/// The denominator is the SUBSCRIBED pool set, never the set that happens to
/// hold a snapshot: a path that has heard from 40 of 683 pools has 6% coverage,
/// and measuring against 40 would report 100% at the moment it is most blind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastCoverage {
    pub priceable: usize,
    pub total: usize,
}

impl FastCoverage {
    /// An empty universe is 0%, not 100%. `0/0` reported as complete would let
    /// a fast path with no pools at all pass a >95% production gate.
    pub fn pct(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.priceable as f64 * 100.0 / self.total as f64
    }
}

/// One CL pool as `load_cl_pool_states_batched` wants it:
/// `(pool, fee_hint, token0, token1)`.
pub type ClSeedTarget = (Address, Option<u32>, Address, Address);

/// Split a pool set into the reads each curve needs.
///
/// CL entries carry `(pool, fee_hint, token0, token1)` because
/// `load_cl_pool_states_batched` reads each pool's token BALANCES for its depth
/// bound, and cannot do that without knowing the pair. Both Solidly curves keep
/// plain reserves, so both seed through the pair loader — the invariant
/// difference is a pricing concern, not a storage one.
///
/// Pools with no metadata are returned in NEITHER list. The CL loader cannot
/// read a pool whose tokens are unknown, and substituting a guess would anchor
/// state against the wrong pair.
pub fn seed_targets(
    pools: &[Address],
    meta: &dashmap::DashMap<Address, PoolMeta>,
) -> (Vec<ClSeedTarget>, Vec<Address>) {
    let mut cl: Vec<ClSeedTarget> = Vec::new();
    let mut v2 = Vec::new();
    for pool in pools {
        let Some(m) = meta.get(pool) else { continue };
        match m.kind {
            PoolKind::ConcentratedLiquidity => {
                cl.push((*pool, Some(m.fee_ppm), m.token0, m.token1))
            }
            PoolKind::ConstantProduct | PoolKind::StableSwap => v2.push(*pool),
        }
    }
    (cl, v2)
}

/// Record a pool's real `token0`/`token1` and mark the pair verified.
///
/// Returns whether the chain disagreed with what config claimed. That is the
/// number worth surfacing: `config/base_aerodrome_pools.json` lists every pair
/// in BOTH directions and `or_insert` keeps whichever line came first, so a
/// disagreement means the inventory would have priced that pool backwards. It
/// is not an error here — the chain's answer simply wins — but a silent win
/// hides a broken inventory.
pub fn adopt_chain_pair(
    meta: &dashmap::DashMap<Address, PoolMeta>,
    pool: Address,
    token0: Address,
    token1: Address,
) -> bool {
    let Some(mut m) = meta.get_mut(&pool) else {
        return false;
    };
    let mismatch = (m.token0, m.token1) != (token0, token1);
    m.token0 = token0;
    m.token1 = token1;
    m.verified = true;
    mismatch
}

/// What one reconciliation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SeedOutcome {
    /// Anchors actually installed. NOT the number attempted: `anchor_*` refuses
    /// any pool already holding newer state, and counting refusals as successes
    /// would report a seed that changed nothing as a full one.
    pub anchored: usize,
    /// Pools asked for that the reads did not return. A pool absent from the
    /// multicall result is a real gap — it stays unpriceable — so it is counted
    /// rather than folded into the anchored total.
    pub missing: usize,
    /// V2 pools whose config token order disagreed with the chain's. Pricing
    /// takes the chain's, so this is a warning about the inventory, not a
    /// failure -- but an inverted pair silently reports the reciprocal rate,
    /// and a reciprocal around a loop manufactures a phantom edge.
    pub order_mismatch: usize,
    /// Pools where local state and the fresh read could both be priced, so the
    /// two were actually comparable.
    pub compared: usize,
    /// Of those, how many disagreed by more than `DIVERGENCE_BPS`. This is the
    /// number that says whether preconfirmed state is drifting from the chain --
    /// the question age alone cannot answer, because a pool that has not traded
    /// has correct state however old it is.
    pub diverged: usize,
    pub max_divergence_bps: f64,
    pub block: u64,
}

/// Price disagreement above which local state is considered to have drifted.
///
/// Not zero: a CL price is compared through an f64 square of a 160-bit integer,
/// and a sealed read races preconfirmed updates by construction, so exact
/// equality would report drift on every busy pool. One basis point is far below
/// any spread worth trading and far above that noise.
pub const DIVERGENCE_BPS: f64 = 1.0;

/// Price implied by a CL `sqrtPriceX96`, as token1 per token0.
fn cl_price(sqrt_price_x96: U256) -> Option<f64> {
    let sp = u256_to_f64(sqrt_price_x96)?;
    let r = sp / 2f64.powi(96);
    let p = r * r;
    (p.is_finite() && p > 0.0).then_some(p)
}

/// Relative gap between two prices in basis points, or `None` if either is
/// unusable. Signed, so the direction of a drift is visible.
fn divergence_bps(local: f64, chain: f64) -> Option<f64> {
    if !(local.is_finite() && chain.is_finite()) || local <= 0.0 || chain <= 0.0 {
        return None;
    }
    let d = (local / chain - 1.0) * 10_000.0;
    d.is_finite().then_some(d)
}

/// The costs a gross edge must clear before it is a candidate.
///
/// Replaces a hardcoded `ARBOT_MAX_CYCLE_FEE_BPS = 60`. A constant fee ceiling
/// rejects a profitable dislocation purely because its nominal fee stack is
/// large, which is not an economic law -- a 100 bps dislocation through two
/// 30 bps pools is profitable and a 60 bps ceiling deletes it. The pool fees are
/// already inside `gross_bps`; these are the costs that are not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostStack {
    pub gas_bps: f64,
    pub flash_fee_bps: f64,
    pub execution_buffer_bps: f64,
    pub competition_bid_bps: f64,
    pub risk_premium_bps: f64,
}

impl CostStack {
    /// Defaults are ASSUMPTIONS, not measurements, and the log says so.
    ///
    /// `gas_bps` is the weakest: gas is a fixed cost, so its bps share depends
    /// on notional, and a single number here silently assumes one. It is a
    /// prefilter input only -- the real figure comes from `eth_estimateGas`
    /// against "pending" per candidate, which is step 9 and is not wired.
    /// `flash_fee_bps` defaults to 9 (Aave); Balancer is 0, so this is the
    /// conservative side.
    ///
    /// Every field is env-tunable because none of them is knowable from here,
    /// and burying a guess in a constant is what `ARBOT_MAX_CYCLE_FEE_BPS = 60`
    /// did.
    pub fn from_env() -> Self {
        let f = |k: &str, d: f64| {
            crate::util::env_parse_opt::<f64>(k)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(d)
        };
        Self {
            gas_bps: f("ARBOT_COST_GAS_BPS", 5.0),
            flash_fee_bps: f("ARBOT_COST_FLASH_FEE_BPS", 9.0),
            execution_buffer_bps: f("ARBOT_COST_EXEC_BUFFER_BPS", 3.0),
            competition_bid_bps: f("ARBOT_COST_COMPETITION_BPS", 10.0),
            risk_premium_bps: f("ARBOT_COST_RISK_BPS", 5.0),
        }
    }

    pub fn total_bps(&self) -> f64 {
        self.gas_bps
            + self.flash_fee_bps
            + self.execution_buffer_bps
            + self.competition_bid_bps
            + self.risk_premium_bps
    }

    /// Net edge after everything. `gross_bps` must ALREADY be net of pool fees,
    /// which `rate_from_live` applies per hop -- adding them here as well would
    /// charge them twice.
    pub fn net_bps(&self, gross_bps: f64) -> f64 {
        gross_bps - self.total_bps()
    }

    /// Gas is a fixed cost, so its bps share depends on notional: the same
    /// trade is unprofitable small and profitable large. A ceiling expressed in
    /// bps alone cannot express that, which is the deeper reason the constant
    /// had to go.
    pub fn clears(&self, gross_bps: f64) -> bool {
        self.net_bps(gross_bps) > 0.0
    }
}

/// `U256` to `f64` for ratio arithmetic.
///
/// Via decimal string rather than `as_u128`, which truncates silently: a
/// `sqrtPriceX96` routinely exceeds 128 bits, and a truncated one produces a
/// plausible-looking wrong price rather than an obvious failure.
fn u256_to_f64(v: U256) -> Option<f64> {
    let f: f64 = v.to_string().parse().ok()?;
    f.is_finite().then_some(f)
}

/// Rate for one hop, priced from the fast path's OWN live state.
///
/// This is the plan's tier-1 local quote, and it needs no `Graph`: the feed
/// already maintains CL and V2 snapshots for every pool it subscribes to. A
/// shared graph would raise the same question sharing `LiveState` did, and that
/// cost 584 continuity breaks.
///
/// Returns `(numerator, denominator)` such that `num/den` is output-per-input
/// for `from -> to`.
///
/// Three ways this returns `None`, all deliberate:
/// - no pool serves the hop;
/// - every candidate pool's snapshot fails `may_price_locally` — §7's
///   "untrusted -> do not price locally", not "price it anyway";
/// - the hop's tokens do not match the pool's recorded pair, which would mean
///   pricing in an unknown direction.
///
/// Direction is the dangerous part: `sqrtPriceX96` gives token1-per-token0, so
/// inverting it does not fail, it silently reports the reciprocal — and a
/// reciprocal rate around a loop manufactures exactly the phantom edge the
/// closing-hop test exists to catch.
/// Which pool to take when several serve the same hop.
///
/// `FirstMatch` is not a mode anyone should run: it exists so a single drain
/// can price the SAME cycle both ways and attribute how much of the reported
/// gross comes from the selection rule rather than from the market. Taking the
/// maximum of several noisy estimates is an upward-biased estimator, and
/// compounding that bias around a loop is indistinguishable from profit unless
/// the two are measured side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopSelect {
    BestNet,
    FirstMatch,
}

/// Everything a hop needs to be priced and chosen.
///
/// Bundled because these must agree across a whole cycle: pricing one hop at
/// the margin and its neighbour at size, or valuing hops against price maps
/// from two different scans, produces a number describing no market.
#[derive(Clone, Copy)]
pub struct PricingCtx<'a> {
    pub fresh: Freshness,
    pub select: HopSelect,
    pub prices: Option<&'a TokenPrices>,
    /// Native-denominated probe size for impact-aware pricing. `0.0` prices at
    /// the margin, which is the old behaviour and is what made a pool holding
    /// forty dollars look identical to one holding four million.
    pub ref_native: f64,
}

impl PricingCtx<'_> {
    /// The probe size in raw units of `token`, or `0.0` when it has no price.
    ///
    /// Falling back to the margin rather than guessing a size: a made-up
    /// notional would produce a made-up impact, and an unpriced token is
    /// exactly the case where that error would be largest.
    fn reference_in(&self, token: Address) -> f64 {
        if self.ref_native <= 0.0 {
            return 0.0;
        }
        match self.prices.and_then(|p| p.get(&token)).copied() {
            Some(px) if px.is_finite() && px > 0.0 => {
                let r = self.ref_native / px;
                if r.is_finite() && r > 0.0 { r } else { 0.0 }
            }
            _ => 0.0,
        }
    }
}

/// Freshness bound for local pricing: refuse any pool unchecked for longer.
#[derive(Debug, Clone, Copy)]
pub struct Freshness {
    pub now: Instant,
    pub ttl: Duration,
}

impl Freshness {
    fn allows(&self, confirmed_at: Option<Instant>) -> bool {
        match confirmed_at {
            Some(t) => self.now.saturating_duration_since(t) <= self.ttl,
            None => false,
        }
    }
}

pub fn rate_from_live(
    live: &LiveState,
    candidate_pools: &[Address],
    pool_tokens: &dashmap::DashMap<Address, PoolMeta>,
    from: Address,
    to: Address,
    ctx: PricingCtx<'_>,
) -> Option<(f64, f64)> {
    hop_quote_from_live(live, candidate_pools, pool_tokens, from, to, ctx)
        .map(|q| (q.num, q.den))
}

/// Choose the pool that serves this hop best, and keep the depth behind it.
///
/// The choice is made on the rate AT `ctx.ref_native` OF SIZE, not at the
/// margin. That is the whole point. A marginal rate says nothing about how much
/// a pool can absorb, so maximising it picks whichever pool is most mispriced --
/// reliably the thinnest, because thin pools stay mispriced precisely because
/// nobody bothers to correct them. Measured 2026-09-03: choosing on marginal
/// rate accounted for 80.6% of all reported gross, a median 48 bps of pure
/// selection artefact on the same cycle.
///
/// At a real size the same comparison penalises thin pools on its own: as the
/// probe approaches a pool's input reserve the effective rate collapses. No
/// separate depth heuristic is needed, and none is used -- depth still only
/// BOUNDS the cycle afterwards.
///
/// With `ref_native = 0`, or for a token with no price, this degrades exactly
/// to the old marginal behaviour rather than to a guess.
pub fn hop_quote_from_live(
    live: &LiveState,
    candidate_pools: &[Address],
    pool_tokens: &dashmap::DashMap<Address, PoolMeta>,
    from: Address,
    to: Address,
    ctx: PricingCtx<'_>,
) -> Option<HopQuote> {
    let reference_in = ctx.reference_in(from);
    let mut best: Option<HopQuote> = None;
    let mut best_rate = f64::NEG_INFINITY;
    for pool in candidate_pools {
        let Some(meta) = pool_tokens.get(pool) else {
            continue;
        };
        if !ctx.fresh.allows(meta.confirmed_at) {
            continue;
        }
        let Some(q) = hop_rate(live, *pool, &meta, from, to, reference_in) else {
            continue;
        };
        if ctx.select == HopSelect::FirstMatch {
            return Some(q);
        }
        let rate = q.num / q.den;
        // `>` not `>=`: on a tie the first pool wins, which keeps the result
        // independent of the order `pools_for_hop` happens to return.
        if rate.is_finite() && rate > best_rate {
            best_rate = rate;
            best = Some(q);
        }
    }
    best
}

/// One pool's rate for one hop, net of that pool's fee.
///
/// `None` when the pool does not serve the hop, its snapshot is missing or
/// untrusted, or its curve is one this function cannot price.
fn hop_rate(
    live: &LiveState,
    pool: Address,
    meta: &PoolMeta,
    from: Address,
    to: Address,
    reference_in: f64,
) -> Option<HopQuote> {
    use crate::live_state::may_price_locally;
    let (token0, token1) = (meta.token0, meta.token1);
    // Charged once, here, so `gross_bps` downstream is already net of pool
    // fees. A cycle priced from raw ratios overstates every hop.
    let keep = 1.0 - (f64::from(meta.fee_ppm) / 1_000_000.0);
    if !(0.0..=1.0).contains(&keep) {
        return None;
    }
    // A cheap structural guard: the pool must serve this pair at all. Which
    // way round it serves it is decided per curve below, because only the V2
    // snapshot carries an authoritative ordering.
    if !((from == token0 && to == token1) || (from == token1 && to == token0)) {
        return None;
    }
    let forward = from == token0;

    match meta.kind {
        PoolKind::ConcentratedLiquidity => {
            let snap = live.cl_snapshot(pool)?;
            if !may_price_locally(&snap.prov.trust) {
                return None;
            }
            let sp = u256_to_f64(snap.sqrt_price_x96)?;
            let root = sp / 2f64.powi(96);
            if !root.is_finite() || root <= 0.0 {
                return None;
            }
            // VIRTUAL reserves, and only for impact. `x = L/sqrt(P)`,
            // `y = L*sqrt(P)` describe the constant-product curve the pool is
            // tangent to, which is the correct local model for how far a swap
            // moves the price INSIDE the current tick. They are the wrong thing
            // entirely for capacity -- they run 0..infinity and overstate real
            // holdings by 16-56x -- so capacity still comes from `balances`
            // below, and the two must not be confused.
            let l = f64::from_bits(0) + snap.liquidity as f64;
            if !l.is_finite() || l <= 0.0 {
                return None;
            }
            let (v0, v1) = (l / root, l * root);
            let (r_in, r_out) = if forward { (v0, v1) } else { (v1, v0) };
            let (num, den) = effective_rate(r_in, r_out, keep, reference_in)?;
            // Unknown balances bound nothing: `INFINITY` lets another hop
            // bind the cycle, and `price_cycle_sized` refuses a cycle no hop
            // bounds at all rather than calling it infinitely large.
            let cap_out = match meta.balances {
                Some((b0, b1)) => {
                    let c = if forward { b1 } else { b0 };
                    if c.is_finite() && c > 0.0 { c } else { return None }
                }
                None => f64::INFINITY,
            };
            Some(HopQuote { num, den, cap_out })
        }
        PoolKind::ConstantProduct => {
            let snap = live.v2_snapshot(pool)?;
            if !may_price_locally(&snap.prov.trust) {
                return None;
            }
            // The direction must come from a CHAIN-read pair. Config lists
            // every Aerodrome pair in both directions and keeps whichever line
            // came first, so its `token0` need not be the token `reserve0`
            // counts. An inverted V2 rate does not fail loudly -- it returns the
            // reciprocal, and a reciprocal around a loop manufactures a phantom
            // edge. Unverified is REFUSED; the seed flips this within a pass.
            if !meta.verified {
                return None;
            }
            let (r0, r1) = (
                u256_to_f64(snap.state.reserve0)?,
                u256_to_f64(snap.state.reserve1)?,
            );
            // Explicit, not `!(r > 0.0)`: NaN must reject and a negated
            // partial comparison hides that.
            if !(r0.is_finite() && r1.is_finite()) || r0 <= 0.0 || r1 <= 0.0 {
                return None;
            }
            let (r_in, r_out) = if forward { (r0, r1) } else { (r1, r0) };
            let (num, den) = effective_rate(r_in, r_out, keep, reference_in)?;
            // A V2 pool's reserves are its holdings, so the output side is
            // the bound directly -- and it is live, not rotation-stale.
            Some(HopQuote { num, den, cap_out: r_out })
        }
        // `r1/r0` is not this curve's marginal price. Applying it to a stable
        // pool produced a 163% phantom edge in the 2026-09-01 spread census,
        // and the correct form needs each token's decimals, which `PoolMeta`
        // does not carry. Refusing costs coverage on the deepest pools on Base;
        // pricing it wrongly costs money.
        PoolKind::StableSwap => None,
    }
}

/// Output per unit input for a constant-product hop of size `reference_in`.
///
/// `out = r_out * (x*keep) / (r_in + x*keep)`, so the rate is
/// `r_out*keep / (r_in + x*keep)`. At `x = 0` this is the marginal rate and the
/// impact term vanishes, which is what makes a zero reference size mean
/// "price at the margin" rather than "price wrong".
///
/// This is the whole fix for pool CHOICE. A marginal rate says nothing about
/// how much a pool can absorb, so maximising it picks whichever pool is most
/// mispriced -- reliably the thinnest one, because thin pools are mispriced
/// precisely because nobody bothers to correct them. At a real size the same
/// comparison penalises thin pools automatically: as `x` approaches `r_in` the
/// effective rate collapses toward zero.
fn effective_rate(r_in: f64, r_out: f64, keep: f64, reference_in: f64) -> Option<(f64, f64)> {
    if !(r_in.is_finite() && r_out.is_finite()) || r_in <= 0.0 || r_out <= 0.0 {
        return None;
    }
    let x = if reference_in.is_finite() && reference_in > 0.0 { reference_in } else { 0.0 };
    let den = r_in + x * keep;
    if !den.is_finite() || den <= 0.0 {
        return None;
    }
    Some((r_out * keep, den))
}

/// Simulate one call against preconfirmed state.
///
/// Returns `Err` only for transport failures. A response that arrives and says
/// the call reverted is `Ok(PreconfSimResult { success: false, .. })` — a
/// revert is information, and collapsing it into an error throws away the
/// reason.
pub async fn simulate_preconf(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    params: Value,
) -> anyhow::Result<PreconfSimResult> {
    use ethers::providers::Middleware;
    let raw: Value = http
        .provider()
        .request("eth_simulateV1", params)
        .await
        .map_err(|e| anyhow::anyhow!("eth_simulateV1 transport: {e}"))?;
    Ok(parse_simulate_v1(&raw))
}

/// One hop's rate and the depth behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HopQuote {
    pub num: f64,
    pub den: f64,
    /// Raw units of the OUTPUT token the pool actually holds.
    ///
    /// `f64::INFINITY` means unknown, and that has to be a deliberate choice at
    /// the call site rather than a default: an unbounded hop cannot bind the
    /// cycle's size, so an unknown depth silently makes a cycle look as large
    /// as its second-thinnest hop allows.
    pub cap_out: f64,
}

/// Share of a pool's holdings treated as reachable.
///
/// NOT a sizing model -- `prepare_candidate` owns that, with real quotes. This
/// is a bound whose only job is to stop the ranker preferring dust. Taking a
/// whole pool would move its price to the point where the edge is gone, so a
/// small fraction is the honest reading of "how much of this pool is usable".
pub const DEPTH_FRACTION: f64 = 0.01;

/// What one cycle looks like once depth is accounted for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizedCycle {
    /// Product of the loop's rates minus one, in bps. Unchanged by depth.
    pub gross_bps: f64,
    /// Largest input, in units of `tokens[0]`, that no hop's depth forbids.
    pub notional_in: f64,
}

/// Price a loop AND bound how much it can carry.
///
/// The size question is the one bps cannot answer. Walking forward with input
/// `x`, the amount arriving at hop `i` is `x * R_i` where `R_i` is the product
/// of rates up to that hop, and it cannot exceed what that pool holds of the
/// token it must pay out. So `x <= cap_i / R_i` for every hop, and the binding
/// one is the minimum.
///
/// This is what separates a 12 bps edge on a deep pool from a 120 bps edge on
/// one holding forty dollars. Measured on 2026-09-03, ranking without it made
/// the selection rule responsible for 70.8% of reported gross: taking the best
/// rate at each hop reliably found the thinnest pool on every pair.
pub fn price_cycle_sized<F>(tokens: &[Address], quote_of: F) -> Option<SizedCycle>
where
    F: Fn(Address, Address) -> Option<HopQuote>,
{
    if tokens.len() < 2 {
        return None;
    }
    let mut product = 1.0f64;
    let mut notional = f64::INFINITY;
    for i in 0..tokens.len() {
        let from = tokens[i];
        let to = tokens[(i + 1) % tokens.len()];
        let q = quote_of(from, to)?;
        if !q.den.is_finite() || !q.num.is_finite() || q.den <= 0.0 {
            return None;
        }
        product *= q.num / q.den;
        if !product.is_finite() || product <= 0.0 {
            return None;
        }
        // `cap_out` is a bound on the amount LEAVING this hop, and `product`
        // is exactly the amount leaving it per unit of input.
        if q.cap_out.is_finite() {
            let allowed = q.cap_out * DEPTH_FRACTION / product;
            if allowed.is_finite() {
                notional = notional.min(allowed);
            }
        }
    }
    // An entirely unbounded cycle is not "infinitely large", it is unmeasured.
    // Reporting infinity here would rank it above every real opportunity.
    if !notional.is_finite() || notional <= 0.0 {
        return None;
    }
    Some(SizedCycle {
        gross_bps: (product - 1.0) * 10_000.0,
        notional_in: notional,
    })
}

/// A cycle priced from cached edges, before costs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricedCycle {
    pub id: crate::cycle_index::CycleId,
    /// Product of the loop's rates minus one, in basis points. GROSS: no fees
    /// beyond what the edge rates already carry, no gas, no flash fee. A
    /// positive value is a candidate, not a profit.
    pub gross_bps: f64,
    pub hops: usize,
    /// Largest input, in raw units of the start token, that no hop's depth
    /// forbids.
    pub notional_in: f64,
    /// `notional_in * gross` converted to native units, or `None` when the
    /// start token has no reliable price. A percentage cannot be compared
    /// across cycles that start in different tokens; this can.
    pub profit_native: Option<f64>,
}

/// What a ranking was actually sorted by.
///
/// Reported because the two are not interchangeable and a run sorted by
/// percentage must never be read as one sorted by value. Percentage ranking is
/// what made the selection rule responsible for 70.8% of reported gross.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankBasis {
    /// Expected gross profit in native units.
    Native,
    /// Basis points only: no start token had a reliable price.
    Bps,
}

/// Price one token loop from cached edges.
///
/// `tokens` is the OPEN form the index stores — the closing hop back to
/// `tokens[0]` is implied, and pricing it is not optional: a loop priced
/// without its closing hop is not a cycle, it is a path, and its product means
/// nothing.
///
/// Returns `None` when ANY hop has no edge. That is the §7 rule applied here:
/// unknown state rejects rather than approximating. Substituting a default rate
/// for a missing hop is precisely how this project manufactured phantom
/// profits before -- a quote claiming more output than the pool held.
pub fn price_cycle<F>(tokens: &[Address], rate_of: F) -> Option<f64>
where
    F: Fn(Address, Address) -> Option<(f64, f64)>,
{
    if tokens.len() < 2 {
        return None;
    }
    let mut product = 1.0f64;
    for i in 0..tokens.len() {
        let from = tokens[i];
        let to = tokens[(i + 1) % tokens.len()];
        let (num, den) = rate_of(from, to)?;
        // Explicit rather than `!(den > 0.0)`: NaN must reject, and a negated
        // partial comparison hides that intent.
        if !den.is_finite() || !num.is_finite() || den <= 0.0 {
            return None;
        }
        product *= num / den;
        if !product.is_finite() {
            return None;
        }
    }
    Some((product - 1.0) * 10_000.0)
}

/// Price the touched cycles and return the best `top_n`, most profitable first.
///
/// Cycles that cannot be priced are DROPPED, and the count is returned
/// separately: a cycle silently missing from the output is indistinguishable
/// from one that priced badly, and those need different responses -- the first
/// is a coverage gap, the second is the market.
pub fn price_touched<F>(
    index: &crate::cycle_index::CycleIndex,
    ids: &[crate::cycle_index::CycleId],
    quote_of: F,
    prices: Option<&std::collections::HashMap<Address, f64>>,
    top_n: usize,
) -> (Vec<PricedCycle>, usize, RankBasis)
where
    F: Fn(Address, Address) -> Option<HopQuote>,
{
    let mut priced = Vec::with_capacity(ids.len().min(top_n * 4));
    let mut unpriceable = 0usize;
    for id in ids {
        let Some(cycle) = index.cycle(*id) else {
            unpriceable += 1;
            continue;
        };
        match price_cycle_sized(&cycle.tokens, &quote_of) {
            Some(sized) => {
                let start = cycle.tokens[0];
                let profit_native = prices
                    .and_then(|p| p.get(&start).copied())
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .map(|px| sized.notional_in * (sized.gross_bps / 10_000.0) * px)
                    .filter(|v| v.is_finite());
                priced.push(PricedCycle {
                    id: *id,
                    gross_bps: sized.gross_bps,
                    hops: cycle.tokens.len(),
                    notional_in: sized.notional_in,
                    profit_native,
                });
            }
            None => unpriceable += 1,
        }
    }

    // Value if any cycle can be valued, percentage only if none can. Mixing the
    // two would rank a large number of a worthless token above a small number
    // of a valuable one, which is the failure this replaces in a new costume.
    let basis = if priced.iter().any(|c| c.profit_native.is_some()) {
        RankBasis::Native
    } else {
        RankBasis::Bps
    };
    priced.sort_by(|a, b| {
        let key = |c: &PricedCycle| match basis {
            // Unvalued cycles sort last under a Native basis rather than being
            // dropped: they are a coverage gap in the price map, not bad trades.
            RankBasis::Native => c.profit_native.unwrap_or(f64::NEG_INFINITY),
            RankBasis::Bps => c.gross_bps,
        };
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            // Shorter loops break ties: less gas, fewer legs to fail.
            .then(a.hops.cmp(&b.hops))
    });
    priced.truncate(top_n);
    (priced, unpriceable, basis)
}

/// Translate priced cycles into the indexed form `prepare_candidate` takes.
///
/// **No profitability filter.** An earlier version gated this on
/// `CostStack::clears`, which was wrong for a reason worth stating: gas is a
/// fixed native cost, so its share of a trade in basis points depends entirely
/// on notional. A 12 bps edge on a $2M-deep pool is worth far more than a 40 bps
/// edge on a $5k one, and a flat 32 bps bar deletes exactly the first kind. The
/// fast path RANKS; `prepare_candidate` does the real sizing, the real flash
/// fee, the real gas and the real minimum-profit test.
///
/// Untranslatable cycles are counted, not dropped silently: a cycle the graph
/// cannot express is a coverage gap between the fast index and the scan graph,
/// and it needs a different response from a cycle that simply priced badly.
pub fn translate_for_prep(
    index: &crate::cycle_index::CycleIndex,
    graph: &crate::graph::Graph,
    priced: &[PricedCycle],
) -> (Vec<(PricedCycle, crate::graph::IndexedCycle)>, usize) {
    let mut out = Vec::with_capacity(priced.len());
    let mut untranslatable = 0usize;
    for c in priced {
        let Some(tokens) = index.cycle(c.id).map(|t| t.tokens.clone()) else {
            untranslatable += 1;
            continue;
        };
        match graph.indexed_cycle_for_tokens(&tokens) {
            Some(ic) => out.push((*c, ic)),
            None => untranslatable += 1,
        }
    }
    (out, untranslatable)
}

/// What the sink did with one batch of ranked candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrepReport {
    /// Candidates that came back sized, with a plan.
    pub sized: usize,
    /// Candidates the economics rejected. The expected outcome, not a fault.
    pub rejected: usize,
    /// Candidates the quote budget ran out before reaching.
    pub budgeted: usize,
    /// Sized candidates whose executor calldata simulated successfully against
    /// preconfirmed state.
    pub sim_ok: usize,
    /// Sized candidates whose simulation reverted. A real answer about a real
    /// candidate, unlike the empty probe this replaced.
    pub sim_failed: usize,
    /// Simulation round-trip microseconds, summed, and the count -- so the mean
    /// is derivable without a histogram.
    pub sim_micros: u64,
    pub sim_samples: u64,
    /// Gas the successful simulations actually reported.
    pub gas_used: u64,
    /// The scan has not published a pricing context yet, so nothing could be
    /// prepared at all. Distinct from `rejected`: one is an answer and the
    /// other is the absence of one.
    pub no_context: bool,
}

/// A value the scan publishes once per pass for readers running between scans.
///
/// `Option` because nothing is published until the first scan completes, and
/// the inner `Arc` so a reader takes a consistent snapshot without holding the
/// lock across its work.
pub type Published<T> = Arc<StdMutex<Option<Arc<T>>>>;

/// Native (wei) per RAW unit of each token.
pub type TokenPrices = std::collections::HashMap<Address, f64>;

/// The scan-published inputs the drain reads each pass.
pub struct DrainFeeds {
    pub graph: Published<crate::graph::Graph>,
    /// Without prices, cycles starting in different tokens can only be compared
    /// by percentage -- which is the thing depth ranking exists to stop.
    pub prices: Published<TokenPrices>,
}

/// Where the fast path hands its ranked candidates.
///
/// A callback because `prepare_candidate` lives on the binary's `Runner` and
/// this module is in the library. The separation is worth having regardless:
/// the fast path's job ends at "these cycles, in this order", and every
/// question about sizing, flash fees, gas and minimum profit belongs to
/// whoever implements this.
/// The `Arc<Graph>` is passed IN, never re-read by the implementation. Node
/// indices are assigned in first-seen order and the scan rebuilds the graph
/// every pass, so an `IndexedCycle` translated against one graph silently
/// repoints if it is resolved against the next one.
pub type CandidateSink = Arc<
    dyn Fn(
            Arc<crate::graph::Graph>,
            Vec<(PricedCycle, crate::graph::IndexedCycle)>,
        ) -> futures_util::future::BoxFuture<'static, PrepReport>
        + Send
        + Sync,
>;

/// How many ranked cycles are handed to `prepare_candidate` each drain.
///
/// Was 4 behind a 32 bps gate that cleared nothing for a whole run. The gate is
/// gone, so the depth has to carry the selection instead — 8 gives the real
/// sizing something to choose between without turning each drain into eight
/// quote round trips.
pub const FAST_PATH_RANKED: usize = 8;

/// Result of simulating a candidate against preconfirmed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreconfSimResult {
    pub success: bool,
    pub gas_used: u64,
    /// Revert reason or error text when `success` is false. Kept because a
    /// candidate that fails simulation is the cheapest signal available about
    /// WHY, and discarding it turns a diagnosable reject into a silent one.
    pub failure: Option<String>,
}

/// `eth_simulateV1` request body for one candidate against `"pending"`.
///
/// Split out so the payload shape is testable without a provider. Verified
/// against the configured provider 2026-09-02: `eth_simulateV1` and
/// `eth_estimateGas`, both with a `"pending"` block parameter, are supported —
/// checked before writing this, because the plan names a different endpoint for
/// production than the public preconf RPC that was verified earlier, and
/// assuming the two have the same capabilities is how the last endpoint
/// mistake happened.
///
/// `validation: true` matters: without it the node skips the checks that make a
/// pass mean the transaction would actually be accepted, which turns the
/// simulation into an expensive no-op.
pub fn simulate_v1_params(
    from: Address,
    to: Address,
    data: &[u8],
    max_fee_per_gas: u128,
) -> Value {
    json!([
        {
            "blockStateCalls": [{
                "calls": [{
                    "from": format!("{from:#x}"),
                    "to": format!("{to:#x}"),
                    "data": format!("0x{}", hex::encode(data)),
                    "maxFeePerGas": format!("{max_fee_per_gas:#x}"),
                }],
                "stateOverrides": {},
            }],
            "validation": true,
            "traceTransfers": false,
        },
        "pending"
    ])
}

/// Interpret an `eth_simulateV1` response for a single call.
///
/// A missing or malformed response is a FAILURE, never a pass. Treating an
/// unparseable simulation as success is how an unverified candidate reaches
/// broadcast, which is the one thing simulation exists to prevent.
pub fn parse_simulate_v1(value: &Value) -> PreconfSimResult {
    let call = value
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|b| b.get("calls"))
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());
    let Some(call) = call else {
        return PreconfSimResult {
            success: false,
            gas_used: 0,
            failure: Some("no call result in eth_simulateV1 response".into()),
        };
    };
    let status = call.get("status").and_then(|v| v.as_str()).unwrap_or("0x0");
    let gas_used = call
        .get("gasUsed")
        .and_then(|v| v.as_str())
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);
    let success = status == "0x1";
    PreconfSimResult {
        success,
        gas_used,
        failure: if success {
            None
        } else {
            Some(
                call.get("error")
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| format!("status {status}")),
            )
        },
    }
}

/// What one flashblock's worth of dirty pools resolved to.
///
/// Deliberately reports what was DROPPED as well as what was selected: a hot
/// path that returns 32 cycles without saying it considered 400 reads as
/// exhaustive, and the whole point of the cap is that it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedCycles {
    /// Cycles to reprice, shortest first.
    pub cycles: Vec<crate::cycle_index::CycleId>,
    /// Cycles touched in total, before the cap.
    pub total_touched: usize,
    /// Dirty pools the universe could not resolve to a token hop. Non-zero
    /// means the fast path is marking pools the search graph does not know
    /// about, so their moves can never become candidates.
    pub unresolved_pools: usize,
}

/// Dirty pools -> token hops -> affected cycles.
///
/// This is the join the whole fast path exists for: `pendingLogs` reports which
/// POOL moved, `CycleIndex` is keyed by token HOP, and the canonical loop
/// bridged them by rebuilding everything. Both hop directions are queried
/// because a pool serves an unordered pair while cycles traverse a direction.
///
/// Pure and synchronous on purpose — no quoting, no RPC. It is the measurable
/// stage between "a log arrived" and "there is something to price", and keeping
/// it free of I/O is what makes that measurement mean anything.
pub fn touched_cycles(
    universe: &crate::cycle_index::PoolUniverse,
    index: &crate::cycle_index::CycleIndex,
    dirty: &HashSet<Address>,
    max_cycles: usize,
) -> TouchedCycles {
    let mut hops: Vec<(Address, Address)> = Vec::with_capacity(dirty.len() * 2);
    let mut unresolved = 0usize;
    for pool in dirty {
        match universe.pair_of(*pool) {
            Some((a, b)) => {
                hops.push((a, b));
                hops.push((b, a));
            }
            None => unresolved += 1,
        }
    }
    let (cycles, total_touched) = index.cycles_touching_limited(hops, max_cycles);
    TouchedCycles {
        cycles,
        total_touched,
        unresolved_pools: unresolved,
    }
}

/// Drain the dirty set and resolve it, timing the whole stage.
///
/// The drain is a single atomic swap, so a log landing mid-drain is not lost.
/// Returns the elapsed time because "flashblock -> candidate" is the first row
/// of the acceptance table and nothing else measures it.
pub fn drain_and_resolve(
    touched: &StdMutex<HashSet<Address>>,
    universe: &crate::cycle_index::PoolUniverse,
    index: &crate::cycle_index::CycleIndex,
    max_cycles: usize,
) -> (TouchedCycles, usize, Duration) {
    let started = Instant::now();
    let dirty = match touched.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(p) => std::mem::take(&mut *p.into_inner()),
    };
    let pools = dirty.len();
    let out = touched_cycles(universe, index, &dirty, max_cycles);
    (out, pools, started.elapsed())
}

/// Decode-only helper: which monitored pool a log belongs to, if any.
///
/// `pendingLogs` is filtered server-side, but a provider that ignores the
/// address filter would silently widen the feed to the whole chain. Checking
/// membership locally makes that a dropped log rather than an unbounded decode
/// cost.
pub fn is_monitored(log: &Log, pools: &HashSet<Address>) -> bool {
    pools.contains(&log.address)
}

/// Topics this feed subscribes to, for assertions and diagnostics.
pub fn subscribed_topics() -> Vec<H256> {
    crate::log_decode::monitored_topics()
}

impl BaseFastPath {
    /// Endpoints for rebuilding the transport, and the backoff between tries.
    ///
    /// Without these the task holds one `Provider<Ws>` for the life of the
    /// process, and once that socket is closed every resubscribe is against a
    /// corpse. The pool monitor shipped exactly that bug: it could detect the
    /// stall and never recover from it.
    /// Ordered `(token0, token1)` per pool, for directional pricing.
    ///
    /// Empty means no cycle can be priced -- every hop rejects rather than
    /// guessing a direction.
    pub fn with_pool_tokens(
        mut self,
        tokens: std::collections::HashMap<Address, PoolMeta>,
    ) -> Self {
        self.pool_tokens = Arc::new(tokens.into_iter().collect());
        self
    }

    /// The pool metadata, for the drain loop's pricing closure.
    pub fn pool_tokens(&self) -> Arc<dashmap::DashMap<Address, PoolMeta>> {
        Arc::clone(&self.pool_tokens)
    }

    /// Endpoint for the preconfirmed-simulation round trip.
    pub fn with_sim_http(mut self, url: &str) -> Self {
        self.sim_http = ethers::providers::Provider::<ethers::providers::Http>::try_from(url)
            .ok()
            .map(Arc::new);
        if self.sim_http.is_none() {
            warn!(url, "base fast path could not build a simulation provider");
        }
        self
    }

    pub fn with_ws_reconnect(mut self, endpoints: Vec<String>, backoff: Duration) -> Self {
        self.ws_endpoints = endpoints;
        self.ws_backoff = backoff;
        self
    }

    /// Fraction of the subscribed universe the pricer would accept right now.
    ///
    /// Goes through `cl_snapshot`/`v2_snapshot` and `may_price_locally` — the
    /// same two calls `rate_from_live` makes — so the gauge and the pricer
    /// cannot drift apart. A cheaper `tracked_cl().len()` would keep reading
    /// full coverage straight through a continuity break.
    pub fn coverage(&self) -> FastCoverage {
        FastCoverage {
            priceable: self.pools.iter().filter(|p| self.pool_priceable(**p)).count(),
            total: self.pools.len(),
        }
    }

    /// Mirrors `rate_from_live`'s acceptance test for a single pool, including
    /// its order: a pool holding an untrusted CL snapshot is rejected outright
    /// rather than falling through to the V2 branch, because a pool is one
    /// curve or the other and a fallthrough would price it on the wrong one.
    fn pool_priceable(&self, pool: Address) -> bool {
        use crate::live_state::may_price_locally;
        let Some(meta) = self.pool_tokens.get(&pool) else {
            // No metadata: `hop_rate` cannot price it in any direction.
            return false;
        };
        match meta.kind {
            PoolKind::ConcentratedLiquidity => self
                .live
                .cl_snapshot(pool)
                .is_some_and(|s| may_price_locally(&s.prov.trust)),
            // `verified` is checked HERE and not only in the pricer for a
            // reason worth naming. A hot V2 pool acquires a Derived snapshot
            // from its first Sync log, which would have satisfied a
            // trust-only test -- so coverage would have read 100% for exactly
            // the busiest pools while `hop_rate` refused every one of them for
            // want of a token order. A gauge that disagrees with the pricer is
            // worse than no gauge.
            PoolKind::ConstantProduct => {
                meta.verified
                    && self
                        .live
                        .v2_snapshot(pool)
                        .is_some_and(|s| may_price_locally(&s.prov.trust))
            }
            // Refused by `hop_rate` until the stable invariant is wired, so it
            // is not coverage no matter how good its state is.
            PoolKind::StableSwap => false,
        }
    }

    /// Seed local state from canonical reads at a SEALED block.
    ///
    /// Without this the fast path starts empty and learns a pool only when that
    /// pool happens to trade. Measured in the 2026-09-02 bridge run: **90.1% of
    /// touched cycles were unpriceable**, because a cycle needs every hop and
    /// per-pool coverage compounds — at 68% per pool a 6-hop loop prices 10% of
    /// the time (`0.68^6`). That exponent is why the acceptance bar is 95% per
    /// pool and not 80%: `0.95^6` is 74%, `0.80^6` is 26%.
    ///
    /// Sealed, never `"pending"`: the loaders pin every sub-call to one block
    /// number so the whole seed is one consistent snapshot, and a pending block
    /// is not a stable pin.
    ///
    /// **Safe to run while the feed is live.** `anchor_*` refuses any pool
    /// already holding newer state, and `anchor_cl` replays deltas buffered
    /// during the read's flight, so a seed racing a preconfirmed log cannot roll
    /// state backwards.
    pub async fn seed_from_chain<C>(
        &self,
        provider: &Arc<ethers::providers::Provider<C>>,
        pools: &[Address],
        block: u64,
    ) -> SeedOutcome
    where
        C: ethers::providers::JsonRpcClient + Clone + Send + Sync + 'static,
    {
        let (cl_targets, v2_targets) = seed_targets(pools, &self.pool_tokens);
        let mut out = SeedOutcome { block, ..Default::default() };
        let at = ethers::types::U64::from(block);

        if !cl_targets.is_empty() {
            let states =
                crate::cl_sim::load_cl_pool_states_batched(Arc::clone(provider), &cl_targets, at)
                    .await;
            for (pool, _, _, _) in &cl_targets {
                match states.get(pool) {
                    Some(st) => {
                        // Compared BEFORE the anchor overwrites it. This is the
                        // only place local state and a sealed read exist side by
                        // side, and it answers the question age cannot: has the
                        // preconfirmed stream actually drifted?
                        if let (Some(local), Some(chain)) = (
                            self.live.cl_snapshot(*pool).and_then(|s| cl_price(s.sqrt_price_x96)),
                            cl_price(st.sqrt_price_x96),
                        ) {
                            if let Some(d) = divergence_bps(local, chain) {
                                out.compared += 1;
                                if d.abs() > DIVERGENCE_BPS {
                                    out.diverged += 1;
                                }
                                if d.abs() > out.max_divergence_bps.abs() {
                                    out.max_divergence_bps = d;
                                }
                            }
                        }
                        if self.live.anchor_cl(
                            *pool,
                            block,
                            st.sqrt_price_x96,
                            st.liquidity,
                            st.tick,
                        ) {
                            out.anchored += 1;
                        }
                        // Real holdings, for the depth bound. Fails closed:
                        // a balance read that did not come back leaves `None`,
                        // and `None` bounds nothing rather than bounding wrong.
                        let bal = match (st.balance0, st.balance1) {
                            (Some(b0), Some(b1)) => match (u256_to_f64(b0), u256_to_f64(b1)) {
                                (Some(a), Some(b)) => Some((a, b)),
                                _ => None,
                            },
                            _ => None,
                        };
                        if let Some(mut m) = self.pool_tokens.get_mut(pool) {
                            m.balances = bal;
                        }
                        self.mark_confirmed(*pool);
                    }
                    None => out.missing += 1,
                }
            }
        }

        if !v2_targets.is_empty() {
            let states = crate::quote_univ2::load_pair_states_batched(
                Arc::clone(provider),
                &v2_targets,
                at,
            )
            .await;
            for pool in &v2_targets {
                match states.get(pool) {
                    Some(st) => {
                        // The one place both orderings are in hand. Config
                        // lists every Aerodrome pair in BOTH directions and
                        // `or_insert` keeps whichever line came first, so this
                        // disagreement is possible and silent. Pricing uses the
                        // chain's answer either way; counting it is what makes
                        // a bad inventory visible instead of merely harmless.
                        // Applied BEFORE the anchor and regardless of whether
                        // it installs. A pool that traded in a preconfirmed
                        // block ahead of `block` has its anchor refused by the
                        // `supersedes` guard -- correctly, its state is newer --
                        // and those are precisely the hot pools. Tying the
                        // ordering to a successful anchor would leave the
                        // busiest pools permanently unpriceable.
                        if adopt_chain_pair(&self.pool_tokens, *pool, st.token0, st.token1) {
                            out.order_mismatch += 1;
                        }
                        if let (Some(local), Some(chain)) = (
                            self.live.v2_snapshot(*pool).and_then(|s| {
                                let r0 = u256_to_f64(s.state.reserve0)?;
                                let r1 = u256_to_f64(s.state.reserve1)?;
                                (r0 > 0.0).then(|| r1 / r0)
                            }),
                            (|| {
                                let r0 = u256_to_f64(st.reserve0)?;
                                let r1 = u256_to_f64(st.reserve1)?;
                                (r0 > 0.0).then(|| r1 / r0)
                            })(),
                        ) {
                            if let Some(d) = divergence_bps(local, chain) {
                                out.compared += 1;
                                if d.abs() > DIVERGENCE_BPS {
                                    out.diverged += 1;
                                }
                                if d.abs() > out.max_divergence_bps.abs() {
                                    out.max_divergence_bps = d;
                                }
                            }
                        }
                        if self.live.anchor_v2(*pool, block, st.clone()) {
                            out.anchored += 1;
                        }
                        self.mark_confirmed(*pool);
                    }
                    None => out.missing += 1,
                }
            }
        }
        out
    }

    /// Stamp a pool as confirmed against chain, as of now.
    fn mark_confirmed(&self, pool: Address) {
        if let Some(mut m) = self.pool_tokens.get_mut(&pool) {
            m.confirmed_at = Some(Instant::now());
        }
    }

    /// The `verify_batch` pools whose confirmation is oldest, excluding any
    /// already queued for repair.
    ///
    /// Oldest-first and bounded, so the whole universe rotates through
    /// verification at a fixed RPC cost rather than in one spike. A pool never
    /// confirmed sorts first: it has the weakest claim to being priced.
    fn oldest_unconfirmed(&self, skip: &HashSet<Address>, limit: usize) -> Vec<Address> {
        let mut aged: Vec<(Option<Instant>, Address)> = self
            .pools
            .iter()
            .filter(|p| !skip.contains(*p))
            .map(|p| (self.pool_tokens.get(p).and_then(|m| m.confirmed_at), *p))
            .collect();
        // `None` first, then oldest `Some` first.
        aged.sort_by(|a, b| match (a.0, b.0) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => x.cmp(&y),
        });
        aged.into_iter().take(limit).map(|(_, p)| p).collect()
    }

    /// The freshness bound this path prices under.
    pub fn freshness(&self) -> Freshness {
        Freshness { now: Instant::now(), ttl: self.verify_ttl }
    }

    /// Seed at startup, then keep repairing coverage against sealed blocks.
    ///
    /// Runs on its own task with its own RPC budget. The feed must never block
    /// on a read: a socket task waiting on a 29-multicall seed stops draining,
    /// and a feed that falls behind reports stale state as fresh.
    ///
    /// Each pass targets only the pools the pricer currently REJECTS, so the
    /// first pass is the whole universe and later ones are the handful that
    /// went untrusted. That bounds the steady-state cost to roughly nothing
    /// while still converging after a gap, which invalidates everything at once.
    ///
    /// What this does NOT do: detect a preconfirmed snapshot that is *present
    /// and wrong*. Anchoring at sealed block N is refused for any pool already
    /// holding state from N+1, which is exactly the pool a divergence check
    /// would care about. Measuring that is `state_validation`'s comparison, and
    /// pointing it at this state is a separate change.
    pub fn spawn_reconcile<C>(
        self: Arc<Self>,
        provider: Arc<ethers::providers::Provider<C>>,
        cadence: Duration,
    ) -> JoinHandle<()>
    where
        C: ethers::providers::JsonRpcClient + Clone + Send + Sync + 'static,
    {
        tokio::spawn(async move {
            let mut tick = interval(cadence);
            loop {
                tick.tick().await;
                let block = match provider.get_block_number().await {
                    Ok(b) => b.as_u64(),
                    Err(err) => {
                        warn!(error = %err, "fast path reconcile: no block number");
                        continue;
                    }
                };
                let broken: Vec<Address> = self
                    .pools
                    .iter()
                    .copied()
                    .filter(|p| !self.pool_priceable(*p))
                    .collect();
                // Plus a rotating slice of pools that ARE priceable but have
                // gone longest without a check. Repairing only what is already
                // broken leaves an unchecked assumption standing indefinitely:
                // the continuity cursor cannot see a missing log on a filtered
                // subscription, so a preconfirmed update that never lands in
                // the sealed block is wrong with no signal anywhere. This pass
                // is what turns that into a measured number.
                let skip: HashSet<Address> = broken.iter().copied().collect();
                let mut targets = broken;
                let rotate = self.oldest_unconfirmed(&skip, self.verify_batch);
                let rotated = rotate.len();
                targets.extend(rotate);
                if targets.is_empty() {
                    self.publish_coverage();
                    continue;
                }
                let started = Instant::now();
                let out = self.seed_from_chain(&provider, &targets, block).await;
                let cov = self.publish_coverage();
                info!(
                    target: "arb_exec::latency",
                    block = out.block,
                    requested = targets.len(),
                    rotated,
                    anchored = out.anchored,
                    missing = out.missing,
                    order_mismatch = out.order_mismatch,
                    compared = out.compared,
                    diverged = out.diverged,
                    max_divergence_bps = out.max_divergence_bps,
                    coverage_pct = cov.pct().round() as u64,
                    priceable = cov.priceable,
                    pools = cov.total,
                    seed_ms = started.elapsed().as_millis(),
                    "fast state reconcile"
                );
            }
        })
    }

    /// Publish coverage to the gauge and return it, so the caller logs exactly
    /// what was exported rather than recomputing and reporting a second number.
    fn publish_coverage(&self) -> FastCoverage {
        let cov = self.coverage();
        if let Some(m) = &self.metrics {
            m.fast_state_coverage_pct.set(cov.pct());
        }
        cov
    }

    /// Simulate one prepared candidate against preconfirmed state.
    ///
    /// `None` when no simulation endpoint is configured -- distinct from a
    /// simulation that ran and failed, which is `Some(success: false)` with the
    /// revert reason. Collapsing those would make a missing endpoint look like
    /// a bad candidate.
    pub async fn simulate_candidate(
        &self,
        from: Address,
        to: Address,
        data: &[u8],
        max_fee_per_gas: u128,
    ) -> Option<(PreconfSimResult, Duration)> {
        let http = self.sim_http.as_ref()?;
        let started = Instant::now();
        let params = simulate_v1_params(from, to, data, max_fee_per_gas);
        match simulate_preconf(http, params).await {
            Ok(r) => Some((r, started.elapsed())),
            Err(e) => {
                warn!(error = %e, "preconf simulation transport failed");
                None
            }
        }
    }

    /// Start the feed. Returns immediately; the socket runs on its own task.
    ///
    /// Also starts a reporter. `FastPathStats` was collected for two runs
    /// before anything printed it, so the receive->applied number the module
    /// exists to produce was invisible in both -- the same miss as adding a
    /// counter and then killing the process without scraping it.
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        let reporter = Arc::clone(&self);
        tokio::spawn(async move { reporter.report_loop().await });
        tokio::spawn(async move { self.run().await })
    }

    /// What the scan publishes for the drain to read.
    ///
    /// Grouped rather than passed separately because they must be read
    /// TOGETHER: a cycle translated against one graph and valued against a
    /// price map from a different scan is two inconsistent views of one market.
    pub fn drain_feeds(
        graph: Published<crate::graph::Graph>,
        prices: Published<TokenPrices>,
    ) -> DrainFeeds {
        DrainFeeds { graph, prices }
    }

    /// Drain the dirty set on a fixed cadence and resolve it to cycles.
    ///
    /// Logs go to `arb_exec::latency`, NOT a bare `latency` target. HANDOFF.md
    /// section 1 runs the bot with `RUST_LOG=arb_exec=info`, and an
    /// `EnvFilter` built from that directive enables only the `arb_exec` tree
    /// -- so every measurement this module produces was silently dropped by the
    /// project's own documented run recipe. Verified 2026-09-03: a startup run
    /// under that recipe emitted zero lines on the bare target.
    ///
    /// This is the join step: the feed writes dirty pools at 3us and, until
    /// now, nothing read them — `dirty_pools` climbed monotonically to 159 in a
    /// six-minute run because the set had a writer and no consumer.
    ///
    /// Timed and reported because "flashblock -> candidate" is the first row of
    /// the acceptance table, and the drain latency is the part this project can
    /// control. Repricing and execution hang off the returned cycle ids; they
    /// are not done here, so the measurement stays free of quoting cost.
    pub fn spawn_drain(
        self: Arc<Self>,
        universe: Arc<crate::cycle_index::PoolUniverse>,
        index: Arc<StdMutex<Option<crate::cycle_index::CycleIndex>>>,
        feeds: DrainFeeds,
        sink: Option<CandidateSink>,
        cadence: Duration,
        max_cycles: usize,
    ) -> JoinHandle<()> {
        let DrainFeeds { graph, prices } = feeds;
        tokio::spawn(async move {
            let costs = CostStack::from_env();
            // One batch in preparation at a time, and NEVER awaited on this
            // task. `prepare_candidate` is RPC-bound and one round trip to the
            // configured provider measured 250-293ms -- longer than the whole
            // drain cadence. Awaiting it here would stop the drain, and a feed
            // whose consumer has stopped reports stale state as fresh. So the
            // batch goes to its own task, and a drain arriving while one is
            // still in flight is DROPPED and counted: the newer flashblock
            // carries better prices than the one being worked on anyway.
            let prep_slot = Arc::new(tokio::sync::Semaphore::new(1));
            let prep_sized = Arc::new(AtomicU64::new(0));
            let prep_rejected = Arc::new(AtomicU64::new(0));
            let prep_no_context = Arc::new(AtomicU64::new(0));
            let sim_ok = Arc::new(AtomicU64::new(0));
            let sim_failed = Arc::new(AtomicU64::new(0));
            let sim_micros = Arc::new(AtomicU64::new(0));
            let sim_samples = Arc::new(AtomicU64::new(0));
            let mut prep_busy: u64 = 0;
            let mut prep_sent: u64 = 0;
            info!(
                gas_bps = costs.gas_bps,
                flash_fee_bps = costs.flash_fee_bps,
                execution_buffer_bps = costs.execution_buffer_bps,
                competition_bid_bps = costs.competition_bid_bps,
                risk_premium_bps = costs.risk_premium_bps,
                total_bps = costs.total_bps(),
                "base fast path cost stack (ASSUMED, not measured -- gas is a \
                 fixed cost and its bps share depends on notional)"
            );
            // Probe size for impact-aware pool choice, in wei of native.
            // Choosing on the MARGINAL rate accounted for 80.6% of reported
            // gross on 2026-09-03; choosing on the rate at a real size is what
            // removes that. Zero restores the marginal behaviour exactly.
            let ref_native = crate::util::env_parse_opt::<f64>("ARBOT_BASE_FAST_REF_NATIVE")
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(1e17); // 0.1 native
            info!(
                ref_native,
                "base fast path prices hops at this probe size, not at the margin"
            );
            let mut tick = interval(cadence);
            tick.tick().await; // immediate first tick; discard
            let mut drains: u64 = 0;
            let mut pools_total: u64 = 0;
            let mut cycles_total: u64 = 0;
            let mut micros_total: u64 = 0;
            let mut unresolved_total: u64 = 0;
            let mut capped: u64 = 0;
            loop {
                tick.tick().await;
                let Some(idx) = index.lock().ok().and_then(|g| g.clone()) else {
                    // No index yet: still drain, or the set grows unbounded
                    // while the graph is warming up.
                    if let Ok(mut g) = self.touched.lock() {
                        g.clear();
                    }
                    continue;
                };
                let (out, pools, elapsed) =
                    drain_and_resolve(&self.touched, &universe, &idx, max_cycles);
                if pools == 0 {
                    continue;
                }
                drains += 1;
                pools_total += pools as u64;
                cycles_total += out.cycles.len() as u64;
                unresolved_total += out.unresolved_pools as u64;
                micros_total += elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
                if out.total_touched > out.cycles.len() {
                    capped += 1;
                }
                // The join: cycle ids -> priced candidates, from this path's
                // own live state. No Graph, so no shared-state question.
                let priced_at = Instant::now();
                let fresh = self.freshness();
                let px = prices.lock().ok().and_then(|g| g.clone());
                let ctx = PricingCtx {
                    fresh,
                    select: HopSelect::BestNet,
                    prices: px.as_deref(),
                    ref_native,
                };
                let (priced, unpriceable, basis) = price_touched(
                    &idx,
                    &out.cycles,
                    |from, to| {
                        hop_quote_from_live(
                            &self.live,
                            universe.pools_for_hop(from, to),
                            &self.pool_tokens,
                            from,
                            to,
                            ctx,
                        )
                    },
                    px.as_deref(),
                    FAST_PATH_RANKED,
                );
                let price_us = priced_at.elapsed().as_micros();

                // The SAME winning cycle, priced again taking the first pool
                // that serves each hop instead of the best. Holding the cycle
                // fixed isolates the selection rule: any gap between these two
                // numbers is manufactured by taking a maximum over parallel
                // pools, not by the market. One cycle, so the cost is nothing.
                let first_match_bps = priced
                    .first()
                    .and_then(|c| idx.cycle(c.id))
                    .and_then(|c| {
                        price_cycle(&c.tokens, |from, to| {
                            rate_from_live(
                                &self.live,
                                universe.pools_for_hop(from, to),
                                &self.pool_tokens,
                                from,
                                to,
                                // Same context, ONE field different. Any gap
                                // between the two numbers is the selection rule
                                // and nothing else.
                                PricingCtx { select: HopSelect::FirstMatch, ..ctx },
                            )
                        })
                    })
                    .unwrap_or(f64::NAN);

                // Translate the RANKED candidates -- all of them, not the ones
                // clearing a bps bar. Reads an immutable snapshot the scan
                // publishes, never the live graph, which the scan rebuilds
                // underneath it.
                let snapshot = graph.lock().ok().and_then(|g| g.clone());
                let mut translated = 0usize;
                let mut untranslatable = 0usize;
                if let Some(g) = snapshot.as_ref() {
                    let (ready, bad) = translate_for_prep(&idx, g, &priced);
                    translated = ready.len();
                    untranslatable = bad;
                    if let (Some(sink), false) = (sink.as_ref(), ready.is_empty()) {
                        match Arc::clone(&prep_slot).try_acquire_owned() {
                            Ok(permit) => {
                                prep_sent += 1;
                                let call = sink(Arc::clone(g), ready);
                                let (sz, rj, nc) = (
                                    Arc::clone(&prep_sized),
                                    Arc::clone(&prep_rejected),
                                    Arc::clone(&prep_no_context),
                                );
                                let (so, sf, sm, ss) = (
                                    Arc::clone(&sim_ok),
                                    Arc::clone(&sim_failed),
                                    Arc::clone(&sim_micros),
                                    Arc::clone(&sim_samples),
                                );
                                tokio::spawn(async move {
                                    let r = call.await;
                                    sz.fetch_add(r.sized as u64, Ordering::Relaxed);
                                    rj.fetch_add(r.rejected as u64, Ordering::Relaxed);
                                    so.fetch_add(r.sim_ok as u64, Ordering::Relaxed);
                                    sf.fetch_add(r.sim_failed as u64, Ordering::Relaxed);
                                    sm.fetch_add(r.sim_micros, Ordering::Relaxed);
                                    ss.fetch_add(r.sim_samples, Ordering::Relaxed);
                                    if r.no_context {
                                        nc.fetch_add(1, Ordering::Relaxed);
                                    }
                                    drop(permit);
                                });
                            }
                            Err(_) => prep_busy += 1,
                        }
                    }
                }

                let best = priced.first().map(|c| c.gross_bps).unwrap_or(f64::NAN);
                // Net of everything the pool fees do not already cover. Pool
                // fees are inside gross_bps already; adding them here would
                // charge them twice.
                let clearing = priced.iter().filter(|c| costs.clears(c.gross_bps)).count();
                let best_net = priced
                    .first()
                    .map(|c| costs.net_bps(c.gross_bps))
                    .unwrap_or(f64::NAN);
                info!(
                    target: "arb_exec::latency",
                    dirty_pools = pools,
                    cycles = out.cycles.len(),
                    priced = priced.len(),
                    unpriceable,
                    best_gross_bps = best,
                    rank_basis = ?basis,
                    ref_native,
                    best_notional_in = priced.first().map(|c| c.notional_in).unwrap_or(f64::NAN),
                    best_profit_native = priced
                        .first()
                        .and_then(|c| c.profit_native)
                        .unwrap_or(f64::NAN),
                    unvalued = priced.iter().filter(|c| c.profit_native.is_none()).count(),
                    first_match_bps,
                    best_net_bps = best_net,
                    clearing_costs = clearing,
                    translated,
                    untranslatable,
                    graph_snapshot = snapshot.is_some(),
                    prep_sent,
                    prep_busy,
                    prep_sized = prep_sized.load(Ordering::Relaxed),
                    prep_rejected = prep_rejected.load(Ordering::Relaxed),
                    prep_no_context = prep_no_context.load(Ordering::Relaxed),
                    sim_ok = sim_ok.load(Ordering::Relaxed),
                    sim_failed = sim_failed.load(Ordering::Relaxed),
                    mean_sim_us = sim_micros.load(Ordering::Relaxed)
                        / sim_samples.load(Ordering::Relaxed).max(1),
                    price_us,
                    total_touched = out.total_touched,
                    unresolved_pools = out.unresolved_pools,
                    drain_us = elapsed.as_micros(),
                    mean_drain_us = micros_total / drains.max(1),
                    mean_cycles = cycles_total / drains.max(1),
                    mean_pools = pools_total / drains.max(1),
                    capped_drains = capped,
                    unresolved_total,
                    "flashblock to candidate"
                );
            }
        })
    }

    /// Print what the feed is actually doing, every 15s.
    ///
    /// Reports the DECLINE breakdown, not just the applies. A feed delivering
    /// thousands of events that apply almost none is the shape of the shared
    /// LiveState failure, and it is only distinguishable from a healthy feed by
    /// looking at the ratio.
    async fn report_loop(&self) {
        let mut tick = interval(Duration::from_secs(15));
        tick.tick().await; // immediate first tick; discard
        loop {
            tick.tick().await;
            let applied = self.stats.applied.load(Ordering::Relaxed);
            let declined = self.stats.declined.load(Ordering::Relaxed);
            let undecodable = self.stats.undecodable.load(Ordering::Relaxed);
            let seen = applied + declined + undecodable;
            if seen == 0 {
                warn!(
                    pools = self.pools.len(),
                    "base fast path has received NOTHING; the subscription is \
                     accepted but dead"
                );
                continue;
            }
            let dirty = self.touched.lock().map(|g| g.len()).unwrap_or(0);
            let cov = self.publish_coverage();
            info!(
                target: "arb_exec::latency",
                seen,
                coverage_pct = cov.pct().round() as u64,
                priceable = cov.priceable,
                applied,
                declined,
                undecodable,
                applied_pct = (applied as f64 * 100.0 / seen as f64).round() as u64,
                mean_apply_us = self.stats.mean_apply_micros().unwrap_or(0),
                dirty_pools = dirty,
                "base fast path"
            );
        }
    }

    async fn run(&self) {
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
        if !worth_subscribing(&self.pools) {
            warn!("base fast path has no pools; refusing to subscribe (pendingLogs with no \
                   address filter is every log on Base)");
            return;
        }
        loop {
            // Built fresh every iteration: a socket we suspect is dead must
            // never be reused, and only a new transport recovers one.
            let provider = {
                {
                    if self.ws_endpoints.is_empty() {
                        warn!("base fast path has no websocket endpoints; stopping");
                        return;
                    }
                    let connect = connect_ws_provider_with_fallbacks(
                        "base-fast-rpc",
                        &self.ws_endpoints,
                        self.ws_backoff,
                    );
                    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
                        Ok(Ok(p)) => Arc::new(p),
                        Ok(Err(err)) => {
                            warn!(error = %err, "base fast path websocket connect failed");
                            sleep(self.ws_backoff).await;
                            continue;
                        }
                        Err(_) => {
                            warn!("base fast path websocket connect timed out");
                            sleep(self.ws_backoff).await;
                            continue;
                        }
                    }
                }
            };

            let params = pending_logs_params(&self.pools);
            let mut sub = match provider
                .subscribe::<serde_json::Value, Log>(params)
                .await
            {
                Ok(s) => {
                    info!(
                        pools = self.pools.len(),
                        "base fast path subscribed to pendingLogs"
                    );
                    s
                }
                Err(err) => {
                    warn!(error = %err, "pendingLogs subscription failed");
                    self.note_gap();
                    sleep(self.ws_backoff).await;
                    continue;
                }
            };

            let connected_at = Instant::now();
            let mut last_event = connected_at;
            let mut events_seen: u64 = 0;
            let mut never_warned = false;
            // An `Interval`, never a `sleep` in the `select!`: select rebuilds
            // its branch futures each iteration, so a sleep restarts on every
            // log and never fires on a feed carrying hundreds per minute.
            let mut lifetime = interval(WS_MAX_CONNECTION_AGE);
            lifetime.tick().await; // immediate first tick; discard

            // Every exit below wants a FRESH transport: a stall and a stream
            // end are faults, and the rotation exists precisely to replace the
            // socket before the provider closes it. So the provider is never
            // put back, which also keeps `sub`'s borrow of it uncontested.
            loop {
                tokio::select! {
                    step = async { next_before_stall!(sub, SUBSCRIPTION_IDLE_TICK) } => {
                        match step {
                            StreamStep::Item(log) => {
                                events_seen = events_seen.saturating_add(1);
                                last_event = Instant::now();
                                self.apply(&log, last_event);
                            }
                            StreamStep::Ended => {
                                warn!("pendingLogs stream ended; reconnecting");
                                break;
                            }
                            StreamStep::Stalled => {
                                match idle_action(events_seen, last_event.elapsed(), connected_at.elapsed()) {
                                    IdleAction::Wait => {}
                                    IdleAction::WarnNeverDelivered => {
                                        if !never_warned {
                                            never_warned = true;
                                            warn!(
                                                pools = self.pools.len(),
                                                "pendingLogs subscribed but has delivered NOTHING; \
                                                 check the address filter and topics. Not \
                                                 reconnecting -- a new socket carries the same filter"
                                            );
                                        }
                                    }
                                    IdleAction::Reconnect => {
                                        warn!(
                                            idle_secs = last_event.elapsed().as_secs(),
                                            events_seen,
                                            "pendingLogs stalled; socket open but no longer \
                                             delivering. Reconnecting"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    _ = lifetime.tick() => {
                        warn!(
                            age_secs = connected_at.elapsed().as_secs(),
                            events_seen,
                            "rotating the base fast path socket before the provider closes it"
                        );
                        break;
                    }
                }
            }

            // Every exit is a hole in the log stream, so local state derived
            // from it can no longer be trusted.
            self.note_gap();
            sleep(self.ws_backoff).await;
        }
    }

    /// Invalidate local state after a break in the feed.
    ///
    /// The continuity cursor cannot detect a missing log — a filtered
    /// subscription has meaningless index gaps — so an interruption has to
    /// invalidate explicitly or snapshots keep their trust across the hole.
    fn note_gap(&self) {
        let epoch = self.live.break_continuity(UnknownReason::WsUnavailable);
        debug!(epoch, "base fast path gap; local state invalidated");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Bytes;

    /// Freshness that accepts anything a test has just confirmed. Tests about
    /// the TTL set their own.
    fn fresh() -> Freshness {
        Freshness { now: Instant::now(), ttl: Duration::from_secs(3600) }
    }

    /// The MARGINAL context: `ref_native` of zero. Most tests are about
    /// direction, fees or trust, and pricing those at size would fold impact
    /// into every expected value for no benefit. Tests about pool CHOICE set a
    /// real probe size, because that is the thing they are testing.
    fn marginal() -> PricingCtx<'static> {
        PricingCtx { fresh: fresh(), select: HopSelect::BestNet, prices: None, ref_native: 0.0 }
    }

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn sync_log(pool: Address, r0: u128, r1: u128, block: u64, li: u64) -> Log {
        let mut data = Vec::new();
        for v in [r0, r1] {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            data.extend_from_slice(&w);
        }
        Log {
            address: pool,
            topics: vec![*crate::log_decode::TOPIC_SOLIDLY_SYNC],
            data: Bytes::from(data),
            block_number: Some(block.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(li.into()),
            removed: Some(false),
            ..Default::default()
        }
    }

    fn fast(pools: Vec<Address>) -> (BaseFastPath, Arc<StdMutex<HashSet<Address>>>) {
        let touched = Arc::new(StdMutex::new(HashSet::new()));
        let f = BaseFastPath::new(
            FlashFeed::PendingLogs {
                ws_url: "wss://example.invalid".into(),
            },
            pools,
            Arc::new(LiveState::new()),
            touched.clone(),
            None,
        );
        (f, touched)
    }

    /// The filter must carry topics DERIVED from signatures. The pool monitor
    /// once shipped fabricated `topic0` values with correct prefixes and
    /// invented tails; it connected, logged success, and matched nothing for the
    /// life of the process.
    #[test]
    fn the_subscription_filter_carries_derived_topics() {
        let p = pending_logs_params(&[addr(1)]);
        assert_eq!(p[0], "pendingLogs");
        let topics = p[1]["topics"][0].as_array().expect("topic0 array");
        assert!(!topics.is_empty(), "an empty topic list subscribes to everything");
        let want = crate::log_decode::monitored_topics();
        assert_eq!(topics.len(), want.len());
        for t in &want {
            let hex = format!("{t:#x}");
            assert!(
                topics.iter().any(|v| v.as_str() == Some(hex.as_str())),
                "derived topic {hex} missing from the subscription"
            );
        }
    }

    /// `pendingLogs` with no address filter is every log on Base. Subscribing
    /// to that by accident would bury the decoder.
    #[test]
    fn an_empty_pool_set_never_subscribes() {
        assert!(!worth_subscribing(&[]));
        assert!(worth_subscribing(&[addr(1)]));
    }

    #[test]
    fn the_filter_pins_the_monitored_addresses() {
        let p = pending_logs_params(&[addr(1), addr(2)]);
        let addrs = p[1]["address"].as_array().expect("address array");
        assert_eq!(addrs.len(), 2);
    }

    /// The point of the module: a preconfirmed log lands in state AND marks the
    /// pool dirty, so the scan loop has something to react to.
    #[test]
    fn an_applied_log_marks_the_pool_dirty() {
        let pool = addr(7);
        let (f, touched) = fast(vec![pool]);
        let out = f.apply(&sync_log(pool, 10, 20, 100, 0), Instant::now());
        assert!(matches!(out, ApplyOutcome::Applied { .. }));
        assert!(touched.lock().unwrap().contains(&pool));
        assert_eq!(f.stats.applied.load(Ordering::Relaxed), 1);
    }

    /// A duplicate still means someone traded that pool. The scan loop wants to
    /// look at it even though our snapshot did not move, so a decline must
    /// still dirty the pool — dropping it would silently narrow the scan.
    #[test]
    fn a_declined_log_still_marks_the_pool_dirty() {
        let pool = addr(8);
        let (f, touched) = fast(vec![pool]);
        let log = sync_log(pool, 10, 20, 100, 0);
        f.apply(&log, Instant::now());
        touched.lock().unwrap().clear();

        let out = f.apply(&log, Instant::now());
        assert_eq!(out, ApplyOutcome::Duplicate);
        assert!(
            touched.lock().unwrap().contains(&pool),
            "a repeat trade on this pool is still news to the scan loop"
        );
        assert_eq!(f.stats.declined.load(Ordering::Relaxed), 1);
    }

    /// An unrecognised topic is a real signal — some venue emits something we do
    /// not decode — so it is counted separately and must NOT dirty the pool,
    /// because nothing about our state changed.
    #[test]
    fn an_undecodable_log_is_counted_and_does_not_dirty() {
        let pool = addr(9);
        let (f, touched) = fast(vec![pool]);
        let log = Log {
            address: pool,
            topics: vec![H256::repeat_byte(0xab)],
            data: Bytes::from(vec![0u8; 32]),
            block_number: Some(100u64.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(0u64.into()),
            removed: Some(false),
            ..Default::default()
        };
        assert_eq!(f.apply(&log, Instant::now()), ApplyOutcome::Undecodable);
        assert_eq!(f.stats.undecodable.load(Ordering::Relaxed), 1);
        assert!(touched.lock().unwrap().is_empty());
    }

    /// The latency counter is the whole justification for the module: it has to
    /// produce a number, or we cannot show the 21x gap closing.
    #[test]
    fn apply_latency_is_measured() {
        let pool = addr(10);
        let (f, _t) = fast(vec![pool]);
        assert_eq!(f.stats.mean_apply_micros(), None, "no samples yet");
        f.apply(&sync_log(pool, 1, 2, 100, 0), Instant::now());
        assert!(f.stats.mean_apply_micros().is_some());
    }

    /// A provider that ignored the address filter would widen this to the whole
    /// chain; membership is checked locally so that costs a drop, not a stall.
    #[test]
    fn logs_outside_the_monitored_set_are_identifiable() {
        let pools: HashSet<Address> = [addr(1), addr(2)].into_iter().collect();
        assert!(is_monitored(&sync_log(addr(1), 1, 2, 1, 0), &pools));
        assert!(!is_monitored(&sync_log(addr(99), 1, 2, 1, 0), &pools));
    }

    /// The join the fast path exists for: a dirty POOL becomes affected CYCLES
    /// without rebuilding the universe. Both hop directions must be queried --
    /// a pool serves an unordered pair, cycles traverse a direction.
    #[test]
    fn dirty_pools_resolve_to_the_cycles_that_traverse_them() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let (t1, t2, t3) = (addr(1), addr(2), addr(3));
        let (p12, p23, p31) = (addr(11), addr(12), addr(13));
        let universe = PoolUniverse::from_pools([(p12, t1, t2), (p23, t2, t3), (p31, t3, t1)]);
        let index = CycleIndex::build(&universe, &[t1], CycleIndexLimits::default());

        let dirty: HashSet<Address> = [p12].into_iter().collect();
        let out = touched_cycles(&universe, &index, &dirty, 32);
        assert!(
            !out.cycles.is_empty(),
            "the triangle traverses this pool's hop; it must be selected"
        );
        assert_eq!(out.unresolved_pools, 0);
        assert_eq!(out.total_touched, out.cycles.len());
    }

    /// A dirty pool the search graph does not know about can never become a
    /// candidate. Counting it is how that shows up as a number instead of as
    /// mysteriously absent opportunities.
    #[test]
    fn pools_the_universe_does_not_know_are_counted_not_hidden() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let universe = PoolUniverse::from_pools([(addr(11), addr(1), addr(2))]);
        let index = CycleIndex::build(&universe, &[addr(1)], CycleIndexLimits::default());
        let dirty: HashSet<Address> = [addr(99)].into_iter().collect();
        let out = touched_cycles(&universe, &index, &dirty, 32);
        assert!(out.cycles.is_empty());
        assert_eq!(out.unresolved_pools, 1);
    }

    /// The drain must be a single atomic swap, and must empty the set — a
    /// collect-then-clear loses anything landing between the two, and leaving
    /// the set populated reprocesses the same pools every cycle.
    #[test]
    fn draining_empties_the_dirty_set_and_reports_its_size() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let universe = PoolUniverse::from_pools([(addr(11), addr(1), addr(2))]);
        let index = CycleIndex::build(&universe, &[addr(1)], CycleIndexLimits::default());
        let touched = StdMutex::new(HashSet::from([addr(11), addr(12)]));

        let (_out, pools, _elapsed) = drain_and_resolve(&touched, &universe, &index, 32);
        assert_eq!(pools, 2);
        assert!(
            touched.lock().unwrap().is_empty(),
            "a drain that leaves the set populated reprocesses forever"
        );
    }

    /// Why the fast path must own its `LiveState`, as a test rather than a
    /// comment. Two feeds delivering the same logs in different orders — which
    /// is exactly preconfirmed vs sealed — drive the single global cursor into
    /// repeated breaks, and each break invalidates every pool.
    #[test]
    fn two_orderings_of_one_stream_destroy_a_shared_live_state() {
        let shared = Arc::new(LiveState::new());
        let pool = addr(11);
        let before = shared.continuity_epoch();

        // Feed A is ahead (preconfirmed); feed B replays the same blocks behind
        // it (sealed). Interleaved, they are not monotonic.
        for block in [10u64, 11, 12] {
            shared.apply_log(&sync_log(pool, 1, 2, block + 5, 0));
            shared.apply_log(&sync_log(pool, 1, 2, block, 0));
        }

        assert!(
            shared.continuity_epoch() > before,
            "interleaved orderings must trip the cursor -- this is why the fast \
             path gets its own LiveState instead of the pool monitor's"
        );
    }

    /// `validation: true` and the `"pending"` block tag are the two things that
    /// make this a preconfirmed check rather than an expensive no-op. Without
    /// validation the node skips the checks that make a pass mean the
    /// transaction would actually be accepted.
    #[test]
    fn the_simulation_targets_preconfirmed_state_with_validation_on() {
        let p = simulate_v1_params(addr(1), addr(2), &[0xde, 0xad], 1_000_000_000);
        assert_eq!(p[1], "pending", "simulating against latest measures the past");
        assert_eq!(p[0]["validation"], true);
        let call = &p[0]["blockStateCalls"][0]["calls"][0];
        assert_eq!(call["data"], "0xdead");
        assert!(call["maxFeePerGas"].as_str().unwrap().starts_with("0x"));
    }

    #[test]
    fn a_successful_simulation_reports_gas() {
        let ok = json!([{"calls": [{"status": "0x1", "gasUsed": "0x64e0"}]}]);
        let r = parse_simulate_v1(&ok);
        assert!(r.success);
        assert_eq!(r.gas_used, 0x64e0);
        assert!(r.failure.is_none());
    }

    /// A revert must carry its reason forward: a candidate that fails
    /// simulation is the cheapest available signal about WHY, and discarding it
    /// turns a diagnosable reject into a silent one.
    #[test]
    fn a_reverted_simulation_keeps_the_reason() {
        let rv = json!([{"calls": [
            {"status": "0x0", "gasUsed": "0x10", "error": "execution reverted"}
        ]}]);
        let r = parse_simulate_v1(&rv);
        assert!(!r.success);
        assert_eq!(r.gas_used, 0x10);
        assert!(r.failure.unwrap().contains("reverted"));
    }

    /// A pass must be positively proven. An unparseable or empty response is a
    /// FAILURE -- treating it as success is how an unverified candidate reaches
    /// broadcast, which is the one thing simulation exists to prevent.
    #[test]
    fn an_unreadable_simulation_response_is_a_failure_not_a_pass() {
        for v in [
            serde_json::json!([]),
            serde_json::json!([{"calls": []}]),
            serde_json::json!({"unexpected": true}),
        ] {
            let r = parse_simulate_v1(&v);
            assert!(!r.success, "malformed response must never read as success");
            assert!(r.failure.is_some());
        }
    }

    #[test]
    fn a_successful_call_reports_its_gas() {
        let v = serde_json::json!([{"calls":[{"status":"0x1","gasUsed":"0x5208"}]}]);
        let r = parse_simulate_v1(&v);
        assert!(r.success);
        assert_eq!(r.gas_used, 21_000);
        assert!(r.failure.is_none());
    }

    /// A revert must carry its reason forward: a failed candidate is the
    /// cheapest signal available about WHY, and dropping it turns a diagnosable
    /// reject into a silent one.
    #[test]
    fn a_reverted_call_keeps_its_reason() {
        let v = serde_json::json!([{"calls":[
            {"status":"0x0","gasUsed":"0x10","error":{"message":"execution reverted"}}
        ]}]);
        let r = parse_simulate_v1(&v);
        assert!(!r.success);
        assert!(r.failure.unwrap().contains("execution reverted"));
    }

    /// The closing hop is not optional. A loop priced without it is a PATH,
    /// and its product means nothing -- 2.0 * 0.5 around a closed triangle is
    /// break-even, but the same two hops open look like a doubling.
    #[test]
    fn the_closing_hop_is_priced() {
        let (a, b) = (addr(1), addr(2));
        // a->b doubles, b->a halves: a closed loop is exactly break-even.
        let rate = |from: Address, _to: Address| {
            if from == a {
                Some((2.0, 1.0))
            } else {
                Some((1.0, 2.0))
            }
        };
        let bps = price_cycle(&[a, b], rate).expect("priceable");
        assert!(
            bps.abs() < 1e-9,
            "closed loop must be break-even, got {bps} bps -- the closing hop was skipped"
        );
    }

    #[test]
    fn a_profitable_loop_reports_positive_bps() {
        let (a, b) = (addr(1), addr(2));
        // 1% edge around the loop.
        let rate = |from: Address, _to: Address| {
            if from == a {
                Some((2.02, 1.0))
            } else {
                Some((1.0, 2.0))
            }
        };
        let bps = price_cycle(&[a, b], rate).expect("priceable");
        assert!((bps - 100.0).abs() < 1e-6, "expected ~100 bps, got {bps}");
    }

    /// §7's rule, applied to pricing: a hop with no edge REJECTS the cycle.
    /// Substituting a default rate is precisely how this project manufactured
    /// phantom profits -- a quote claiming more output than the pool held.
    #[test]
    fn a_missing_hop_rejects_the_cycle_rather_than_assuming_a_rate() {
        let (a, b) = (addr(1), addr(2));
        let rate = |from: Address, _to: Address| {
            if from == a {
                Some((2.0, 1.0))
            } else {
                None // no edge back
            }
        };
        assert!(
            price_cycle(&[a, b], rate).is_none(),
            "an unpriceable hop must reject, never approximate"
        );
    }

    #[test]
    fn degenerate_rates_reject_rather_than_producing_infinities() {
        let (a, b) = (addr(1), addr(2));
        for bad in [(1.0, 0.0), (f64::INFINITY, 1.0), (f64::NAN, 1.0)] {
            let rate = move |_f: Address, _t: Address| Some(bad);
            assert!(price_cycle(&[a, b], rate).is_none(), "bad rate {bad:?} must reject");
        }
    }

    /// Unpriceable cycles are counted, not silently absent. A cycle missing
    /// from the output is otherwise indistinguishable from one that priced
    /// badly, and those need different responses: a coverage gap versus the
    /// market simply not offering anything.
    #[test]
    fn unpriceable_cycles_are_counted_separately_from_unprofitable_ones() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let (t1, t2, t3) = (addr(1), addr(2), addr(3));
        let universe = PoolUniverse::from_pools([
            (addr(11), t1, t2),
            (addr(12), t2, t3),
            (addr(13), t3, t1),
        ]);
        let index = CycleIndex::build(&universe, &[t1], CycleIndexLimits::default());
        let ids: Vec<_> = (0..index.len() as u32).collect();

        // No rates at all: everything is unpriceable, nothing is "unprofitable".
        let (priced, unpriceable, basis) = price_touched(&index, &ids, |_, _| None, None, 8);
        assert!(priced.is_empty());
        assert_eq!(unpriceable, ids.len());
        assert_eq!(basis, RankBasis::Bps, "nothing priced, so nothing valued");
    }

    /// Ranking must put the most profitable first, and break ties toward
    /// shorter loops -- fewer legs is less gas and less to go wrong.
    #[test]
    fn pricing_ranks_by_profit_then_prefers_shorter_loops() {
        let mk = |id, bps, hops| PricedCycle {
            id, gross_bps: bps, hops, notional_in: 1.0, profit_native: None,
        };
        let mut v = [mk(0, 5.0, 4), mk(1, 50.0, 6), mk(2, 50.0, 2)];
        v.sort_by(|x, y| {
            y.gross_bps
                .partial_cmp(&x.gross_bps)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(x.hops.cmp(&y.hops))
        });
        assert_eq!(v[0].id, 2, "same profit, shorter loop wins");
        assert_eq!(v[1].id, 1);
        assert_eq!(v[2].id, 0);
    }

    fn v2_sync(pool: Address, r0: u128, r1: u128, block: u64) -> Log {
        sync_log(pool, r0, r1, block, 0)
    }

    /// A V2 pool as production actually sees it: SEEDED from chain -- which is
    /// the only place the token ordering comes from -- and then moved by Sync
    /// logs. A bare Sync carries reserves and no tokens, so a pool built from
    /// logs alone cannot say which token `reserve0` counts.
    fn seeded_v2(live: &LiveState, pool: Address, t0: Address, t1: Address) {
        assert!(live.anchor_v2(
            pool,
            99,
            crate::quote_univ2::UniV2PairState {
                token0: t0,
                token1: t1,
                reserve0: U256::from(1u64),
                reserve1: U256::from(1u64),
            }
        ));
    }

    /// Direction is the dangerous part. `sqrtPriceX96` is token1-per-token0, so
    /// pricing a hop backwards does not fail -- it silently returns the
    /// reciprocal, and a reciprocal around a loop manufactures a phantom edge.
    #[test]
    fn a_hop_is_priced_in_the_direction_it_is_asked_for() {
        let (t0, t1, pool) = (addr(1), addr(2), addr(11));
        let live = LiveState::new();
        // reserves 1000 / 4000 -> 4 token1 per token0
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&v2_sync(pool, 1_000, 4_000, 100));
        let tokens = dashmap::DashMap::from_iter([(
            pool,
            // Zero fee: these tests are about DIRECTION and reciprocity, so the
            // fee is held out rather than folded into every expected value.
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None },
        )]);

        let (n, d) = rate_from_live(&live, &[pool], &tokens, t0, t1, marginal()).expect("forward");
        assert!((n / d - 4.0).abs() < 1e-9, "t0->t1 must be 4.0, got {}", n / d);

        let (n, d) = rate_from_live(&live, &[pool], &tokens, t1, t0, marginal()).expect("reverse");
        assert!((n / d - 0.25).abs() < 1e-9, "t1->t0 must be 0.25, got {}", n / d);
    }

    /// A round trip through one pool must be break-even, which is only true if
    /// the two directions are exact reciprocals.
    #[test]
    fn the_two_directions_are_reciprocal() {
        let (t0, t1, pool) = (addr(1), addr(2), addr(11));
        let live = LiveState::new();
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&v2_sync(pool, 7_919, 104_729, 100));
        let tokens = dashmap::DashMap::from_iter([(
            pool,
            // Zero fee: these tests are about DIRECTION and reciprocity, so the
            // fee is held out rather than folded into every expected value.
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None },
        )]);
        let (a, b) = rate_from_live(&live, &[pool], &tokens, t0, t1, marginal()).unwrap();
        let (c, d) = rate_from_live(&live, &[pool], &tokens, t1, t0, marginal()).unwrap();
        let round = (a / b) * (c / d);
        assert!((round - 1.0).abs() < 1e-9, "round trip must be 1.0, got {round}");
    }

    /// §7: an untrusted snapshot is NOT priced locally. Pricing it anyway is
    /// the optimistic fallback the parity gate exists to forbid.
    #[test]
    fn an_untrusted_snapshot_is_not_priced() {
        let (t0, t1, pool) = (addr(1), addr(2), addr(11));
        let live = LiveState::new();
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&v2_sync(pool, 1_000, 4_000, 100));
        let tokens = dashmap::DashMap::from_iter([(
            pool,
            // Zero fee: these tests are about DIRECTION and reciprocity, so the
            // fee is held out rather than folded into every expected value.
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None },
        )]);
        assert!(rate_from_live(&live, &[pool], &tokens, t0, t1, marginal()).is_some());

        live.break_continuity(UnknownReason::WsUnavailable);
        assert!(
            rate_from_live(&live, &[pool], &tokens, t0, t1, marginal()).is_none(),
            "invalidated state must not be priced from"
        );
    }

    /// A hop whose tokens do not match the pool's recorded pair would be priced
    /// in an unknown direction; it must be skipped, not guessed.
    #[test]
    fn a_pool_that_does_not_serve_the_hop_is_skipped() {
        let (t0, t1, pool) = (addr(1), addr(2), addr(11));
        let live = LiveState::new();
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&v2_sync(pool, 1_000, 4_000, 100));
        let tokens = dashmap::DashMap::from_iter([(
            pool,
            // Zero fee: these tests are about DIRECTION and reciprocity, so the
            // fee is held out rather than folded into every expected value.
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None },
        )]);
        assert!(rate_from_live(&live, &[pool], &tokens, t0, addr(99), marginal()).is_none());
        assert!(rate_from_live(&live, &[], &tokens, t0, t1, marginal()).is_none());
    }

    /// `as_u128` would truncate a sqrtPriceX96 silently and yield a plausible
    /// wrong price; the decimal-string path must survive full-width values.
    #[test]
    fn wide_u256_values_convert_without_truncating() {
        let wide = U256::from(1u64) << 200;
        let f = u256_to_f64(wide).expect("finite");
        assert!(f > 1e60, "a 200-bit value must not truncate to something small");
    }

    /// The unit trap: `MonitoredPool.fee_bps` holds PPM. Reading 3_000 as bps
    /// charges 30% instead of 0.30% -- a 10x error that would reject every real
    /// candidate while looking like conservatism.
    #[test]
    fn pool_fees_are_charged_in_ppm_not_bps() {
        let (t0, t1, pool) = (addr(1), addr(2), addr(11));
        let live = LiveState::new();
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&sync_log(pool, 1_000, 4_000, 100, 0));
        // 3_000 ppm = 0.30%, so 4.0 becomes 4.0 * 0.997 = 3.988.
        let tokens = dashmap::DashMap::from_iter([(
            pool,
            PoolMeta { token0: t0, token1: t1, fee_ppm: 3_000, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None },
        )]);
        let (n, d) = rate_from_live(&live, &[pool], &tokens, t0, t1, marginal()).expect("priced");
        let r = n / d;
        assert!(
            (r - 3.988).abs() < 1e-6,
            "0.30% fee on 4.0 must give 3.988, got {r} -- 30% would give 2.8"
        );
    }

    /// Pool fees belong INSIDE gross_bps, charged once per hop. A two-hop loop
    /// through two 30bps pools loses ~60bps to fees alone, which is why raw
    /// ratios overstated every candidate in the last run.
    #[test]
    fn a_two_hop_loop_pays_both_pool_fees() {
        let (t0, t1) = (addr(1), addr(2));
        let (p1, p2) = (addr(11), addr(12));
        let live = LiveState::new();
        // Two pools at the same price: a round trip is break-even before fees.
        seeded_v2(&live, p1, t0, t1);
        seeded_v2(&live, p2, t0, t1);
        live.apply_log(&sync_log(p1, 1_000, 2_000, 100, 0));
        live.apply_log(&sync_log(p2, 1_000, 2_000, 100, 1));
        let tokens = dashmap::DashMap::from_iter([
            (p1, PoolMeta { token0: t0, token1: t1, fee_ppm: 3_000, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None }),
            (p2, PoolMeta { token0: t0, token1: t1, fee_ppm: 3_000, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None }),
        ]);
        let bps = price_cycle(&[t0, t1], |f, t| {
            let pools = if f == t0 { [p1] } else { [p2] };
            rate_from_live(&live, &pools, &tokens, f, t, marginal())
        })
        .expect("priceable");
        assert!(
            (bps + 59.91).abs() < 0.5,
            "two 0.30% fees should cost ~60 bps, got {bps}"
        );
    }

    /// The constant this replaces rejected on the fee stack alone. A 100bps
    /// dislocation through two 30bps pools is profitable, and a 60bps ceiling
    /// deleted it.
    #[test]
    fn the_cost_stack_judges_net_not_nominal_fees() {
        let costs = CostStack {
            gas_bps: 5.0,
            flash_fee_bps: 9.0,
            execution_buffer_bps: 3.0,
            competition_bid_bps: 10.0,
            risk_premium_bps: 5.0,
        };
        assert!((costs.total_bps() - 32.0).abs() < 1e-9);
        // 40 bps gross (already net of pool fees) clears a 32 bps stack.
        assert!(costs.clears(40.0));
        assert!((costs.net_bps(40.0) - 8.0).abs() < 1e-9);
        // 30 does not, and break-even does not count as profit.
        assert!(!costs.clears(30.0));
        assert!(!costs.clears(32.0), "break-even is not a candidate");
    }

    /// The feed is an enum so the documented Denim migration — native 200ms
    /// blocks replacing flashblocks — is a variant, not a rewrite of the state
    /// and search engine above it.
    #[test]
    fn the_feed_is_addressed_behind_an_interface() {
        let f = FlashFeed::PendingLogs {
            ws_url: "wss://example.invalid/ws".into(),
        };
        assert_eq!(f.ws_url(), "wss://example.invalid/ws");
    }

    /// Silence handling is delegated, not re-derived, so it cannot drift from
    /// the pool monitor's: never-delivered warns (a new socket carries the same
    /// deaf filter), delivered-then-quiet reconnects.
    #[test]
    fn idle_handling_matches_the_pool_monitor() {
        let (f, _t) = fast(vec![addr(1)]);
        let long = Duration::from_secs(600);
        assert_eq!(f.idle_verdict(0, long, long), IdleAction::WarnNeverDelivered);
        assert_eq!(f.idle_verdict(5_000, long, long), IdleAction::Reconnect);
        assert_eq!(f.stall_limit(), SUBSCRIPTION_STALL_LIMIT);
    }

    /// Wiring the socket is the next commit. Until then this must fail loudly
    /// rather than return Ok and consume nothing — a fast path that reports
    /// success while delivering no logs is the exact failure this module is
    /// written to prevent.
    #[test]
    fn without_endpoints_the_task_cannot_rebuild_a_dead_socket() {
        let (f, _t) = fast(vec![addr(1)]);
        assert!(f.ws_endpoints.is_empty(), "the default is the trapped state");
        let f = f.with_ws_reconnect(vec!["wss://example.invalid".into()], Duration::from_secs(5));
        assert_eq!(f.ws_endpoints.len(), 1);
    }

    /// Rotation must beat the provider's own close. BlockPI closes websockets
    /// at 30 minutes and the original incident was that close arriving
    /// unannounced at 28.
    #[test]
    fn the_socket_rotates_before_the_provider_closes_it() {
        assert!(WS_MAX_CONNECTION_AGE < Duration::from_secs(30 * 60));
    }

    // ---- state coverage (the seeding gap) ----

    /// Coverage must be measured through the SAME gate the pricer uses.
    ///
    /// A count of "pools holding any snapshot" would have read 100% in the
    /// 2026-09-02 bridge run, where `rate_from_live` priced 9.9% of touched
    /// cycles. The number that matters is what the pricer would ACCEPT.
    #[test]
    fn coverage_counts_only_what_the_pricer_would_accept() {
        let (cl, v2, dark) = (addr(1), addr(2), addr(3));
        let (f, _t) = fast(vec![cl, v2, dark]);
        let f = f.with_pool_tokens(std::collections::HashMap::from([
            (cl, PoolMeta { token0: addr(80), token1: addr(81), fee_ppm: 500,
                            kind: PoolKind::ConcentratedLiquidity, verified: true, confirmed_at: Some(Instant::now()), balances: None }),
            (v2, PoolMeta { token0: addr(90), token1: addr(91), fee_ppm: 3_000,
                            kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None }),
            (dark, PoolMeta { token0: addr(92), token1: addr(93), fee_ppm: 3_000,
                              kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None }),
        ]));
        assert_eq!(f.coverage().priceable, 0, "nothing seeded yet");
        assert_eq!(f.coverage().total, 3);

        assert!(f.live.anchor_cl(cl, 100, U256::from(1u64) << 96, 1_000, 0));
        assert!(f.live.anchor_v2(
            v2,
            100,
            crate::quote_univ2::UniV2PairState {
                token0: addr(90),
                token1: addr(91),
                reserve0: U256::from(1_000u64),
                reserve1: U256::from(2_000u64),
            }
        ));

        let c = f.coverage();
        assert_eq!(c.priceable, 2);
        assert_eq!(c.total, 3, "the denominator is the SUBSCRIBED set");
        assert!((c.pct() - 66.667).abs() < 0.01, "got {}", c.pct());
    }

    /// A feed gap invalidates local state, and the gauge has to fall with it.
    /// One that stayed high across a break would report the fast path as
    /// production-ready at the moment it went blind.
    #[test]
    fn a_feed_gap_collapses_coverage() {
        let pool = addr(1);
        let (f, _t) = fast(vec![pool]);
        let f = f.with_pool_tokens(std::collections::HashMap::from([(
            pool,
            PoolMeta { token0: addr(80), token1: addr(81), fee_ppm: 500,
                       kind: PoolKind::ConcentratedLiquidity, verified: true, confirmed_at: Some(Instant::now()), balances: None },
        )]));
        assert!(f.live.anchor_cl(pool, 100, U256::from(1u64) << 96, 1_000, 0));
        assert_eq!(f.coverage().priceable, 1);
        f.note_gap();
        assert_eq!(
            f.coverage().priceable,
            0,
            "a continuity break must not leave state priceable"
        );
    }

    /// An empty universe is 0% covered, not 100%. `0/0` reported as complete
    /// would let a fast path with no pools pass a >95% production gate.
    #[test]
    fn an_empty_universe_is_uncovered_not_complete() {
        assert_eq!(FastCoverage { priceable: 0, total: 0 }.pct(), 0.0);
    }

    /// The seed has to read each pool with the loader for ITS curve. Sending a
    /// V2 pair to the CL loader returns nothing, and the pool stays dark
    /// forever while the log says the seed succeeded.
    #[test]
    fn the_seed_routes_each_pool_to_the_loader_for_its_curve() {
        let (cl, vol, stable) = (addr(1), addr(2), addr(3));
        let meta = dashmap::DashMap::new();
        meta.insert(cl, PoolMeta { token0: addr(10), token1: addr(11), fee_ppm: 500, kind: PoolKind::ConcentratedLiquidity, verified: true, confirmed_at: Some(Instant::now()), balances: None });
        meta.insert(vol, PoolMeta { token0: addr(12), token1: addr(13), fee_ppm: 3_000, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None });
        meta.insert(stable, PoolMeta { token0: addr(14), token1: addr(15), fee_ppm: 100, kind: PoolKind::StableSwap, verified: true, confirmed_at: Some(Instant::now()), balances: None });

        let (cl_targets, v2_targets) = seed_targets(&[cl, vol, stable], &meta);
        assert_eq!(cl_targets.len(), 1);
        assert_eq!(cl_targets[0].0, cl);
        assert_eq!(cl_targets[0].2, addr(10), "token0 is needed for the balance read");
        // Both Solidly curves store plain reserves, so both seed through the
        // pair loader; the curve difference is a PRICING concern, not a read.
        assert_eq!(v2_targets.len(), 2);
        assert!(v2_targets.contains(&vol) && v2_targets.contains(&stable));
    }

    /// A pool the fast path has no metadata for cannot be seeded: the CL loader
    /// needs its token pair to read balances, and guessing one would anchor a
    /// pool against the wrong tokens.
    #[test]
    fn a_pool_without_metadata_is_not_seeded() {
        let meta = dashmap::DashMap::new();
        let (cl, v2) = seed_targets(&[addr(1)], &meta);
        assert!(cl.is_empty() && v2.is_empty());
    }

    /// A triangle the graph can express, and the cycle index's id for it.
    fn triangle_for_translation() -> (
        crate::cycle_index::CycleIndex,
        crate::graph::Graph,
        crate::cycle_index::CycleId,
    ) {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let (t1, t2, t3) = (addr(1), addr(2), addr(3));
        let (p12, p23, p31) = (addr(11), addr(12), addr(13));
        let uni = PoolUniverse::from_pools([(p12, t1, t2), (p23, t2, t3), (p31, t3, t1)]);
        let idx = CycleIndex::build(&uni, &[t1], CycleIndexLimits::default());
        let dirty: HashSet<Address> = [p12].into_iter().collect();
        let id = *touched_cycles(&uni, &idx, &dirty, 32)
            .cycles
            .first()
            .expect("the triangle traverses p12");

        let mut g = crate::graph::Graph::default();
        for (f, t) in [(t1, t2), (t2, t3), (t3, t1), (t2, t1), (t3, t2), (t1, t3)] {
            g.add_edge(crate::graph::Edge {
                from: f,
                to: t,
                rate_num: U256::from(1u64),
                rate_den: U256::from(1u64),
                venue: crate::graph::VenueEdge::UniV3 {
                    path: vec![(f, None), (t, Some(500))],
                    pool: Address::zero(),
                    fee: 500,
                    state: None,
                },
                estimated_gas: 0,
                weight: 0,
                max_input: U256::from(1_000u64),
                tolerance_bps: 0,
                observed_slippage_bps: 0,
                quote_block: None,
                active: true,
                tick_ladder: None,
            });
        }
        (idx, g, id)
    }

    /// The whole point of removing the gate: a candidate the 32 bps cost stack
    /// would have deleted must still reach `prepare_candidate`.
    ///
    /// Gas is a fixed native cost, so a bps bar is a bar on TRADE SIZE wearing a
    /// percentage costume. The run that motivated this had a best gross of 21.54
    /// bps against a 32 bps stack and translated nothing for five minutes.
    #[test]
    fn a_candidate_below_the_cost_stack_still_reaches_preparation() {
        let (idx, g, id) = triangle_for_translation();
        let thin = PricedCycle { id, gross_bps: 12.0, hops: 3, notional_in: 1.0, profit_native: None };
        assert!(
            !CostStack::from_env().clears(thin.gross_bps),
            "the premise: 12 bps does not clear a 32 bps stack"
        );

        let (ready, untranslatable) = translate_for_prep(&idx, &g, &[thin]);
        assert_eq!(untranslatable, 0);
        assert_eq!(ready.len(), 1, "the bps bar must not delete it");
        assert_eq!(ready[0].0.gross_bps, 12.0);
        assert!(ready[0].1.cycle.first() == ready[0].1.cycle.last(), "closed loop");
    }

    /// A cycle the scan graph cannot express is a coverage gap, not a bad price.
    /// Counting it separately is what makes the two distinguishable in the log.
    #[test]
    fn a_cycle_the_graph_cannot_express_is_counted_not_dropped() {
        let (idx, _g, id) = triangle_for_translation();
        let empty = crate::graph::Graph::default();
        let (ready, untranslatable) =
            translate_for_prep(&idx, &empty, &[PricedCycle { id, gross_bps: 99.0, hops: 3, notional_in: 1.0, profit_native: None }]);
        assert!(ready.is_empty());
        assert_eq!(untranslatable, 1);
    }

    // ---- parallel pools ----

    /// Every token pair on Base is served by several pools at several fee
    /// tiers. Taking the FIRST one that prices is taking an arbitrary one:
    /// `pools_for_hop` returns insertion order, so the route depended on the
    /// order the inventory happened to load.
    #[test]
    fn the_best_of_several_pools_serving_a_hop_is_the_one_used() {
        let (t0, t1) = (addr(1), addr(2));
        let (cheap, rich) = (addr(10), addr(11));
        let live = LiveState::new();
        // Same pair, same fee; `rich` simply holds a better price.
        assert!(live.anchor_v2(cheap, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(1_000u64),
        }));
        assert!(live.anchor_v2(rich, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(1_500u64),
        }));
        let meta = dashmap::DashMap::new();
        for p in [cheap, rich] {
            meta.insert(p, PoolMeta {
                token0: t0, token1: t1, fee_ppm: 0, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None,
            });
        }

        let (n, d) = rate_from_live(&live, &[cheap, rich], &meta, t0, t1, marginal()).expect("priced");
        assert!((n / d - 1.5).abs() < 1e-9, "got {}", n / d);
        // ...and it must not depend on which order the pools arrive in.
        let (n2, d2) = rate_from_live(&live, &[rich, cheap], &meta, t0, t1, marginal()).expect("priced");
        assert!((n2 / d2 - n / d).abs() < 1e-12, "order changed the route");
    }

    /// A cheaper fee tier can beat a better raw ratio, and the comparison has to
    /// happen AFTER the fee is charged or the ranking is on the wrong number.
    #[test]
    fn the_comparison_happens_after_fees_not_before() {
        let (t0, t1) = (addr(1), addr(2));
        let (expensive, cheap) = (addr(10), addr(11));
        let live = LiveState::new();
        // `expensive` has the better ratio (1.10) but charges 1%; `cheap` has
        // 1.05 and charges nothing. Net: 1.089 vs 1.05, so expensive still wins
        // -- flip the fee and the answer must flip with it.
        assert!(live.anchor_v2(expensive, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(1_100u64),
        }));
        assert!(live.anchor_v2(cheap, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(1_050u64),
        }));
        let meta = dashmap::DashMap::new();
        meta.insert(expensive, PoolMeta {
            token0: t0, token1: t1, fee_ppm: 100_000, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None,
        });
        meta.insert(cheap, PoolMeta {
            token0: t0, token1: t1, fee_ppm: 0, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None,
        });
        let (n, d) = rate_from_live(&live, &[expensive, cheap], &meta, t0, t1, marginal()).expect("priced");
        // 1.10 * 0.90 = 0.99 < 1.05, so the cheap pool wins on NET.
        assert!((n / d - 1.05).abs() < 1e-9, "fees were not charged before ranking: {}", n / d);
    }

    /// The Solidly stable curve is `x^3*y + x*y^3 = k`, so `r1/r0` is not its
    /// marginal price. Pricing one as constant-product manufactured a 163%
    /// phantom edge in the spread census. Until the decimals needed for the
    /// real form are wired, the pool must be REFUSED, not approximated.
    #[test]
    fn a_stable_pool_is_refused_rather_than_priced_as_constant_product() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let live = LiveState::new();
        assert!(live.anchor_v2(pool, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(2_000u64),
        }));
        let meta = dashmap::DashMap::new();
        meta.insert(pool, PoolMeta {
            token0: t0, token1: t1, fee_ppm: 100, kind: PoolKind::StableSwap, verified: true, confirmed_at: Some(Instant::now()), balances: None,
        });
        assert!(rate_from_live(&live, &[pool], &meta, t0, t1, marginal()).is_none());
    }

    /// A pool whose snapshot is untrusted must not shadow a healthy pool
    /// serving the same hop. The old loop returned `None` for the hop entirely
    /// if the untrusted pool came first in some orderings.
    #[test]
    fn an_untrusted_pool_does_not_hide_a_healthy_one() {
        let (t0, t1) = (addr(1), addr(2));
        let (dark, good) = (addr(10), addr(11));
        let live = LiveState::new();
        assert!(live.anchor_v2(good, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(1_200u64),
        }));
        let meta = dashmap::DashMap::new();
        for p in [dark, good] {
            meta.insert(p, PoolMeta {
                token0: t0, token1: t1, fee_ppm: 0, kind: PoolKind::ConstantProduct, verified: true, confirmed_at: Some(Instant::now()), balances: None,
            });
        }
        // `dark` has no snapshot at all.
        let (n, d) = rate_from_live(&live, &[dark, good], &meta, t0, t1, marginal()).expect("priced");
        assert!((n / d - 1.2).abs() < 1e-9);
    }

    // ---- fee units ----

    /// The 100x trap. Both inventories call the field `fee_bps`; only one of
    /// them means it. Verified on 2026-09-03 against the Base data: every one
    /// of the 141,364 `uniswap_v2` records carries `fee: 30`, and
    /// `config/base_aerodrome_pools.json` carries `feeBps: 30` -- both real
    /// basis points. `uniswap_v3` carries 100/500/3000/10000, the ppm tiers.
    #[test]
    fn a_venue_fee_is_converted_by_its_source_not_its_curve() {
        assert_eq!(fee_to_ppm(FeeUnit::Ppm, 3_000), 3_000, "UniV3 0.30% is ppm already");
        assert_eq!(fee_to_ppm(FeeUnit::Bps, 30), 3_000, "Aerodrome 0.30% is 30 bps");
        assert_eq!(fee_to_ppm(FeeUnit::Bps, 5), 500, "a 5 bps stable pool");
        // Both spellings of 0.30% must land on the same `keep`.
        let keep = |ppm: u32| 1.0 - f64::from(ppm) / 1_000_000.0;
        assert!((keep(fee_to_ppm(FeeUnit::Ppm, 3_000)) - keep(fee_to_ppm(FeeUnit::Bps, 30))).abs() < 1e-12);
        // And reading bps as ppm is the 100x error, spelled out.
        assert!((keep(30) - 0.99997).abs() < 1e-9, "the wrong reading keeps 99.997%");
        assert!((keep(3_000) - 0.997).abs() < 1e-9, "the right one keeps 99.7%");
    }

    // ---- token ordering ----

    /// Config lists each Aerodrome pair in BOTH directions; `or_insert` keeps
    /// whichever came first. When that disagrees with the pool's real
    /// `token0()`, pricing from config does not fail -- it returns the
    /// RECIPROCAL, and a reciprocal around a loop manufactures a phantom edge.
    /// The seed reads the pair and overwrites the claim.
    #[test]
    fn the_seed_overwrites_a_config_pair_the_chain_disagrees_with() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let meta: dashmap::DashMap<Address, PoolMeta> = dashmap::DashMap::from_iter([(
            pool,
            // Config claims t0 is token0, unverified.
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0,
                       kind: PoolKind::ConstantProduct, verified: false, confirmed_at: Some(Instant::now()), balances: None },
        )]);

        // The chain says the opposite way round.
        assert!(
            adopt_chain_pair(&meta, pool, t1, t0),
            "the disagreement must be reported, not silently corrected"
        );
        let m = *meta.get(&pool).expect("still present");
        assert_eq!((m.token0, m.token1), (t1, t0), "the chain's answer wins");
        assert!(m.verified);

        // Reserves (t1: 1000, t0: 4000), so t0 -> t1 is 1000/4000 = 0.25.
        let live = LiveState::new();
        assert!(live.anchor_v2(pool, 100, crate::quote_univ2::UniV2PairState {
            token0: t1, token1: t0,
            reserve0: U256::from(1_000u64), reserve1: U256::from(4_000u64),
        }));
        let (n, d) = rate_from_live(&live, &[pool], &meta, t0, t1, marginal()).expect("priced");
        assert!(
            (n / d - 0.25).abs() < 1e-9,
            "config ordering was trusted: got {}, the reciprocal is 4.0",
            n / d
        );
    }

    /// Agreement is not a mismatch, and it still verifies the pair.
    #[test]
    fn a_config_pair_the_chain_confirms_is_not_reported_as_a_mismatch() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let meta: dashmap::DashMap<Address, PoolMeta> = dashmap::DashMap::from_iter([(
            pool,
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0,
                       kind: PoolKind::ConstantProduct, verified: false, confirmed_at: Some(Instant::now()), balances: None },
        )]);
        assert!(!adopt_chain_pair(&meta, pool, t0, t1));
        assert!(meta.get(&pool).expect("present").verified);
    }

    /// A Sync log carries reserves and no tokens, so a pool seen only through
    /// logs cannot say which token `reserve0` counts. Guessing is what the seed
    /// exists to make unnecessary.
    #[test]
    fn a_v2_pool_never_seeded_is_refused_rather_than_guessed() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let live = LiveState::new();
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&v2_sync(pool, 1_000, 4_000, 100));
        // State is perfectly good; only the token ORDER is unconfirmed.
        let meta: dashmap::DashMap<Address, PoolMeta> = dashmap::DashMap::from_iter([(
            pool,
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0,
                       kind: PoolKind::ConstantProduct, verified: false, confirmed_at: Some(Instant::now()), balances: None },
        )]);
        assert!(
            rate_from_live(&live, &[pool], &meta, t0, t1, marginal()).is_none(),
            "an unconfirmed pair must be refused, not guessed"
        );
        // The seed confirms it, and the same state prices.
        assert!(!adopt_chain_pair(&meta, pool, t0, t1));
        let (n, d) = rate_from_live(&live, &[pool], &meta, t0, t1, marginal()).expect("priced once verified");
        assert!((n / d - 4.0).abs() < 1e-9, "got {}", n / d);
    }

    /// The trap that made the `verified` flag part of `pool_priceable` and not
    /// only of the pricer. A hot V2 pool acquires a Derived snapshot from its
    /// first Sync log, so a trust-only coverage test would have called it
    /// covered -- for exactly the busiest pools, which the pricer was refusing.
    #[test]
    fn an_unverified_pool_with_good_state_is_not_counted_as_coverage() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let (f, _t) = fast(vec![pool]);
        let f = f.with_pool_tokens(std::collections::HashMap::from([(
            pool,
            PoolMeta { token0: t0, token1: t1, fee_ppm: 0,
                       kind: PoolKind::ConstantProduct, verified: false, confirmed_at: Some(Instant::now()), balances: None },
        )]));
        seeded_v2(&f.live, pool, t0, t1);
        f.live.apply_log(&v2_sync(pool, 1_000, 4_000, 100));
        assert_eq!(
            f.coverage().priceable,
            0,
            "trusted state with an unconfirmed pair is not coverage"
        );
        adopt_chain_pair(&f.pool_tokens, pool, t0, t1);
        assert_eq!(f.coverage().priceable, 1, "confirming the pair is what covers it");
    }

    // ---- freshness ----

    fn cp_meta(t0: Address, t1: Address, confirmed_at: Option<Instant>) -> PoolMeta {
        PoolMeta {
            token0: t0, token1: t1, fee_ppm: 0,
            kind: PoolKind::ConstantProduct, verified: true, confirmed_at,
            // V2 depth comes off the live snapshot's reserves, not from here.
            balances: None,
        }
    }

    /// A pool nobody has ever read from chain is not priced, however good its
    /// state looks. Inventory metadata is a claim about a pool, not a look at it.
    #[test]
    fn a_pool_never_confirmed_against_chain_is_not_priced() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let live = LiveState::new();
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&v2_sync(pool, 1_000, 4_000, 100));
        let meta = dashmap::DashMap::from_iter([(pool, cp_meta(t0, t1, None))]);
        assert!(rate_from_live(&live, &[pool], &meta, t0, t1, marginal()).is_none());
    }

    /// The backstop. A pool checked long enough ago stops being priced, because
    /// the continuity cursor cannot see a MISSING log on a filtered
    /// subscription -- a preconfirmed update that never lands in the sealed
    /// block leaves state wrong with no signal at all.
    #[test]
    fn a_pool_unchecked_for_longer_than_the_ttl_is_not_priced() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let live = LiveState::new();
        seeded_v2(&live, pool, t0, t1);
        live.apply_log(&v2_sync(pool, 1_000, 4_000, 100));
        let now = Instant::now();
        let checked = now - Duration::from_secs(300);
        let meta = dashmap::DashMap::from_iter([(pool, cp_meta(t0, t1, Some(checked)))]);

        let inside = Freshness { now, ttl: Duration::from_secs(600) };
        assert!(
            rate_from_live(&live, &[pool], &meta, t0, t1, PricingCtx { fresh: inside, ..marginal() }).is_some(),
            "300s old under a 600s ttl must still price"
        );
        let outside = Freshness { now, ttl: Duration::from_secs(120) };
        assert!(
            rate_from_live(&live, &[pool], &meta, t0, t1, PricingCtx { fresh: outside, ..marginal() }).is_none(),
            "300s old under a 120s ttl must not"
        );
    }

    /// Rotation order. Never-confirmed first -- it has the weakest claim to
    /// being priced -- then oldest first, so the universe rotates through
    /// verification at a bounded RPC cost instead of in one spike.
    #[test]
    fn verification_rotates_through_the_least_recently_checked() {
        let (never, old, recent) = (addr(1), addr(2), addr(3));
        let (f, _t) = fast(vec![never, old, recent]);
        let now = Instant::now();
        let f = f.with_pool_tokens(std::collections::HashMap::from([
            (never, cp_meta(addr(90), addr(91), None)),
            (old, cp_meta(addr(92), addr(93), Some(now - Duration::from_secs(300)))),
            (recent, cp_meta(addr(94), addr(95), Some(now))),
        ]));
        let order = f.oldest_unconfirmed(&HashSet::new(), 3);
        assert_eq!(order, vec![never, old, recent]);
        assert_eq!(f.oldest_unconfirmed(&HashSet::new(), 1), vec![never], "bounded");

        // Pools already queued for repair are not re-requested in the same pass.
        let skip: HashSet<Address> = [never].into_iter().collect();
        assert_eq!(f.oldest_unconfirmed(&skip, 3), vec![old, recent]);
    }

    // ---- selection-rule attribution ----

    /// The diagnostic that separates a real spread from one manufactured by
    /// taking a maximum over parallel pools. Same cycle, same state, two rules.
    #[test]
    fn first_match_and_best_net_disagree_by_exactly_the_selection_rule() {
        let (t0, t1) = (addr(1), addr(2));
        let (first, better) = (addr(10), addr(11));
        let live = LiveState::new();
        for (p, r1) in [(first, 1_000u128), (better, 1_500u128)] {
            assert!(live.anchor_v2(p, 100, crate::quote_univ2::UniV2PairState {
                token0: t0, token1: t1,
                reserve0: U256::from(1_000u64), reserve1: U256::from(r1),
            }));
        }
        let meta = dashmap::DashMap::from_iter([
            (first, cp_meta(t0, t1, Some(Instant::now()))),
            (better, cp_meta(t0, t1, Some(Instant::now()))),
        ]);
        let pools = [first, better];

        let (n, d) = rate_from_live(&live, &pools, &meta, t0, t1, PricingCtx { select: HopSelect::BestNet, ..marginal() }).expect("best");
        assert!((n / d - 1.5).abs() < 1e-9, "best takes the better pool");

        let (n, d) = rate_from_live(&live, &pools, &meta, t0, t1, PricingCtx { select: HopSelect::FirstMatch, ..marginal() }).expect("first");
        assert!((n / d - 1.0).abs() < 1e-9, "first takes whichever came first");
    }

    /// A stale pool must not be silently skipped by first-match either, or the
    /// two rules would be measuring different pool sets and the comparison
    /// between them would mean nothing.
    #[test]
    fn both_selection_rules_honour_the_same_freshness_bound() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let live = LiveState::new();
        assert!(live.anchor_v2(pool, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(2_000u64),
        }));
        let meta = dashmap::DashMap::from_iter([(pool, cp_meta(t0, t1, None))]);
        for rule in [HopSelect::BestNet, HopSelect::FirstMatch] {
            assert!(
                rate_from_live(&live, &[pool], &meta, t0, t1, PricingCtx { select: rule, ..marginal() }).is_none(),
                "{rule:?} priced an unconfirmed pool"
            );
        }
    }

    // ---- divergence ----

    /// The measurement that answers what age cannot: has preconfirmed state
    /// actually drifted from the chain?
    #[test]
    fn divergence_is_signed_and_relative() {
        assert!(divergence_bps(1.0, 1.0).expect("equal").abs() < 1e-9);
        let up = divergence_bps(1.01, 1.0).expect("above");
        assert!((up - 100.0).abs() < 1e-6, "1% above is +100 bps, got {up}");
        let down = divergence_bps(0.99, 1.0).expect("below");
        assert!((down + 100.0).abs() < 1e-6, "1% below is -100 bps, got {down}");
        assert!(divergence_bps(0.0, 1.0).is_none(), "a zero price is not a price");
        assert!(divergence_bps(f64::NAN, 1.0).is_none());
    }

    // ---- depth-weighted ranking ----

    /// The failure this exists to fix, in miniature. Two cycles: one with a
    /// huge percentage edge through a pool holding almost nothing, one with a
    /// small edge through a deep pool. Percentage ranking prefers the first;
    /// value ranking prefers the second, and the second is the one worth doing.
    #[test]
    fn a_deep_thin_edge_outranks_a_shallow_fat_one() {
        let q = |num: f64, cap: f64| Some(HopQuote { num, den: 1.0, cap_out: cap });
        // 100 bps round trip, but the pool can only pay out 100 units.
        let dust = price_cycle_sized(&[addr(1), addr(2)], |from, _| {
            if from == addr(1) { q(1.01, 100.0) } else { q(1.0, 100.0) }
        })
        .expect("priced");
        // 10 bps round trip through a pool holding 100 million units.
        let deep = price_cycle_sized(&[addr(1), addr(2)], |from, _| {
            if from == addr(1) { q(1.001, 100_000_000.0) } else { q(1.0, 100_000_000.0) }
        })
        .expect("priced");

        assert!(dust.gross_bps > deep.gross_bps, "the dust pool wins on percentage");
        let profit = |c: SizedCycle| c.notional_in * c.gross_bps / 10_000.0;
        assert!(
            profit(deep) > profit(dust),
            "and loses on value: deep {} vs dust {}",
            profit(deep),
            profit(dust)
        );
    }

    /// The binding hop is the one that runs out first, and it is not
    /// necessarily the thinnest in raw units -- what matters is depth relative
    /// to the amount ARRIVING there, which the rates upstream determine.
    #[test]
    fn the_binding_hop_is_the_one_that_runs_out_first() {
        // Hop 1 multiplies by 1000, so hop 2's 1000 units of headroom are
        // reached by an input of only 1.
        let sized = price_cycle_sized(&[addr(1), addr(2)], |from, _| {
            if from == addr(1) {
                Some(HopQuote { num: 1000.0, den: 1.0, cap_out: 1_000.0 })
            } else {
                Some(HopQuote { num: 0.001, den: 1.0, cap_out: 1e12 })
            }
        })
        .expect("priced");
        // x * 1000 <= 1000 * DEPTH_FRACTION  ->  x <= 0.01
        let expected = 1_000.0 * DEPTH_FRACTION / 1000.0;
        assert!(
            (sized.notional_in - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            sized.notional_in
        );
    }

    /// A cycle no hop bounds is UNMEASURED, not infinitely large. Returning
    /// infinity would put it above every real opportunity forever.
    #[test]
    fn a_cycle_with_no_known_depth_is_refused_not_ranked_first() {
        let out = price_cycle_sized(&[addr(1), addr(2)], |_, _| {
            Some(HopQuote { num: 2.0, den: 1.0, cap_out: f64::INFINITY })
        });
        assert!(out.is_none(), "unbounded must not mean unbeatable");
    }

    /// Value ranking needs a numeraire. Cycles starting in different tokens
    /// cannot be compared by percentage OR by raw notional -- a million units
    /// of a worthless token is not a bigger trade than one unit of WETH.
    #[test]
    fn ranking_uses_value_when_prices_are_known_and_says_so_when_not() {
        use crate::cycle_index::{CycleIndex, CycleIndexLimits, PoolUniverse};
        let (t1, t2, t3) = (addr(1), addr(2), addr(3));
        let uni = PoolUniverse::from_pools([
            (addr(11), t1, t2), (addr(12), t2, t3), (addr(13), t3, t1),
        ]);
        let idx = CycleIndex::build(&uni, &[t1], CycleIndexLimits::default());
        let ids: Vec<_> = (0..idx.len() as u32).collect();
        let quote = |_: Address, _: Address| {
            Some(HopQuote { num: 1.05, den: 1.0, cap_out: 1_000_000.0 })
        };

        let (priced, _, basis) = price_touched(&idx, &ids, quote, None, 8);
        assert_eq!(basis, RankBasis::Bps, "no price map means no value ranking");
        assert!(priced.iter().all(|c| c.profit_native.is_none()));

        let prices = std::collections::HashMap::from([(t1, 2.0f64)]);
        let (priced, _, basis) = price_touched(&idx, &ids, quote, Some(&prices), 8);
        assert_eq!(basis, RankBasis::Native);
        let top = priced.first().expect("one cycle");
        let want = top.notional_in * (top.gross_bps / 10_000.0) * 2.0;
        assert!(
            (top.profit_native.expect("valued") - want).abs() < 1e-6,
            "profit must be notional x edge x price"
        );
    }

    /// An unreliable or missing price must not silently become 1:1. A cycle
    /// that cannot be valued sorts LAST under a value ranking, rather than
    /// being dropped -- it is a gap in the price map, not a bad trade.
    #[test]
    fn an_unvalued_cycle_sorts_last_rather_than_vanishing() {
        let mk = |id, profit: Option<f64>| PricedCycle {
            id, gross_bps: 10.0, hops: 2, notional_in: 1.0, profit_native: profit,
        };
        let mut v = [mk(0, None), mk(1, Some(5.0)), mk(2, Some(50.0))];
        v.sort_by(|a, b| {
            let k = |c: &PricedCycle| c.profit_native.unwrap_or(f64::NEG_INFINITY);
            k(b).partial_cmp(&k(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
        assert_eq!(v.iter().map(|c| c.id).collect::<Vec<_>>(), vec![2, 1, 0]);
        assert_eq!(v.len(), 3, "the unvalued cycle is still present");
    }

    /// A CL pool whose balance read failed must bound nothing rather than
    /// bounding wrong. `liquidity` is NOT a substitute: with sqrt_price it
    /// gives virtual reserves that overstate real holdings by 16-56x on deep
    /// pools and by orders of magnitude on thin ones.
    #[test]
    fn a_cl_pool_without_balances_reports_unknown_depth() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let live = LiveState::new();
        assert!(live.anchor_cl(pool, 100, U256::from(1u64) << 96, 1_000, 0));
        let meta = dashmap::DashMap::from_iter([(
            pool,
            PoolMeta {
                token0: t0, token1: t1, fee_ppm: 0,
                kind: PoolKind::ConcentratedLiquidity, verified: true,
                confirmed_at: Some(Instant::now()), balances: None,
            },
        )]);
        let q = hop_quote_from_live(&live, &[pool], &meta, t0, t1, marginal())
        .expect("priced");
        assert!(q.cap_out.is_infinite(), "unknown depth must not be a number");

        // With balances, the OUTPUT side is the bound and direction matters.
        meta.get_mut(&pool).expect("present").balances = Some((7.0, 11.0));
        let fwd = hop_quote_from_live(&live, &[pool], &meta, t0, t1, marginal())
        .expect("priced");
        assert_eq!(fwd.cap_out, 11.0, "t0->t1 pays out token1");
        let rev = hop_quote_from_live(&live, &[pool], &meta, t1, t0, marginal())
        .expect("priced");
        assert_eq!(rev.cap_out, 7.0, "t1->t0 pays out token0");
    }

    // ---- pool choice at size ----

    /// The fix, stated as a test. Two pools on the same pair: a thin one at a
    /// better MARGINAL price, and a deep one. At the margin the thin pool wins
    /// and the trade is a fiction. At a real size it loses, because its price
    /// moves under the trade and the deep pool's does not.
    #[test]
    fn at_a_real_size_the_deep_pool_wins_the_hop() {
        let (t0, t1) = (addr(1), addr(2));
        let (thin, deep) = (addr(10), addr(11));
        let live = LiveState::new();
        // thin: 1000 in / 1100 out  -> marginal 1.10, and tiny
        assert!(live.anchor_v2(thin, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000u64), reserve1: U256::from(1_100u64),
        }));
        // deep: 10^9 in / 1.05*10^9 out -> marginal 1.05, and enormous
        assert!(live.anchor_v2(deep, 100, crate::quote_univ2::UniV2PairState {
            token0: t0, token1: t1,
            reserve0: U256::from(1_000_000_000u64), reserve1: U256::from(1_050_000_000u64),
        }));
        let meta = dashmap::DashMap::from_iter([
            (thin, cp_meta(t0, t1, Some(Instant::now()))),
            (deep, cp_meta(t0, t1, Some(Instant::now()))),
        ]);
        let pools = [thin, deep];

        // At the margin the thin pool's 1.10 beats the deep pool's 1.05.
        let (n, d) = rate_from_live(&live, &pools, &meta, t0, t1, marginal()).expect("marginal");
        assert!((n / d - 1.10).abs() < 1e-9, "marginal choice takes the thin pool: {}", n / d);

        // Priced at 500 units -- half the thin pool's entire input reserve --
        // the thin pool's effective rate collapses and the deep one wins.
        let prices = std::collections::HashMap::from([(t0, 1.0f64)]);
        let sized = PricingCtx {
            fresh: fresh(), select: HopSelect::BestNet,
            prices: Some(&prices), ref_native: 500.0,
        };
        let (n, d) = rate_from_live(&live, &pools, &meta, t0, t1, sized).expect("sized");
        let rate = n / d;
        assert!(
            (rate - 1.05).abs() < 1e-3,
            "at size the deep pool must win; got {rate}"
        );
    }

    /// The effective rate has to reduce to the marginal one as size goes to
    /// zero, or a zero probe would silently mean something other than "price at
    /// the margin".
    #[test]
    fn a_zero_probe_prices_exactly_at_the_margin() {
        let (num, den) = effective_rate(1_000.0, 2_000.0, 1.0, 0.0).expect("rate");
        assert!((num / den - 2.0).abs() < 1e-12);
        // and a size big enough to matter must move it DOWN, never up
        let (n2, d2) = effective_rate(1_000.0, 2_000.0, 1.0, 1_000.0).expect("rate");
        assert!(n2 / d2 < num / den, "impact must reduce the rate");
        assert!((n2 / d2 - 1.0).abs() < 1e-12, "x = r_in halves the output rate");
    }

    /// A token with no price cannot be given a probe size, and a made-up one
    /// would produce a made-up impact on precisely the pools we know least
    /// about. It falls back to the margin rather than to a guess.
    #[test]
    fn an_unpriced_token_falls_back_to_marginal_pricing() {
        let (t0, t1) = (addr(1), addr(2));
        let empty = std::collections::HashMap::new();
        let ctx = PricingCtx {
            fresh: fresh(), select: HopSelect::BestNet,
            prices: Some(&empty), ref_native: 1e17,
        };
        assert_eq!(ctx.reference_in(t0), 0.0, "no price, no probe size");

        let prices = std::collections::HashMap::from([(t1, 2.0f64)]);
        let ctx = PricingCtx { prices: Some(&prices), ..ctx };
        assert_eq!(ctx.reference_in(t1), 1e17 / 2.0, "probe size is native / price");
        assert_eq!(ctx.reference_in(t0), 0.0);
    }

    /// A CL pool's impact comes from VIRTUAL reserves and its capacity from
    /// REAL balances. Confusing the two is what makes a dust pool look deep:
    /// virtual reserves overstate real holdings by 16-56x on Base.
    #[test]
    fn a_cl_hop_takes_impact_from_virtual_reserves_and_capacity_from_real_ones() {
        let (t0, t1) = (addr(1), addr(2));
        let pool = addr(10);
        let live = LiveState::new();
        // sqrtPriceX96 = 2^96 means price 1.0, so virtual reserves are (L, L).
        assert!(live.anchor_cl(pool, 100, U256::from(1u64) << 96, 1_000_000, 0));
        let meta = dashmap::DashMap::from_iter([(
            pool,
            PoolMeta {
                token0: t0, token1: t1, fee_ppm: 0,
                kind: PoolKind::ConcentratedLiquidity, verified: true,
                confirmed_at: Some(Instant::now()),
                // Real holdings are far smaller than the virtual reserves.
                balances: Some((10.0, 20.0)),
            },
        )]);

        let q = hop_quote_from_live(&live, &[pool], &meta, t0, t1, marginal()).expect("marginal");
        assert!((q.num / q.den - 1.0).abs() < 1e-9, "price 1.0 at the margin");
        assert_eq!(q.cap_out, 20.0, "capacity is the REAL token1 balance");

        // A probe of 1,000,000 equals the virtual input reserve, so the
        // effective rate must halve. Capacity is unchanged by the probe.
        let prices = std::collections::HashMap::from([(t0, 1.0f64)]);
        let sized = PricingCtx {
            fresh: fresh(), select: HopSelect::BestNet,
            prices: Some(&prices), ref_native: 1_000_000.0,
        };
        let q = hop_quote_from_live(&live, &[pool], &meta, t0, t1, sized).expect("sized");
        assert!((q.num / q.den - 0.5).abs() < 1e-9, "got {}", q.num / q.den);
        assert_eq!(q.cap_out, 20.0, "impact must not touch capacity");
    }
}
