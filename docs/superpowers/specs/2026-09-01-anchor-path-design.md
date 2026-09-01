# Anchor path design

**Status:** design, not built. Blocks Phase 2b.
**Context:** `docs/superpowers/specs/2026-08-29-live-state-dirty-scan-design.md` §10.

## 1. The problem

`anchor_cl` and `anchor_v2` exist in `live_state.rs` and are **never called from
production**. Every reference is inside that file. So `TrustState::Anchored` and
`SnapshotSource::Anchor` are unreachable, and `anchor_id` is 0 in every
reconciliation record ever recorded.

That was harmless until `e5e76fe`, which made only an **absolute write** able to
restore trust after a websocket gap. Absolute writes come from exactly two
places: a CL `Swap` and a V2 `Sync`. So today a pool recovers from a gap **only
when it next trades**.

Pools that trade rarely therefore stay `Unknown` indefinitely after any gap —
and those are precisely the pools whose liquidity the Mint/Burn path was built
to track. The measured cost is already recorded: under `CHAOS_WS_GAP_SECS=60`,
**5194 liquidity deltas were dropped across 12 gaps** (24.9% of all
state-bearing events) because their base was invalidated and nothing could
re-establish it.

An anchor is the missing absolute write for pools that are not trading.

## 2. Why this is not a five-line wire-up

`Provenance.ordinal` is `Option<Ordinal>`, documented as *"`None` for an
anchor"*. That is the whole difficulty. An anchor with no position cannot be
ordered against the log stream, and ordering is the only thing keeping local
state correct.

Ordering today is enforced **globally**, by one `Cursor` (`live_state.rs:319`).
There is no per-pool ordinal check anywhere — per-pool ordering is *implied* by
global ordering, because every update arrives through one ordered stream. An
anchor does not arrive through that stream, so it breaks the implication.

Two concrete failure modes if anchors are wired naively:

- **Double-count.** Anchor pool X at block N. A `Mint` from block N — already
  reflected in the anchor, because `eth_call` returns end-of-block state —
  arrives afterwards and is applied on top. Liquidity is now too high by exactly
  that Mint.
- **Regression.** Anchor pool X at block N, then a `Swap` from block N−1 lands
  and overwrites `sqrt_price_x96` with an older value, which then looks
  `Derived` and current.

Both reintroduce the mid-block class of fault the settled-block fix removed.

## 3. Design

### 3.1 An anchor has a position: the top of its block

`load_cl_pool_states_batched` **already takes a `block: U64`** (`cl_sim.rs:165`),
so a block-pinned anchor read needs no new plumbing. An `eth_call` pinned to
block N returns state as of the **end** of block N. That is exactly:

```rust
impl Ordinal {
    /// Position of end-of-block state. Sorts after every log in block N,
    /// because `derive(Ord)` compares block, then tx_index, then log_index.
    pub fn end_of_block(block: u64) -> Self {
        Ordinal { block, tx_index: u64::MAX, log_index: u64::MAX }
    }
}
```

`Provenance.ordinal` becomes `Some(Ordinal::end_of_block(n))` for anchors. This
is not a hack: it is the true position of that state in the chain's total order,
and it makes anchors and logs directly comparable under the existing `Ord`.

### 3.2 Per-pool monotonicity, as an explicit rule

Add to the apply path, before any state write:

> A log for pool X is applied only if its ordinal is **strictly greater** than
> the ordinal already recorded on X's snapshot.

A log that fails this test is **`Superseded`** — a new `ApplyOutcome` variant —
and must **not** be routed through `Observation::Break(OutOfOrder)`. This
distinction is load-bearing: `OutOfOrder` calls `break_continuity`, which
invalidates all 683 pools. A log superseded by an anchor is expected traffic,
not disorder, and treating it as disorder would make anchoring catastrophically
worse than not anchoring.

The global cursor keeps its current job unchanged — stream-level disorder and
reorg detection. It never sees anchors, and logs still arrive in global order,
so it continues to say `Accept`; the new per-pool check runs after it.

### 3.3 Anchored snapshots must be excluded from validation

**This is the detail most likely to be missed, and it silently corrupts the
numbers we make decisions from.**

`validation_select::select` and `state_validation` do not special-case
`SnapshotSource::Anchor`. Wired naively, the validator would compare an anchored
snapshot against an `eth_call` — *the same source the snapshot came from*. It
would pass every time, by construction.

The effect is not a harmless no-op. It inflates
`live_state_checks_total{outcome="measured"}` with guaranteed passes and drives
the observed divergence rate toward zero regardless of whether decoding is
correct. This session has already produced two conclusions from exactly this
class of artefact — the retracted "our arithmetic is wrong" finding, and the
0/541 run whose target population was empty.

Rule: **validation measures log-derived snapshots only.** A snapshot with
`source == Anchor` is skipped (`SelectOutcome::NotIndependent`) until a log has
been applied on top of it. Deltas applied *onto* an anchored base remain
measurable and should be measured — that is still the Mint/Burn arithmetic under
test, just from a known-good starting point.

### 3.4 What to anchor, and when

Candidates, in priority order:

1. Pools that are `Unknown` after a gap **and** appear in the cycle index — the
   ones whose absence actually costs opportunities.
