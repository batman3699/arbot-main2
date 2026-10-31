# R41 Denim readiness Implementation Plan

> **For agentic workers:** executed inline (superpowers:executing-plans), then
> a self-review of the whole branch (no subagents: the operator's standing
> rule). Steps are test first and use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The armed lane trades through Base's Denim hardfork (2026-11-05
18:00:00 UTC, 200 ms blocks) with no one awake, spends no more BlockPI than
before, and the evaluator and census measure post-fork days in the right unit.

**Architecture:**
- **Two regimes, two configs.** `ops/{canary,shadow}.denim.yaml` differ from
  the base files in seven fields only. A load-time check refuses a
  half-flipped file, and `ShadowConfig::denim()` is the one switch every
  path asks.
- **The runtime learns `timestampMs`** (`Head.timestamp_ms`) and gains a small
  `denim` module: the read clock, the 20-head window (chain cadence and head
  lag), and the ten-block lattice proof. The armed worker's Denim guard, the
  head loop, the reader's nonce tag and Tier 2's clock use them.
- **The evaluator** gets `start_at` (each arm records each pool's
  applied-through point) and a block mode with its own landing rule and
  `offset_unit`. **The census** gets a 300 s trigger window and per-day units.
- **On the host**, `apex-denim-switch.timer` runs `ops/host/denim-switch.sh`
  at 17:59 UTC.
  - `apex denim-detect` proves the fork.
  - Drop-ins point the canary and the evaluator at the Denim configs, and
    the canary restarts.
  - `apex denim-check` is the rehearsal and the gate.

**Tech Stack:**
- Rust: `apex-chain`, `apex-runtime`.
- Python 3.12: `scripts/data/arb_census`.
- bash and systemd: `ops/host`.

**Spec:** `docs/superpowers/specs/2026-10-31-denim-readiness-design.md`
(approved 2026-10-31). **Branch:** `r41-denim`, from `main` at `a15f55a`.

## Global Constraints

- **Secrets:** never printed, echoed or logged (`BLOCKPI_KEY`, `PRIVATE_KEY`,
  `TRADER_PRIVATE_KEY`, `APEX_SECRET_*`). Keys go by file or environment,
  never argv. On the host, run a keyed `apex` command with
  `systemd-run --uid=apex -p EnvironmentFile=/etc/apex/shadow.env`, never
  `env $(cat …)`.
- **The repo's habits:**
  - never `cargo fmt`;
  - cargo with `-j 3`;
  - stage named paths only;
  - no new non-secret environment variables (config fields are fine);
  - no `unwrap`/`expect` in `--bins --lib` (`no_runtime_panics.sh`).
- **Before any push**, all five gates (the `scripts/ci/*.sh` sweep, CI-wide
  clippy `--all-targets -- -D warnings`, `cargo test --workspace
  --all-targets`, `cargo test --workspace --doc`, `forge test`), plus
  `python3 -m pytest scripts/data/arb_census`.
  `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh` are known red.
- **The operator approves** each merge, push and deploy; the deploy needs
  "yes, push and deploy <rev>". `canary.stop` stays in place. No getLogs
  bursts on the BlockPI key while the canary is live. No archiving of
  `var/apex` before 11-07.
- **The pre-fork run is unchanged.**
  - `ops/canary.base.yaml` and `ops/shadow.base.yaml` are not edited.
  - Every new field defaults to today's behaviour.
  - With a base config, no status line may differ.
- **Nothing changes the bid, the tier model or sender funding** (`best_bid`
  held, 16 senders, 0.01 ETH).
- **Values (the spec's):**
  - Denim at timestamp `1_793_901_600` on mainnet (`1_792_605_600` on
    Sepolia), on chain 8453. The switch's timer fires at
    `2026-11-05 17:59:00 UTC`, and the switch gives up at `1_793_908_800`
    (20:00).
  - `armed.block_time_ms` is 2000 or 200.
  - At 200: `feed.pending_logs: false`, no `flashblocks:` section,
    `armed.max_head_lag_ms` 1–5,000 (shipped 1,000). At 2000:
    `pending_logs: true`, `max_head_lag_ms` 0.
  - `policy.head_reads_every_ms` is 0 or 1,000–10,000 (Denim 2,000).
  - Denim's `policy.l1_every_blocks` is 300 and `max_evidence_age_blocks`
    18,000.
  - The guard's window is 20 heads. Chain cadence must be within half of
    `block_time_ms` either way, and head lag at most `max_head_lag_ms`.
  - Stops are fresh for `3 × max(block_time_ms, head_reads_every_ms)`.
  - Settling takes a minute: 30 blocks at 2 s, 300 at 200 ms.
  - The census trigger window is 300 s: 150 blocks before the fork block,
    1,500 from it.
  - The switch reads `https://mainnet.base.org`, falling back to
    `https://base-rpc.publicnode.com`.

## Rulings this plan makes (beyond the spec)

- **HTTP bytes by method already exist.** `MethodUsage.bytes_in/bytes_out`
  (R28.1) are in the report's `rpc`. The spec's fact is corrected. Only
  websocket bytes are new (Task 1). *Cost if wrong:* none.
- **`start_at` needs a new `PoolSnapshot.through`.** A Mint or Burn applied
  in place moves liquidity but not `last_log` (`live/book.rs` ~1245), so
  "the last applied log" is the newest log the state holds: a swap, a sync,
  or a position change. *Cost if wrong:* an arm taken after an in-place
  burn still double-counts it.
- **A book behind the head is not caught up.** The evaluator still fetches
  logs from `armed_block + 1`. Only the case the spec names, the book ahead
  of the head, is fixed. *Cost if wrong:* a rare arm is judged on words one
  block stale, as today.
- **One header read per fire block.** In block mode, the fire block's
  transaction count and its unit both come from one
  `eth_getBlockByNumber(n, false)`, cached per run: `transactions.len()`,
  and `timestamp ≥ 1_793_901_600` → `block_200ms`. This replaces the spec's
  `eth_getBlockTransactionCountByNumber` plus a second read. *Cost if
  wrong:* ~4 KB a fire block.
- **Detection is Rust.** The detection round is `apex denim-detect`, which
  shares the lattice code with `denim-check`; the script loops it and acts.
  - The first source that answers decides, so the spec's "both answer and
    disagree" cannot arise.
  - `--chain-id` (default 8453) lets gate S rehearse on Sepolia (84532).
  - *Cost if wrong:* none in behaviour.
- **The switch script is tested from `cargo test`** (CI runs no bash
  tests). For the test, it takes `--apex`, `--systemctl`, `--etc`,
  `--record`, `--as-user`, `--give-up-at` and `--every-s`.
- **`--dry-run` runs one round:** it exits 0 on Denim and 1 otherwise, as
  gate S needs.
- **`denim-check` loads through `ShadowConfig::load_canary`.** The file's
  `chain_id` stays 8453. The endpoint's own chain ID comes from
  `eth_chainId` and is printed.
- **`send-probe` and `armed-moves-seed` keep their pre-fork behaviour.**
  `ARMED.md` says not to run either across or after the fork. They are
  operator tools, never run by the canary.
- **The drop-ins survive a deploy.** Checked while planning: `deploy.sh`
  only builds archives, `provision.sh` installs packages and directories,
  and units are installed by hand (`README.md`). Nothing touches
  `/etc/systemd/system/*.service.d/`.
- **The capacity sampler is not spawned under `denim()`**
  (`samples_capacity(config)`, tested).
- **A `block_2s` census day ranks routes exactly as today:** offset 0 at
  block grain. Only its unit is recorded, and `contest.py` never fits it.

## Review Focus

1. **A `timestampMs` that is not a hex quantity** (a JSON number, or
   decimal text) must read as absent, never as a wrong time. Pinned in
   Task 1: `a_timestamp_ms_that_is_not_a_quantity_is_absent`.
2. **A head stamped after the host's now** (the host clock behind the
   chain): its lag is 0 and the send goes ahead. A clock that steps back
   must not freeze the read clock. Pinned in Task 2:
   `a_head_stamped_after_now_has_no_lag` and
   `a_clock_that_steps_back_starts_a_round`.
