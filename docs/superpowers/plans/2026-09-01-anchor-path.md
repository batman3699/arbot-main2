# Anchor Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a pool regain trust from a block-pinned RPC read instead of waiting for its next trade, without letting that read collide with the log stream.

**Architecture:** An `eth_call` pinned to block N returns end-of-block-N state, whose true position in the chain's total order is `Ordinal::end_of_block(N)` — it sorts after every log in block N. Giving anchors that ordinal makes RPC reads and log events directly comparable under the existing `Ord`, so a per-pool monotonicity rule can discard logs the anchor already reflects. Anchored snapshots are then excluded from validation, because comparing an `eth_call` against the `eth_call` it came from passes by construction.

**Tech Stack:** Rust 1.97.0 (pinned), ethers 2.0.14, dashmap 6, tokio, prometheus 0.13.

**Design:** `docs/superpowers/specs/2026-09-01-anchor-path-design.md`

## Global Constraints

- **Never run `cargo fmt`.** The repo has never been formatted; it would rewrite every file including ones holding uncommitted work.
- **Never `git add -A`.** The working tree holds ~7 unrelated modified files that must stay untouched. Add named paths only.
- Pre-existing baseline: `cargo clippy --all-targets` emits exactly **3 warnings** (all `edge_capacity_from_cl_state` dead-code). Any 4th warning is a regression introduced by this work.
- Full suite before every commit: `cargo test`. Baseline at plan time: **440 lib + 604 bin passing, 1 ignored, 0 failures.**
- `main.rs` compiles its own copy of `continuity.rs`; a change there must build under both.
- Metrics port is **9100** (`PROMETHEUS_PORT` in `.env`), not 9184.
- Scrape `curl -s http://localhost:9100/metrics` **before** killing any field run.

---

### Task 1: `Ordinal::end_of_block`

Gives an RPC read a position in the same order the log stream uses. Everything else in this plan depends on it.

**Files:**
- Modify: `src/continuity.rs` (add an `impl Ordinal` block after the struct definition at :15-20)
- Test: `src/continuity.rs` (`mod tests`, ~:79)