2. Pools that are `Stale` past the gate's TTL.

Never anchor everything on a timer. Anchoring is `eth_call` traffic and the
whole point of local state is to not do that; the budget is the recovery
backlog, not a refresh cycle.

Batch through the existing loader at a block **at or below head**. Anchoring at
head is safe under §3.2 — later logs for blocks ≤ N are correctly discarded
because the anchor already reflects them.

## 4. Tension to keep in view

Anchoring **masks decoder bugs**. A pool that is anchored often has its
event-derived error erased before validation can see it. §3.3 mitigates this by
measuring only log-derived snapshots, but the deeper point stands: an aggressive
anchor policy buys availability with diagnostic power. Keep the anchor rate low
enough that Mint/Burn arithmetic still has room to be wrong measurably, and
watch `live_state_untrusted_base_total` — if anchoring makes that number
collapse, coverage improved; if it makes the divergence rate collapse too,
suspect masking before celebrating.

## 5. Out of scope

- Coverage. This does not subscribe PancakeSwap V3 (different topic0, 29 pools,
  currently zero coverage) or any unsubscribed pool.
- The pool-addition gap, which still needs per-pool cursors.
- Reorg handling, which stays global and conservative: a reorg invalidates a
  block range across all pools, and `break_continuity` is the right response.

## 6. Open questions

- **Anchor at head, or at `settled_through`?** Head recovers faster; a settled
  block is easier to reason about against the validator. §3.2 makes both safe,
  so this is a latency/simplicity call, not a correctness one.
- **TTL.** `Provenance.anchored_at` already exists and the state gate expires on
  it. Should an anchor expire faster than a log-derived snapshot, given it has
  no ongoing event confirmation?
- **V2 symmetry.** `anchor_v2` needs the same treatment; nothing here is
  CL-specific except which loader is called.
- **Does `anchor_id` still earn its place?** It is bumped per anchor and stamped
  into provenance, but nothing reads it. Either give it a consumer — lineage
  attribution in the reconciliation record would be the obvious one — or drop
  it rather than carry a third never-wired field.

## 7. Field result, 2026-09-01 — the design has a lost-update window

Run: `CHAOS_WS_GAP_SECS=60`, 17m14s, 16 forced gaps, 538 reconciliations.

| metric | predicted | measured | |
|---|---|---|---|
| `live_state_anchors_total` | > 0 | 10788 | met |
| `live_state_untrusted_base_total` | far below 5194 | **472** | met — an 11x fall |
| `live_state_superseded_total` | small, non-zero | 128 | met — anchors do race logs |
| records with `source=Anchor` | 0 | **0** | met — §3.3 holds |
| Liquidity divergence | unchanged | **6/153 (3.92%)** vs 1/69 (1.45%) | **MISSED** |

The exclusion, the ordering and the coverage recovery all work. The divergence
rate does not: it went UP, and rising divergence was flagged in §4 as the
signal to stop and look rather than celebrate.

### The mechanism

An anchor read is not instantaneous. Between reading state at block N and
installing it, in-flight logs for blocks > N arrive for that pool. The pool is
still `Unknown` at that moment, so a Mint/Burn is dropped as `UntrustedBase` —
and the anchor, being end-of-block-N state, does not contain it either. The
event is lost from both paths. Subsequent deltas then build on a base that is
short by exactly that event.

Every observable matches:

- all six divergences are `Liquidity`-sourced, `Derived`, with `anchor_id > 0`,
  so all descend from an anchor;
- `sqrt_price_x96` and `tick` are exact in all six — the anchor is authoritative
  for those, and only the accumulated `liquidity` is wrong;
- five are LOW (a missed Mint) and one is HIGH (+2262 bps, a missed Burn), which
  is the symmetry a lost-update window predicts and a decoder bug would not;
- all six land 10–55s after a forced gap, inside the re-anchoring window;
- the rate rose while dropped deltas fell 11x, so this is not merely
  pre-existing error becoming visible — that would leave the rate flat.

A Swap during the window is harmless: it writes absolutely, restores trust, and
the anchor guard then refuses the now-older read. Only Mint/Burn-only pools lose
events.

### Not proven

The mechanism is inferred from provenance, sign symmetry and timing, not
observed directly. The decisive test is to log the pool and block of every
`UntrustedBase` drop and check that a divergent pool had one for a block > N
between its anchor's read and install.

### Options

1. **Buffer, don't drop.** Queue Mint/Burn for a pool with an anchor in flight
   and replay after it installs. Correct, and the most machinery.
2. **Re-anchor pools that dropped a delta during their window.** Track the drop,
   re-anchor next cycle. Converges, cheap, leaves a transient wrong state.
3. **Anchor only quiescent pools** — no dropped delta since the last cycle.
   Slowest recovery, no wrong state.
4. **Accept it in production.** The window scales with anchor rate, and this run
   anchored 10788 times in 17 minutes because forced gaps invalidate everything
   every 60s. After `c706157` real gaps are rare, so real anchor volume is a
   fraction of this. Needs measuring, not assuming.

Do not enable local pricing on anchored lineage until this is resolved: the
divergent snapshots reported `Derived`, so `may_price_locally` would have said
yes to state that was wrong by up to 5559 bps.
