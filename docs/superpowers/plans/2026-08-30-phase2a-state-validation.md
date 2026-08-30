# Phase 2a — State Validation Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure whether log-derived pool state actually matches the chain, producing the divergence evidence the Phase 2 gate needs.

**Architecture:** A background task samples pools due for check, reads their true state over RPC **pinned to the block the snapshot came from**, compares, and records a verdict in `StateGate`. Comparison is a pure function so it is testable without a provider. Nothing prices from local state in this phase — that is Phase 2b, gated on 24h of what this produces.

**Tech Stack:** Rust 1.97.0, `ethers` 2.0.14, `prometheus` 0.13, `tokio` 1. Tests inline per module.

## Global Constraints

- Toolchain pinned at `1.97.0`. Do not bump.
- No new dependencies.
- Modules declared twice: `pub mod x;` in `src/lib.rs`, `mod x;` in `src/main.rs`.
- Items used only by lib/tests need `#[allow(dead_code)]`.
- Never hold a `std::sync::Mutex` guard across an `.await`.
- **Never run `cargo fmt`** — the repo has never been formatted and it would rewrite every file. `cargo clippy --all-targets` must add no NEW warnings; `edge_capacity_from_cl_state` in `venues.rs` is pre-existing.
- Verify with `cargo test --lib --bins --tests` plus `cargo test --doc`.
- **Validation must never block the hot path** (spec §4.5). The hot path calls only `gate.trusted()`, a cheap read. All RPC lives in the background task.

## THE CRITICAL CONSTRAINT: pin the read to the snapshot's block

A log-derived snapshot describes the pool **at the block its log came from**. Reading
the pool's current state and comparing gives you *market movement*, not decoder
correctness — the pool legitimately changed in between.

Both loaders take a block argument. **Always pass `snapshot.prov.ordinal.block`,
never `latest`.** Getting this wrong does not fail loudly; it produces a
plausible-looking divergence number that measures the wrong thing entirely, and
every downstream decision built on it would be wrong.

Consequences:
- Only snapshots carrying an `ordinal` can be validated. Anchored snapshots have
  `ordinal: None` — nothing to check, skip them.
- Reading an old block needs archive access. BlockPI's Base endpoint documents
  Archive Mode, but if the read fails the correct response is
  `gate.record(pool, None)` — "no measurement", NOT "agrees".
- Skip snapshots older than `MAX_VALIDATION_LAG_BLOCKS` (128) so we do not lean
  on deep archive.

## Scope

**In:** enumeration of tracked pools; a pure comparison producing the §9.1
reconciliation record; divergence metrics; the background task; wiring behind
`ARBOT_LIVE_STATE_SHADOW`.

**Out:** enabling local pricing (`ARBOT_LIVE_STATE`), `ScanSnapshot` (§4.2),
candidate staleness guards (§6). All Phase 2b, gated on this data.

**Reference spec:** `docs/superpowers/specs/2026-08-29-live-state-dirty-scan-design.md` §3.3, §4.5, §9.1

---

### Task 1: Enumerate tracked pools

The validation task needs candidates. Only pools the store actually holds a
snapshot for are worth checking.

**Files:**
- Modify: `src/live_state.rs`