3. **Pool strings in `start_at`:** `start_at[i].pool` and every write's
   `pool` are both `{:#x}` lowercase, as `arm.pools` is. Otherwise the skip
   silently never applies. Pinned in Task 7:
   `start_at_names_the_arm_s_pools_as_written`.
4. **An evaluator run that straddles the fork:** a pre-fork fire with no
   ledger is `block_2s` and a post-fork one is `block_200ms`, in one run.
   Pinned in Task 8: `the_unit_is_the_fire_block_s_era`.
5. **A switch interrupted between its two drop-ins** must be completed by
   the next run, not taken as done. Pinned in Task 12:
   `a_half_written_switch_is_completed_not_skipped`.

---

## Phase 1: heads, the Denim clock, and the configs

### Task 1: Heads carry `timestampMs`; the feed counts bytes by subscription

**Files:**
- Modify: `crates/apex-chain/src/rpc/ws.rs` (`Head`, `parse_notification`, `WsStats`, `session`)
- Test: `crates/apex-chain/tests/ws_feed.rs`
- Modify: every `Head { … }` literal: `grep -rn 'Head {' crates --include=*.rs` (`apex-runtime/tests/live_feed.rs` 366, 647, 680, 744; `live_reader.rs` 165) gains `timestamp_ms: None`.

**Interfaces:**
- Produces:
  - `Head.timestamp_ms: Option<u64>`, parsed with `quantity` from
    `timestampMs`. It is `None` when absent or not a `0x` quantity, and the
    head still parses.
  - `WsStats::bytes(&self, kind: Kind) -> u64`: the text-frame bytes of
    notifications on that kind's subscriptions.
  - `WsStats::bytes_other(&self) -> u64`: every other text frame
    (subscription answers, unknown subscriptions, unparsed text).
  - The kind is read from `params.subscription` through `ids`, in
    `session`, before `parse_notification`.

- [ ] **Step 1: Write the failing tests** in `tests/ws_feed.rs`:
  - `a_denim_head_carries_its_millisecond_timestamp`: `head_json(100)` with
    `"timestamp": "0x6ad8fda1"` and `"timestampMs": "0x1a15f9ebe78"` gives
    `timestamp == 1_792_605_601` and `timestamp_ms == Some(1_792_605_601_400)`.
  - `a_head_without_timestamp_ms_has_none`: `head_json(100)` as it is gives
    `None`.
  - `a_timestamp_ms_that_is_not_a_quantity_is_absent`: `"timestampMs":
    1792605601400` (a number) and `"1792605601400"` (no `0x`) each give
    `None`, with `number == 100`.
  - `bytes_are_counted_by_subscription`: `WsNode` pushes one head, one
    pending log and one log, as in `the_feed_subscribes_and_forwards`. Each
    of `bytes(Kind::NewHeads)`, `bytes(Kind::PendingLogs)` and
    `bytes(Kind::Logs)` equals its pushed frame's length as `support/ws.rs`
    serialises it. `bytes(Kind::NewFlashblocks) == 0`, and `bytes_other()`
    equals the three subscription answers' lengths.
- [ ] **Step 2: Run them to see them fail.**
  `cargo test -j 3 -p apex-chain --test ws_feed`. Expected: no field
  `timestamp_ms`, no method `bytes`.
- [ ] **Step 3: Implement**, then fix the `Head` literals the grep lists.
- [ ] **Step 4: Run them to see them pass.**
  `cargo test -j 3 -p apex-chain --test ws_feed`, and
  `cargo test -j 3 -p apex-runtime --test live_feed --test live_reader`.
- [ ] **Step 5: Commit** `crates/apex-chain/src/rpc/ws.rs`,
  `crates/apex-chain/tests/ws_feed.rs`, and the two runtime test files:
  `feat(chain): heads carry timestampMs; feed bytes by subscription (Task 8.5 R41)`.

### Task 2: The `denim` module: read clock, head window, lattice proof

**Files:**
- Create: `crates/apex-runtime/src/denim.rs` (and `pub mod denim;` in `src/lib.rs`)
- Create: `crates/apex-runtime/tests/denim.rs`
- Create: `crates/apex-runtime/tests/fixtures/sepolia_denim_heads.json`

**Interfaces:**
- Produces (in `apex_runtime::denim`):
  - Constants: `DENIM_MAINNET_TS: u64 = 1_793_901_600`,
    `DENIM_SEPOLIA_TS: u64 = 1_792_605_600`, `DENIM_BLOCK_MS: u64 = 200`,
    `PRE_DENIM_BLOCK_MS: u64 = 2_000`, `HEAD_WINDOW: usize = 20`.
  - `pub struct ReadClock`, with `pub const fn new(every_ms: u64) -> Self`
    and `pub fn due(&mut self, now_ms: u64) -> bool`.
    - It is due when `every_ms == 0`, on the first call, when
      `now_ms − last ≥ every_ms`, or when `now_ms < last` (the clock stepped
      back).
    - A `true` answer records `now_ms` as the round's start.
  - `pub struct HeadWindow`, with `pub fn new() -> Self` and
    `pub fn observe(&mut self, number: u64, timestamp_ms: Option<u64>)`.
    - It ignores `number ≤ last`.
    - It adds `number − last − 1` to `skips` on a gap.
    - A head with a timestamp is pushed as `(number, ms)`, keeping the
      newest `HEAD_WINDOW`. Without one, it counts `without_ms` and marks
      the newest head unstamped until a stamped one follows.
  - `HeadWindow::chain_cadence_ms(&self) -> Option<u64>`: with 20 samples
    and a stamped newest head, `(newest.ms − oldest.ms) /
    (newest.number − oldest.number)`, integer.
  - `HeadWindow::head_lag_ms(&self, now_ms: u64) -> Option<u64>`:
    `now_ms.saturating_sub(newest.ms)`, or `None` when the newest head is
    unstamped.
  - `HeadWindow::holds(&self, block_time_ms: u64, max_lag_ms: u64, now_ms:
    u64) -> bool`: cadence `c.abs_diff(bt) * 2 <= bt`, and lag `≤
    max_lag_ms`.
  - `HeadWindow::skips(&self) -> u64` and `without_ms(&self) -> u64`.
  - `pub struct LatticeBlock { pub number: u64, pub timestamp: u64, pub
    timestamp_ms: Option<u64> }`.
  - `pub fn lattice(blocks: &[LatticeBlock], fork_at: u64) -> Result<(),
    String>` requires:
    - exactly ten blocks, ascending and consecutive;
    - every one stamped, with `ms % 200 == 0` and `ms / 1000 == timestamp`;
    - every step exactly 200;
    - the oldest's `timestamp ≥ fork_at`.

    The `Err` names the first rule broken and the block.
  - `pub enum Detect { Denim, NotYet(String), Anomaly(String) }` and
    `pub fn detect(chain_id: u64, want_chain: u64, blocks: &[LatticeBlock],
    fork_at: u64) -> Detect`, in this order:
    1. a wrong chain is `Anomaly`;
    2. an oldest timestamp `< fork_at` is `NotYet`;
    3. `lattice` passing is `Denim`;
    4. anything else is `Anomaly`.

