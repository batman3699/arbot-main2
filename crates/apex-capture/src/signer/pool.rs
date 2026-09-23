//! The pool (§18.2, §27.5). Seven hard requirements, each with a test in
//! `tests/signer_pool.rs`.
//!
//! The assignment rule is §18.2's, literally: **the healthiest currently-free
//! lane with sufficient gas reserve and the required chain/contract
//! authorization**. Everything else here exists to make one of those four words
//! mean something.
//!
//! # Why a lane leaving the hot pool must not stop the chain
//!
//! A single shared breaker is the easy design and it is wrong: one lane that
//! runs into a stuck nonce or a drained gas reserve would halt every lane,
//! converting a local fault into a total capture outage. §18.2 requires the
//! opposite -- "a slow or conflicted lane is removed from the hot pool without
//! stopping the chain" -- so the breaker, the health score and the gas reserve
//! are all per lane, and `assign` simply has fewer candidates to choose from.
//!
//! # Exclusivity is RAII, for the same reason it is in the registry
//!
//! Two holders of one lane would each reserve a nonce, and the second would get
//! the first's `reserved_nonce + 1` only if the first had already finished --
//! which is exactly the race `tests/nonce_loom.rs` explores. [`LaneAssignment`]
//! marks the lane busy for its lifetime, so the window does not exist.

use crate::signer::nonce::{NonceError, NonceLane, ReservedNonce};
use crate::sync::{recover, Mutex};
use apex_types::ids::{ChainId, SignerLaneId};
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

/// Shared, immutable, and identical across every lane (§18.2). Held by value in
/// the pool and handed out by reference: a lane that could hold its *own* copy
/// could hold a different one, and a ticket signed against the wrong executor
/// address is a transaction that reverts at best.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorAuth {
    pub chain: ChainId,
    /// The executor contract every lane is authorized to call.
    pub executor: [u8; 20],
    /// §26.2's executor version, so a lane cannot sign against a stale one.
    pub executor_version: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneConfig {
    pub id: SignerLaneId,
    pub address: [u8; 20],
    /// Pre-funded gas reserve, in wei (§18.2). A lane below the requirement is
    /// not assignable: a signer that runs out of gas mid-flight produces a
    /// ticket that can never be dispatched and must then be reconciled, which
    /// is strictly worse than never assigning it.
    pub gas_reserve_wei: u128,
}

/// §18.2's per-lane health score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaneHealth {
    score: f64,
    consecutive_failures: u32,
}

impl LaneHealth {
    /// Below this a lane leaves the hot pool.
    pub const HOT_THRESHOLD: f64 = 0.35;
    /// Consecutive failures that trip the per-lane breaker.
    pub const BREAKER_TRIPS_AT: u32 = 3;

    pub const fn new() -> Self {
        Self { score: 1.0, consecutive_failures: 0 }
    }
    pub const fn score(&self) -> f64 {
        self.score
    }
    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
    /// The breaker is per lane and trips on consecutive failures alone. Loss
    /// limits live in `apex-risk` (Task 6.7) and gate the chain, not the lane --
    /// mixing the two here is how one lane's bad run halts everything.
    pub const fn breaker_is_open(&self) -> bool {
        self.consecutive_failures >= Self::BREAKER_TRIPS_AT
    }
    pub fn is_hot(&self) -> bool {
        !self.breaker_is_open() && self.score >= Self::HOT_THRESHOLD
    }

    fn record(&mut self, ok: bool) {
        if ok {
            self.consecutive_failures = 0;
            // Recovers, but not instantly: a lane that just failed three times
            // should not be the healthiest candidate again after one success.
            self.score = (self.score * 1.2 + 0.05).min(1.0);
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.score *= 0.5;
        }
    }
}

impl Default for LaneHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// What a ticket needs of a lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRequirements {
    pub chain: ChainId,
    pub executor: [u8; 20],
    pub executor_version: [u8; 32],
    pub min_gas_reserve_wei: u128,
}