**Interfaces:**
- Produces: `LiveState::tracked_v2(&self) -> Vec<Address>`, `LiveState::tracked_cl(&self) -> Vec<Address>`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/live_state.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib live_state::tests::tracked_lists`
Expected: FAIL to compile — `no method named tracked_v2`.

- [ ] **Step 3: Write minimal implementation**

Add to the `impl LiveState` block:

```rust
    /// Pools with a V2 snapshot. Validation candidates: a pool with no
    /// snapshot has nothing to compare against.
    pub fn tracked_v2(&self) -> Vec<Address> {
        self.v2.iter().map(|e| *e.key()).collect()
    }

    /// Pools with a CL snapshot.
    pub fn tracked_cl(&self) -> Vec<Address> {
        self.cl.iter().map(|e| *e.key()).collect()
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib live_state`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/live_state.rs
git commit -m "feat(live-state): enumerate tracked pools for validation

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: The reconciliation record and its comparison

Pure functions, no provider. This is where the §9.1 record is built and where
the worse-of-two-fields rule lives.

**Files:**
- Create: `src/reconcile.rs`
- Modify: `src/lib.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `crate::live_state::{V2Snapshot, ClSnapshot, TrustState}`, `crate::state_gate::divergence_bps`, `crate::quote_univ2::UniV2PairState`, `crate::cl_sim::ClPoolState`.
- Produces: `Reconciliation` struct, `compare_v2(&V2Snapshot, &UniV2PairState) -> Reconciliation`, `compare_cl(&ClSnapshot, &ClPoolState) -> Reconciliation`.

- [ ] **Step 1: Write the failing test**

Create `src/reconcile.rs` with only tests:

```rust
//! Comparison of log-derived state against a fresh RPC read.
//!
//! Pure: no provider, no async. The RPC read happens in the validation task;
//! this decides what the numbers mean.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_state::{ClSnapshot, Provenance, TrustState, V2Snapshot};
    use crate::quote_univ2::UniV2PairState;
    use ethers::types::{Address, U256};
    use std::time::Instant;

    fn prov(version: u64) -> Provenance {
        Provenance {
            state_version: version,
            anchor_id: 7,
            continuity_epoch: 3,
            ordinal: Some(crate::continuity::Ordinal {
                block: 500,
                tx_index: 1,
                log_index: 2,
            }),
            anchored_at: Instant::now(),
            trust: TrustState::Derived,
        }
    }

    fn v2_snap(r0: u64, r1: u64) -> V2Snapshot {
        V2Snapshot {
            state: UniV2PairState {
                token0: Address::from_low_u64_be(10),
                token1: Address::from_low_u64_be(11),
                reserve0: U256::from(r0),
                reserve1: U256::from(r1),
            },
            prov: prov(42),
        }
    }

    fn chain_v2(r0: u64, r1: u64) -> UniV2PairState {
        UniV2PairState {
            token0: Address::from_low_u64_be(10),
            token1: Address::from_low_u64_be(11),
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
        }
    }

    #[test]
    fn identical_v2_state_reports_zero_divergence() {
        let r = compare_v2(&v2_snap(1_000, 2_000), &chain_v2(1_000, 2_000));
        assert_eq!(r.relative_delta_bps, Some(0));
        assert_eq!(r.local_state_version, 42);
        assert_eq!(r.anchor_id, 7);
        assert_eq!(r.continuity_epoch, 3);
    }

    /// Both reserves are compared and the WORSE one wins. A decoder that reads
    /// one field correctly and the other from the wrong offset must not be
    /// averaged into looking healthy.
    #[test]
    fn v2_takes_the_worse_of_the_two_reserves() {
        // reserve0 exact, reserve1 out by 50%.
        let r = compare_v2(&v2_snap(1_000, 2_000), &chain_v2(1_000, 4_000));
        assert_eq!(
            r.relative_delta_bps,
            Some(-5_000),
            "must report the 50% miss, not average it away"
        );
    }

    #[test]
    fn v2_divergence_is_signed() {
        let over = compare_v2(&v2_snap(1_100, 2_000), &chain_v2(1_000, 2_000));
        assert_eq!(over.relative_delta_bps, Some(1_000));
        let under = compare_v2(&v2_snap(900, 2_000), &chain_v2(1_000, 2_000));
        assert_eq!(under.relative_delta_bps, Some(-1_000));
    }

    /// A zero on-chain reserve makes the ratio meaningless. It must report
    /// "no measurement" rather than agreement — the whole point of Option.
    #[test]
    fn v2_declines_on_a_zero_reference() {
        let r = compare_v2(&v2_snap(1_000, 2_000), &chain_v2(0, 2_000));
        assert_eq!(r.relative_delta_bps, None);
    }

    fn cl_snap(sqrt: u64, liq: u128, tick: i32) -> ClSnapshot {
        ClSnapshot {
            sqrt_price_x96: U256::from(sqrt),
            liquidity: liq,
            tick,
            prov: prov(9),
        }
    }

    fn chain_cl(sqrt: u64, liq: u128, tick: i32) -> crate::cl_sim::ClPoolState {
        crate::cl_sim::ClPoolState {
            sqrt_price_x96: U256::from(sqrt),
            liquidity: liq,
            tick,
            tick_spacing: 60,
            fee_ppm: 3000,
            balance0: None,
            balance1: None,
        }
    }

    #[test]
    fn identical_cl_state_reports_zero_divergence() {
        let r = compare_cl(&cl_snap(1_000_000, 5_000, -100), &chain_cl(1_000_000, 5_000, -100));
        assert_eq!(r.relative_delta_bps, Some(0));
        assert_eq!(r.tick_delta, Some(0));
    }

    /// sqrt_price and liquidity are both compared; the worse wins. A decoder
    /// reading sqrt_price from the right offset and liquidity from the wrong
    /// one is exactly the bug this must catch.
    #[test]
    fn cl_takes_the_worse_of_price_and_liquidity() {
        let r = compare_cl(&cl_snap(1_000_000, 5_000, 0), &chain_cl(1_000_000, 10_000, 0));
        assert_eq!(r.relative_delta_bps, Some(-5_000), "liquidity halved must surface");
    }

    /// Tick is reported separately and NOT folded into bps: it is a signed
    /// exponent, so a bps ratio on it is meaningless. An off-by-sign tick is
    /// catastrophic and must be visible on its own axis.
    #[test]
    fn cl_reports_tick_delta_separately() {
        let r = compare_cl(&cl_snap(1_000, 5_000, -198_238), &chain_cl(1_000, 5_000, 198_238));
        assert_eq!(r.relative_delta_bps, Some(0), "price and liquidity agree");
        assert_eq!(
            r.tick_delta,
            Some(-396_476),
            "a sign-flipped tick must be visible even when price matches"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib reconcile`
Expected: FAIL to compile — `cannot find function compare_v2`, module unknown.

- [ ] **Step 3: Write minimal implementation**

Add above the tests in `src/reconcile.rs`:

```rust
use crate::live_state::{ClSnapshot, TrustState, V2Snapshot};
use crate::state_gate::divergence_bps;
use ethers::types::U256;

/// One comparison of local state against the chain — the spec §9.1 record.
///
/// Field names match the spec so the emitted log and the document stay in step.
#[derive(Clone, Debug)]
pub struct Reconciliation {
    pub local: String,
    pub on_chain: String,
    pub absolute_delta: String,
    /// Signed, worst field. `None` means the ratio is not representable, which
    /// is "no measurement", NOT agreement.
    pub relative_delta_bps: Option<i64>,
    /// CL only. Reported separately because a tick is a signed exponent and a
    /// bps ratio on it is meaningless.
    pub tick_delta: Option<i32>,
    pub local_state_version: u64,
    pub anchor_id: u64,
    pub continuity_epoch: u64,
    pub trust_state: TrustState,
    pub venue: &'static str,
}

fn abs_delta(a: U256, b: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        b - a
    }
}

/// Worse of two divergences, preserving sign. `None` from either is contagious:
/// an unmeasurable field means the comparison as a whole is unmeasurable.
fn worse(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if x.abs() >= y.abs() { x } else { y }),
        _ => None,
    }
}

pub fn compare_v2(
    local: &V2Snapshot,
    chain: &crate::quote_univ2::UniV2PairState,
) -> Reconciliation {
    let d0 = divergence_bps(local.state.reserve0, chain.reserve0);
    let d1 = divergence_bps(local.state.reserve1, chain.reserve1);
    Reconciliation {
        local: format!("r0={} r1={}", local.state.reserve0, local.state.reserve1),
        on_chain: format!("r0={} r1={}", chain.reserve0, chain.reserve1),
        absolute_delta: format!(
            "r0={} r1={}",
            abs_delta(local.state.reserve0, chain.reserve0),
            abs_delta(local.state.reserve1, chain.reserve1)
        ),
        relative_delta_bps: worse(d0, d1),
        tick_delta: None,
        local_state_version: local.prov.state_version,
        anchor_id: local.prov.anchor_id,
        continuity_epoch: local.prov.continuity_epoch,
        trust_state: local.prov.trust,
        venue: "v2",
    }
}

pub fn compare_cl(local: &ClSnapshot, chain: &crate::cl_sim::ClPoolState) -> Reconciliation {
    let dp = divergence_bps(local.sqrt_price_x96, chain.sqrt_price_x96);
    let dl = divergence_bps(U256::from(local.liquidity), U256::from(chain.liquidity));
    Reconciliation {
        local: format!(
            "sqrtP={} L={} tick={}",
            local.sqrt_price_x96, local.liquidity, local.tick
        ),
        on_chain: format!(
            "sqrtP={} L={} tick={}",
            chain.sqrt_price_x96, chain.liquidity, chain.tick
        ),
        absolute_delta: format!(
            "sqrtP={} L={}",
            abs_delta(local.sqrt_price_x96, chain.sqrt_price_x96),
            abs_delta(U256::from(local.liquidity), U256::from(chain.liquidity))
        ),
        relative_delta_bps: worse(dp, dl),
        tick_delta: Some(local.tick - chain.tick),
        local_state_version: local.prov.state_version,
        anchor_id: local.prov.anchor_id,
        continuity_epoch: local.prov.continuity_epoch,
        trust_state: local.prov.trust,
        venue: "cl",
    }
}
```

Declare `pub mod reconcile;` in `src/lib.rs` and `mod reconcile;` in `src/main.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib reconcile`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add src/reconcile.rs src/lib.rs src/main.rs
git commit -m "feat(reconcile): compare log-derived state against the chain

Worse-of-fields rather than an average: a decoder that reads one field
correctly and another from the wrong offset must not look healthy. Tick is
reported on its own axis because a bps ratio on a signed exponent is
meaningless.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Divergence metrics

**Files:**
- Modify: `src/metrics.rs`

**Interfaces:**
- Produces: `Metrics::live_state_divergence_bps: HistogramVec` (label `venue`), `Metrics::live_state_checks: CounterVec` (label `outcome`), `Metrics::live_state_trusted: Gauge`, `Metrics::live_state_untrusted: Gauge`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/metrics.rs`:

```rust
    #[test]
    fn validation_metrics_are_registered_and_labelled() {
        let m = Metrics::new().expect("metrics");
        m.live_state_divergence_bps
            .with_label_values(&["cl"])
            .observe(3.0);
        m.live_state_checks.with_label_values(&["measured"]).inc();
        m.live_state_checks.with_label_values(&["unreachable"]).inc();
        m.live_state_trusted.set(5.0);
        m.live_state_untrusted.set(2.0);
        assert_eq!(m.live_state_trusted.get(), 5.0);
        assert_eq!(
            m.live_state_checks.with_label_values(&["measured"]).get(),
            1.0
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib metrics`
Expected: FAIL to compile — `no field live_state_divergence_bps`.

- [ ] **Step 3: Write minimal implementation**

Add fields to `pub struct Metrics`:

```rust
    /// Signed divergence of log-derived state from a fresh RPC read, by venue.
    pub live_state_divergence_bps: HistogramVec,
    /// Validation attempts by outcome: measured | unreachable | skipped_lag | no_ordinal.
    pub live_state_checks: CounterVec,
    pub live_state_trusted: Gauge,
    pub live_state_untrusted: Gauge,
```

Register in `new()`, following the existing `HistogramVec`/`CounterVec` patterns in this file:

```rust
        let live_state_divergence_bps = HistogramVec::new(
            HistogramOpts::new(
                "live_state_divergence_bps",
                "Signed divergence of log-derived state from a fresh RPC read",
            )
            .buckets(vec![-10000.0, -1000.0, -100.0, -5.0, 0.0, 5.0, 100.0, 1000.0, 10000.0]),
            &["venue"],
        )?;
        registry
            .register(Box::new(live_state_divergence_bps.clone()))
            .context("register live_state_divergence_bps histogram")?;

        let live_state_checks = CounterVec::new(
            Opts::new(
                "live_state_checks_total",
                "State validation attempts by outcome",
            ),
            &["outcome"],
        )?;
        registry
            .register(Box::new(live_state_checks.clone()))
            .context("register live_state_checks_total counter")?;

        let live_state_trusted = Gauge::with_opts(Opts::new(
            "live_state_trusted_pools",
            "Pools whose log-derived state currently passes the state gate",
        ))?;
        registry
            .register(Box::new(live_state_trusted.clone()))
            .context("register live_state_trusted_pools gauge")?;

        let live_state_untrusted = Gauge::with_opts(Opts::new(
            "live_state_untrusted_pools",
            "Pools tracked by the state gate that do not currently pass",
        ))?;
        registry
            .register(Box::new(live_state_untrusted.clone()))
            .context("register live_state_untrusted_pools gauge")?;
```

Add all four to the `Self { ... }` literal. No import changes needed: `Counter`, `CounterVec`, `Gauge`, `GaugeVec`, `Histogram`, `HistogramOpts`, `HistogramVec` and `Opts` are all already imported at `metrics.rs:5-8`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib metrics`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs
git commit -m "feat(metrics): add state-validation divergence metrics

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Selecting what to validate

Pure selection logic, split out so the block-pinning rule is testable without a
provider. This is the task that enforces THE CRITICAL CONSTRAINT.

**Files:**
- Create: `src/validation_select.rs`
- Modify: `src/lib.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `crate::continuity::Ordinal`.
- Produces: `MAX_VALIDATION_LAG_BLOCKS: u64`, `enum SelectOutcome { Check { block: u64 }, NoOrdinal, TooOld { lag: u64 } }`, `fn select(ordinal: Option<Ordinal>, head_block: u64) -> SelectOutcome`.

- [ ] **Step 1: Write the failing test**

Create `src/validation_select.rs` with only tests:

```rust
//! Which snapshots can be validated, and at which block.
//!
//! Split out from the validation task so the block-pinning rule is testable
//! without a provider. Getting it wrong does not fail loudly — it measures
//! market movement and calls it decoder divergence.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuity::Ordinal;

    fn ord(block: u64) -> Option<Ordinal> {
        Some(Ordinal {
            block,
            tx_index: 0,
            log_index: 0,
        })
    }

    /// The read MUST be pinned to the snapshot's own block. Comparing against
    /// `latest` measures how much the pool moved since, not whether the
    /// decoder is right.
    #[test]
    fn selects_the_snapshots_own_block_not_the_head() {
        assert_eq!(select(ord(1_000), 1_050), SelectOutcome::Check { block: 1_000 });
    }

    #[test]
    fn a_snapshot_at_the_head_is_checked_at_the_head() {
        assert_eq!(select(ord(1_050), 1_050), SelectOutcome::Check { block: 1_050 });
    }

    /// An anchored snapshot came from RPC, not a log — there is nothing to
    /// validate and no ordinal to pin to.
    #[test]
    fn a_snapshot_without_an_ordinal_is_not_checkable() {
        assert_eq!(select(None, 1_050), SelectOutcome::NoOrdinal);
    }

    /// Reading far-back state needs deep archive. Decline rather than depend
    /// on it.
    #[test]
    fn a_snapshot_older_than_the_lag_budget_is_skipped() {
        let head = 10_000;
        let old = head - MAX_VALIDATION_LAG_BLOCKS - 1;
        assert_eq!(select(ord(old), head), SelectOutcome::TooOld { lag: MAX_VALIDATION_LAG_BLOCKS + 1 });
    }

    #[test]
    fn a_snapshot_exactly_at_the_lag_budget_is_still_checked() {
        let head = 10_000;
        let edge = head - MAX_VALIDATION_LAG_BLOCKS;
        assert_eq!(select(ord(edge), head), SelectOutcome::Check { block: edge });
    }

    /// A snapshot ahead of our notion of the head is not an error to divide by:
    /// heads and logs arrive on different sockets and can race.
    #[test]
    fn a_snapshot_ahead_of_the_head_is_checked_not_rejected() {
        assert_eq!(select(ord(1_051), 1_050), SelectOutcome::Check { block: 1_051 });
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib validation_select`
Expected: FAIL to compile — `cannot find function select`.

- [ ] **Step 3: Write minimal implementation**

Add above the tests:

```rust
use crate::continuity::Ordinal;

/// How far back a snapshot may be and still be validated.
///
/// Reading state at an old block needs archive access; 128 blocks is ~4 minutes
/// on Base, comfortably inside any provider's recent-state window.
pub const MAX_VALIDATION_LAG_BLOCKS: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectOutcome {
    /// Validate, reading chain state AT THIS BLOCK — never at the head.
    Check { block: u64 },
    /// Anchored snapshot: came from RPC, nothing to compare.
    NoOrdinal,
    TooOld { lag: u64 },
}

pub fn select(ordinal: Option<Ordinal>, head_block: u64) -> SelectOutcome {
    let Some(o) = ordinal else {
        return SelectOutcome::NoOrdinal;
    };
    // Saturating: the log socket and the head socket race, so a snapshot can
    // legitimately be ahead of our head. That is not an error.
    let lag = head_block.saturating_sub(o.block);
    if lag > MAX_VALIDATION_LAG_BLOCKS {
        return SelectOutcome::TooOld { lag };
    }
    SelectOutcome::Check { block: o.block }
}
```

Declare `pub mod validation_select;` in `src/lib.rs` and `mod validation_select;` in `src/main.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib validation_select`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/validation_select.rs src/lib.rs src/main.rs
git commit -m "feat(validation): pin state checks to the snapshot's own block

Comparing a log-derived snapshot against head state measures how far the
pool moved since, not whether the decoder is correct. That failure is
silent, so the rule is split out and tested on its own.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The background validation task

**Files:**
- Create: `src/state_validation.rs`
- Modify: `src/lib.rs`, `src/main.rs`

**Interfaces:**
- Consumes: everything above, plus `crate::quote_univ2::load_pair_states_batched`, `crate::cl_sim::load_cl_pool_states_batched`, `crate::state_gate::gate()`.
- Produces: `pub async fn run_state_validation<C>(provider: Arc<Provider<C>>, live: Arc<LiveState>, metrics: Option<Arc<Metrics>>, interval: Duration) -> !`

- [ ] **Step 1: Write the failing test**

Create `src/state_validation.rs` with only tests:

```rust
//! Background validation: does log-derived state match the chain?
//!
//! Runs entirely off the hot path (spec §4.5). The searcher only ever calls
//! `gate.trusted()`, a cheap read. A slow or failing RPC here downgrades trust
//! by letting the TTL expire; it never stalls anything.

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed read must record NOTHING. Recording it as agreement would grant
    /// trust on the strength of an RPC outage — the exact failure the gate
    /// exists to prevent.
    #[test]
    fn an_unreachable_pool_records_no_verdict() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(1);
        record_outcome(&gate, pool, None, None);
        assert!(!gate.trusted(pool), "an unreachable read must not grant trust");
    }

    #[test]
    fn a_matching_pool_records_a_passing_verdict() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(2);
        record_outcome(&gate, pool, Some(0), None);
        assert!(gate.trusted(pool));
    }

    #[test]
    fn a_diverging_pool_records_a_failing_verdict() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(3);
        record_outcome(&gate, pool, Some(4_000), None);
        assert!(!gate.trusted(pool));
    }

    /// A tick mismatch is disqualifying even when price and liquidity agree:
    /// the pool is on the wrong side of the curve, which no bps figure shows.
    #[test]
    fn a_tick_mismatch_fails_even_when_bps_is_zero() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(4);
        record_outcome(&gate, pool, Some(0), Some(-396_476));
        assert!(
            !gate.trusted(pool),
            "price agreeing does not excuse a wrong tick"
        );
    }

    #[test]
    fn a_zero_tick_delta_does_not_disqualify() {
        let gate = crate::state_gate::StateGate::for_test(5, 300, 8);
        let pool = ethers::types::Address::from_low_u64_be(5);
        record_outcome(&gate, pool, Some(1), Some(0));
        assert!(gate.trusted(pool));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib state_validation`
Expected: FAIL to compile — `cannot find function record_outcome`.

- [ ] **Step 3: Write minimal implementation**

Add above the tests:

```rust
use crate::live_state::LiveState;
use crate::metrics::Metrics;
use crate::reconcile::{compare_cl, compare_v2, Reconciliation};
use crate::state_gate::StateGate;
use crate::validation_select::{select, SelectOutcome};
use ethers::prelude::*;
use ethers::providers::JsonRpcClient;
use ethers::types::Address;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Turn one comparison into a gate verdict.
///
/// A non-zero `tick_delta` fails regardless of bps: the pool is on the wrong
/// side of the curve, and no price ratio reveals that.
pub(crate) fn record_outcome(
    gate: &StateGate,
    pool: Address,
    bps: Option<i64>,
    tick_delta: Option<i32>,
) {
    let verdict = match (bps, tick_delta) {
        (None, _) => None,
        (Some(_), Some(t)) if t != 0 => Some(i64::MAX),
        (Some(b), _) => Some(b),
    };
    gate.record(pool, verdict);
}

fn emit(rec: &Reconciliation, pool: Address, block: u64, metrics: Option<&Arc<Metrics>>) {
    if let (Some(m), Some(bps)) = (metrics, rec.relative_delta_bps) {
        m.live_state_divergence_bps
            .with_label_values(&[rec.venue])
            .observe(bps as f64);
    }
    // The spec §9.1 reconciliation record, one line per check.
    debug!(
        target: "state_validation",
        pool = %format!("{pool:#x}"),
        venue = rec.venue,
        block,
        local = %rec.local,
        on_chain = %rec.on_chain,
        absolute_delta = %rec.absolute_delta,
        relative_delta_bps = ?rec.relative_delta_bps,
        tick_delta = ?rec.tick_delta,
        local_state_version = rec.local_state_version,
        anchor_id = rec.anchor_id,
        continuity_epoch = rec.continuity_epoch,
        trust_state = ?rec.trust_state,
        "state reconciliation"
    );
}

/// One validation pass. Returns the number of pools actually measured.
pub async fn validate_once<C>(
    provider: &Arc<Provider<C>>,
    live: &Arc<LiveState>,
    gate: &StateGate,
    metrics: Option<&Arc<Metrics>>,
) -> usize
where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let Ok(head) = provider.get_block_number().await else {
        warn!("state validation: block number unavailable; skipping pass");
        return 0;
    };
    let head = head.as_u64();
    let mut measured = 0usize;

    let count = |outcome: &str| {
        if let Some(m) = metrics {
            m.live_state_checks.with_label_values(&[outcome]).inc();
        }
    };

    // --- V2 ---
    for pool in gate.due_for_check(live.tracked_v2()) {
        let Some(snap) = live.v2_snapshot(pool) else {
            continue;
        };
        let block = match select(snap.prov.ordinal, head) {
            SelectOutcome::Check { block } => block,
            SelectOutcome::NoOrdinal => {
                count("no_ordinal");
                continue;
            }
            SelectOutcome::TooOld { .. } => {
                count("skipped_lag");
                continue;
            }
        };
        // Pinned to the snapshot's block, NOT the head.
        let states =
            crate::quote_univ2::load_pair_states_batched(provider.clone(), &[pool], block.into())
                .await;
        match states.get(&pool) {
            Some(chain) => {
                let rec = compare_v2(&snap, chain);
                emit(&rec, pool, block, metrics);
                record_outcome(gate, pool, rec.relative_delta_bps, rec.tick_delta);
                count("measured");
                measured += 1;
            }
            None => {
                record_outcome(gate, pool, None, None);
                count("unreachable");
            }
        }
    }

    // --- CL ---
    for pool in gate.due_for_check(live.tracked_cl()) {
        let Some(snap) = live.cl_snapshot(pool) else {
            continue;
        };
        let block = match select(snap.prov.ordinal, head) {
            SelectOutcome::Check { block } => block,
            SelectOutcome::NoOrdinal => {
                count("no_ordinal");
                continue;
            }
            SelectOutcome::TooOld { .. } => {
                count("skipped_lag");
                continue;
            }
        };
        // Zero token addresses skip the balanceOf sub-calls: this phase compares
        // slot0 and liquidity only, and balances are Phase 2b's concern.
        let req = [(pool, None, Address::zero(), Address::zero())];
        let states =
            crate::cl_sim::load_cl_pool_states_batched(provider.clone(), &req, block.into()).await;
        match states.get(&pool) {
            Some(chain) => {
                let rec = compare_cl(&snap, chain);
                emit(&rec, pool, block, metrics);
                record_outcome(gate, pool, rec.relative_delta_bps, rec.tick_delta);
                count("measured");
                measured += 1;
            }
            None => {
                record_outcome(gate, pool, None, None);
                count("unreachable");
            }
        }
    }

    if let Some(m) = metrics {
        let (trusted, untrusted, _) = gate.stats();
        m.live_state_trusted.set(trusted as f64);
        m.live_state_untrusted.set(untrusted as f64);
    }
    measured
}