- [ ] **Step 1: Record the fixture.**
  - Record 60 s of `newHeads` from `wss://base-sepolia-rpc.publicnode.com`:
    `[{number, timestamp, timestampMs, arrival_ms}]`, hex fields kept as
    sent.
  - Use a throwaway node script in the scratchpad, with the `ws` module at
    `/usr/local/lib/node_modules/wscat/node_modules/ws`.
  - Add `"_source"` (endpoint, UTC time, "from NZ") at the top.
  - Check that every step is 200 and lag p99 < 500.
- [ ] **Step 2: Write the failing tests** in `tests/denim.rs`:
  - `the_read_clock_runs_a_round_at_most_every_interval`: `new(2_000)` fed
    0..=10_000 every 200 ms is due exactly at 0, 2,000, 4,000, 6,000, 8,000
    and 10,000.
  - `a_zero_interval_runs_every_head`.
  - `a_clock_that_steps_back_starts_a_round`: due at 10,000, then at 5,000,
    then not at 6,999, then due at 7,000.
  - `sepolia_heads_hold_at_200_and_not_at_2000`: the fixture. From the 20th
    head on, `holds(200, 1_000, arrival_ms)` at every head and
    `holds(2_000, 1_000, …)` at none.
  - `a_burst_of_arrivals_keeps_the_chain_cadence`: the same heads with five
    arrivals inside 10 ms give `chain_cadence_ms() == Some(200)` and still
    hold.
  - `a_skipped_head_keeps_the_cadence_and_is_counted`: one head removed
    gives cadence 200 and `skips() == 1`.
  - `a_repeated_head_is_ignored`.
  - `head_lag_bounds_sends`: the newest stamped T, with
    `holds(200, 1_000, T + 999)` and `T + 1_000` true and `T + 1_001` false.
  - `a_head_without_timestamp_ms_fails_both_checks`: after 20 stamped heads,
    an unstamped one gives `chain_cadence_ms() == None`,
    `head_lag_ms(..) == None` and `holds` false. The next stamped head
    holds again.
  - `a_head_stamped_after_now_has_no_lag`: `head_lag_ms(T − 50) ==
    Some(0)`, and it holds.
  - `the_lattice_proves_denim_and_nothing_else`: ten fixture blocks with
    `fork_at = DENIM_SEPOLIA_TS` pass. One `Err` each for nine blocks, ten
    2 s blocks without ms, a gap, a 400 ms step, `ms % 200 != 0`,
    `ms / 1000 != timestamp`, and an oldest block before `fork_at`.
  - `detect_orders_its_answers`: chain 84532 against 8453 is `Anomaly`;
    ten mainnet 2 s blocks at 1_793_901_590 are `NotYet`; the Sepolia ten
    with `want_chain` 84532 are `Denim`; ten blocks after the fork with one
    unstamped are `Anomaly`.
- [ ] **Step 3: Run them to see them fail.**
  `cargo test -j 3 -p apex-runtime --test denim`. Expected: unresolved
  `apex_runtime::denim`.
- [ ] **Step 4: Implement `src/denim.rs`.**
- [ ] **Step 5: Run them to see them pass.**
- [ ] **Step 6: Commit** `src/denim.rs`, `src/lib.rs`, `tests/denim.rs` and
  the fixture:
  `feat(denim): read clock, head window, lattice proof (Task 8.5 R41)`.

### Task 3: The config: two regimes, the load check, the Denim files

**Files:**
- Modify: `crates/apex-runtime/src/shadow/config.rs`
- Create: `ops/canary.denim.yaml`, `ops/shadow.denim.yaml`
- Test: `crates/apex-runtime/tests/shadow_parts.rs`

**Interfaces:**
- Consumes: `denim::{DENIM_BLOCK_MS, PRE_DENIM_BLOCK_MS}`.
- Produces:
  - `Policy.head_reads_every_ms: u64` (`#[serde(default)]`, 0);
  - `ArmedConfig.max_head_lag_ms: u64` (`#[serde(default)]`, 0);
  - `ShadowConfig::denim(&self) -> bool`, which is
    `armed.block_time_ms == DENIM_BLOCK_MS`.
- The load check, in `parse` after the armed bounds. It replaces
  `a.block_time_ms == 0`, and each message names its field:
  - `block_time_ms ∉ {2000, 200}` → `"armed.block_time_ms {v}: 2000 before
    Denim, 200 after"`;
  - at 200, `pending_logs` true → `"feed.pending_logs must be false at 200
    ms"`;
  - at 200, `flashblocks` present → `"the flashblocks section must be absent
    at 200 ms"`;
  - at 200, `max_head_lag_ms ∉ 1..=5000` → `"armed.max_head_lag_ms {v} is
    outside 1-5000 at 200 ms"`;
  - at 2000, `pending_logs` false → `"feed.pending_logs must be true at 2000
    ms"`;
  - at 2000, `max_head_lag_ms != 0` → `"armed.max_head_lag_ms must be 0 at
    2000 ms: pre-fork heads carry no timestampMs"`;
  - `head_reads_every_ms ∉ {0} ∪ 1000..=10000` →
    `"policy.head_reads_every_ms {v}: 0, or 1000-10000"`.
- The Denim files are the base files with exactly the spec's §1 table
  applied. Their header says so and points at `ops/host/ARMED.md` "The Denim
  day".

- [ ] **Step 1: Write the failing tests** in `tests/shadow_parts.rs`:
  - `a_half_flipped_config_is_refused`. One `Invalid` per load-check row,
    each message containing its field's name. Accepted:
    - 0, 1,000 and 10,000 reads;
    - a full Denim YAML (200, `pending_logs: false`, no flashblocks,
      `max_head_lag_ms: 1000`), whose `denim()` is true.
  - `the_denim_configs_differ_from_the_base_ones_only_by_the_switch`:
    - Each pair parsed as `serde_yaml::Value` is equal once these are taken
      out: `armed.block_time_ms`, `feed`, `flashblocks`,
      `policy.l1_every_blocks`, `policy.max_evidence_age_blocks`,
      `policy.head_reads_every_ms` and `armed.max_head_lag_ms`.
    - The Denim side's values are 200, `{pending_logs: false}`, absent, 300,
      18,000, 2,000 and 1,000.
    - The base side's are 2000, absent, present, 30, 1,800, absent and
      absent.
    - All four files load through the real loaders (`load_canary` for the
      canary files, `load` for the shadow ones) with
      `APEX_SECRET_BLOCKPI_KEY` set to a test value. `denim()` is false for
      both base files and true for both Denim ones, and the Denim window is
      `window_s * 1000 / block_time_ms == 50` blocks.
  - Rewrite `after_denim_the_feed_is_heads_and_canonical_logs`'s last half
    to load the full Denim YAML. Its 2000-with-`pending_logs: false` case is
    now refused (assert that).
- [ ] **Step 2: Run them to see them fail.**
  `cargo test -j 3 -p apex-runtime --test shadow_parts`.
- [ ] **Step 3: Implement the fields, the check, `denim()`**, and write the
  two YAML files.
- [ ] **Step 4: Run them to see them pass**, together with
  `--test size_search_harness`, which loads `shadow.base.yaml`.