/// Why no lane was available. Distinguished on purpose: "no lane" is otherwise
/// a silent capture miss, and the four causes call for four different responses
/// (wait, fund, fix authorization, investigate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoLane {
    /// The pool is authorized and funded but everything is in use. Transient.
    AllBusy,
    /// Every free lane is below the required gas reserve. Operational.
    NoneFunded,
    /// The requirements do not match this pool's executor authorization.
    NotAuthorized,
    /// Every free, funded, authorized lane has an open breaker or a cold health
    /// score. The chain keeps running; this pool does not.
    AllOutOfTheHotPool,
}

impl std::fmt::Display for NoLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::AllBusy => "every lane is in use",
            Self::NoneFunded => "no free lane holds the required gas reserve",
            Self::NotAuthorized => "no lane is authorized for that chain and executor",
            Self::AllOutOfTheHotPool => "every candidate lane is out of the hot pool",
        };
        f.write_str(s)
    }
}

impl std::error::Error for NoLane {}

struct Lane {
    cfg: LaneConfig,
    nonce: Mutex<NonceLane>,
    state: Mutex<LaneState>,
}

struct LaneState {
    busy: bool,
    health: LaneHealth,
}

pub struct SignerPool {
    auth: ExecutorAuth,
    lanes: Vec<Lane>,
}

impl SignerPool {
    /// §18.4's initial sizing on Base is four lanes. The pool does not choose
    /// that number; the caller does, from measurement.
    pub fn new(auth: ExecutorAuth, lanes: Vec<LaneConfig>) -> Self {
        let lanes = lanes
            .into_iter()
            .map(|cfg| Lane {
                nonce: Mutex::new(NonceLane::new(cfg.id)),
                state: Mutex::new(LaneState { busy: false, health: LaneHealth::new() }),
                cfg,
            })
            .collect();
        Self { auth, lanes }
    }

    pub const fn auth(&self) -> &ExecutorAuth {
        &self.auth
    }

    pub fn len(&self) -> usize {
        self.lanes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    /// §18.2: the healthiest currently-free lane with sufficient gas reserve and
    /// the required chain/contract authorization.
    pub fn assign(&self, need: &LaneRequirements) -> Result<LaneAssignment<'_>, NoLane> {
        if need.chain != self.auth.chain
            || need.executor != self.auth.executor
            || need.executor_version != self.auth.executor_version
        {
            return Err(NoLane::NotAuthorized);
        }

        let mut saw_busy = false;
        let mut saw_underfunded = false;
        let mut best: Option<(usize, f64)> = None;

        for (i, lane) in self.lanes.iter().enumerate() {
            let state = recover(&lane.state);
            if state.busy {
                saw_busy = true;
                continue;
            }
            if lane.cfg.gas_reserve_wei < need.min_gas_reserve_wei {
                saw_underfunded = true;
                continue;
            }
            if !state.health.is_hot() {
                continue;
            }
            let score = state.health.score();
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((i, score));
            }
        }

        let Some((i, _)) = best else {
            return Err(if saw_underfunded {
                NoLane::NoneFunded
            } else if saw_busy {
                NoLane::AllBusy
            } else {
                NoLane::AllOutOfTheHotPool
            });
        };