/// Loop forever, validating a bounded sample each pass.
pub async fn run_state_validation<C>(
    provider: Arc<Provider<C>>,
    live: Arc<LiveState>,
    metrics: Option<Arc<Metrics>>,
    interval: Duration,
) where
    C: JsonRpcClient + Clone + Send + Sync + 'static,
{
    let gate = crate::state_gate::gate();
    loop {
        let measured = validate_once(&provider, &live, gate, metrics.as_ref()).await;
        if measured > 0 {
            let (trusted, untrusted, total) = gate.stats();
            debug!(measured, trusted, untrusted, total, "state validation pass");
        }
        tokio::time::sleep(interval).await;
    }
}
```

Declare `pub mod state_validation;` in `src/lib.rs` and `mod state_validation;` in `src/main.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib state_validation && cargo build --all-targets 2>&1 | grep -E "^error" | head`
Expected: 5 tests PASS, no build errors.

- [ ] **Step 5: Commit**

```bash
git add src/state_validation.rs src/lib.rs src/main.rs
git commit -m "feat(validation): add the background state validation loop

Reads each pool's true state pinned to the snapshot's own block, compares,
and records a gate verdict. A failed read records nothing — granting trust
on the strength of an RPC outage is the failure the gate exists to prevent.

Runs off the hot path: the searcher only calls gate.trusted().

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Wire the task in, behind the existing flag