- [ ] **Step 5: Commit** `src/shadow/config.rs`, `tests/shadow_parts.rs`,
  `ops/canary.denim.yaml` and `ops/shadow.denim.yaml`:
  `feat(config): Denim's regime, its load check and its two files (Task 8.5 R41)`.

## Phase 2: the runtime under Denim

### Task 4: The armed live worker's Denim guard and stops clock

**Files:**
- Modify: `crates/apex-runtime/src/armed/live.rs`
- Test: `crates/apex-runtime/tests/armed_live.rs`

**Interfaces:**
- Consumes: `denim::{HeadWindow, ReadClock}`.
- Produces:
  - `LivePolicy` gains `pub denim: bool`, `pub max_head_lag_ms: u64` and
    `pub head_reads_every_ms: u64`.
  - `ArmedLive::on_head(&self, head: u64, head_seen_ms: u64, timestamp_ms:
    Option<u64>, now_ms: u64)`.
  - `LiveReport` gains `chain_cadence_ms: Option<u64>`, `head_lag_ms:
    Option<u64>` (at report time), `head_skips: u64`, `heads_without_ms:
    u64` and `stops_reads: u64`.
- Behaviour:
  - `Inner` gains a `HeadWindow`, a stops `ReadClock` and a `stops_reads`
    count.
  - `on_head` keeps today's arrival intervals and also feeds the window.
  - It runs the stops multicall only when the clock is due. Settlement
    still runs on every head.
  - `cadence_ok(now_ms)`: under `denim`, `window.holds(block_time_ms,
    max_head_lag_ms, now_ms)`; otherwise today's median, unchanged.
    `submit_arm` passes its own `now_ms`.
  - `stops_allow`: fresh while `now − read ≤ 3 × max(block_time_ms,
    head_reads_every_ms)`.

- [ ] **Step 1: Write the failing tests** in `tests/armed_live.rs`. Fixture
  changes:
  - every `on_head(h, s, s)` becomes `on_head(h, s, None, s)`;
  - `policy()` gains `denim: false, max_head_lag_ms: 0,
    head_reads_every_ms: 0`;
  - `denim_policy()` sets `block_time_ms: 200, window_blocks: 50, denim:
    true, max_head_lag_ms: 1_000, head_reads_every_ms: 2_000`;
  - `denim_heads(f, last, at_ms)` feeds 20 heads stamped 200 ms apart, each
    seen 166 ms after its stamp;
  - `Chain` gains `stops_asks: AtomicUsize`.

  The tests:
  - `denim_sends_on_chain_cadence_and_fresh_heads`: booted on
    `denim_heads`, an arm is sent.
  - `a_denim_burst_still_sends`: the same stamps, all 20 seen within 10 ms,
    still send. With `denim: false` the median guard refuses them.
  - `a_newest_head_too_old_stops_denim_sends`: a send at newest stamp +
    1,001 is not sent; one at + 999 is.
  - `a_denim_head_without_timestamp_ms_stops_sends`.
  - `stops_are_read_on_the_clock`: 10 s of 200 ms heads with reads every
    2,000 call `stops` 5 or 6 times (`stops_asks`), and
    `report().stops_reads` agrees.
  - `a_send_between_stops_rounds_goes_out_under_denim`: block time 200,
    reads every 2,000, a send 1,900 ms after the last stops read is sent.
    This kills `max(…)` → `block_time_ms`.
  - `the_denim_report_carries_cadence_lag_and_skips`: one head skipped
    gives `chain_cadence_ms == Some(200)` and `head_skips == 1`.
  - `a_cadence_off_the_configuration_stops_sends` stays as it is: the
    pre-fork guard.
- [ ] **Step 2: Run them to see them fail.**
  `cargo test -j 3 -p apex-runtime --test armed_live`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them to see them pass.**
- [ ] **Step 5: Commit** `src/armed/live.rs` and `tests/armed_live.rs`:
  `feat(armed): the Denim guard is chain cadence and head lag; stops on a clock (Task 8.5 R41)`.

### Task 5: No `pending` under Denim; Tier 2's clock from the config

**Files:**
- Modify: `crates/apex-runtime/src/live/reader.rs`, `src/live/sim.rs`, `src/live/feed.rs`
- Test: `tests/live_feed.rs`, `tests/live_sim.rs`
- Create: `tests/denim_requests.rs`

**Interfaces:**
- Produces:
  - `ReaderConfig.denim: bool`. The nonce is read at `state_tag(denim)`.
  - `pub fn state_tag(denim: bool) -> &'static str` in `live/sim.rs`:
    `"latest"` or `"pending"`.
  - `simulate_request(call, from, gas_limit, at, state: &str)` puts
    `state` where `"pending"` is today.
  - `LiveSimulator::on_denim(self, denim: bool) -> Self`: a builder, so
    `new`'s callers are unchanged.
  - `BlockContext` gains `pub time_ms: u64` and `pub block_time_ms: u64`;
    `timestamp` stays as `time_ms / 1000`.
  - `BlockContext::being_built(head: &Head, newest_preconfirmed:
    Option<u64>, block_time_ms: u64, denim: bool) -> Option<Self>`.
    - `head_ms` is `head.timestamp_ms?` under Denim, else `head.timestamp
      × 1000`.
    - The block is `k = number − head.number` past the head, with
      `time_ms = head_ms + k × block_time_ms`.
  - `BlockContext::at(self, now)`: today's rule in milliseconds. It
    compares in nanoseconds against `time_ms × 1_000_000`, advances
    `1 + (now_ms − time_ms) / block_time_ms` blocks, and `timestamp =
    time_ms / 1000`. Pre-fork contexts give exactly today's numbers.
  - `FeedHandler::with_clock(self, block_time_ms: u64, denim: bool) ->
    Self`. `new` keeps 2000 and false.
- Every `BlockContext { … }` literal in tests gains `time_ms: timestamp ×
  1000, block_time_ms: 2_000` (`live_feed.rs` 757; `live_sim.rs` 348, 487,
  505), and `ReaderConfig` literals gain `denim: false`.

- [ ] **Step 1: Write the failing tests:**
  - `live_feed.rs` `a_denim_block_being_built_takes_the_head_s_milliseconds`:
    head 100 at `timestamp 1_792_605_601` and `timestamp_ms
    1_792_605_601_400`, under `with_clock(200, true)`:
    - the block being built is 101, at `time_ms …601_600`, timestamp
      `…601`;
    - with a preconfirmed log at 103, it is 103, at `time_ms …602_000`,
      timestamp `…602`.
  - `live_feed.rs` `a_denim_head_without_milliseconds_builds_no_block`:
    `None`.
  - `live_sim.rs` `at_carries_on_in_the_config_s_block_time`: a Denim
    context at `time_ms` T, with now T + 450 ms, is three blocks on at
    T + 600.
  - `live_sim.rs` `a_pre_fork_context_carries_on_as_before`: today's
    numbers, through the new fields.
  - `tests/denim_requests.rs` `no_request_names_pending_under_denim`. Under
    `denim: true`, one recording `RpcTransport` (as in `live_reader.rs`)
    serves every runtime read that names a block or a tag:
    - `ChainReader::refresh`;
    - `LiveSimulator`'s simulation;
    - `armed::live_chain::RpcChain`'s `head`, `nonce`, `nonce_at`,
      `balance`, `stops` and `receipt`;
    - `costs::read_l1`;
    - `PoolBook::refresh_twaps` and `refresh_cp_fees`.

    No serialised request contains `"pending"`. With `denim: false` the
    reader's nonce asks `"pending"`, as today.
