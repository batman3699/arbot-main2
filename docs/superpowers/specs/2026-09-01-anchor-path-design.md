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

## 8. Probe result, 2026-09-01 — mechanism real, conclusion retracted

Instrumented run: `CHAOS_WS_GAP_SECS=60`, 19m05s, 18 forced gaps, 11946
anchors, 540 reconciliations.

### The window is real

**8 lost updates confirmed, and every single one at
`dropped_block == anchor_block + 1`.** That is the window's exact signature: a
delta arriving one block after the read, refused for want of a trusted base,
and absent from the anchor because the anchor is end-of-block-N state. Not
inference any more — a count, with the arithmetic to explain each one.

### It does not explain the divergence

8 events against 11946 anchors is 0.07%. This run produced **one** divergent
record, on a pool that had **no** lost update. So the window exists, is rare,
and was not the cause of what §7 attributed to it.

### §7's conclusion is retracted

| run | code | Liquidity divergence |
|---|---|---|
| r9 | pre-anchor | 1/69 (1.45%) |
| r10 | anchor | 6/153 (3.92%) |
| r11 | anchor + probe | **1/178 (0.56%)** |

Pre-anchor against both anchor runs pooled: 7/331 vs 1/69, Fisher p = 0.586.
**There is no established increase in divergence from anchoring.** §7 read a
step change out of one noisy sample.

The sharpest evidence is r10 against r11, which differ in no behavioural
respect: Fisher p = **0.040**. Two runs of effectively identical code separate
at p < 0.05 on this measure. That is the calibration to keep: at these sample
sizes a p-value cannot distinguish a code change from run-to-run variance, so
single-run comparisons must not be treated as findings. This is the third time
in this project a conclusion has been drawn from one sample and then failed to
reproduce.

### Where that leaves the anchor path

- Coverage recovery works: `untrusted_base` drops fell from 5194 to ~435.
- Ordering works: 39 superseded, 0 anchored snapshots measured.
- The lost-update window is real but marginal at 0.07% of anchors. Option 2
  (re-anchor pools that dropped a delta above their anchor block) is now cheap
  to implement, because `note_anchor_window` already detects exactly that case.
- The residual ~0.5-1% Liquidity divergence is unchanged by any of this work and
  remains unexplained. It predates anchoring: the same residual appeared as
  1/224 in run 5, before any of it existed.

Local pricing on anchored lineage is no longer blocked by §7's finding, but the
unexplained residual is its own gate and has not moved.

## 9. Going after the residual, 2026-09-01 — the arithmetic is exonerated

The residual had been chased across five runs at roughly one observation per
178 validations, which is why it was never characterised: the sampled validator
is too slow an instrument for a sub-1% effect.

`audit_against_swap` replaces sampling with a census. A CL `Swap` carries
ABSOLUTE in-range liquidity, so when the tick has not moved it is a free,
exact check on everything the Mint/Burn path accumulated since the last
absolute write. No RPC, no sampling, and exact equality rather than a bps
threshold.

Run: 22m24s, no forced gaps.

| | |
|---|---|
| swap audits | **3664** |
| audit mismatches | **0** |
| validator reconciliations | 751 |
| validator divergences | 0 |
| `live_state_untrusted_base_total` | 0 |
| `live_state_lost_updates` | 0 |

**The Mint/Burn arithmetic is correct.** By the rule of three, 0 mismatches in
3664 trials bounds the true error rate below **0.082%** at 95% confidence —
roughly six times below the LOW end of the 0.5–3.9% residual the validator has
reported. Whatever the residual is, it is very unlikely to be the accumulation
arithmetic, and a lost Mint/Burn log would also have shown here.

### What this does not settle

The residual did not occur at all in this run, so it has not been caught by the
new instrument — only made catchable. The audit is also blind to a pool that
diverges and never afterwards receives a tick-unchanged swap.

What it does give is discrimination. If the residual recurs while
`live_state_swap_audit_mismatches` stays 0, the fault is in the comparison or
the validator, not in local state — which inverts where to look, and is exactly
the question five runs of sampling could not answer.

### Incidental: anchoring bootstraps pools that never trade

636 anchors with zero continuity breaks. These are pools with no snapshot at
all: previously a pool that emitted no event never entered `LiveState`, because
a delta with no base returns `NotStateBearing`. The anchor path now gives them a
base. That is a coverage gain independent of gap recovery, and it was not a
stated goal of the design.

## 10. The residual, characterised — 2026-09-01

A 30-minute run (the first to exceed 25 minutes) plus the swap audit finally
produced a characterisation. Two of my hypotheses died on the way.

### Established

- **Every pool that has ever diverged is an Aerodrome Slipstream GAUGE pool.**
  8 of 8, against a base rate of 140/414 = 33.8% of validated pools.
  p = 1.7e-4. This is the strongest signal in the whole investigation.
- **The Mint/Burn arithmetic is correct.** 4224 swap audits across two runs,
  1 mismatch. `ours == liquidity()` EXACTLY in 5 of 6 observations of the pool
  that did diverge.
- **Gauge pools do carry a second bucket.** `stakedLiquidity()` on the
  divergent pool returned 11797183206753424888 against `liquidity()`'s
  12431930927139055136 — nearly half the pool.
- **The 25-minute socket rotation works.** Connected 06:13:54, rotated
  06:38:54.585967, reconnected 1.9s later, `ingestion_ws_stalls_total` = 0.
  BlockPI never got to close it.

### Refuted, both mine

- **"We track total liquidity; the validator reads unstaked."** No. In 5 of 6
  observations `ours == liquidity()` exactly, so our value tracks the unstaked
  figure and the validator's reference is right.
- **"There is an undecoded event type on gauge pools."** No. `eth_getLogs` over
  the window returned only Mint, Burn and Collect. Nothing arrives that we fail
  to recognise.

### What the log replay actually shows

The divergent pool runs an **auto-compounding position**: a burn and re-mint of
~1.0007e19 every one to two blocks, each cycle slightly larger as fees compound.
Blocks 50727005-50727016 contain 10 in-range Mint/Burn events, and the position
is roughly 80% of the pool's unstaked liquidity.

Our excess at block 50727016 was 10007271035074632970. The final Burn in that
block, which has no matching Mint, was 10007339507485756762 — the same value to
within 0.0007%. Our snapshot behaved as though that last Burn had not been
applied.

### Where that points

Not at staking. At **event density**. A gauge pool hosting an auto-compounder
gets ~10 in-range position events per block, each worth ~80% of pool liquidity,
so any residual mid-block or ordering effect is both far more likely to occur
and enormously amplified when it does. That reframes the gauge correlation:
gauge pools are not special because of `stakedLiquidity`, but because they are
where the auto-compounders live.

The 0.0007% shortfall against a clean "we missed exactly that Burn" is not
explained and matters — an exact miss would be exact.

### Next

Replay this pool from a known anchor across the full lineage, event by event,
and find the first transition where our value parts from `liquidity()`. The
tooling now exists: `BASE_RPC_URLS` works, and both detectors agree on which
pool and block to examine.

**Note on the RPC:** `BASE_RPC_URLS` was never misconfigured. BlockPI returns
403 to requests carrying Python's default `urllib` User-Agent; the endpoint and
key are fine, and the bot has used them all along. An earlier claim in this
project that the key had been rotated was wrong.