**Files:**
- Modify: `src/main.rs` (where `ARBOT_LIVE_STATE_SHADOW` constructs `LiveState`)

**Interfaces:**
- Consumes: `run_state_validation` from Task 5.

- [ ] **Step 1: Find the wiring point**

Run: `grep -n "ARBOT_LIVE_STATE_SHADOW" src/main.rs`
Expected: one hit, inside the `Ok(monitor) =>` arm of the `PoolMonitor::new` match.

- [ ] **Step 2: Write minimal implementation**

Replace the shadow-mode block so the store is also handed to the validation task:

```rust
                    let monitor = if crate::util::env_flag("ARBOT_LIVE_STATE_SHADOW", false) {
                        let live = Arc::new(crate::live_state::LiveState::new());
                        info!(
                            "live-state shadow mode enabled; state is recorded and measured, \
                             never priced"
                        );
                        // Validation runs off the hot path: it owns its own RPC
                        // budget and the searcher never waits on it.
                        let validation_live = Arc::clone(&live);
                        let validation_provider = provider.clone();
                        let validation_metrics = metrics.clone();
                        let interval_secs =
                            crate::util::env_parse_opt::<u64>("ARBOT_STATE_VALIDATION_SECS")
                                .unwrap_or(15)
                                .max(1);
                        spawn_supervised(
                            "state_validation",
                            cfg.name.clone(),
                            metrics.clone(),
                            move || {
                                let live = Arc::clone(&validation_live);
                                let provider = validation_provider.clone();
                                let metrics = validation_metrics.clone();
                                async move {
                                    crate::state_validation::run_state_validation(
                                        provider,
                                        live,
                                        metrics,
                                        Duration::from_secs(interval_secs),
                                    )
                                    .await;
                                }
                            },
                        );
                        monitor.with_live_state(live)
                    } else {
                        monitor
                    };
```