- [ ] **Step 2: Run them to see them fail.**
  `cargo test -j 3 -p apex-runtime --test live_feed --test live_sim --test denim_requests`.
- [ ] **Step 3: Implement**, then fix the literals.
- [ ] **Step 4: Run them to see them pass**, together with
  `--test live_reader`.
- [ ] **Step 5: Commit** the three source files, the three test files and
  `tests/denim_requests.rs`:
  `feat(live): no pending tag under Denim; Tier 2's clock from the config (Task 8.5 R41)`.

### Task 6: The shadow's wiring: head reads on the clock, no capacity sampling, the report

**Files:**
- Modify: `crates/apex-runtime/src/shadow/mod.rs`
- Test: `tests/shadow_parts.rs`

**Interfaces:**
- Consumes: everything Tasks 1–5 produce.
- Produces:
  - `pub fn samples_capacity(config: &ShadowConfig) -> bool`, which is
    `!config.denim()`. The `"capacity"` task is spawned only when it holds.
  - `Report.head_reads: HeadReadsReport { every_ms: u64, rounds: u64 }`.
  - `FeedReport` gains `bytes_heads`, `bytes_logs`, `bytes_pending_logs`
    and `bytes_other`, all `u64`, from `WsStats`.
- Wiring:
  - `head_loop`:
    - a `ReadClock::new(policy.head_reads_every_ms)` gates the view, the
      TWAPs and the fees together, counting `rounds`;
    - the L1 check (by blocks) and `costs.set` (from the head) run on every
      head, as today.
  - `armed_head_loop` passes `h.timestamp_ms`.
  - `FeedHandler::new(..).with_clock(armed.block_time_ms, config.denim())`.
  - `ReaderConfig.denim = config.denim()`.
  - The `LiveSimulator` gets `.on_denim(config.denim())`.
  - `LivePolicy`'s three new fields come from the config.
  - Boot logs `info!(denim, block_time_ms, head_reads_every_ms,
    max_head_lag_ms, "the chain regime")`.

- [ ] **Step 1: Write the failing test.**
  `the_capacity_sampler_runs_only_before_denim`: `samples_capacity` is true
  for both base files and false for both Denim files.
- [ ] **Step 2: Run it to see it fail.**
  `cargo test -j 3 -p apex-runtime --test shadow_parts`.
- [ ] **Step 3: Implement the wiring.**
- [ ] **Step 4: Run the runtime's tests to see them pass.**
  `cargo test -j 3 -p apex-runtime --all-targets --no-fail-fast`, to a log
  in the plan's workspace; read its tail.
- [ ] **Step 5: Commit** `src/shadow/mod.rs` and `tests/shadow_parts.rs`:
  `feat(shadow): Denim's head reads, sampler and usage report (Task 8.5 R41)`.

## Phase 3: the evaluator

### Task 7: `start_at`: arms record where their start words stand

**Files:**
- Modify: `crates/apex-runtime/src/live/book.rs` (`PoolSnapshot`, the apply and reload paths)
- Modify: `src/armed/lines.rs`, `src/armed/engine.rs` (`arm`), `src/armed/eval.rs`, `src/armed/run.rs` (`effects_of`, the `own` filter)
- Modify: every `PoolSnapshot` literal (`grep -rn 'last_log:' crates/apex-runtime`, about 25 sites, including `src/live/cp.rs`), which gains `through: None`
- Test: `tests/live_book.rs`, `tests/armed_engine.rs`, `tests/armed_eval.rs`

**Interfaces:**
- Produces:
  - `PoolSnapshot.through: Option<LogPosition>`: the newest log the state
    holds.
    - A swap, a sync or a position change applied sets it to
      `max(through, at)`.
    - A reload that keeps the old state keeps the old `through`.
    - A fresh read sets the newest change carried onto it (`pos.applied`
      after the read), or `None`.
    - A first load is `None`.
  - `pub struct StartAt { pub pool: String, pub block: u64, pub log_index:
    Option<u64> }` in `lines.rs`, with `log_index` skipped when `None`.
  - `Arm.start_at: Vec<StartAt>`, `#[serde(default, skip_serializing_if =
    "Vec::is_empty")]`.
    - The engine writes one per pool, in `arm.pools`'s order:
      `{pool: format!("{:#x}", spec.pool), block: through.map_or(snapshot.block,
      |t| t.0), log_index: through.map(|t| t.1)}`.
  - `pub struct Logged { pub log_index: u64, pub pool: String, pub write:
    Write }`, and `TxEffect.writes: Vec<Logged>`.
    - `pool` is the log's address, or the v4 key address for a
      PoolManager log.
    - `effects_of` fills it. The run's `own` filter becomes
      `arm.pools.contains(&logged.pool)`.
  - `judge` skips a write when the arm has a `start_at` for its pool and
    `(e.block, log_index) ≤ (sa.block, sa.log_index.unwrap_or(u64::MAX))`.
    Fire and rival detection are otherwise unchanged. An arm without
    `start_at` is judged exactly as today.

- [ ] **Step 1: Write the failing tests:**
  - `live_book.rs` `through_follows_every_applied_log`:
    - a swap at (10, 5) gives `through == Some((10, 5))`;
    - a mint at (11, 2) applied in place gives `through == Some((11, 2))`,
      with `last_log` still (10, 5);
    - a reload at 10 carrying the mint keeps (11, 2);
    - a reload at 12 gives `None`, with `block == 12`.
  - `armed_engine.rs` `start_at_names_the_arm_s_pools_as_written`: an
    engine-built arm's `start_at[i].pool == arm.pools[i]`.
    - A pool with `through` (11, 2) gives `{block: 11, log_index: Some(2)}`.
    - A pool read at 9 gives `{block: 9, log_index: None}`.
  - `armed_eval.rs`: the `tx()` helper wraps each write as `Logged {
    log_index: position, pool: the write's address (or its pool, for
    Liquidity), write }`. New tests:
    - `a_start_point_ahead_of_the_armed_block_is_not_replayed_twice`:
      - An arm at `armed_block` 100 whose start words already hold a burn
        of B on the other pool. Liquidity is L − B, with L − B at the floor
        and L − 2B below it.
      - Its `start_at` for that pool is (101, 3).
      - The effects are that burn at (101, log 3), then a trigger swap at
        (101, log 5).
      - The fire is the swap's transaction, with no rival.
    - `without_start_at_the_answer_is_today_s`: the same arm with
      `start_at` empty does not fire, which is today's double count.
    - `a_read_start_point_skips_its_whole_block`: `{block: 101, log_index:
      None}` skips every write of block 101 on that pool, and none of 102.
- [ ] **Step 2: Run them to see them fail.**
  `cargo test -j 3 -p apex-runtime --test live_book --test armed_engine --test armed_eval`.
- [ ] **Step 3: Implement**, then add `through: None` at every literal.
- [ ] **Step 4: Run the runtime's tests to see them pass**
  (`--all-targets --no-fail-fast`, to a log, then read its tail).
- [ ] **Step 5: Commit** the five source files and every touched test:
  `feat(armed): arms carry start_at; judge replays only after it (Task 8.5 R41)`.

### Task 8: The evaluator in blocks

**Files:**
- Modify: `crates/apex-runtime/src/armed/eval.rs`, `src/armed/run.rs`
- Test: `tests/armed_eval.rs`, `tests/armed_run.rs`

