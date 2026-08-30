//! Live pool state, maintained from decoded logs.
//!
//! In Phase 1 this store is WRITTEN and MEASURED but never read for pricing.
//! `ScanSnapshot` (spec §4.2) and the candidate staleness guards (§6) arrive
//! with Phase 2, when something finally reads it.

use crate::continuity::{BreakReason, Cursor, Observation, Ordinal};
use crate::log_decode::{decode_cl_swap, decode_v2_sync};
use crate::quote_univ2::UniV2PairState;
use dashmap::DashMap;
use ethers::types::{Address, Log, U256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

// The trust vocabulary is complete by design, but Phase 1 only ever
// constructs Anchored/Derived/Unknown(ContinuityBreak|Reorg). The rest are
// produced by state_gate and Phase 2; defining them now is what makes
// `may_price_locally` exhaustive, which is the point.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownReason {
    NeverAnchored,
    ContinuityBreak,
    Reorg,
    WsUnavailable,
}

// The trust vocabulary is complete by design, but Phase 1 only ever
// constructs Anchored/Derived/Unknown(ContinuityBreak|Reorg). The rest are
// produced by state_gate and Phase 2; defining them now is what makes
// `may_price_locally` exhaustive, which is the point.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    AnchorTtlExpired,
    DriftBudgetExhausted,
}

/// Why a pool may or may not be priced from local state.
///
/// `Diverged` carries its magnitude: a pool measured wrong is a different
/// condition from one that merely aged out, and the size of the disagreement is
/// what tells a decoder bug from an RPC timing artefact.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    Anchored,
    Derived,
    Stale(StaleReason),
    Diverged { err_bps: i64 },
    Unknown(UnknownReason),
}

/// May this state be used to price a route?
///
/// Exhaustive on purpose — NO wildcard arm. A new `TrustState` variant must
/// fail to compile here until someone decides its policy.
#[allow(dead_code)]
pub fn may_price_locally(trust: &TrustState) -> bool {
    match trust {
        TrustState::Anchored | TrustState::Derived => true,
        TrustState::Stale(_) => false,
        TrustState::Diverged { .. } => false,
        TrustState::Unknown(_) => false,
    }
}

/// Where a snapshot came from and what lineage it belongs to.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct Provenance {
    /// Monotonic per pool, bumped on every accepted update.
    pub state_version: u64,
    /// Identity of the RPC anchor this lineage descends from.
    pub anchor_id: u64,
    /// Global epoch at application time; a break invalidates every snapshot
    /// carrying an older value, in O(1).
    pub continuity_epoch: u64,
    /// Cursor position of the log that produced this, `None` for an anchor.
    pub ordinal: Option<Ordinal>,
    pub anchored_at: Instant,
    pub trust: TrustState,
}

#[derive(Clone, Debug)]
pub struct V2Snapshot {
    pub state: UniV2PairState,
    pub prov: Provenance,
}

/// CL state as carried by a `Swap` log — exactly what `slot0()` plus
/// `liquidity()` return, which is what makes it checkable against RPC.
/// Tick ladders and balances arrive in Phase 2.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ClSnapshot {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub prov: Provenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied { pool: Address, version: u64 },
    Duplicate,
    ContinuityBroken(BreakReason),
    Undecodable,
}

#[derive(Default)]
pub struct LiveState {
    v2: DashMap<Address, Arc<V2Snapshot>>,
    cl: DashMap<Address, Arc<ClSnapshot>>,
    /// pool -> highest published version. A `HashMap` behind a mutex, because
    /// the drain must be one atomic swap: collect-then-clear erases any mark
    /// landing between the two, which is permanent loss, not delay.
    dirty: StdMutex<HashMap<Address, u64>>,
    cursor: StdMutex<Cursor>,
    next_version: AtomicU64,
    continuity_epoch: AtomicU64,
    next_anchor_id: AtomicU64,
    /// Bumped on every accepted application and on every epoch change. Phase 2
    /// uses this for `ScanSnapshot` generation validation (spec §4.2).
    generation: AtomicU64,
}