- [ ] **Step 3: Verify it builds and the suite is green**

Run: `cargo build --all-targets 2>&1 | grep -E "^error" | head && cargo test --lib --bins --tests 2>&1 | grep "test result"`
Expected: no errors, all suites ok.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(validation): spawn the state validation task in shadow mode

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Verification and first divergence measurement

**Files:** none modified.

- [ ] **Step 1: Static verification**

Run: `cargo test 2>&1 | grep -E "test result|FAILED"` — all ok.
Run: `cargo clippy --all-targets 2>&1 | grep -E "^warning:|^error:" | grep -v generated` — only `edge_capacity_from_cl_state`.
Run: `cargo test --features loom-model --lib loom_dirty` — ok.
Do NOT run `cargo fmt`.

- [ ] **Step 2: Field run**

```bash
cargo build --release
ARBOT_LIVE_STATE_SHADOW=1 RUST_LOG=info,state_validation=debug ./target/release/arb-exec > validation.log 2>&1 &
```

Let it run 10 minutes, then:

```bash
curl -s localhost:9100/metrics | grep -E "^live_state_(checks|trusted|untrusted|divergence)"
grep -a "state reconciliation" validation.log | head -5
```

- [ ] **Step 3: Read the result honestly**

| Observation | Meaning | Action |
|---|---|---|
| `live_state_checks_total{outcome="measured"}` climbing | validation is running | good |
| `outcome="unreachable"` dominant | provider will not serve the pinned block | archive access problem, NOT a decoder problem — check before concluding anything about state |
| `outcome="skipped_lag"` dominant | snapshots aging out before checks run | lower `ARBOT_STATE_VALIDATION_SECS` or raise `ARBOT_STATE_GATE_CHECKS_PER_SCAN` |
| `live_state_trusted_pools` > 0 and rising | **the decoders genuinely match the chain** — the first evidence of this | good |
| divergence clustered at one venue | a decoder bug in that venue | investigate before Phase 2b |
| large `tick_delta` in the records | sign-extension bug | investigate immediately; this is catastrophic in pricing |