**Interfaces:**
- Consumes: `Ordinal { block, tx_index, log_index }`, which already `derive(Ord)` comparing block, then tx_index, then log_index.
- Produces: `Ordinal::end_of_block(block: u64) -> Ordinal`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/continuity.rs`:

```rust
    /// An `eth_call` pinned to block N returns state as of the END of N, so its
    /// position is above every log in that block. This is what makes an RPC
    /// read and the log stream comparable at all.
    #[test]
    fn an_anchor_sorts_after_every_log_in_its_block() {
        let anchor = Ordinal::end_of_block(100);
        for (t, l) in [(0u64, 0u64), (5, 3), (u64::MAX - 1, u64::MAX)] {
            assert!(
                ord(100, t, l) < anchor,
                "a log at tx {t} log {l} must precede end-of-block state"
            );
        }
        assert!(
            anchor < ord(101, 0, 0),
            "but the next block's first log must still come after"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib continuity::tests::an_anchor_sorts_after 2>&1 | tail -20`
Expected: FAIL — `no function or associated item named 'end_of_block' found`

- [ ] **Step 3: Write minimal implementation**

Insert directly after the `Ordinal` struct definition in `src/continuity.rs`:

```rust
impl Ordinal {
    /// Position of end-of-block state, as returned by an `eth_call` pinned to
    /// `block`.
    ///
    /// Saturating tx_index and log_index puts this above every log in that
    /// block under `derive(Ord)`, which is not a trick: end-of-block state
    /// genuinely comes after all of them. It is what lets an RPC read be
    /// ordered against the log stream instead of sitting outside it.
    pub fn end_of_block(block: u64) -> Self {
        Ordinal {
            block,
            tx_index: u64::MAX,
            log_index: u64::MAX,
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib continuity 2>&1 | grep "test result"`
Expected: PASS, no failures.

- [ ] **Step 5: Commit**

```bash
git add src/continuity.rs
git commit -m "feat(continuity): give end-of-block RPC state an ordinal"
```

---

### Task 2: `Superseded` outcome and the per-pool monotonicity guard

The correctness core. Without this, an anchor and a log for the same block both apply and double-count.

**Files:**
- Modify: `src/live_state.rs` — `ApplyOutcome` (:129-143), `apply_log` (guard goes after the cursor observation at :319-323)
- Modify: `src/ingestion.rs` — the `ApplyOutcome` match (~:562)
- Test: `src/live_state.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Ordinal::end_of_block` (Task 1); existing `Provenance.ordinal: Option<Ordinal>`.
- Produces: `ApplyOutcome::Superseded`; `LiveState::current_ordinal(&self, pool: Address) -> Option<Ordinal>`; free fn `supersedes(existing: Option<Ordinal>, incoming: Ordinal) -> bool`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/live_state.rs`:

```rust
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
        assert!(supersedes(None, Ordinal { block: 1, tx_index: 0, log_index: 0 }));
    }
```

Note: these tests call `anchor_cl` with the **new** 5-argument signature from Task 3 (`pool, block, sqrt_price_x96, liquidity, tick`). Implement Task 3's signature change as part of Step 3 below so this task compiles standalone; Task 3 then only adds the ordinal stamping and its own tests.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib live_state 2>&1 | grep -E "^error" -A5 | head -20`
Expected: FAIL — `no variant named 'Superseded'`, `cannot find function 'supersedes'`.

- [ ] **Step 3: Write minimal implementation**

In `src/live_state.rs`, add the variant to `ApplyOutcome`:

```rust
    /// A log older than the pool's own snapshot — normally because an anchor
    /// has already carried that pool past this point, and an end-of-block RPC
    /// read already includes every log in its block.
    ///
    /// Deliberately NOT `ContinuityBroken`: that path calls
    /// `break_continuity`, which invalidates every pool at once. This is
    /// expected traffic on any anchored pool, so treating it as disorder would
    /// make anchoring catastrophically worse than leaving pools untrusted.
    Superseded,
```

Add the free function near `may_price_locally`:

```rust
/// True when `incoming` is strictly newer than what the pool already holds.
///
/// `None` accepts anything: a pool with no ordinal has no position to be older
/// than. That case shrinks to nothing once anchors carry
/// `Ordinal::end_of_block`, but a snapshot predating this change may still have
/// one, and refusing those would freeze the pool.
pub fn supersedes(existing: Option<Ordinal>, incoming: Ordinal) -> bool {
    match existing {
        None => true,
        Some(prev) => incoming > prev,
    }
}
```

Add the lookup to `impl LiveState`:

```rust
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
```

In `apply_log`, immediately after the `match observation` block resolves to acceptance and **before** any decoder runs, insert:

```rust
        // Per-pool ordering. The global cursor guarantees the STREAM is
        // ordered, which used to imply per-pool ordering because every update
        // arrived through it. Anchors do not, so the implication has to become
        // an explicit check.
        if !supersedes(self.current_ordinal(log.address), ordinal) {
            return ApplyOutcome::Superseded;
        }
```

Also apply Task 3's signature change now so the tests compile — change `anchor_cl` and `anchor_v2` to take `block: u64` as their second parameter and ignore it for the moment:

```rust
    pub fn anchor_cl(
        &self,
        pool: Address,
        block: u64,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
    ) {
```

```rust
    pub fn anchor_v2(&self, pool: Address, block: u64, state: UniV2PairState) {
```

Stamp the ordinal in both (this is what makes the tests pass — `provenance`'s second argument is the ordinal):

```rust
        let prov = self.provenance(
            version,
            Some(Ordinal::end_of_block(block)),
            TrustState::Anchored,
            SnapshotSource::Anchor,
        );
```

In `src/ingestion.rs`, add the arm to the `ApplyOutcome` match:

```rust
                // The anchor already covered this log's block. Expected on any
                // anchored pool; counted so an unexpected volume is visible.
                ApplyOutcome::Superseded => {
                    if let Some(m) = &self.metrics {
                        m.live_state_superseded.inc();
                    }
                }
```

In `src/metrics.rs`, add the field beside `live_state_untrusted_base`:

```rust
    /// Logs dropped because an anchor had already carried the pool past them.
    /// Expected traffic; a spike means anchoring is running too far ahead of
    /// the log stream.
    pub live_state_superseded: Counter,
```

Register it beside the others:

```rust
        let live_state_superseded = Counter::with_opts(Opts::new(
            "live_state_superseded_total",
            "Logs dropped because an anchor already covered their block",
        ))?;
        registry
            .register(Box::new(live_state_superseded.clone()))
            .context("register live_state_superseded_total counter")?;
```

and add `live_state_superseded,` to the struct literal.

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | grep -E "test result|^error"`
Expected: all pass. Any pre-existing test calling `anchor_cl`/`anchor_v2` with the old arity must be updated to pass a block, not deleted.

Run: `cargo clippy --all-targets 2>&1 | grep -cE "^warning"`
Expected: `3`.

- [ ] **Step 5: Commit**

```bash
git add src/live_state.rs src/ingestion.rs src/metrics.rs
git commit -m "feat(live-state): order anchors against the log stream"
```

---

### Task 3: Anchor ordinal round-trip tests

Task 2 made anchors carry an ordinal to satisfy its own tests. This task proves the property directly rather than as a side effect, and covers V2.

**Files:**
- Test only: `src/live_state.rs` (`mod tests`)

**Interfaces:**
- Consumes: `anchor_cl(pool, block, sqrt_price_x96, liquidity, tick)`, `anchor_v2(pool, block, state)`, `Ordinal::end_of_block`.
- Produces: nothing new.

- [ ] **Step 1: Write the test**

```rust
    /// An anchor must be positioned, not positionless. `Provenance.ordinal`
    /// used to be documented as "None for an anchor", which is exactly why
    /// anchors could not be ordered against logs.
    #[test]
    fn an_anchor_records_its_own_block_position() {
        let ls = LiveState::new();
        let cl = Address::from_low_u64_be(31);
        ls.anchor_cl(cl, 500, U256::from(1u64) << 96, 42, 0);
        assert_eq!(
            ls.cl_snapshot(cl).unwrap().prov.ordinal,
            Some(Ordinal::end_of_block(500))
        );
        assert_eq!(
            ls.cl_snapshot(cl).unwrap().prov.source,
            SnapshotSource::Anchor
        );
        assert!(may_price_locally(&ls.cl_snapshot(cl).unwrap().prov.trust));
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

    /// An anchor must also restore trust after a gap — that is the entire
    /// reason for this work. Only an absolute write can, and an anchor is one.
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
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib live_state 2>&1 | grep "test result"`
Expected: PASS. If `an_anchor_restores_trust_after_a_gap` fails, `provenance` is not stamping the current epoch on anchors — fix that, not the test.

- [ ] **Step 3: Commit**

```bash
git add src/live_state.rs
git commit -m "test(live-state): pin the anchor ordinal and post-gap trust recovery"
```

---

### Task 4: Exclude anchored snapshots from validation

The highest-risk item in the plan. Get it wrong and the divergence rate goes to zero for a reason that has nothing to do with correctness.

**Files:**
- Modify: `src/validation_select.rs` — `SelectOutcome` (:16-26), `select` (:41-58)
- Modify: `src/state_validation.rs` — `validatable` (:44-57), both `select` call sites (~:139 and ~:183)
- Modify: `src/metrics.rs` — the `live_state_checks` doc comment
- Test: `src/validation_select.rs`, `src/state_validation.rs`

**Interfaces:**
- Consumes: `SnapshotSource` from `crate::live_state`.
- Produces:
  - `pub struct SnapshotPosition { pub ordinal: Option<Ordinal>, pub source: SnapshotSource }`
  - `pub fn select(pos: SnapshotPosition, head_block: u64, settled_through: u64) -> SelectOutcome`
  - `SelectOutcome::NotIndependent`
  - `validatable(pools: Vec<Address>, head: u64, settled_through: u64, position_of: F) -> Vec<Address>` where `F: Fn(Address) -> Option<SnapshotPosition>`

The struct exists so `select` **cannot** be called without a source. Passing the source as a separate argument invites a call site that forgets it, and this codebase has already shipped one bug of exactly that shape — a fix applied at one call site and missed at the refresh path.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/validation_select.rs`:

```rust
    fn pos(block: u64, source: SnapshotSource) -> SnapshotPosition {
        SnapshotPosition {
            ordinal: Some(Ordinal {
                block,
                tx_index: 0,
                log_index: 0,
            }),
            source,
        }
    }

    /// An anchored snapshot IS an `eth_call` result. Validating it compares an
    /// `eth_call` against the `eth_call` it came from: it passes every time, by
    /// construction, and drives the observed divergence rate toward zero
    /// regardless of whether decoding works. Two conclusions in this project
    /// have already been produced by artefacts of exactly this shape.
    #[test]
    fn an_anchored_snapshot_is_never_measured() {
        assert_eq!(
            select(pos(1_000, SnapshotSource::Anchor), 1_050, 1_049),
            SelectOutcome::NotIndependent
        );
    }

    /// Independence is checked FIRST. An anchor that is also unsettled or too
    /// old must still report why it is really excluded, or the metric will
    /// attribute anchor traffic to lag.
    #[test]
    fn independence_outranks_every_other_exclusion() {
        assert_eq!(
            select(pos(1_050, SnapshotSource::Anchor), 1_050, 1_050),
            SelectOutcome::NotIndependent
        );
        assert_eq!(
            select(pos(1, SnapshotSource::Anchor), 100_000, 99_999),
            SelectOutcome::NotIndependent
        );
    }

    /// Log-derived snapshots are unaffected, including one built on an anchored
    /// base — that is still the Mint/Burn arithmetic under test.
    #[test]
    fn log_derived_snapshots_are_still_measured() {
        for source in [
            SnapshotSource::Swap,
            SnapshotSource::Liquidity,
            SnapshotSource::Sync,
        ] {
            assert_eq!(
                select(pos(1_000, source), 1_050, 1_049),
                SelectOutcome::Check { block: 1_000 }
            );
        }
    }
```

Add to `mod tests` in `src/state_validation.rs`:

```rust
    /// The exclusion has to hold in the pre-filter too. `validatable` and
    /// `validate_once` are two doors into the same decision, and a rule
    /// enforced at only one of them is the shape of bug this repo has shipped
    /// before.
    #[test]
    fn validatable_drops_anchored_pools() {
        use crate::live_state::SnapshotSource;
        let anchored = ethers::types::Address::from_low_u64_be(1);
        let logged = ethers::types::Address::from_low_u64_be(2);
        let head = 1_000u64;
        let position_of = |p: ethers::types::Address| {
            let source = if p == anchored {
                SnapshotSource::Anchor
            } else {
                SnapshotSource::Swap
            };
            Some(crate::validation_select::SnapshotPosition {
                ordinal: Some(crate::continuity::Ordinal {
                    block: 900,
                    tx_index: 0,
                    log_index: 0,
                }),
                source,
            })
        };
        let out = validatable(vec![anchored, logged], head, head, position_of);
        assert_eq!(out, vec![logged]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib validation_select 2>&1 | grep -E "^error" -A4 | head -20`
Expected: FAIL — `cannot find struct 'SnapshotPosition'`, `no variant named 'NotIndependent'`.

- [ ] **Step 3: Write minimal implementation**

In `src/validation_select.rs`:

```rust
use crate::live_state::SnapshotSource;

/// Everything validation needs to decide whether a snapshot is checkable.
///
/// A struct rather than loose arguments so `select` cannot be called without a
/// source. The source is what distinguishes an independent measurement from a
/// circular one, and a call site that omits it fails silently by passing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotPosition {
    pub ordinal: Option<Ordinal>,
    pub source: SnapshotSource,
}
```

Add the variant to `SelectOutcome`:

```rust
    /// The snapshot IS an RPC read. Comparing it against `eth_call` compares
    /// that read with itself, so it says nothing about decoder correctness and
    /// inflates the measured pass rate.
    NotIndependent,
```

Rewrite `select`:

```rust
pub fn select(pos: SnapshotPosition, head_block: u64, settled_through: u64) -> SelectOutcome {
    // First, ahead of every other exclusion: an anchor is not evidence about
    // our own arithmetic, whatever its block or age.
    if matches!(pos.source, SnapshotSource::Anchor) {
        return SelectOutcome::NotIndependent;
    }
    let Some(o) = pos.ordinal else {
        return SelectOutcome::NoOrdinal;
    };
    if o.block >= settled_through {
        return SelectOutcome::Unsettled;
    }
    let lag = head_block.saturating_sub(o.block);
    if lag > MAX_VALIDATION_LAG_BLOCKS {
        return SelectOutcome::TooOld { lag };
    }
    SelectOutcome::Check { block: o.block }
}
```

In `src/state_validation.rs`, change `validatable`:

```rust
pub(crate) fn validatable<F>(
    pools: Vec<Address>,
    head: u64,
    settled_through: u64,
    position_of: F,
) -> Vec<Address>
where
    F: Fn(Address) -> Option<crate::validation_select::SnapshotPosition>,
{
    pools
        .into_iter()
        .filter(|p| {
            position_of(*p).is_some_and(|pos| {
                matches!(select(pos, head, settled_through), SelectOutcome::Check { .. })
            })
        })
        .collect()
}
```

Update both `validate_once` call sites. The V2 one becomes:

```rust
        let block = match select(
            SnapshotPosition {
                ordinal: snap.prov.ordinal,
                source: snap.prov.source,
            },
            head,
            settled,
        ) {
```

and add the arm alongside the existing `count("no_ordinal")`:

```rust
            SelectOutcome::NotIndependent => {
                count("not_independent");
                continue;
            }
```

Make the identical change at the CL call site. Update the closures feeding `validatable` (`live.v2_snapshot(p).and_then(|s| s.prov.ordinal)` at :133 and the CL one at :177) to build a `SnapshotPosition`:

```rust
        live.v2_snapshot(p).map(|s| crate::validation_select::SnapshotPosition {
            ordinal: s.prov.ordinal,
            source: s.prov.source,
        })
```

In `src/metrics.rs`, extend the `live_state_checks` doc comment to list `not_independent` alongside the existing outcomes.

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | grep -E "test result|^error"`
Expected: all pass. Existing `validation_select` tests call `select` with the old signature — rewrite them to pass a `SnapshotPosition` with a log source. **Do not delete them**; they encode the settled-block and lag contracts, which are unchanged.

Run: `cargo clippy --all-targets 2>&1 | grep -cE "^warning"`
Expected: `3`.

- [ ] **Step 5: Commit**

```bash
git add src/validation_select.rs src/state_validation.rs src/metrics.rs
git commit -m "fix(validation): never measure a snapshot against its own source"
```

---

### Task 5: Anchor the pools that need it, from the poller

**Files:**
- Modify: `src/ingestion.rs` — new method on `PoolMonitor`, called from `poll_all_pools` inside the existing `Ok(block)` arm (~:778)
- Modify: `src/metrics.rs` — anchor counter
- Test: `src/ingestion.rs`

**Interfaces:**
- Consumes: `LiveState::anchor_cl(pool, block, sqrt_price_x96, liquidity, tick)`, `LiveState::cl_snapshot`, `crate::cl_sim::load_cl_pool_states_batched(provider, &[(Address, Option<u32>, Address, Address)], block) -> HashMap<Address, ClPoolState>`, `may_price_locally`.
- Produces: `PoolMonitor::anchor_candidates(&self, pools: &[MonitoredPool], live: &LiveState, cap: usize) -> Vec<MonitoredPool>`, and `PoolMonitor::anchor_untrusted(&self, block: U64)`.

`poll_all_pools` already calls `self.provider.get_block_number()` and pins `load_pair_states_batched` to it, so the anchor pass reuses that same block rather than adding a round-trip.

- [ ] **Step 1: Write the failing test**

The existing `monitored(n)` helper builds a `PoolMonitorKind::UniV2` pool, and
`anchor_candidates` filters for `ConcentratedLiquidity` — so these tests need a
CL variant or they pass vacuously on an empty result. Add it beside `monitored`
in `mod tests`:

```rust
    fn monitored_cl(n: u64) -> MonitoredPool {
        MonitoredPool {
            kind: PoolMonitorKind::ConcentratedLiquidity,
            ..monitored(n)
        }
    }
```

```rust
    /// Anchoring is `eth_call` traffic, and avoiding that traffic is the point
    /// of local state. Only pools that cannot recover on their own are worth
    /// it: a trusted pool will be corrected by its next Swap for free.
    #[test]
    fn only_untrusted_pools_are_anchor_candidates() {
        use crate::live_state::LiveState;
        let live = LiveState::new();
        let trusted = monitored_cl(1);
        let untrusted = monitored_cl(2);
        let never_seen = monitored_cl(3);

        for p in [&trusted, &untrusted] {
            live.anchor_cl(p.pair, 100, ethers::types::U256::from(1u64), 5, 0);
        }
        // Only `untrusted` loses trust: re-anchor `trusted` under the new epoch.
        live.break_continuity(crate::live_state::UnknownReason::WsUnavailable);
        live.anchor_cl(trusted.pair, 101, ethers::types::U256::from(1u64), 5, 0);

        let monitor = PoolMonitor::new(
            Arc::new(Provider::new(MockProvider::default())),
            None,
            vec![],
            Duration::from_secs(1),
            Duration::from_secs(10),
            None,
        )
        .expect("monitor should construct");

        let got: Vec<_> = monitor
            .anchor_candidates(&[trusted.clone(), untrusted.clone(), never_seen.clone()], &live, 8)
            .into_iter()
            .map(|p| p.pair)
            .collect();

        assert!(got.contains(&untrusted.pair), "invalidated pools are the point");
        assert!(got.contains(&never_seen.pair), "a pool with no snapshot cannot recover alone");
        assert!(!got.contains(&trusted.pair), "spending an eth_call here buys nothing");
    }

    /// The cap is the RPC budget. Without it a gap would anchor all 683 pools
    /// in one cycle.
    #[test]
    fn anchor_candidates_respect_the_cap() {
        use crate::live_state::LiveState;
        let live = LiveState::new();
        let pools: Vec<_> = (10..40).map(monitored_cl).collect();
        let monitor = PoolMonitor::new(
            Arc::new(Provider::new(MockProvider::default())),
            None,
            vec![],
            Duration::from_secs(1),
            Duration::from_secs(10),
            None,
        )
        .expect("monitor should construct");
        assert_eq!(monitor.anchor_candidates(&pools, &live, 8).len(), 8);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib ingestion::tests::only_untrusted 2>&1 | grep -E "^error" -A4 | head`
Expected: FAIL — `no method named 'anchor_candidates'`.

- [ ] **Step 3: Write minimal implementation**

Add to `impl PoolMonitor` in `src/ingestion.rs`:

```rust
    /// Cap on anchor reads per poll cycle.
    ///
    /// Anchoring is the `eth_call` traffic local state exists to avoid, so the
    /// budget is a recovery backlog, not a refresh cycle. A gap invalidates all
    /// 683 pools at once; draining that over several cycles is the intent.
    const ANCHOR_BUDGET_PER_CYCLE: usize = 16;

    /// Pools that cannot recover without an RPC read.
    ///
    /// A trusted pool is deliberately excluded: its next Swap overwrites its
    /// state for free, so an anchor buys nothing. Only pools that are
    /// invalidated, or that have no snapshot at all, are worth the call.
    pub(crate) fn anchor_candidates(
        &self,
        pools: &[MonitoredPool],
        live: &crate::live_state::LiveState,
        cap: usize,
    ) -> Vec<MonitoredPool> {
        use crate::live_state::may_price_locally;
        pools
            .iter()
            .filter(|p| matches!(p.kind, PoolMonitorKind::ConcentratedLiquidity))
            .filter(|p| match live.cl_snapshot(p.pair) {
                None => true,
                Some(s) => !may_price_locally(&s.prov.trust),
            })
            .take(cap)
            .cloned()
            .collect()
    }

    /// Read true state for pools that cannot recover on their own and install
    /// it as an anchor, pinned to `block`.
    async fn anchor_untrusted(&self, block: U64) {
        let Some(live) = &self.live_state else {
            return;
        };
        let pools = {
            let guard = self.pools.read().await;
            guard.clone()
        };
        let candidates = self.anchor_candidates(&pools, live, Self::ANCHOR_BUDGET_PER_CYCLE);
        if candidates.is_empty() {
            return;
        }
        let request: Vec<(Address, Option<u32>, Address, Address)> = candidates
            .iter()
            .map(|p| (p.pair, None, p.token_in, p.token_out))
            .collect();
        let states =
            crate::cl_sim::load_cl_pool_states_batched(self.provider.clone(), &request, block)
                .await;
        for (pool, state) in states.iter() {
            live.anchor_cl(
                *pool,
                block.as_u64(),
                state.sqrt_price_x96,
                state.liquidity,
                state.tick,
            );
            if let Some(m) = &self.metrics {
                m.live_state_anchors.inc();
            }
        }
        debug!(
            requested = candidates.len(),
            anchored = states.len(),
            block = block.as_u64(),
            "anchored pools that could not recover from the log stream"
        );
    }
```

Call it from `poll_all_pools`, inside the existing `Ok(block) => {` arm, after the `batch_misses` loop:

```rust
                    self.anchor_untrusted(block).await;
```

Add to `src/metrics.rs`, beside `live_state_superseded`:

```rust
    /// Pools restored to trust by an RPC read rather than by trading.
    pub live_state_anchors: Counter,
```

```rust
        let live_state_anchors = Counter::with_opts(Opts::new(
            "live_state_anchors_total",
            "Pools anchored from a block-pinned RPC read",
        ))?;
        registry
            .register(Box::new(live_state_anchors.clone()))
            .context("register live_state_anchors_total counter")?;
```

and add `live_state_anchors,` to the struct literal.

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | grep -E "test result|^error"`
Expected: all pass.

Run: `cargo clippy --all-targets 2>&1 | grep -cE "^warning"`
Expected: `3`.

- [ ] **Step 5: Commit**

```bash
git add src/ingestion.rs src/metrics.rs
git commit -m "feat(ingestion): anchor pools that cannot recover from the log stream"
```

---

### Task 6: Field verification under forced gaps

Unit tests cannot show whether anchoring actually restores coverage, or whether it masks decoder error. Only a run can.

**Files:**
- Modify: `docs/superpowers/specs/2026-08-29-live-state-dirty-scan-design.md` (§10 result)
- No source changes unless the run finds a fault.

**Interfaces:**
- Consumes: `CHAOS_WS_GAP_SECS` (`9480030`), the counters from Tasks 2 and 5.
- Produces: a recorded result.

- [ ] **Step 1: Build and start a forced-gap run**

```bash
cargo build --release
```

```bash
APP_ENV=chaos-test ARBOT_LIVE_STATE_SHADOW=1 CHAOS_WS_GAP_SECS=60 RUST_LOG=info,state_validation=debug ./target/release/arb-exec > validation-anchor.log 2>&1
```

`APP_ENV` must be overridden because production mode rejects `CHAOS_WS_GAP_SECS`. All three `production_mode_enabled()` sites are startup validation only — placeholder secrets, pinned bytecode, chaos flags — so nothing in the measurement path changes.

- [ ] **Step 2: Let it run at least 16 minutes, then scrape BEFORE killing**

```bash
curl -s http://localhost:9100/metrics | grep -E "^(live_state_anchors_total|live_state_superseded_total|live_state_untrusted_base_total|live_state_applied_total|live_state_trusted_pools|live_state_untrusted_pools)"
```

```bash
curl -s http://localhost:9100/metrics | grep "^live_state_checks"
```

- [ ] **Step 3: Judge against the predictions**

| metric | prediction | what a miss means |
|---|---|---|
| `live_state_anchors_total` | > 0 | the anchor pass never ran; check `live_state` is wired and pools are CL |
| `live_state_untrusted_base_total` | well below 5194 per 12 gaps | anchoring is not restoring bases; check trust actually flips to `Anchored` |
| `live_state_superseded_total` | small but non-zero | zero means anchors never race logs, so the ordinal work is untested by this run |
| `live_state_checks{outcome="not_independent"}` | > 0 | zero means anchored snapshots are still being measured — the circular-validation trap is open |
| divergence rate, log-derived only | unchanged from the 0/518 baseline | a **fall** is the warning sign, not the win: anchoring may be erasing decoder error before validation sees it |

The last row is the one to read carefully. Anchoring buys availability with diagnostic power, and a divergence rate that improves alongside coverage is more likely masking than correctness.

- [ ] **Step 4: Record the result**

Append the measured numbers to §10 of the design spec, including any prediction that missed, then:

```bash
git add docs/superpowers/specs/2026-08-29-live-state-dirty-scan-design.md
git commit -m "docs(spec): record the anchor path field result"
```

---

## Self-Review

**Spec coverage.** Design §3.1 → Task 1 and Task 2 Step 3. §3.2 → Task 2. §3.3 → Task 4. §3.4 → Task 5. §4 (masking tension) → Task 6 Step 3. §5 out-of-scope items stay out. §6 open questions: "anchor at head vs settled" is resolved to head, because Task 5 reuses the block the poller already pinned; TTL is left alone since `anchored_at` and the gate's expiry already exist; V2 symmetry is covered for the ordinal in Tasks 2–3 but **`anchor_v2` is never called from the poller** — the poller's V2 path caches into `CachedState`, not `LiveState`, so wiring it is a separate change; `anchor_id` still has no consumer and is untouched.

**Known gaps, stated rather than hidden:**
- V2 pools get the anchor *ordinal* but no anchor *caller*. CL is where the untrusted-base cost was measured (5194 dropped deltas are all Mint/Burn), so CL is where the value is. V2 wiring should follow once this is proven.
- `ANCHOR_BUDGET_PER_CYCLE = 16` is a guess, not a measurement. With a 1.2s poll interval it drains 683 pools in ~43 cycles (~52s). Task 6 will show whether that is too slow.
- Task 5's tests cover candidate selection, not the batched read itself, which needs a live provider.

**Two defects this review caught, both compile failures:** `UniV2PairState` is
`#[derive(Clone, Debug)]` with no `Default`, so Task 3's `..Default::default()`
would not build — it now names all four fields. And `monitored(n)` builds a
`UniV2` pool while `anchor_candidates` filters for `ConcentratedLiquidity`, so
Task 5's tests would have passed vacuously against an empty result; a
`monitored_cl` helper is now part of the task.

**Type consistency.** `anchor_cl(pool, block, sqrt_price_x96, liquidity, tick)` and `anchor_v2(pool, block, state)` take `block: u64`; `anchor_untrusted(block: U64)` converts with `.as_u64()`. `SnapshotPosition { ordinal: Option<Ordinal>, source: SnapshotSource }` is used identically in `select`, `validatable`, and both `validate_once` call sites. `supersedes` and `current_ordinal` are used only inside `apply_log`.