**Interfaces:**
- Produces:
  - `pub fn settle_blocks(block_time_ms: u64) -> u64`, which is
    `60_000 / block_time_ms.max(1)`. It replaces `SETTLE_BLOCKS`, and the
    run uses `settle_blocks(config.armed.block_time_ms)`.
  - `pub fn landing_in_blocks(fire_block: u64, closes: bool, rival_block:
    Option<u64>, bid: u128, rival_bid: u128) -> Landing`.
    - Ours is `fire_block + u64::from(closes)`.
    - Optimistic is `at(ours)` and pessimistic `at(ours + 1)`, where
      `at(o)`: a rival block before `o` loses, one equal to `o` is
      `bid > rival_bid`, and one after `o` wins. No rival wins both.
  - `pub enum Placement { Flashblocks { fire: (u64, u32), closes: bool,
    rival: Option<(u64, u32)> }, Blocks { fire: u64, closes: bool, rival:
    Option<u64> } }` and `pub fn land(p: &Placement, ours: bool, bid: u128,
    rival_bid: u128) -> Landing`.
    - `ours` is `own_landing`.
    - Otherwise `land` is `landing` or `landing_in_blocks`.
    - `run.rs`'s `judge_bid` calls it.
  - `pub enum OffsetUnit { Flashblock, Block2s, Block200ms }`, serialised
    as `"flashblock"`, `"block_2s"` and `"block_200ms"` (explicit
    `#[serde(rename)]`).
  - `pub fn unit_of(granular: bool, fire_timestamp: Option<u64>) ->
    Option<OffsetUnit>`.
    - Granular is `Flashblock`.
    - A timestamp `≥ DENIM_MAINNET_TS` is `Block200ms`, and one below it is
      `Block2s`.
    - No timestamp is `None`.
  - `Verdict` gains `offset_unit: Option<OffsetUnit>` and
    `fire_closes_block: Option<bool>`, both `#[serde(default,
    skip_serializing_if = "Option::is_none")]`.
  - Not granular:
    - The run reads `eth_getBlockByNumber(fire.block, false)` once per
      block per run (a `BTreeMap` cache).
    - `fire_closes_block` is `Some(fire.tx_index + 1 ==
      transactions.len())`, and the landing is `Placement::Blocks`.
    - A failed read leaves both `None` and judges with `closes = false`.

- [ ] **Step 1: Write the failing tests** in `tests/armed_eval.rs`:
  - The spec's four:
    - `in_blocks_a_trigger_that_closes_its_block_is_contested_in_the_next`:
      closes, a rival in the next block bidding more → lost; ours higher →
      won.
    - `in_blocks_a_rival_after_the_trigger_in_its_own_block_is_a_fee_contest`:
      not closing, a rival in the same block bidding more → lost.
    - `in_blocks_a_rival_in_a_later_block_loses`.
    - `our_own_landing_in_block_mode_wins_at_the_bid_sent_and_above`:
      `land(&Placement::Blocks{..}, true, b, b)` won, and `b − 1` lost.
  - The order tests (`judge`):
    - `a_write_before_the_fire_in_its_block_is_never_the_rival`;
    - `of_two_rivals_after_the_fire_the_first_decides`.
  - `settle_is_a_minute_in_the_config_s_blocks`: 30 and 300.
  - `the_unit_is_the_fire_block_s_era`:
    - `unit_of(false, Some(1_793_901_599)) == Some(Block2s)`;
    - `Some(1_793_901_600)` → `Block200ms`;
    - `unit_of(true, …) == Some(Flashblock)`;
    - `None` → `None`.
  - `tests/armed_run.rs` `old_verdicts_parse_and_units_serialise`: a verdict
    line without the new fields parses. Each unit serialises to its name.
- [ ] **Step 2: Run them to see them fail.**
  `cargo test -j 3 -p apex-runtime --test armed_eval --test armed_run`.
- [ ] **Step 3: Implement.** Keep `landing` and `next_flashblock` for the
  flashblock era.
- [ ] **Step 4: Run them to see them pass.**
- [ ] **Step 5: Commit** `src/armed/eval.rs`, `src/armed/run.rs` and the two
  test files:
  `feat(armed): the evaluator in blocks, with offset_unit (Task 8.5 R41)`.

## Phase 4: the census

### Task 9: The trigger window in time, and per-day units

**Files:**
- Modify: `scripts/data/arb_census/race.py`, `targets.py`, `contest.py`
- Test: `test_race.py`, `test_targets.py`, `test_contest.py`

**Interfaces:**
- Produces:
  - `race.py`:
    - `DENIM_TS = 1_793_901_600` and `def back_blocks(lo: int,
      fork_block: int | None) -> int`, which is 1,500 when `fork_block is
      not None and lo >= fork_block`, else 150.
    - `race_day(…, back=BACK_BLOCKS)` and `day_ledger(var, lo, hi,
      back=BACK_BLOCKS)` use it where `BACK_BLOCKS` is used today.
    - Days carry `"back_blocks"`.
  - `targets.py`:
    - `def fork_block(rpc, resolver) -> int | None`.
      - It returns `resolver.meta["_denim_fork_block"]` when cached.
      - It returns `None` while the head's timestamp is before `DENIM_TS`,
        and caches nothing.
      - Otherwise it finds the block with
        `_first_block_at_or_after(rpc, head, DENIM_TS)` and stores it
        through `resolver.meta` and `resolver.save()`.
    - `def offset_unit(has_flashblocks: bool, lo: int, fork: int | None) ->
      str` is `"flashblock"`, `"block_200ms"` or `"block_2s"`.
    - `_ensure_race_day` takes the run's `fork` and stamps
      `data["offset_unit"]`.
      - A cached day without a unit is stamped without a rebuild.
      - A cached day whose unit disagrees is rebuilt.
    - `build()` records `"units": {day: unit}`. The ranking is unchanged.
  - `contest.py`:
    - `classify` handles `offset_unit == "block_200ms"`:
      - Our earliest block is `fire.block + int(fire_closes_block)`.
        `fire_closes_block` missing → `excluded:fire not placed`.
      - A rival block after it is `uncontested`, one before it is
        `excluded:rival before our block`, and one equal to it is `exact`.
      - Our own send is `censored`.
    - `"block_2s"` → `excluded:block_2s`. Verdicts without a unit keep
      today's flashblock path.
    - Rows carry `"unit"`.
    - `contest_section(…, unit: str)` fits and holds out only rows of
      `unit`. `attach_contest` passes `"block_200ms"` when the held-out day
      is after `FORK_DAY = "2026-11-05"`, else `"flashblock"`.

- [ ] **Step 1: Write the failing tests:**
  - `test_back_blocks_is_five_minutes_in_each_era`: 150, 1,500 and 150
    (no fork).
  - `test_fork_block_is_found_once_and_cached`: a fake RPC counts its
    calls. The second call reads the cache and makes none.
  - `test_fork_block_is_none_before_the_fork`.
  - `test_offset_unit_by_day`.
  - `test_a_cached_day_gains_its_unit_without_a_rebuild`.
  - `test_block_200ms_classify`: closing → next-block contest; a
    same-block rival after a non-closing fire → `exact`; later →
    `uncontested`; earlier → excluded.
  - `test_block_2s_never_fits`.
  - `test_units_are_never_pooled`: a section for `block_200ms` holds only
    those rows.
- [ ] **Step 2: Run them to see them fail.**
  `cd scripts/data && python3 -m pytest arb_census -q`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them to see them pass.**