**Do not proceed to Phase 2b on a short run.** The §9 gate is per-venue p99
`relative_delta_bps` <= 5 over >= 24h. A ten-minute sample can only show the
loop works, never that the state is trustworthy.

- [ ] **Step 4: Record the baseline**

Append the observed per-venue divergence distribution and the trusted/untrusted
split to spec §10, replacing the current note that the gate has no data.

---

## Self-Review

**Spec coverage.** §3.3 state gate consumers → Tasks 5, 6. §4.5 validation off the
hot path → Task 5 (all RPC in the background task; hot path unchanged). §9.1
reconciliation record → Task 2 (`Reconciliation`, field names matching the spec)
and Task 5 (`emit`). §9.2 metrics → Task 3. Phase 2 gate data → Task 7.

**Deliberately out, stated in Scope:** enabling local pricing, §4.2
`ScanSnapshot`, §6 candidate guards. All Phase 2b.

**Type consistency.** `Ordinal` (continuity) → Task 4 `select`. `SelectOutcome`
Task 4 → Task 5. `Reconciliation` Task 2 → Task 5 `emit`/`record_outcome`.
`tracked_v2`/`tracked_cl` Task 1 → Task 5. `StateGate::{due_for_check, record,
trusted, stats}` already exist. `load_pair_states_batched(provider, &[Address],
U64)` and `load_cl_pool_states_batched(provider, &[(Address, Option<u32>,
Address, Address)], U64)` verified against source.

**The one constraint that decides whether any of this means anything** is
block-pinning, which is why it has its own module and six tests. Comparing
against head state would produce a confident, plausible, entirely wrong number —
and nothing downstream would reveal the error.

**Known limitation.** CL comparison covers `sqrt_price_x96`, `liquidity` and
`tick` — not `balance0`/`balance1`, which Phase 1 does not track. So this
validates the price/liquidity decode, not pool depth. Phase 2b must not read a
passing verdict here as evidence that CL *capacity* is correct.