// main.rs compiles its own copy; several accessors are Phase 2's consumers.
#[allow(dead_code)]
impl LiveState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn continuity_epoch(&self) -> u64 {
        self.continuity_epoch.load(Ordering::SeqCst)
    }

    fn ordinal_of(log: &Log) -> Option<Ordinal> {
        Some(Ordinal {
            block: log.block_number?.as_u64(),
            tx_index: log.transaction_index?.as_u64(),
            log_index: log.log_index?.as_u64(),
        })
    }

    fn provenance(&self, version: u64, ordinal: Option<Ordinal>, trust: TrustState) -> Provenance {
        Provenance {
            state_version: version,
            anchor_id: self.next_anchor_id.load(Ordering::SeqCst),
            continuity_epoch: self.continuity_epoch(),
            ordinal,
            anchored_at: Instant::now(),
            trust,
        }
    }

    /// Publish a pool as dirty at `version`, keeping the highest.
    ///
    /// Called only AFTER the snapshot is in the map, so a consumer that drains
    /// this pool always sees state at least as new as the version published.
    fn publish(&self, pool: Address, version: u64) {
        if let Ok(mut guard) = self.dirty.lock() {
            let slot = guard.entry(pool).or_insert(version);
            if *slot < version {
                *slot = version;
            }
        }
    }

    /// Take the dirty set and leave an empty one, atomically.
    pub fn drain_dirty(&self) -> HashMap<Address, u64> {
        match self.dirty.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => HashMap::new(),
        }
    }

    /// Invalidate every snapshot with ONE atomic increment.
    ///
    /// No map sweep and no per-pool writes: snapshots carry the epoch they were
    /// applied under, so bumping it makes all of them `Unknown` at once. That is
    /// what lets recovery be background work while the searcher keeps running.
    pub fn break_continuity(&self, reason: UnknownReason) -> u64 {
        let epoch = self.continuity_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut c) = self.cursor.lock() {
            *c = Cursor::new();
        }
        tracing::warn!(?reason, epoch, "continuity broken; all local state untrusted");
        epoch
    }

    /// Trust for a snapshot, resolved against the CURRENT epoch.
    fn resolve_trust(&self, prov: &Provenance) -> TrustState {
        if prov.continuity_epoch != self.continuity_epoch() {
            return TrustState::Unknown(UnknownReason::ContinuityBreak);
        }
        prov.trust
    }

    pub fn v2_snapshot(&self, pool: Address) -> Option<V2Snapshot> {
        let entry = self.v2.get(&pool)?;
        let mut snap = (**entry).clone();
        snap.prov.trust = self.resolve_trust(&snap.prov);
        Some(snap)
    }

    pub fn cl_snapshot(&self, pool: Address) -> Option<ClSnapshot> {
        let entry = self.cl.get(&pool)?;
        let mut snap = (**entry).clone();
        snap.prov.trust = self.resolve_trust(&snap.prov);
        Some(snap)
    }

    pub fn anchor_v2(&self, pool: Address, state: UniV2PairState) {
        let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
        self.next_anchor_id.fetch_add(1, Ordering::SeqCst);
        let prov = self.provenance(version, None, TrustState::Anchored);
        self.v2.insert(pool, Arc::new(V2Snapshot { state, prov }));
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn anchor_cl(&self, pool: Address, sqrt_price_x96: U256, liquidity: u128, tick: i32) {
        let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
        self.next_anchor_id.fetch_add(1, Ordering::SeqCst);
        let prov = self.provenance(version, None, TrustState::Anchored);
        self.cl.insert(
            pool,
            Arc::new(ClSnapshot { sqrt_price_x96, liquidity, tick, prov }),
        );
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Decode a log and apply it.
    ///
    /// Order is load-bearing (spec 4.1):
    ///   accept -> build snapshot -> bump version -> swap into map -> publish dirty
    pub fn apply_log(&self, log: &Log) -> ApplyOutcome {
        let Some(ordinal) = Self::ordinal_of(log) else {
            return ApplyOutcome::Undecodable;
        };
        let removed = log.removed.unwrap_or(false);

        let observation = match self.cursor.lock() {
            Ok(mut c) => c.observe(ordinal, removed),
            Err(_) => return ApplyOutcome::Undecodable,
        };
        match observation {
            Observation::Duplicate => return ApplyOutcome::Duplicate,
            Observation::Break(reason) => {
                let r = match reason {
                    BreakReason::Reorg => UnknownReason::Reorg,
                    BreakReason::OutOfOrder => UnknownReason::ContinuityBreak,
                };
                self.break_continuity(r);
                return ApplyOutcome::ContinuityBroken(reason);
            }
            Observation::Accept => {}
        }

        let pool = log.address;
        if let Some(d) = decode_v2_sync(log) {
            let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
            let existing = self.v2.get(&pool).map(|e| e.state.clone());
            let state = UniV2PairState {
                token0: existing.as_ref().map(|s| s.token0).unwrap_or_default(),
                token1: existing.as_ref().map(|s| s.token1).unwrap_or_default(),
                reserve0: d.reserve0,
                reserve1: d.reserve1,
            };
            let prov = self.provenance(version, Some(ordinal), TrustState::Derived);
            self.v2.insert(pool, Arc::new(V2Snapshot { state, prov }));
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        if let Some(d) = decode_cl_swap(log) {
            let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
            let prov = self.provenance(version, Some(ordinal), TrustState::Derived);
            self.cl.insert(
                pool,
                Arc::new(ClSnapshot {
                    sqrt_price_x96: d.sqrt_price_x96,
                    liquidity: d.liquidity,
                    tick: d.tick,
                    prov,
                }),
            );
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        ApplyOutcome::Undecodable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy must be an exhaustive match with no wildcard, so adding a
    /// trust state without deciding its policy fails to COMPILE rather than
    /// silently defaulting to tradable.
    #[test]
    fn only_anchored_and_derived_may_price_locally() {
        assert!(may_price_locally(&TrustState::Anchored));
        assert!(may_price_locally(&TrustState::Derived));
        assert!(!may_price_locally(&TrustState::Stale(
            StaleReason::AnchorTtlExpired
        )));
        assert!(!may_price_locally(&TrustState::Stale(
            StaleReason::DriftBudgetExhausted
        )));
        assert!(!may_price_locally(&TrustState::Diverged { err_bps: 1 }));
        assert!(!may_price_locally(&TrustState::Unknown(
            UnknownReason::NeverAnchored
        )));
        assert!(!may_price_locally(&TrustState::Unknown(
            UnknownReason::ContinuityBreak
        )));
        assert!(!may_price_locally(&TrustState::Unknown(UnknownReason::Reorg)));
        assert!(!may_price_locally(&TrustState::Unknown(
            UnknownReason::WsUnavailable
        )));
    }

    use crate::log_decode::{TOPIC_CL_SWAP, TOPIC_SOLIDLY_SYNC};
    use ethers::types::{Address, Bytes, Log, H256};

    fn sync_log(pool: Address, r0: u128, r1: u128, block: u64, li: u64) -> Log {
        let mut data = Vec::new();
        for v in [r0, r1] {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            data.extend_from_slice(&w);
        }
        Log {
            address: pool,
            topics: vec![*TOPIC_SOLIDLY_SYNC],
            data: Bytes::from(data),
            block_number: Some(block.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(li.into()),
            removed: Some(false),
            ..Default::default()
        }
    }

    #[test]
    fn applying_a_sync_bumps_the_version_and_marks_dirty() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(1);
        match ls.apply_log(&sync_log(pool, 10, 20, 100, 0)) {
            ApplyOutcome::Applied { pool: p, version } => {
                assert_eq!(p, pool);
                assert_eq!(version, 1);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        let snap = ls.v2_snapshot(pool).expect("snapshot");
        assert_eq!(snap.state.reserve0, U256::from(10u64));
        assert_eq!(ls.drain_dirty().get(&pool), Some(&1));
    }

    /// Drain must be a swap, leaving nothing behind and losing nothing.
    #[test]
    fn drain_takes_everything_and_leaves_the_set_empty() {
        let ls = LiveState::new();
        for i in 1..=3u64 {
            ls.apply_log(&sync_log(Address::from_low_u64_be(i), 1, 2, 100, i));
        }
        assert_eq!(ls.drain_dirty().len(), 3);
        assert!(ls.drain_dirty().is_empty(), "second drain sees nothing");
    }

    /// State must be applied BEFORE the pool is published dirty, or a consumer
    /// can drain a pool and price it against the previous value.
    #[test]
    fn state_is_visible_before_the_pool_is_published_dirty() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(7);
        ls.apply_log(&sync_log(pool, 42, 43, 100, 0));
        let batch = ls.drain_dirty();
        let published = *batch.get(&pool).expect("published");
        let snap = ls.v2_snapshot(pool).expect("snapshot");
        assert!(
            snap.prov.state_version >= published,
            "a drained pool's snapshot must never be behind its published version"
        );
    }

    #[test]
    fn a_duplicate_log_changes_nothing() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(2);
        let log = sync_log(pool, 5, 6, 100, 3);
        ls.apply_log(&log);
        ls.drain_dirty();
        assert_eq!(ls.apply_log(&log), ApplyOutcome::Duplicate);
        assert_eq!(ls.v2_snapshot(pool).unwrap().prov.state_version, 1);
        assert!(ls.drain_dirty().is_empty(), "a duplicate must not re-dirty");
    }

    /// One atomic increment invalidates every extant snapshot — no map sweep,
    /// which is what lets the searcher keep running during recovery.
    #[test]
    fn a_continuity_break_invalidates_every_snapshot_in_one_step() {
        let ls = LiveState::new();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        ls.apply_log(&sync_log(a, 1, 2, 100, 0));
        ls.apply_log(&sync_log(b, 3, 4, 100, 1));
        assert!(may_price_locally(&ls.v2_snapshot(a).unwrap().prov.trust));

        ls.break_continuity(UnknownReason::ContinuityBreak);

        for p in [a, b] {
            let t = ls.v2_snapshot(p).unwrap().prov.trust;
            assert_eq!(t, TrustState::Unknown(UnknownReason::ContinuityBreak));
            assert!(!may_price_locally(&t));
        }
    }

    #[test]
    fn a_removed_log_breaks_continuity() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(3);
        ls.apply_log(&sync_log(pool, 1, 2, 100, 0));
        let mut gone = sync_log(pool, 9, 9, 101, 0);
        gone.removed = Some(true);
        assert!(matches!(
            ls.apply_log(&gone),
            ApplyOutcome::ContinuityBroken(_)
        ));
    }

    #[test]
    fn an_unknown_topic_is_undecodable_and_harmless() {
        let ls = LiveState::new();
        let mut log = sync_log(Address::from_low_u64_be(4), 1, 2, 100, 0);
        log.topics = vec![H256::zero()];
        assert_eq!(ls.apply_log(&log), ApplyOutcome::Undecodable);
        assert!(ls.drain_dirty().is_empty());
    }

    #[test]
    fn a_cl_swap_updates_the_cl_snapshot() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(9);
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]);
        data.extend_from_slice(&[0u8; 32]);
        let mut w = [0u8; 32];
        w[31] = 7;
        data.extend_from_slice(&w);
        let mut l = [0u8; 32];
        l[31] = 5;
        data.extend_from_slice(&l);
        data.extend_from_slice(&[0u8; 32]);
        let log = Log {
            address: pool,
            topics: vec![*TOPIC_CL_SWAP],
            data: Bytes::from(data),
            block_number: Some(100u64.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(0u64.into()),
            removed: Some(false),
            ..Default::default()
        };
        assert!(matches!(ls.apply_log(&log), ApplyOutcome::Applied { .. }));
        let snap = ls.cl_snapshot(pool).expect("cl snapshot");
        assert_eq!(snap.liquidity, 5);
        assert_eq!(snap.sqrt_price_x96, U256::from(7u64));
    }

    /// Stale is NOT a flavour of Derived. Conflating them is how a pool that
    /// aged out keeps getting priced locally.
    #[test]
    fn stale_is_not_derived() {
        assert_ne!(
            may_price_locally(&TrustState::Derived),
            may_price_locally(&TrustState::Stale(StaleReason::AnchorTtlExpired))
        );
    }
}

/// Exhaustive interleaving model for spec §8 I1.
///
/// The assertion is about ABSENCE OF LOST VERSIONS, not presence of dirty
/// entries: coalescing and redundant processing are permitted, erasure of the
/// newest transition is not. Multiple writers are modelled even though
/// production has one ingestion writer, so the store stays correct if venue
/// streams are parallelised later.
///
/// Run with: `cargo test --features loom-model --lib loom_dirty`
#[cfg(all(test, feature = "loom-model"))]
mod loom_dirty {
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::sync::{Arc, Mutex};
    use std::collections::HashMap;

    #[test]
    fn no_published_version_is_ever_erased() {
        loom::model(|| {
            let dirty: Arc<Mutex<HashMap<u64, u64>>> = Arc::new(Mutex::new(HashMap::new()));
            let version = Arc::new(AtomicU64::new(0));
            let drained: Arc<Mutex<HashMap<u64, u64>>> = Arc::new(Mutex::new(HashMap::new()));

            let writers: Vec<_> = (0..2)
                .map(|_| {
                    let dirty = dirty.clone();
                    let version = version.clone();
                    loom::thread::spawn(move || {
                        let v = version.fetch_add(1, Ordering::SeqCst) + 1;
                        let mut g = dirty.lock().unwrap();
                        let slot = g.entry(0).or_insert(v);
                        if *slot < v {
                            *slot = v;
                        }
                    })
                })
                .collect();

            let reader = {
                let dirty = dirty.clone();
                let drained = drained.clone();
                loom::thread::spawn(move || {
                    let batch = std::mem::take(&mut *dirty.lock().unwrap());
                    let mut d = drained.lock().unwrap();
                    for (k, v) in batch {
                        let e = d.entry(k).or_insert(v);
                        if *e < v {
                            *e = v;
                        }
                    }
                })
            };

            for w in writers {
                w.join().unwrap();
            }
            reader.join().unwrap();

            let tail = std::mem::take(&mut *dirty.lock().unwrap());
            let mut d = drained.lock().unwrap();
            for (k, v) in tail {
                let e = d.entry(k).or_insert(v);
                if *e < v {
                    *e = v;
                }
            }
            let highest = version.load(Ordering::SeqCst);
            assert_eq!(
                d.get(&0).copied(),
                Some(highest),
                "the newest published version must survive every interleaving"
            );
        });
    }
}