- [ ] **Step 5: Commit** the three modules and the three test files:
  `feat(census): a 300 s trigger window and per-day offset units (Task 8.5 R41)`.

### Task 10: Measure the census's daily run, and estimate a Denim day

The host, read only. Nothing is restarted. Public endpoints only.

- [ ] **Step 1:** Read the last three `apex-targets.service` runs:
  - `journalctl -u apex-targets --since 2026-10-29 --no-pager` through
    `ops/host/remote.sh`;
  - the start and end times;
  - the "arbitrages so far" lines (one every 5,000 blocks);
  - the receipt and block batches.
- [ ] **Step 2:** Estimate a Denim day:
  - 432,000 blocks at `STEP = 50` is 8,640 `getLogs` windows, against 864
    today;
  - receipts are unchanged;
  - time a window from Step 1.
- [ ] **Step 3:** Write the figures into the ledger.
  - **If the estimate exceeds six hours, stop.** It goes to the operator
    before deploy (spec §7).
  - Otherwise it is part of the deploy report.

## Phase 5: the rehearsal, the switch and the runbook

### Task 11: `apex denim-check`

**Files:**
- Create: `crates/apex-runtime/src/denim_check.rs` (and `pub mod denim_check;` in `lib.rs`)
- Modify: `src/main.rs` (dispatch `denim-check`)
- Test: `tests/denim.rs`

**Interfaces:**
- Consumes: `HeadWindow`, `ReadClock`, `lattice` rules, `WsStats::bytes`,
  `shadow::subscriptions`.
- Produces:
  - `pub struct Tally`, with `Tally::new(block_time_ms, max_head_lag_ms,
    head_reads_every_ms)` and `fn head(&mut self, number, timestamp,
    timestamp_ms: Option<u64>, arrival_ms)`.
    - It counts heads, skips, repeats, unstamped heads and off-lattice
      steps (consecutive stamped heads not 200 apart, `ms % 200`, or
      `ms / 1000 != timestamp`).
    - It records each head's lag.
    - From the 21st head, it counts `guard_checked` and `guard_held`.
    - It drives a `ReadClock`, keeping `read_rounds` and the smallest gap
      between rounds.
  - `fn log(&mut self)` counts logs.
  - `fn verdict(&self, chain_id, refusals, sessions, bytes) -> Verdict`.
    `Verdict` serialises as one JSON line with:
    - `chain_id`, `for_s`, `heads`, `logs`, `skips`, `repeats`,
      `without_ms`, `off_lattice`;
    - `guard_checked`, `guard_held`, `lag_ms {p50, p99, max}`;
    - `read_rounds`, `read_min_gap_ms`;
    - `bytes {heads, logs, pending_logs, other}`, `refusals`, `sessions`;
    - `pass` and `reasons`.
  - `pass` requires:
    - `refusals == 0` and `sessions == 1`;
    - `guard_held ≥ 99%` of `guard_checked`, with `guard_checked > 0`;
    - lag p99 ≤ `max_head_lag_ms`;
    - `read_min_gap_ms ≥ head_reads_every_ms`.
  - `apex denim-check --config PATH --for-s N`:
    - `load_canary`;
    - `eth_chainId` over `rpc.http[0]`, a plain POST, printed;
    - a `WsFeed` on `subscriptions(&config.feed, filters)`, where the
      filters are the inventory's pools, as the shadow's feed builds them;
    - a `Tally` for N seconds, then the verdict on stdout.

    Exit 0 on pass, 1 on fail. It reads no key the config does not name.

- [ ] **Step 1: Write the failing tests** in `tests/denim.rs`:
  - `the_check_passes_sepolia_s_heads`: the fixture through `Tally` at
    (200, 1,000, 2,000) passes.
    - `read_min_gap_ms ≥ 2,000`.
    - `read_rounds` is 30 ± 1 for 60 s.
  - `the_check_fails_at_2000`.
  - `the_check_fails_when_lag_p99_exceeds_the_limit`: at
    `max_head_lag_ms` 300, p99 347 fails, with a reason naming the lag.
  - `the_check_counts_skips_repeats_and_off_lattice`.
  - `a_refused_subscription_fails_the_check`.
- [ ] **Step 2: Run them to see them fail.**
- [ ] **Step 3: Implement** `denim_check.rs` and the subcommand.
- [ ] **Step 4: Run them to see them pass.** Then a smoke run against
  public Sepolia:
  - with a scratch copy of `canary.denim.yaml` whose `rpc` is
    `https://sepolia.base.org` and `wss://base-sepolia-rpc.publicnode.com`;
  - `cargo run -j 3 --release -p apex-runtime -- denim-check --config
    <scratch> --for-s 30`;
  - expected: `"pass": true`, `"chain_id": 84532`.
- [ ] **Step 5: Commit** `src/denim_check.rs`, `src/lib.rs`, `src/main.rs`
  and `tests/denim.rs`:
  `feat(denim): apex denim-check (Task 8.5 R41)`.

### Task 12: The switch: `apex denim-detect`, `denim-switch.sh`, its timer

**Files:**
- Modify: `crates/apex-runtime/src/main.rs` (dispatch `denim-detect`)
- Create: `ops/host/denim-switch.sh` (mode 0755), `ops/host/apex-denim-switch.service`, `ops/host/apex-denim-switch.timer`
- Create: `crates/apex-runtime/tests/denim_switch.rs`

**Interfaces:**
- Consumes: `denim::{detect, LatticeBlock, Detect, DENIM_MAINNET_TS}`.
- Produces:
  - **`apex denim-detect --rpc URL [--rpc URL …] [--chain-id 8453]
    [--fork-at 1793901600]`.** It takes the URLs in order; the first that
    answers decides.
    - It posts `[eth_chainId, eth_blockNumber]`, then a batch of
      `eth_getBlockByNumber(n−9 … n, false)`.
    - It prints one JSON line: `{"source": <host only>, "chain_id",
      "blocks": [{"number", "timestamp", "timestampMs"}], "verdict":
      "denim" | "not_yet" | "anomaly", "reason"}`.
    - It exits 0 for Denim, 1 for not yet, 2 for an anomaly, and 3 when no
      source answered.
  - **`ops/host/denim-switch.sh`** (root). Its flags and defaults:
    - `--rpc`, repeatable: the two sources;
    - `--chain-id 8453`, `--fork-at 1793901600`, `--give-up-at 1793908800`,
      `--every-s 2`;
    - `--dry-run`;
    - `--apex /opt/apex/bin/apex`, `--systemctl systemctl`,
      `--etc /etc/systemd/system`;
    - `--record /opt/apex/var/apex/denim-switch.jsonl`;
    - `--as-user apex`, with `runuser -u` for the detect. Empty runs it
      directly.

    Its steps:
    1. **Already done.** If both drop-ins hold their Denim `ExecStart`,
       print "already switched" and exit 0.
    2. **Loop.** Run until `--give-up-at`, then append `{"event":
       "gave_up", "at"}` and exit 1.
    3. **Each round.** Run detect. Append its line with `"at"` to the
       record when it says Denim or an anomaly; a not-yet round writes
       nothing.
    4. **On Denim.** Write each drop-in (temp file, then `mv`), run
       `daemon-reload` and `restart apex-canary`, append `{"event":
       "switched", …detect, "restarted_at"}`, and exit 0.
    5. **Dry run.** One round: print the drop-ins it would write and
       "would restart apex-canary", write nothing, and exit 0 on Denim, 1
       otherwise.
  - **The drop-ins.** `apex-canary.service.d/denim.conf` and
    `apex-armed-eval.service.d/denim.conf`. Each is a comment line (R41,
    the time, and "remove at R41.1"), then `[Service]`, `ExecStart=`, and
    `ExecStart=/opt/apex/bin/apex canary --config ops/canary.denim.yaml`.
    The evaluator's is `… armed-eval --config ops/shadow.denim.yaml`.
  - **The units.**
    - `apex-denim-switch.service`: `Type=oneshot`, root,
      `ExecStart=/opt/apex/ops/host/denim-switch.sh`,
      `TimeoutStartSec=2h30min`.
    - `apex-denim-switch.timer`:
      `OnCalendar=2026-11-05 17:59:00 UTC`, `AccuracySec=1s`,
      `Persistent=true`. A reboot after 17:59 still switches, and one
      after 20:00 gives up at once.