        // Re-checked under the lock rather than trusting the survey above: two
        // callers can both pick lane `i` from a free reading. The winner is
        // whoever flips `busy` first, and the loser retries.
        {
            let mut state = recover(&self.lanes[i].state);
            if state.busy {
                return Err(NoLane::AllBusy);
            }
            state.busy = true;
        }
        Ok(LaneAssignment { pool: self, index: i })
    }

    /// Lanes currently eligible to be assigned, ignoring busy. §18.2's "hot
    /// pool".
    pub fn hot_lanes(&self) -> Vec<SignerLaneId> {
        self.lanes
            .iter()
            .filter(|l| recover(&l.state).health.is_hot())
            .map(|l| l.cfg.id)
            .collect()
    }

    pub fn health(&self) -> BTreeMap<SignerLaneId, LaneHealth> {
        self.lanes.iter().map(|l| (l.cfg.id, recover(&l.state).health)).collect()
    }

    /// Feed a lane the outcome of a ticket it signed. The only mutator of health.
    pub fn record_outcome(&self, lane: SignerLaneId, ok: bool) {
        if let Some(l) = self.lanes.iter().find(|l| l.cfg.id == lane) {
            recover(&l.state).health.record(ok);
        }
    }

    /// A read-only look at a lane's nonce state, for metrics and tests.
    pub fn with_nonce_lane<R>(&self, lane: SignerLaneId, f: impl FnOnce(&NonceLane) -> R) -> Option<R> {
        self.lanes.iter().find(|l| l.cfg.id == lane).map(|l| f(&recover(&l.nonce)))
    }

    fn release(&self, index: usize) {
        if let Some(l) = self.lanes.get(index) {
            recover(&l.state).busy = false;
        }
    }
}

/// Exclusive possession of one lane. Released on drop.
pub struct LaneAssignment<'p> {
    pool: &'p SignerPool,
    index: usize,
}

impl std::fmt::Debug for LaneAssignment<'_> {
    // Hand-written: the pool behind the reference is not `Debug` and should not
    // be -- printing it would print every lane's nonce state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaneAssignment").field("lane", &self.lane().0).finish()
    }
}

/// Two assignments are equal when they hold the same lane of the same pool.
/// Exists so a test can write `assert_eq!(pool.assign(..), Err(..))` -- a pool
/// error is the interesting half, and spelling it `is_err()` would throw away
/// WHICH error, which is the entire point of [`NoLane`] having four variants.
impl PartialEq for LaneAssignment<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.pool, other.pool) && self.index == other.index
    }
}

impl LaneAssignment<'_> {
    pub fn lane(&self) -> SignerLaneId {
        self.pool.lanes[self.index].cfg.id
    }

    pub fn address(&self) -> [u8; 20] {
        self.pool.lanes[self.index].cfg.address
    }

    /// Shared and immutable -- every lane returns the same reference.
    pub const fn auth(&self) -> &ExecutorAuth {
        &self.pool.auth
    }

    /// Take the next nonce for this lane. Only reachable through an assignment,
    /// which is what makes "one allocator at a time per lane" structural.
    pub fn reserve_nonce(&self, chain_pending: u64, now: UnixNanos) -> ReservedNonce {
        recover(&self.pool.lanes[self.index].nonce).reserve(chain_pending, now)
    }

    pub fn mark_submitted(&self, n: ReservedNonce) -> Result<(), NonceError> {
        recover(&self.pool.lanes[self.index].nonce).mark_submitted(n)
    }

    pub fn mark_replacement(&self, n: ReservedNonce) -> Result<(), NonceError> {
        recover(&self.pool.lanes[self.index].nonce).mark_replacement(n)
    }

    /// Tied to the ticket's terminal close (§18.3).
    pub fn release_nonce(&self, n: ReservedNonce, landed: bool) -> Result<(), NonceError> {
        recover(&self.pool.lanes[self.index].nonce).release(n, landed)
    }
}

impl Drop for LaneAssignment<'_> {
    fn drop(&mut self) {
        self.pool.release(self.index);
    }
}

/// INV-40. "No lane" is a miss, and the four causes are three different
/// buckets: waiting for capacity is slowness, and everything else is a risk or
/// funding state that stopped the trade.
impl apex_types::miss::ExplainsMiss for NoLane {
    fn miss_reason(&self) -> apex_types::miss::MissReason {
        use apex_types::miss::MissReason as R;
        match self {
            Self::AllBusy => R::TooSlow,
            Self::NoneFunded | Self::NotAuthorized | Self::AllOutOfTheHotPool => R::RiskFail,
        }
    }
}
