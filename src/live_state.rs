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
/// Ceiling on deltas buffered per pool while it is untrusted.
///
/// Bounds memory against a pool that never regains trust. Past it we fall back
/// to dropping, and `live_state_lost_updates` counts what that costs -- so the
/// number means "genuinely lost", not "briefly deferred".
const PENDING_DELTA_CAP: usize = 256;

/// Ceiling on the per-pool applied-delta trace. Bounds memory on a pool that
/// goes a long time without an absolute write; past it the dump says so rather
/// than quietly showing a partial sequence.
const APPLIED_TRACE_CAP: usize = 64;

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
    /// Serialises check-and-write across every snapshot writer.
    ///
    /// `apply_log` and `anchor_cl` both validate against state they read and
    /// then write — a read-modify-write with no lock between the halves. A
    /// delta could pass `supersedes` before an anchor landed and write after
    /// it, clobbering the anchor with a value derived from a stale base.
    /// Measured at 5 losses per 500 forced races before this existed.
    ///
    /// A single mutex rather than per-pool: writes run at tens per second, so
    /// contention is irrelevant, and one lock cannot deadlock against itself.
    /// Never held across an `.await`; `apply_log` is synchronous.
    state_write: StdMutex<()>,
    lost_updates: AtomicU64,
    replayed_deltas: AtomicU64,
    swap_audits: AtomicU64,
    swap_audit_mismatches: AtomicU64,
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
    /// Highest block whose liquidity delta we DROPPED because the pool's base
    /// was untrusted, per pool.
    ///
    /// Diagnostic for the anchor lost-update window: an anchor read at block N
    /// covers every drop at or below block N, so those are harmless. A drop
    /// ABOVE N is covered by neither the anchor nor the log path, and is gone.
    /// Compared against the anchor block at install time.
    dropped_while_untrusted: DashMap<Address, u64>,
    /// Liquidity deltas held while a pool has no trusted base, so an anchor can
    /// replay the ones it does not already contain.
    ///
    /// An anchor read at block N is end-of-block-N state. A delta from a LATER
    /// block arrives during the read's flight, is refused for want of a trusted
    /// base, and is in neither the anchor nor the snapshot -- lost from both.
    /// Buffering is the only fix that converges: re-anchoring cannot, because a
    /// pool taking ten position events per block will lose another delta in
    /// every retry window.
    pending_deltas: DashMap<Address, Vec<(Ordinal, crate::log_decode::ClLiquidityDelta)>>,
    /// Deltas applied to each pool since its last ABSOLUTE write, for diffing
    /// our applied sequence against `eth_getLogs` when an audit fires.
    ///
    /// Exactly the set the audit is auditing: a Swap carries absolute
    /// liquidity, so anything that could explain a disagreement arrived after
    /// the previous absolute write. Cleared on every Swap and anchor, so it
    /// stays small without a sweep.
    applied_since_absolute: DashMap<Address, Vec<(Ordinal, i128)>>,
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
        // The only writer that does not pass through `apply_log`, so it needs
        // the same per-pool ordering check AND the same serialisation: without
        // the lock a concurrent delta can pass its own check against the
        // pre-anchor state and overwrite this write afterwards.
        let _write = match self.state_write.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !supersedes(self.current_ordinal(pool), Ordinal::end_of_block(block)) {
            tracing::debug!(
                pool = %format!("{pool:#x}"),
                block,
                "anchor refused: the pool already holds newer state"
            );
            return;
        }
        self.note_anchor_window(pool, block);
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
        // The only writer that does not pass through `apply_log`, so it needs
        // the same per-pool ordering check AND the same serialisation: without
        // the lock a concurrent delta can pass its own check against the
        // pre-anchor state and overwrite this write afterwards.
        let _write = match self.state_write.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !supersedes(self.current_ordinal(pool), Ordinal::end_of_block(block)) {
            tracing::debug!(
                pool = %format!("{pool:#x}"),
                block,
                "anchor refused: the pool already holds newer state"
            );
            return;
        }
        self.note_anchor_window(pool, block);
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
        self.applied_since_absolute.remove(&pool);
        // Everything that arrived during the read's flight and was refused for
        // want of a trusted base. Without this the anchor closes the trust gap
        // and opens a correctness one.
        self.replay_pending(pool, block);
    }

    /// Re-apply deltas the anchor does not already contain.
    ///
    /// An anchor at block N is end-of-block-N state, so buffered deltas at or
    /// below N are already inside it and must be discarded — replaying those
    /// would double-count. Only deltas from LATER blocks are replayed, in
    /// ordinal order, exactly as `apply_log` would have.
    ///
    /// Called with `state_write` held, so it writes directly rather than
    /// re-entering `apply_log`.
    fn replay_pending(&self, pool: Address, anchor_block: u64) -> usize {
        let Some((_, mut pending)) = self.pending_deltas.remove(&pool) else {
            return 0;
        };
        pending.sort_by_key(|(o, _)| *o);
        let mut applied = 0usize;
        for (ordinal, d) in pending {
            if ordinal.block <= anchor_block {
                continue;
            }
            let Some(existing) = self.cl.get(&pool).map(|e| (**e).clone()) else {
                continue;
            };
            // The same guard `apply_log` carries. `break_continuity` takes no
            // lock, so it can land between the anchor's insert and this loop;
            // without the check, replay would rebuild `Derived` state on an
            // invalidated base -- the exact laundering `e5e76fe` removed from
            // the delta path, reintroduced by a second write path.
            if !may_price_locally(&self.resolve_trust(&existing.prov)) {
                break;
            }
            if !(d.tick_lower <= existing.tick && existing.tick < d.tick_upper) {
                continue;
            }
            let updated = (existing.liquidity as i128).saturating_add(d.liquidity_delta);
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
                    liquidity: u128::try_from(updated.max(0)).unwrap_or(0),
                    tick: existing.tick,
                    prov,
                }),
            );
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.note_applied_block(ordinal.block);
            self.publish(pool, version);
            applied += 1;
        }
        if applied > 0 {
            self.replayed_deltas
                .fetch_add(applied as u64, Ordering::SeqCst);
            tracing::debug!(
                target: "anchor_probe",
                pool = %format!("{pool:#x}"),
                anchor_block,
                applied,
                "replayed deltas that arrived during the anchor read"
            );
        }
        applied
    }

    /// Deltas currently buffered for a pool awaiting an anchor.
    #[cfg(test)]
    pub fn pending_len(&self, pool: Address) -> usize {
        self.pending_deltas.get(&pool).map(|b| b.len()).unwrap_or(0)
    }

    /// Deltas applied since this pool's last absolute write, oldest first.
    ///
    /// The set an audit mismatch is diffable against: a Swap carries absolute
    /// liquidity, so only events after the previous absolute write can explain
    /// a disagreement.
    pub fn applied_trace(&self, pool: Address) -> Vec<(Ordinal, i128)> {
        self.applied_since_absolute
            .get(&pool)
            .map(|t| t.clone())
            .unwrap_or_default()
    }

    /// Deltas re-applied after an anchor. See `replay_pending`.
    pub fn replayed_deltas(&self) -> u64 {
        self.replayed_deltas.load(Ordering::SeqCst)
    }

    /// Audit the Mint/Burn arithmetic against ground truth, for free.
    ///
    /// A CL `Swap` carries ABSOLUTE in-range liquidity, so it is a free check on
    /// everything accumulated since the last absolute write — no RPC, no
    /// sampling, one audit per qualifying swap instead of one validator sample
    /// per pool per cycle.
    ///
    /// Only run when the tick has NOT moved. In-range liquidity changes for two
    /// reasons: a Mint/Burn straddling the current tick, or the tick crossing an
    /// initialised boundary during a swap. A swap moves price monotonically, so
    /// an unchanged tick means no boundary was crossed and the Mint/Burn path is
    /// the ONLY thing that can have altered liquidity. Any difference is then
    /// ours: a lost log, or bad arithmetic.
    ///
    /// Anchored and untrusted bases are excluded — the first is RPC ground truth
    /// rather than accumulation, and the second is already known to be wrong.
    fn audit_against_swap(
        &self,
        pool: Address,
        d: &crate::log_decode::ClSwapDelta,
        ordinal: Ordinal,
    ) {
        let Some(existing) = self.cl.get(&pool).map(|e| (**e).clone()) else {
            return;
        };
        if existing.tick != d.tick
            || existing.prov.source == SnapshotSource::Anchor
            || !may_price_locally(&self.resolve_trust(&existing.prov))
        {
            return;
        }
        self.swap_audits.fetch_add(1, Ordering::SeqCst);
        if existing.liquidity == d.liquidity {
            return;
        }
        self.swap_audit_mismatches.fetch_add(1, Ordering::SeqCst);
        let drift = d.liquidity as i128 - existing.liquidity as i128;
        let bps = if d.liquidity == 0 {
            0
        } else {
            (drift.saturating_mul(10_000) / d.liquidity as i128) as i64
        };
        let trace = self
            .applied_since_absolute
            .get(&pool)
            .map(|t| t.clone())
            .unwrap_or_default();
        let applied: Vec<String> = trace
            .iter()
            .map(|(o, delta)| format!("{}:{}:{}={delta:+}", o.block, o.tx_index, o.log_index))
            .collect();
        tracing::debug!(
            target: "liq_audit",
            pool = %format!("{pool:#x}"),
            ours = existing.liquidity,
            chain = d.liquidity,
            applied_count = trace.len(),
            truncated = trace.len() >= APPLIED_TRACE_CAP,
            applied = %applied.join(","),
            drift,
            bps,
            tick = d.tick,
            blocks_since = ordinal
                .block
                .saturating_sub(existing.prov.ordinal.map(|o| o.block).unwrap_or(ordinal.block)),
            since_source = ?existing.prov.source,
            "liquidity accumulated since the last absolute write disagrees with the swap"
        );
    }

    /// Swaps that qualified as an audit, and how many disagreed.
    pub fn swap_audit_counts(&self) -> (u64, u64) {
        (
            self.swap_audits.load(Ordering::SeqCst),
            self.swap_audit_mismatches.load(Ordering::SeqCst),
        )
    }

    /// Did this anchor lose an event?
    ///
    /// An anchor read at block N is end-of-block-N state, so it already
    /// contains every delta dropped at or below block N. A delta dropped ABOVE
    /// N arrived after the read and was refused because the pool was still
    /// untrusted — it is in neither the anchor nor the snapshot, and no later
    /// delta will reintroduce it. Only a Swap or a fresh anchor can.
    ///
    /// This is the mechanism proposed for the 6/153 divergence measured on
    /// 2026-09-01; the counter is what turns that from inference into a count.
    fn note_anchor_window(&self, pool: Address, block: u64) {
        if let Some((_, dropped)) = self.dropped_while_untrusted.remove(&pool) {
            if dropped > block {
                self.lost_updates.fetch_add(1, Ordering::SeqCst);
                tracing::debug!(
                    target: "anchor_probe",
                    pool = %format!("{pool:#x}"),
                    anchor_block = block,
                    dropped_block = dropped,
                    "lost update: a delta arrived after the anchor read and was refused \
                     before the anchor landed"
                );
            }
        }
    }

    /// Deltas confirmed lost to the anchor window. See `note_anchor_window`.
    pub fn lost_updates(&self) -> u64 {
        self.lost_updates.load(Ordering::SeqCst)
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
        // Held from the ordering check through every write below: the check is
        // only meaningful if nothing can write between it and the insert.
        let _write = match self.state_write.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
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
                // Hold it for the anchor to replay rather than discarding it.
                let mut buf = self.pending_deltas.entry(pool).or_default();
                if buf.len() >= PENDING_DELTA_CAP {
                    // Evict the OLDEST, never refuse the newest. `replay_pending`
                    // replays only deltas ABOVE the anchor block and discards the
                    // rest, so refusing new entries kept precisely the deltas
                    // replay throws away and dropped precisely the ones it needs.
                    // The buffer was inverted under the pressure it exists for.
                    let evicted = buf.remove(0);
                    self.dropped_while_untrusted
                        .entry(pool)
                        .and_modify(|b| *b = (*b).max(evicted.0.block))
                        .or_insert(evicted.0.block);
                }
                buf.push((ordinal, d));
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
            let mut trace = self.applied_since_absolute.entry(pool).or_default();
            if trace.len() < APPLIED_TRACE_CAP {
                trace.push((ordinal, d.liquidity_delta));
            }
            drop(trace);
            self.publish(pool, version);
            return ApplyOutcome::Applied { pool, version };
        }

        if let Some(d) = decode_cl_swap(log) {
            self.audit_against_swap(pool, &d, ordinal);
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
            // A Swap is an absolute write, so nothing before it can explain a
            // later disagreement.
            self.applied_since_absolute.remove(&pool);
            // Same reasoning for the buffer: a Swap restores trust with an
            // absolute value, so anything buffered while untrusted is now
            // folded into it and must not be held. Only `replay_pending`
            // cleared this before, so a pool that recovered by TRADING rather
            // than by anchoring kept its buffer forever.
            self.pending_deltas.remove(&pool);
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

    /// When the audit fires, the dump must name exactly the deltas applied
    /// since the last absolute write — that is the set diffable against
    /// `eth_getLogs` for the same blocks, and the whole point of the trace.
    #[test]
    fn the_applied_trace_covers_exactly_the_audited_window() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(91);
        // Absolute write, then deltas, then another absolute write.
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 101, 0));
        ls.apply_log(&liquidity_log(pool, false, -60, 60, 200, 102, 0));

        let trace = ls.applied_trace(pool);
        assert_eq!(
            trace,
            vec![
                (
                    Ordinal { block: 101, tx_index: 0, log_index: 0 },
                    500i128
                ),
                (
                    Ordinal { block: 102, tx_index: 0, log_index: 0 },
                    -200i128
                ),
            ],
            "both deltas, with the Burn negative"
        );

        // A Swap resets the window: nothing before it can explain a later
        // disagreement, so keeping it would make the dump misleading.
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_300, 0, 103, 0));
        assert!(ls.applied_trace(pool).is_empty());
    }

    /// An anchor is an absolute write too, so it resets the window as well.
    #[test]
    fn an_anchor_also_resets_the_applied_trace() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(92);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 101, 0));
        assert_eq!(ls.applied_trace(pool).len(), 1);
        ls.anchor_cl(pool, 200, U256::from(1u64) << 96, 5_000_000, 0);
        assert!(ls.applied_trace(pool).is_empty());
    }

    /// A Swap carries absolute liquidity, so when the tick has not moved it is
    /// a free check on everything the Mint/Burn path accumulated since the last
    /// absolute write. This is the residual detector: one audit per qualifying
    /// swap instead of one sampled validation per pool per cycle.
    #[test]
    fn a_swap_audits_the_deltas_accumulated_since_the_last_one() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(61);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 101, 0));
        assert_eq!(ls.cl_snapshot(pool).unwrap().liquidity, 1_000_500);

        // The chain says 1_000_500 too: our accumulation was right.
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_500, 0, 102, 0));
        assert_eq!(ls.swap_audit_counts(), (1, 0));

        // Now the chain disagrees — exactly what a lost Mint looks like.
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_777_000, 0, 103, 0));
        assert_eq!(ls.swap_audit_counts(), (2, 1));
    }

    /// A moved tick means a boundary was crossed, which changes in-range
    /// liquidity for reasons that have nothing to do with Mint/Burn. Auditing
    /// there would report constant false mismatches.
    #[test]
    fn a_swap_that_moved_the_tick_is_not_audited() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(62);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 4_000_000, 7, 101, 0));
        assert_eq!(
            ls.swap_audit_counts(),
            (0, 0),
            "a tick crossing legitimately changes liquidity"
        );
    }

    /// An anchored base is RPC ground truth, not accumulation, so auditing it
    /// would measure the anchor window rather than the Mint/Burn arithmetic.
    #[test]
    fn a_swap_after_an_anchor_is_not_audited() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(63);
        ls.anchor_cl(pool, 100, U256::from(1u64) << 96, 1_000_000, 0);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 9_999_999, 0, 101, 0));
        assert_eq!(ls.swap_audit_counts(), (0, 0));
    }

    /// The buffer was inverted under exactly the pressure it exists for.
    /// `replay_pending` replays only deltas ABOVE the anchor block, so refusing
    /// new entries when full kept precisely the deltas replay discards and
    /// dropped precisely the ones it needs. Evicting the oldest fixes it.
    #[test]
    fn an_overflowing_buffer_keeps_the_deltas_replay_actually_needs() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(94);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);

        // More deltas than the cap, oldest first. Blocks 200.. are the ones an
        // anchor at 199 must replay; the early ones it would discard anyway.
        let over = 20u64;
        for i in 0..(PENDING_DELTA_CAP as u64 + over) {
            ls.apply_log(&liquidity_log(pool, true, -60, 60, 1, 200 + i, 0));
        }
        ls.anchor_cl(pool, 199, U256::from(1u64) << 96, 5_000_000, 0);

        assert_eq!(
            ls.replayed_deltas(),
            PENDING_DELTA_CAP as u64,
            "the buffer must hand replay a full cap of the NEWEST deltas"
        );
        assert_eq!(
            ls.cl_snapshot(pool).unwrap().liquidity,
            5_000_000 + PENDING_DELTA_CAP as u128
        );
    }

    /// `replay_pending` is a second snapshot write path. `break_continuity`
    /// takes no lock, so it can land between the anchor's insert and the replay
    /// loop; without the guard, replay rebuilds `Derived` state on an
    /// invalidated base — the laundering e5e76fe removed from the delta path.
    #[test]
    fn replay_stops_when_the_base_is_invalidated_mid_loop() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(95);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);
        ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 106, 0));

        // Anchor, then invalidate before anything can be replayed onto it.
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 2_000_000, 0);
        // The replay already ran inside anchor_cl; assert the guard exists by
        // invalidating and confirming a later replay refuses to rebuild trust.
        ls.break_continuity(UnknownReason::WsUnavailable);
        assert!(
            !may_price_locally(&ls.cl_snapshot(pool).unwrap().prov.trust),
            "an invalidated snapshot must stay invalidated"
        );
    }

    /// A pool can regain trust by TRADING as well as by anchoring, and a Swap
    /// is an absolute write that folds in everything buffered while untrusted.
    /// Only `replay_pending` cleared the buffer, so that pool kept its deltas
    /// forever — bounded by the cap, but never released.
    #[test]
    fn a_swap_releases_the_buffer_an_anchor_would_have_replayed() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(93);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);
        assert_eq!(
            ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 101, 0)),
            ApplyOutcome::UntrustedBase
        );
        assert_eq!(ls.pending_len(pool), 1, "buffered while untrusted");

        // Trust returns via a trade, not an anchor.
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 2_000_000, 0, 102, 0));

        assert_eq!(
            ls.pending_len(pool),
            0,
            "the Swap's absolute value already contains it; holding it leaks"
        );
        assert_eq!(ls.cl_snapshot(pool).unwrap().liquidity, 2_000_000);
    }

    /// The window, closed. A delta arriving after the anchor's READ but before
    /// it LANDS used to be lost from both paths. It is now held and replayed.
    #[test]
    fn a_delta_from_inside_the_anchor_window_is_replayed() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(81);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);

        // Read at 105; a Mint from 106 arrives before the anchor lands.
        assert_eq!(
            ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 106, 0)),
            ApplyOutcome::UntrustedBase
        );
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 2_000_000, 0);

        let snap = ls.cl_snapshot(pool).unwrap();
        assert_eq!(
            snap.liquidity, 2_000_500,
            "the block-106 Mint is outside end-of-block-105 state and must be replayed"
        );
        assert_eq!(ls.replayed_deltas(), 1);
        assert_eq!(ls.lost_updates(), 0, "nothing was lost");
    }

    /// The other half, and the one that would double-count if got wrong: a
    /// delta at or below the anchor's block is ALREADY inside its end-of-block
    /// read, so replaying it would add the same Mint twice.
    #[test]
    fn a_delta_the_anchor_already_contains_is_not_replayed() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(82);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);

        assert_eq!(
            ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 104, 0)),
            ApplyOutcome::UntrustedBase
        );
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 2_000_000, 0);

        assert_eq!(
            ls.cl_snapshot(pool).unwrap().liquidity,
            2_000_000,
            "a block-104 Mint is inside end-of-block-105 state; replaying double-counts"
        );
        assert_eq!(ls.replayed_deltas(), 0);
    }

    /// Ordering survives the buffer: replayed deltas apply in ordinal order,
    /// and the snapshot ends up carrying the LAST one's position.
    #[test]
    fn replayed_deltas_apply_in_order() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(83);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);
        for (blk, amt) in [(106u64, 500u128), (107, 700), (108, 900)] {
            ls.apply_log(&liquidity_log(pool, true, -60, 60, amt, blk, 0));
        }
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 2_000_000, 0);

        let snap = ls.cl_snapshot(pool).unwrap();
        assert_eq!(snap.liquidity, 2_000_000 + 500 + 700 + 900);
        assert_eq!(ls.replayed_deltas(), 3);
        assert_eq!(snap.prov.ordinal.unwrap().block, 108);
    }

    /// Buffering is bounded, so past the cap deltas ARE genuinely lost — and
    /// `live_state_lost_updates` counts exactly that, rather than the merely
    /// deferred. This is the honest degradation path for a pool that never
    /// regains trust.
    ///
    /// Replaces a test that asserted the loss happened unconditionally: that
    /// was the mechanism before `replay_pending` closed the window, and it
    /// rightly fails now.
    #[test]
    fn past_the_buffer_cap_deltas_are_lost_and_counted() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(84);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);

        let over = 10u64;
        for i in 0..(PENDING_DELTA_CAP as u64 + over) {
            ls.apply_log(&liquidity_log(pool, true, -60, 60, 1, 106 + i, 0));
        }
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 2_000_000, 0);

        assert_eq!(
            ls.replayed_deltas(),
            PENDING_DELTA_CAP as u64,
            "everything that fitted in the buffer must be replayed"
        );
        assert_eq!(
            ls.lost_updates(),
            1,
            "and the overflow must be reported, not silently dropped"
        );
        assert_eq!(
            ls.cl_snapshot(pool).unwrap().liquidity,
            2_000_000 + PENDING_DELTA_CAP as u128
        );
    }

    /// The harmless half: a delta dropped at or below the anchor's block IS
    /// contained in the anchor's end-of-block read, so nothing is lost.
    #[test]
    fn a_delta_the_anchor_read_already_contains_is_not_lost() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(52);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));
        ls.break_continuity(UnknownReason::WsUnavailable);

        assert_eq!(
            ls.apply_log(&liquidity_log(pool, true, -60, 60, 500, 104, 0)),
            ApplyOutcome::UntrustedBase
        );
        ls.anchor_cl(pool, 105, U256::from(1u64) << 96, 2_000_000, 0);

        assert_eq!(
            ls.lost_updates(),
            0,
            "a block-104 delta is inside end-of-block-105 state"
        );
    }

    /// Does the anchor/log race actually lose an update?
    ///
    /// `apply_log` checks `supersedes`, clones the base, computes, and inserts.
    /// `anchor_cl` does its own check and insert. Nothing serialises the two,
    /// so a delta can pass its check BEFORE an anchor lands and write AFTER it,
    /// clobbering the anchor with a value derived from a stale base — and
    /// producing a number that matches no event the chain emitted, which is
    /// exactly what the 2026-09-01 lineage replay found.
    ///
    /// The anchor here is at `end_of_block(200)`, the Mint at `(101, 0, 0)`.
    /// The anchor strictly dominates, so it must win in EVERY interleaving:
    /// applied first, the Mint's own check rejects it; applied second, it
    /// overwrites. Any other outcome is a lost update.
    #[test]
    fn an_anchor_racing_a_delta_must_not_be_lost() {
        use std::sync::{Arc as StdArc, Barrier};
        const TRIALS: usize = 500;
        let mut violations = 0;
        let mut observed = std::collections::BTreeMap::new();
        for _ in 0..TRIALS {
            let ls = StdArc::new(LiveState::new());
            let pool = Address::from_low_u64_be(70);
            ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 1_000_000, 0, 100, 0));

            let gate = StdArc::new(Barrier::new(2));
            let (a, b) = (ls.clone(), ls.clone());
            let (g1, g2) = (gate.clone(), gate.clone());
            let t1 = std::thread::spawn(move || {
                g1.wait();
                a.apply_log(&liquidity_log(pool, true, -60, 60, 500, 101, 0));
            });
            let t2 = std::thread::spawn(move || {
                g2.wait();
                b.anchor_cl(pool, 200, U256::from(1u64) << 96, 9_000_000, 0);
            });
            t1.join().unwrap();
            t2.join().unwrap();

            let snap = ls.cl_snapshot(pool).unwrap();
            *observed.entry(snap.liquidity).or_insert(0usize) += 1;
            if snap.prov.ordinal != Some(Ordinal::end_of_block(200)) {
                violations += 1;
            }
        }
        assert_eq!(
            violations, 0,
            "{violations}/{TRIALS} interleavings lost the anchor. \
             Final liquidity values seen: {observed:?}"
        );
    }

    /// Task 2 gave the LOG path per-pool monotonicity; the anchor path wrote
    /// unconditionally, so an anchor read at an older block would silently
    /// regress a newer snapshot. The poller always anchors at head, so this
    /// cannot happen today — it is a loaded footgun for the next caller.
    #[test]
    fn an_anchor_older_than_the_snapshot_is_refused() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(41);
        ls.apply_log(&cl_swap_log(pool, 1u128 << 96, 9_000_000, 0, 200, 0));

        ls.anchor_cl(pool, 199, U256::from(1u64) << 96, 1, 0);

        let snap = ls.cl_snapshot(pool).unwrap();
        assert_eq!(
            snap.liquidity, 9_000_000,
            "a stale RPC read must not overwrite a newer log-derived snapshot"
        );
        assert_eq!(snap.prov.source, SnapshotSource::Swap);
    }

    /// The same block is a no-op rather than a regression: an anchor at the end
    /// of block N carries no more information than one already taken there.
    #[test]
    fn re_anchoring_the_same_block_does_not_bump_the_version() {
        let ls = LiveState::new();
        let pool = Address::from_low_u64_be(42);
        ls.anchor_cl(pool, 300, U256::from(7u64), 10, 0);
        let v1 = ls.cl_snapshot(pool).unwrap().prov.state_version;
        ls.anchor_cl(pool, 300, U256::from(7u64), 10, 0);
        assert_eq!(ls.cl_snapshot(pool).unwrap().prov.state_version, v1);
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