- [ ] **Step 1: Write the failing tests** in `tests/denim_switch.rs`.
  - **The harness.** Each test runs `bash ops/host/denim-switch.sh` in a
    temp directory with:
    - `--apex` set to a stub script, which answers each call with the next
      canned detect line and exit code from a file;
    - `--systemctl` set to a stub, which appends its arguments to a file;
    - `--etc`, `--record`, `--as-user ''` and `--every-s 0`.
  - `the_switch_writes_both_drop_ins_and_restarts_on_denim`: not_yet,
    not_yet, then denim. Both drop-ins have the empty `ExecStart=` line
    before the Denim one. `systemctl` saw `daemon-reload` then
    `restart apex-canary`. The record is one `switched` line holding ten
    blocks.
  - `a_dry_run_writes_nothing`.
  - `nothing_changes_when_base_postpones`: `--give-up-at` in the past gives
    one `gave_up` line, no drop-ins and no `systemctl`.
  - `an_anomaly_is_recorded_and_not_switched`: an anomaly, then denim,
    records an `anomaly` line before `switched`.
  - `a_second_run_finds_the_drop_ins_and_does_nothing`: across two runs,
    `systemctl` is called once.
  - `a_half_written_switch_is_completed_not_skipped`: with only the canary
    drop-in present, the run detects, writes the evaluator's, and restarts.
  - `the_timer_fires_at_17_59_utc_on_the_fork_day`: the timer file holds
    `OnCalendar=2026-11-05 17:59:00 UTC`, and the service runs the script.
- [ ] **Step 2: Run them to see them fail.**
  `cargo test -j 3 -p apex-runtime --test denim_switch`.
- [ ] **Step 3: Implement** `denim-detect`, the script and the units.
- [ ] **Step 4: Run them to see them pass.** Then, read only:
  - `target/release/apex denim-detect --rpc https://sepolia.base.org
    --chain-id 84532 --fork-at 1792605600` → `denim`;
  - `target/release/apex denim-detect --rpc https://mainnet.base.org` →
    `not_yet`.
- [ ] **Step 5: Commit** `src/main.rs`, the script, the two units and
  `tests/denim_switch.rs`:
  `feat(ops): the chain-gated Denim switch and its timer (Task 8.5 R41)`.

### Task 13: The runbook

**Files:**
- Modify: `ops/host/ARMED.md` ("The Denim day"), `ops/host/README.md`

- [ ] **Step 1: Rewrite "The Denim day"** in `ARMED.md`:
  - **The switch:** the timer, what it proves, its drop-ins, and how to read
    `denim-switch.jsonl`.
  - **Gate M's checks**, each with its command:
    - `shadow-status.sh`;
    - the report's `armed.live` cadence, lag and skips, `head_reads`,
      `feed.bytes_*` and `rpc`;
    - `systemd-run … apex denim-check --for-s 300`.
  - **Gate M's four responses**, with the spec's values.
  - **The manual fallback** (§Rollback).
  - Don't run `armed-moves-seed` or `send-probe` across or after the fork.
  - **R41.1:** make the base configs the Denim ones and remove the drop-ins.
- [ ] **Step 2: Add to `README.md`:**
  - installing `apex-denim-switch.{service,timer}` and checking
    `systemctl list-timers apex-denim-switch.timer`;
  - the drop-ins;
  - a warning not to archive `var/apex` before 11-07.
- [ ] **Step 3: Commit** both files:
  `docs(ops): the Denim day, automated (Task 8.5 R41)`.

## Gate B (11-01–02): the branch

- [ ] **The five gates and pytest** (Global Constraints), each to a log;
  read the tails.
- [ ] **The mutation pass.**
  - **Method.** Apply each mutant by hand. Run the owning test file with
    `--no-fail-fast`. Restore it and `touch` the file, or cargo serves the
    mutant's stale build.
  - **A survivor** means deleting the guard or adding the test that kills
    it, and a ledger line.
  - **Load check:** `== 200` → `!= 2000`; drop the `pending_logs` rule;
    `1..=5000` → `0..=5000`; drop the 2000 lag-0 rule.
  - **Read clock:** `>=` → `>`; the first call not due; `now < last` not
    due.
  - **Stops freshness:** `max(block_time, reads)` → `block_time`.
  - **Chain cadence:**
    - `* 2 <=` → `* 2 <`;
    - divide by the window's length, not the block span;
    - an unstamped newest head still holds.
  - **Head lag:** `<=` → `<`; lag from arrival, not the stamp.
  - **Block landing:** ignore `closes`; `r == o` → `r >= o`.
  - **`start_at`:** `≤` → `<`; `start_at` ignored; `unwrap_or(u64::MAX)` →
    `unwrap_or(0)`.
  - **Lattice:** each of its rules removed in turn.
  - **`settle_blocks`:** `60_000` → `6_000`.
  - **`unit_of`:** `>=` → `>`.
- [ ] **Self-review** of the whole branch against the spec. Its rulings and
  deferred minors go to the ledger and the report.

## Gate S (11-02): Sepolia rehearsal, read only, public endpoints

- [ ] `apex denim-check` for 600 s on the scratch Sepolia config passes.
- [ ] `denim-switch.sh --dry-run --rpc https://sepolia.base.org --chain-id
  84532 --fork-at 1792605600 --as-user '' --apex target/release/apex`
  prints both drop-ins and exits 0. The same against mainnet exits 1 (not
  yet).
- [ ] The drift test has loaded all four configs (Task 3).

## The deploy (11-03) and after: operator steps, each approved at the time

- **Before asking:**
  - Task 10's estimate;
  - Gate B's and S's results;
  - the rev.
- **On "yes, push and deploy <rev>":**
  - push, then deploy with `ops/host/deploy.sh`;
  - restart `apex-canary` without archiving;
  - install and enable `apex-denim-switch.timer`.
- **Check after the deploy:**
  - The status lines match the pre-deploy run: the same read rate (30
    rounds a minute at 2 s heads), the guard holds, arms are sent.
  - `systemctl list-timers` shows 2026-11-05 17:59:00 UTC.
  - Record the report's `rpc` bytes by method and `feed.bytes_*`: the
    baseline for gate M.
- **11-04 rehearsal on the host, and 11-05 ~12:00 last look:**
  - the switch in `--dry-run` against mainnet ("not yet") and Sepolia (the
    drop-ins);
  - `chronyc tracking` within 10 ms;
  - the latest stable `base/base` release still has Denim at
    1_793_901_600, else re-arm the timer;
  - Base's status page.
- **Gates M (18:00–19:00) and D (11-06–07):** as the spec states, read with
  the commands in `ARMED.md`.
- **R41.1 (about 11-06):** a follow-up plan.
