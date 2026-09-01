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
/// True when `incoming` is strictly newer than what the pool already holds.
///
/// `None` accepts anything: a pool with no ordinal has no position to be older
/// than. That case shrinks to nothing once anchors carry
/// `Ordinal::end_of_block`, but refusing it would freeze any pool whose
/// snapshot predates that change.
pub fn supersedes(existing: Option<Ordinal>, incoming: Ordinal) -> bool {
    match existing {
        None => true,
        Some(prev) => incoming > prev,
    }
}

pub fn may_price_locally(trust: &TrustState) -> bool {
    match trust {
        TrustState::Anchored | TrustState::Derived => true,
        TrustState::Stale(_) => false,
        TrustState::Diverged { .. } => false,
        TrustState::Unknown(_) => false,
    }
}

/// Which event produced a snapshot.
///
/// The discriminator for liquidity drift. `Swap` carries the pool's post-swap
/// liquidity outright, so a Swap-sourced snapshot is an authoritative reset
/// point; a `Liquidity`-sourced one is our own arithmetic (previous + delta).
/// Divergence following the first is intra-block noise we cannot fix; following
/// the second it is our application being wrong. Without this the two are
/// indistinguishable in the reconciliation record.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotSource {
    /// RPC read — ground truth, no arithmetic.
    Anchor,
    /// Swap log: liquidity taken directly from the payload.
    Swap,
    /// Mint/Burn log: liquidity computed as previous + delta.
    Liquidity,
    /// V2 Sync: reserves taken directly from the payload.
    Sync,
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
    /// Which event produced this snapshot — see [`SnapshotSource`].
    pub source: SnapshotSource,
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
    /// A topic we subscribe to but intentionally carry no decoder for — a
    /// V2/Solidly `Swap`, whose paired `Sync` already carries full state.
    /// Expected traffic, NOT a gap in coverage.
    NotStateBearing,
    /// A delta arrived for a pool whose base snapshot is no longer trusted.
    /// Dropped rather than applied: see the guard in the Mint/Burn path.
    UntrustedBase,
    /// A log older than the pool's own snapshot — normally because an anchor
    /// has already carried that pool past this point, and an end-of-block RPC
    /// read already includes every log in its block.
    ///
    /// Deliberately NOT `ContinuityBroken`: that path calls
    /// `break_continuity`, which invalidates every pool at once. This is
    /// expected traffic on any anchored pool, so treating it as disorder would
    /// make anchoring catastrophically worse than leaving pools untrusted.
    Superseded,
    /// A topic no decoder recognises. This one is a real signal: some venue is
    /// emitting something we do not understand.
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
    /// Highest block from which a log has been APPLIED.
    ///
    /// A snapshot at block N is only comparable against `eth_call` once this
    /// exceeds N: the cursor guarantees ordering, so seeing a later block means
    /// every block-N log was delivered and the snapshot is that pool's
    /// end-of-block state. Without it the validator compares a mid-block
    /// snapshot against end-of-block chain state.
    max_applied_block: AtomicU64,
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

    /// Highest block from which a log has been applied. See
    /// [`crate::validation_select::select`].
    pub fn settled_through(&self) -> u64 {
        self.max_applied_block.load(Ordering::SeqCst)
    }

    fn note_applied_block(&self, block: u64) {
        self.max_applied_block.fetch_max(block, Ordering::SeqCst);
    }

    fn ordinal_of(log: &Log) -> Option<Ordinal> {
        Some(Ordinal {
            block: log.block_number?.as_u64(),
            tx_index: log.transaction_index?.as_u64(),
            log_index: log.log_index?.as_u64(),
        })
    }

    fn provenance(
        &self,
        version: u64,
        ordinal: Option<Ordinal>,
        trust: TrustState,
        source: SnapshotSource,
    ) -> Provenance {
        Provenance {
            state_version: version,
            anchor_id: self.next_anchor_id.load(Ordering::SeqCst),
            continuity_epoch: self.continuity_epoch(),
            ordinal,
            anchored_at: Instant::now(),
            trust,
            source,
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

    /// Pools with a V2 snapshot. Validation candidates: a pool with no
    /// snapshot has nothing to compare against.
    pub fn tracked_v2(&self) -> Vec<Address> {
        self.v2.iter().map(|e| *e.key()).collect()
    }

    /// Pools with a CL snapshot.
    pub fn tracked_cl(&self) -> Vec<Address> {
        self.cl.iter().map(|e| *e.key()).collect()
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

    pub fn anchor_v2(&self, pool: Address, block: u64, state: UniV2PairState) {
        let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
        self.next_anchor_id.fetch_add(1, Ordering::SeqCst);
        let prov = self.provenance(
            version,
            Some(Ordinal::end_of_block(block)),
            TrustState::Anchored,
            SnapshotSource::Anchor,
        );
        self.v2.insert(pool, Arc::new(V2Snapshot { state, prov }));
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn anchor_cl(
        &self,
        pool: Address,
        block: u64,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
    ) {
        let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
        self.next_anchor_id.fetch_add(1, Ordering::SeqCst);
        let prov = self.provenance(
            version,
            Some(Ordinal::end_of_block(block)),
            TrustState::Anchored,
            SnapshotSource::Anchor,
        );
        self.cl.insert(
            pool,
            Arc::new(ClSnapshot { sqrt_price_x96, liquidity, tick, prov }),
        );
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The position of whatever this pool currently holds, across both venue
    /// families. A pool is in exactly one of the two maps.
    fn current_ordinal(&self, pool: Address) -> Option<Ordinal> {
        if let Some(s) = self.cl.get(&pool) {
            return s.prov.ordinal;
        }
        if let Some(s) = self.v2.get(&pool) {
            return s.prov.ordinal;
        }
        None
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
        // Per-pool ordering. The global cursor guarantees the STREAM is
        // ordered, which used to imply per-pool ordering because every update
        // arrived through it. Anchors do not, so the implication has to become
        // an explicit check.
        if !supersedes(self.current_ordinal(pool), ordinal) {
            return ApplyOutcome::Superseded;
        }
        if let Some(d) = decode_v2_sync(log) {
            let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
            let existing = self.v2.get(&pool).map(|e| e.state.clone());
            let state = UniV2PairState {
                token0: existing.as_ref().map(|s| s.token0).unwrap_or_default(),
                token1: existing.as_ref().map(|s| s.token1).unwrap_or_default(),
                reserve0: d.reserve0,
                reserve1: d.reserve1,
            };
            let prov =
                self.provenance(version, Some(ordinal), TrustState::Derived, SnapshotSource::Sync);
            self.v2.insert(pool, Arc::new(V2Snapshot { state, prov }));
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.note_applied_block(ordinal.block);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        // A position change. Only the IN-RANGE liquidity is tracked here: an
        // out-of-range Mint/Burn alters the tick ladder, which Phase 1 does not
        // hold, and applying it to the live value would corrupt it.
        //
        // Deliberately ignores amount0/amount1. Balances are RPC-anchored, never
        // log-derived (spec §3.2.1) — Collect, CollectProtocol, Flash and plain
        // ERC-20 transfers also move them, and a direct transfer emits no pool
        // event at all.
        if let Some(d) = crate::log_decode::decode_cl_liquidity(log) {
            let Some(existing) = self.cl.get(&pool).map(|e| (**e).clone()) else {
                // No base to apply a delta to. Inventing one would fabricate
                // state; the pool stays unknown until an anchor or a Swap.
                return ApplyOutcome::NotStateBearing;
            };
            // A delta is only meaningful on a base we still trust. After a
            // continuity break the previous value silently missed whatever
            // landed in the gap, and adding to it would launder an invalidated
            // snapshot straight back to `Derived` -- the epoch stamp would be
            // current while the number underneath it is not. Only an absolute
            // write (a Swap, or an anchor) can restore trust after a gap.
            //
            // Same principle as the missing-base check above, which was already
            // handled; this is the untrusted-base case, which was not. It is
            // why the Liquidity path diverged after every gap while the Swap
            // path, which overwrites `liquidity` outright, never did.
            if !may_price_locally(&self.resolve_trust(&existing.prov)) {
                return ApplyOutcome::UntrustedBase;
            }
            // UniV3 ranges are half-open: [tickLower, tickUpper).
            let in_range = d.tick_lower <= existing.tick && existing.tick < d.tick_upper;
            if !in_range {
                return ApplyOutcome::NotStateBearing;
            }
            let updated = (existing.liquidity as i128).saturating_add(d.liquidity_delta);
            let liquidity = u128::try_from(updated.max(0)).unwrap_or(0);
            let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
            let prov = self.provenance(
                version,
                Some(ordinal),
                TrustState::Derived,
                SnapshotSource::Liquidity,
            );
            self.cl.insert(
                pool,
                Arc::new(ClSnapshot {
                    sqrt_price_x96: existing.sqrt_price_x96,
                    liquidity,
                    tick: existing.tick,
                    prov,
                }),
            );
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.note_applied_block(ordinal.block);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        if let Some(d) = decode_cl_swap(log) {
            let version = self.next_version.fetch_add(1, Ordering::SeqCst) + 1;
            let prov =
                self.provenance(version, Some(ordinal), TrustState::Derived, SnapshotSource::Swap);
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
            self.note_applied_block(ordinal.block);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        if log
            .topics
            .first()
            .map(crate::log_decode::is_known_non_state_topic)
            .unwrap_or(false)
        {
            return ApplyOutcome::NotStateBearing;
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

    /// The gap fix was necessary but not sufficient. `break_continuity` marks
    /// every snapshot untrusted, but the Mint/Burn path read the old value
    /// anyway and republished the sum under the CURRENT epoch — laundering an
    /// invalidated number straight back to `Derived`. Observed 2026-08-31: two
    /// Liquidity-sourced divergences appeared within two minutes of a gap, both
    /// reporting `Derived`, both LOW, while the Swap path stayed exact because
    /// it overwrites instead of accumulating.
    #[test]
    fn a_delta_is_never_applied_onto_an_invalidated_base() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(11);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        assert!(may_price_locally(&ls.cl_snapshot(pool).unwrap().prov.trust));

        ls.break_continuity(UnknownReason::WsUnavailable);

        // An in-range Mint that would previously have been absorbed silently.
        let outcome = ls.apply_log(&liquidity_log(pool, true, -60, 60, 500_000, 101, 0));
        assert_eq!(
            outcome,
            ApplyOutcome::UntrustedBase,
            "adding to a value that missed the gap produces a confident wrong answer"
        );
        let snap = ls.cl_snapshot(pool).unwrap();
        assert_eq!(snap.liquidity, 1_000_000, "the base must not have moved");
        assert!(
            !may_price_locally(&snap.prov.trust),
            "and it must still be untrusted; only an absolute write restores trust"
        );
    }

    /// The other half: an absolute write DOES restore trust after a gap,
    /// otherwise the pool could never recover without an anchor.
    #[test]
    fn an_absolute_write_restores_trust_after_a_gap() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(12);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);
        assert!(!may_price_locally(&ls.cl_snapshot(pool).unwrap().prov.trust));

        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 2_000_000, 0, 101, 0));

        let snap = ls.cl_snapshot(pool).unwrap();
        assert!(may_price_locally(&snap.prov.trust));
        assert_eq!(snap.liquidity, 2_000_000);
    }

    /// An anchor must be positioned, not positionless. `Provenance.ordinal`
    /// used to be documented as "None for an anchor", which is exactly why
    /// anchors could not be ordered against logs.
    #[test]
    fn an_anchor_records_its_own_block_position() {
        let ls = LiveState::new();
        let cl = Address::from_low_u64_be(31);
        ls.anchor_cl(cl, 500, U256::from(1u64) << 96, 42, 0);
        let snap = ls.cl_snapshot(cl).unwrap();
        assert_eq!(snap.prov.ordinal, Some(Ordinal::end_of_block(500)));
        assert_eq!(snap.prov.source, SnapshotSource::Anchor);
        assert!(may_price_locally(&snap.prov.trust));
    }

    /// V2 is symmetric; nothing here is CL-specific.
    #[test]
    fn a_v2_anchor_records_its_own_block_position() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(32);
        ls.anchor_v2(
            pool,
            500,
            UniV2PairState {
                token0: Address::from_low_u64_be(901),
                token1: Address::from_low_u64_be(902),
                reserve0: U256::from(1u64),
                reserve1: U256::from(2u64),
            },
        );
        assert_eq!(
            ls.v2_snapshot(pool).unwrap().prov.ordinal,
            Some(Ordinal::end_of_block(500))
        );
    }

    /// An anchor must restore trust after a gap — that is the entire reason for
    /// this work. Only an absolute write can, and an anchor is one.
    #[test]
    fn an_anchor_restores_trust_after_a_gap() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(33);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);
        assert!(!may_price_locally(&ls.cl_snapshot(pool).unwrap().prov.trust));

        ls.anchor_cl(pool, 101, U256::from(1u64) << 96, 2_000_000, 0);

        let snap = ls.cl_snapshot(pool).unwrap();
        assert!(
            may_price_locally(&snap.prov.trust),
            "without this a pool that never trades stays Unknown forever"
        );
        assert_eq!(snap.liquidity, 2_000_000);
    }

    /// A log the anchor already reflects must be DROPPED, not treated as
    /// disorder. `Break(OutOfOrder)` calls `break_continuity`, which
    /// invalidates all 683 pools — routing expected traffic through it would
    /// make anchoring far worse than not anchoring.
    #[test]
    fn a_log_the_anchor_already_covers_is_superseded_not_disorder() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(21);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 5_000_000, 0);
        let epoch_before = ls.continuity_epoch();

        // A Mint from block 105 — already inside the anchor's end-of-block read.
        let outcome = ls.apply_log(&liquidity_log(pool, true, -60, 60, 777, 105, 0));

        assert_eq!(outcome, ApplyOutcome::Superseded);
        assert_eq!(
            ls.cl_snapshot(pool).unwrap().liquidity,
            5_000_000,
            "applying it would double-count a Mint the anchor already includes"
        );
        assert_eq!(
            ls.continuity_epoch(),
            epoch_before,
            "this is expected traffic; it must not invalidate every pool"
        );
    }

    /// The other half: a log from AFTER the anchor still applies normally,
    /// otherwise anchoring would freeze the pool.
    #[test]
    fn a_log_after_the_anchor_still_applies() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(22);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 5_000_000, 0);

        let outcome = ls.apply_log(&liquidity_log(pool, true, -60, 60, 777, 106, 0));

        assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
        assert_eq!(ls.cl_snapshot(pool).unwrap().liquidity, 5_000_777);
    }

    #[test]
    fn a_pool_with_no_snapshot_accepts_anything() {
        assert!(supersedes(
            None,
            Ordinal {
                block: 1,
                tx_index: 0,
                log_index: 0
            }
        ));
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

    /// A V2 Swap is subscribed-to but has no payload decoder, because its
    /// paired Sync already carries complete reserves. It must NOT count as
    /// undecodable, or the metric that warns about unknown venues is drowned
    /// by expected traffic.
    #[test]
    fn a_v2_swap_is_not_state_bearing_rather_than_undecodable() {
        use crate::log_decode::TOPIC_V2_SWAP;
        let ls = LiveState::new();
        let mut log = sync_log(Address::from_low_u64_be(5), 1, 2, 100, 0);
        log.topics = vec![*TOPIC_V2_SWAP];
        assert_eq!(ls.apply_log(&log), ApplyOutcome::NotStateBearing);
        assert!(ls.drain_dirty().is_empty(), "no state, so nothing to re-price");
    }

    /// A genuinely unknown topic still reports Undecodable — that signal must
    /// survive.
    #[test]
    fn an_unrecognised_topic_is_still_undecodable() {
        let ls = LiveState::new();
        let mut log = sync_log(Address::from_low_u64_be(6), 1, 2, 100, 0);
        log.topics = vec![H256::repeat_byte(0xAB)];
        assert_eq!(ls.apply_log(&log), ApplyOutcome::Undecodable);
    }

    #[test]
    fn tracked_lists_only_pools_with_snapshots() {
        let ls = LiveState::new();
        assert!(ls.tracked_v2().is_empty());
        assert!(ls.tracked_cl().is_empty());

        let v2 = Address::from_low_u64_be(1);
        ls.apply_log(&sync_log(v2, 10, 20, 100, 0));

        assert_eq!(ls.tracked_v2(), vec![v2]);
        assert!(
            ls.tracked_cl().is_empty(),
            "a V2 pool must not appear in the CL list"
        );
    }

    fn cl_swap_log(
        pool: Address,
        sqrt_p: u128,
        liquidity: u128,
        tick: i32,
        block: u64,
        li: u64,
    ) -> Log {
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 32]); // amount0
        data.extend_from_slice(&[0u8; 32]); // amount1
        for v in [sqrt_p, liquidity] {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            data.extend_from_slice(&w);
        }
        let mut w = [0u8; 32];
        let fill = if tick < 0 { 0xffu8 } else { 0x00u8 };
        for b in w.iter_mut().take(28) {
            *b = fill;
        }
        w[28..].copy_from_slice(&tick.to_be_bytes());
        data.extend_from_slice(&w);
        Log {
            address: pool,
            topics: vec![*TOPIC_CL_SWAP],
            data: Bytes::from(data),
            block_number: Some(block.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(li.into()),
            removed: Some(false),
            ..Default::default()
        }
    }

    fn liquidity_log(pool: Address, mint: bool, lower: i32, upper: i32, amount: u128, block: u64, li: u64) -> Log {
        let enc = |v: i32| {
            let mut w = [0u8; 32];
            let fill = if v < 0 { 0xffu8 } else { 0x00u8 };
            for b in w.iter_mut().take(24) {
                *b = fill;
            }
            w[24..].copy_from_slice(&(v as i64).to_be_bytes());
            H256(w)
        };
        let mut data = Vec::new();
        if mint {
            data.extend_from_slice(&[0u8; 32]);
        }
        let mut w = [0u8; 32];
        w[16..].copy_from_slice(&amount.to_be_bytes());
        data.extend_from_slice(&w);
        data.extend_from_slice(&[0u8; 64]);
        let topic = if mint {
            *crate::log_decode::TOPIC_CL_MINT
        } else {
            *crate::log_decode::TOPIC_CL_BURN
        };
        Log {
            address: pool,
            topics: vec![topic, H256::zero(), enc(lower), enc(upper)],
            data: Bytes::from(data),
            block_number: Some(block.into()),
            transaction_index: Some(0u64.into()),
            log_index: Some(li.into()),
            removed: Some(false),
            ..Default::default()
        }
    }

    /// The measured Phase 2a failure: liquidity from Swap alone goes stale when
    /// a position changes. A Burn spanning the current tick must reduce it.
    #[test]
    fn a_burn_spanning_the_current_tick_reduces_liquidity() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(1);
        ls.anchor_cl(pool, 1, U256::from(1u64), 10_000, 0);
        assert!(matches!(
            ls.apply_log(&liquidity_log(pool, false, -100, 100, 4_000, 100, 0)),
            ApplyOutcome::Applied { .. }
        ));
        assert_eq!(ls.cl_snapshot(pool).unwrap().liquidity, 6_000);
    }

    #[test]
    fn a_mint_spanning_the_current_tick_increases_liquidity() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(2);
        ls.anchor_cl(pool, 1, U256::from(1u64), 10_000, 0);
        ls.apply_log(&liquidity_log(pool, true, -100, 100, 4_000, 100, 0));
        assert_eq!(ls.cl_snapshot(pool).unwrap().liquidity, 14_000);
    }

    /// An out-of-range position changes the tick LADDER, not in-range
    /// liquidity. Applying it would corrupt the live value.
    #[test]
    fn a_position_outside_the_current_tick_does_not_change_liquidity() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(3);
        ls.anchor_cl(pool, 1, U256::from(1u64), 10_000, 0);
        ls.apply_log(&liquidity_log(pool, true, 500, 900, 4_000, 100, 0));
        assert_eq!(
            ls.cl_snapshot(pool).unwrap().liquidity,
            10_000,
            "out-of-range liquidity must not move the in-range value"
        );
    }

    /// UniV3 ranges are [lower, upper): a position starting exactly at the
    /// current tick is in range; one ending there is not.
    #[test]
    fn range_bounds_follow_the_half_open_convention() {
        let ls = LiveState::new();
        let a = Address::from_low_u64_be(4);
        ls.anchor_cl(a, 1, U256::from(1u64), 10_000, 100);
        ls.apply_log(&liquidity_log(a, true, 100, 200, 1_000, 100, 0));
        assert_eq!(ls.cl_snapshot(a).unwrap().liquidity, 11_000, "lower bound is inclusive");

        let b = Address::from_low_u64_be(5);
        ls.anchor_cl(b, 1, U256::from(1u64), 10_000, 200);
        ls.apply_log(&liquidity_log(b, true, 100, 200, 1_000, 100, 1));
        assert_eq!(ls.cl_snapshot(b).unwrap().liquidity, 10_000, "upper bound is exclusive");
    }

    /// A position change on a pool we have never seen has no base to apply to.
    /// Inventing one would fabricate state.
    #[test]
    fn a_liquidity_event_for_an_unknown_pool_is_not_applied() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(6);
        assert_eq!(
            ls.apply_log(&liquidity_log(pool, true, -100, 100, 4_000, 100, 0)),
            ApplyOutcome::NotStateBearing
        );
        assert!(ls.cl_snapshot(pool).is_none());
    }

    /// The discriminator for liquidity drift. A Swap-sourced snapshot takes
    /// liquidity straight from the payload — an authoritative reset. A
    /// Liquidity-sourced one is our own arithmetic. Divergence following the
    /// first is intra-block noise; following the second it is our bug. Without
    /// the tag the two are indistinguishable in the reconciliation record.
    #[test]
    fn snapshots_record_which_event_produced_them() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(42);

        ls.anchor_cl(pool, 1, U256::from(1u64), 10_000, 0);
        assert_eq!(
            ls.cl_snapshot(pool).unwrap().prov.source,
            SnapshotSource::Anchor
        );

        ls.apply_log(&liquidity_log(pool, true, -100, 100, 1_000, 100, 0));
        assert_eq!(
            ls.cl_snapshot(pool).unwrap().prov.source,
            SnapshotSource::Liquidity,
            "a Mint-derived snapshot is our arithmetic, not the chain's word"
        );

        let v2 = Address::from_low_u64_be(43);
        ls.apply_log(&sync_log(v2, 1, 2, 101, 0));
        assert_eq!(
            ls.v2_snapshot(v2).unwrap().prov.source,
            SnapshotSource::Sync
        );
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
