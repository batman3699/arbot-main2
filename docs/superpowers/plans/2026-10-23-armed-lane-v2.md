# Armed lane v2 Implementation Plan (Task 8.5 R36, phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Arm Cobalt validity transactions on the 50 routes where trigger-block arbitrage that our executor can run actually happens, fire them on ordinary moves, bid per gas against what the winners pay, and run them live from 16 senders inside a 0.01 ETH daily budget that bounds realized loss plus every pending worst case.

**Architecture:** A daily census on the host (Python, `arb_census`) writes `targets.json`: the top routes and the winners' priority fees per gas. The runtime admits those routes' pools to the book, arms only their cycles with R26's engine (rungs moved close in, a bid per arm), and in `mode: live` signs each arm from its own sender and submits it with `base_sendRawTransactionValidity`. A write-ahead live ledger reserves every pending submission's worst case against the daily budget, reconciles on restart, and accounts every landing; the feed can drop `pendingLogs` by config for Denim.

**Tech Stack:** Rust (apex-runtime, apex-chain; ethers-signers, alloy-primitives, serde, tokio), Python 3 stdlib (census), Foundry (owner scripts).

**Spec:** `docs/superpowers/specs/2026-10-23-armed-lane-v2-design.md` (approved 2026-10-23). R26's spec `docs/superpowers/specs/2026-10-13-armed-backruns-design.md` §4–6 defines the pilot pieces this plan builds; read both.

## Global Constraints

- Phase 1 only: two-hop, WETH-start cycles on our five venues (`UniswapV3`, `Slipstream`, `PancakeV3`, `SlipstreamV3`, `AerodromeV2`). No v4, no USDC start, no 3-hop.
- Targets: top **K = 50** routes by trailing **7-day** trigger-block value; a file is live-usable only with `ok: true` (at least 3 days of coverage, at least one route, no non-finite values).
- Senders: **N = 16** (`max_arms: 16`), funded **0.002 ETH** each (operator decision pending, see the revision note).
- **Daily loss budget: 0.01 ETH, ETH-denominated** (≈US$24 at $2,450; ruling after the 2026-10-24 review: a fixed ETH figure rather than a daily ETH/USD oracle). It bounds **realized loss plus the worst-case cost of every pending submission**: before any send, `max(0, −day_net) + reserved + worst_case(tx) ≤ budget`, with `worst_case = gas_limit × max_fee_per_gas + l1_fee_bound_wei`.
- A sender is used only when `balance ≥ gas_limit × max_fee_per_gas + l1_fee_bound_wei + sender_reserve_wei`.
- Window **W = 10 s** as a `block_number` bound, recomputed from the freshest head at send: `to_block = head + ceil(W / block_time_ms)`. **No `flashblock_index` predicates.** No live sends while the measured median head interval is off `block_time_ms` by more than 50%.
- Rung offsets: **`[0, 1, 2, 4, 8, 16]`** steps past the threshold.
- Band cap 5 bps, liquidity floor 9800 bps (R26's). **Bands and thresholds clear the hurdle plus the arm's own tip** (`margin = bid × gas_expected`), so a fire within the predicates nets ≥ 0 after its tip.
- **The bid is per gas:** `b` maximising `F(b) × (net − b × gas_expected)`, `F` the share of the route's measured winners whose effective priority per gas is below `b` (all targeted routes' winners when the route has fewer than 20), the tip paid only on a win (a validity loser is never included). Ranking: `P(trigger within W) × F(b) × (surplus − b × gas_expected)`. Capped by `max_priority_wei_per_gas`.
- Submit only to `https://mainnet-sequencer.base.org`, never BlockPI. Predicates on the wire as `{"type": …, "params": {…}}`.
- Every submission is written before it is sent and reserved until its bound passes and its nonce is reconciled; a `Silent` answer is a possible send; a refused replacement leaves the earlier transaction tracked. Fee bumps are integer (`× 1125 / 1000`, rounded up), at most two, never above the caps.
- Gate A′ (shadow, ≥ 1 day): quote and simulation agree on ≥ 95% of checked fires, reported weighted by value and by route; every armed pool's storage words match; the API probe passed with every action reconciled; request units under today's ~124M/day.
- Gate B (live): 7 days **and** at least 30 landings; realized net positive after every fee; included-revert fees < 20% of gross; zero tripwires; net reported by day and by route.
- Secrets never printed, logged or on argv; keys file `/etc/apex/armed.keys`, owner `apex`, mode `0400`, refused if group- or world-readable; only addresses printed.
- Owner transactions are prepared and dry-run only; the operator broadcasts. Merges, pushes, deploys and every mainnet transaction need the operator's approval at the time.
- No new non-secret environment variables (config fields and files are fine). Python `urllib` calls carry a `User-Agent`; JSON is written with `allow_nan=False`. Never `cargo fmt`; build with `-j 3`; run the five gates (`gates.sh`) before pushing, known-red: `check_placeholder_endpoints.sh`, `cl_parity_sweep.sh`; verify each commit in a clean worktree.
- `ops/host/README.md` carries an uncommitted edit that is not ours: do not stage it; the pilot's operations go in a new `ops/host/ARMED.md`.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A restart with submissions still pending.** Boot rebuilds pending hashes, reserved exposure and the day's net from the ledger, waits for every pending bound to pass, reconciles receipts and nonces, and only then sends. Pinned in Task 10 (`boot_settles_every_pending_submission_before_any_send`).
2. **Sixteen arms that could all be included at once.** Their summed worst case can never exceed the budget, whatever the realized net. Pinned in Task 9 (`concurrent_worst_cases_never_exceed_the_budget`).
3. **An answer that is neither yes nor no.** A `Silent` send keeps its hash and its reservation until its bound passes and the nonce says what happened. Pinned in Task 9 (`a_silent_answer_is_a_possible_send`).
4. **A missing, stale, empty or corrupt `targets.json` at boot:** in `live` mode a boot error; in `shadow` mode R26's universe and the report says so. Pinned in Task 3 (`targets_absent_or_unfit_is_fatal_live_and_falls_back_in_shadow`).
5. **The Denim day with the wrong `block_time_ms`.** The cadence guard stops live sends until the configuration matches the chain. Pinned in Task 10 (`a_cadence_off_the_configuration_stops_sends`).

---

## File Structure

| File | Responsibility |
|---|---|
| `scripts/data/arb_census/race.py` (modify) | Record each arbitrage's pools, its WETH-in pool and its gain tokens; offsets by block when a day has no flashblock ledger (Denim). |
| `scripts/data/arb_census/targets.py` (create) | Build `targets.json` from the trailing race days. |
| `ops/host/apex-targets.service`, `ops/host/apex-targets.timer` (create) | The daily census, then a restart of `apex-canary`. |
| `crates/apex-runtime/src/armed/targets.rs` (create) | Load and validate `targets.json`; route membership. |
| `crates/apex-runtime/src/armed/bid.rs` (create) | The bid share and the per-gas bid. |
| `crates/apex-runtime/src/armed/keys.rs` (create) | Generate and load the senders' keys file. |
| `crates/apex-runtime/src/armed/live_ledger.rs` (create) | The live ledger's records, the day's net, the halt. |
| `crates/apex-runtime/src/armed/live.rs` (create) | The live worker: sign, submit, renew, nonces, landings, tripwire, balances. |
| `crates/apex-runtime/src/validity_probe.rs` (create) | `apex validity-probe`. |
| `crates/apex-chain/src/base/validity.rs` (create) | `base_sendRawTransactionValidity` over HTTP. |
| `crates/apex-runtime/src/armed/{threshold,engine,lines,mod}.rs` (modify) | Rung offsets; the bid on each arm; cycles from targets. |
| `crates/apex-runtime/src/live/{inventory,admission}.rs` (modify) | Admit target pools below the depth floor. |
| `crates/apex-runtime/src/shadow/{config,mod}.rs` (modify) | Config sections; boot checks; wiring; the report. |
| `crates/apex-runtime/src/main.rs`, `lib.rs` (modify) | `apex keys new`, `apex validity-probe`. |
| `script/AuthorizeArmedSenders.s.sol`, `script/FundArmedSenders.s.sol` (create) | Owner authorization/revocation and funding, read from `ops/armed-senders.json`. |
| `ops/host/ARMED.md` (create) | The pilot's operations. |
| `ops/canary.base.yaml` (modify) | The `armed` and `feed` sections. |

---

### Task 1: The census records each arbitrage's route

**Files:**
- Modify: `scripts/data/arb_census/race.py` (`_race_day`, `race_day`'s result)
- Test: `scripts/data/arb_census/test_race.py`

**Interfaces:**
- Produces: each arbitrage dict in `race-YYYY-MM-DD.json.gz` gains `"pools": [str, …]` (its legs' pools, log order), `"weth_in": str | None` (the pool whose pool-side WETH delta is positive, when exactly one is), `"gain": [str, …]` (sorted tokens with a positive trader change).
- Produces: `race.route_of(legs) -> tuple[str | None, list[str], list[str]]` returning `(weth_in, pools, gain)`.
- Produces: `race.block_offset(arb_block, trigger_block) -> int`, used when the day's ledger has no line for the arbitrage's block (Denim's 200 ms blocks: same block is offset 0).

- [ ] **Step 1: Write the failing tests**

```python
class Route(unittest.TestCase):
    def test_the_weth_in_pool_is_the_one_that_took_weth(self):
        a = Leg("0xh", 1, "uniswap_v3", "pa", {WETH: 100, T: -50})
        b = Leg("0xh", 1, "slipstream_5e7b", "pb", {T: 50, WETH: -103})
        self.assertEqual(race.route_of([b, a]), ("pa", ["pb", "pa"], [WETH]))

    def test_no_weth_in_without_exactly_one_pool_taking_weth(self):
        a = Leg("0xh", 1, "v", "pa", {U: 100, T: -50})
        b = Leg("0xh", 1, "v", "pb", {T: 50, U: -103})
        self.assertEqual(race.route_of([a, b])[0], None)

class Denim(unittest.TestCase):
    def test_without_a_flashblock_ledger_the_offset_is_counted_in_blocks(self):
        self.assertEqual(race.block_offset(500, 500), 0)
        self.assertEqual(race.block_offset(502, 500), 2)
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd scripts/data && python3 -m unittest arb_census.test_race`
Expected: FAIL with `AttributeError: module 'arb_census.race' has no attribute 'route_of'`

- [ ] **Step 3: Implement `route_of` and `block_offset` in `race.py`; in `_race_day` store `pools`, `weth_in`, `gain` on each arbitrage; in `race_day`, when `ledger.locate` finds neither the arbitrage's nor its trigger's flashblock and `day_ledger` loaded no line in the day's range, set `offset = block_offset(...)` and `timing = "ok"`.**

- [ ] **Step 4: Run them to verify they pass**

Run: `cd scripts/data && python3 -m unittest arb_census.test_race arb_census.test_offset0`
Expected: OK

- [ ] **Step 5: Commit**

```bash
git add scripts/data/arb_census/race.py scripts/data/arb_census/test_race.py
git commit -m "feat(data): the census records each arbitrage's route (Task 8.5 R36)"
```

---

### Task 2: The daily targets file

**Files:**
- Create: `scripts/data/arb_census/targets.py`, `scripts/data/arb_census/test_targets.py`
- Create: `ops/host/apex-targets.service`, `ops/host/apex-targets.timer`

**Interfaces:**
- Consumes: Task 1's `pools`, `weth_in`, `gain`; R34's `offset`, `usd`, `prio` (the winner's effective priority per gas: receipt `effectiveGasPrice` − block base fee), `gas`, and each day's `eth_usd`.
- Produces: `targets.json`, written with `allow_nan=False`:
  ```json
  {"generated_at": "2026-10-25T01:10:00Z", "days": ["2026-10-18", "…"], "top": 50, "ok": true,
   "routes": [{"pools": ["0x<weth-in pool>", "0x<other pool>"], "usd": 123.4, "arbs": 7, "bids_wei_per_gas": [1200000, …]}],
   "bids_wei_per_gas": [0, 1000000, …],
   "calibration": [{"bid": 16000000, "fit": 0.42, "held_out": 0.39}, …]}
  ```
  - `routes` are sorted by `usd` descending, and route keys are always WETH-in pool first.
  - A route carries its own `bids_wei_per_gas` only when it has at least 20.
  - The top-level `bids_wei_per_gas` holds the winners on every selected route, sorted ascending.
  - `ok` is true only with at least 3 days, at least one route, and finite values.
  - `calibration` fits `F` on all but the last day and checks it on the last, at the fit's 25th, 50th, 75th and 90th percentile bids.
- Produces:
  - `targets.select(arb) -> bool`: two hops, every venue in `race.OURS`, `gain == [WETH]`, `weth_in` set, `offset == 0`.
  - `targets.route_key(arb) -> tuple[str, str]`: `(weth_in, the other pool)`.
  - `targets.build(days, top) -> dict`.
  - `targets.calibration(fit_bids, held_out_bids) -> list[dict]`.
  - `targets.ours_vs_market(days, landings) -> list[dict]`: per route, the census's trigger-block value against our `Landed` records (Task 8's `armed-live.jsonl` lines with `kind == "landed"`, matched on `pools`), by day: `{"day", "pools", "market_usd", "ours_landed", "ours_net_wei"}`. `main` writes it to `--share-out`.

- [ ] **Step 1: Write the failing tests** (`test_targets.py`)

```python
def arb(pools=("pa", "pb"), weth_in="pa", venues=("uniswap_v3", "aerodrome_v2"), gain=(WETH,), offset=0, usd=10.0, prio=None, gas=None, hops=2):
    return {"pools": list(pools), "weth_in": weth_in, "venues": list(venues), "gain": list(gain),
            "offset": offset, "usd": usd, "prio": prio, "gas": gas, "hops": hops}

class Select(unittest.TestCase):
    def test_only_two_hop_weth_routes_on_our_venues_taken_in_the_trigger_block(self):
        self.assertTrue(T.select(arb()))
        self.assertFalse(T.select(arb(offset=1)))
        self.assertFalse(T.select(arb(venues=("uniswap_v3", "uniswap_v4"))))
        self.assertFalse(T.select(arb(gain=(USDC,))))
        self.assertFalse(T.select(arb(hops=3)))
        self.assertFalse(T.select(arb(weth_in=None)))

class Build(unittest.TestCase):
    def test_routes_keyed_weth_in_first_whatever_the_log_order(self):
        day = {"eth_usd": 2500.0, "arbs": [arb(usd=5.0), arb(pools=("pb", "pa"), weth_in="pa", usd=4.0), arb(pools=("pc", "pd"), weth_in="pd", usd=20.0)]}
        out = T.build([day] * 3, top=2)
        self.assertEqual([(r["pools"], r["usd"], r["arbs"]) for r in out["routes"]], [(["pd", "pc"], 60.0, 3), (["pa", "pb"], 27.0, 6)])

    def test_bids_are_the_winners_priority_per_gas(self):
        out = T.build([{"eth_usd": 2500.0, "arbs": [arb(prio=3_000_000, gas=300_000), arb(prio=1_000_000, gas=500_000)]}] * 3, top=50)
        self.assertEqual(out["bids_wei_per_gas"], [1_000_000] * 3 + [3_000_000] * 3)

    def test_a_route_carries_its_own_bids_only_from_twenty(self):
        out = T.build([{"eth_usd": 2500.0, "arbs": [arb(prio=10**8, gas=300_000) for _ in range(19)]}], top=50)
        self.assertNotIn("bids_wei_per_gas", out["routes"][0])

    def test_unfit_files_say_so(self):
        self.assertFalse(T.build([], top=50)["ok"])                                           # no days
        self.assertFalse(T.build([{"eth_usd": 2500.0, "arbs": [arb()]}] * 2, top=50)["ok"])   # two days
        self.assertTrue(T.build([{"eth_usd": 2500.0, "arbs": [arb()]}] * 3, top=50)["ok"])
        with self.assertRaises(ValueError):
            T.write({"x": float("nan")}, os.path.join(tempfile.mkdtemp(), "t.json"))

    def test_calibration_compares_the_fit_with_the_day_held_out(self):
        rows = T.calibration([1, 2, 3, 4], [1, 1, 5, 5])
        self.assertEqual([r["bid"] for r in rows], [2, 3, 4, 4])                # p25, p50, p75, p90 of the fit
        self.assertEqual((rows[0]["fit"], rows[0]["held_out"]), (0.25, 0.5))   # shares strictly below the bid

    def test_our_landings_are_set_against_the_market_per_route_and_day(self):
        day = {"day": "2026-10-25", "eth_usd": 2500.0, "arbs": [arb(usd=10.0), arb(usd=5.0)]}
        landed = [{"kind": "landed", "pools": ["pa", "pb"], "net_wei": "1000", "at_ms": 1792900000000}]
        self.assertEqual(T.ours_vs_market([day], landed), [{"day": "2026-10-25", "pools": ["pa", "pb"], "market_usd": 15.0, "ours_landed": 1, "ours_net_wei": 1000}])
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd scripts/data && python3 -m unittest arb_census.test_targets`
Expected: FAIL with `ModuleNotFoundError: No module named 'arb_census.targets'`

- [ ] **Step 3: Implement `targets.py`:** `select`, `route_key`, `build`, `calibration`, `ours_vs_market`, `write(obj, path)` (`json.dumps(…, allow_nan=False)` to `path.tmp`, then `os.replace`), and `main()` with `--race DIR --days 7 --top 50 --out PATH --share-out PATH --var /opt/apex/var/apex --meta PATH --gaps PATH --live-ledger PATH`. `main` first runs `race.race_day` for each trailing day whose `race-*.json.gz` is missing. A day after Denim, with no flashblock ledger, is counted in blocks (Task 1).

- [ ] **Step 4: Run them to verify they pass**

Run: `cd scripts/data && python3 -m unittest arb_census.test_targets arb_census.test_race`
Expected: OK

- [ ] **Step 5: Write the systemd units.** `apex-targets.service`: `Type=oneshot`, runs `runuser -u apex -- nice -n 19 python3 -m arb_census.targets --race /opt/apex/var/census --days 7 --top 50 --out /opt/apex/var/apex/targets.json --share-out /opt/apex/var/apex/armed-share.json --meta /opt/apex/var/census/meta.json --gaps /opt/apex/var/census/r33-gaps.json --live-ledger /opt/apex/var/apex/armed-live.jsonl` from `/opt/apex/scripts/data`, then `ExecStartPost=/bin/systemctl restart apex-canary`. `apex-targets.timer`: `OnCalendar=*-*-* 00:20:00 UTC`, `Persistent=true`.

- [ ] **Step 6: Commit**

```bash
git add scripts/data/arb_census/targets.py scripts/data/arb_census/test_targets.py ops/host/apex-targets.service ops/host/apex-targets.timer
git commit -m "feat(data): the daily targets file: where trigger-block arbitrage happens (Task 8.5 R36)"
```

---

### Task 3: Targets in the runtime

**Files:**
- Create: `crates/apex-runtime/src/armed/targets.rs`
- Modify: `crates/apex-runtime/src/armed/mod.rs`, `crates/apex-runtime/src/live/inventory.rs` (`load`), `crates/apex-runtime/src/live/admission.rs` (`admit_book`), `crates/apex-runtime/src/shadow/mod.rs` (`universe`, boot, the cycle sets), `crates/apex-runtime/src/shadow/config.rs` (`ArmedConfig`)
- Test: `crates/apex-runtime/tests/armed_targets.rs`

**Interfaces:**
- Produces: `pub struct TargetRoute { pub pools: [Address; 2], pub usd: f64, pub arbs: u32, pub bids_wei_per_gas: Option<Vec<u128>> }`; `pub struct Targets { pub generated_at: String, pub days: Vec<String>, pub ok: bool, pub routes: Vec<TargetRoute>, pub bids_wei_per_gas: Vec<u128> }`.
- Produces: `Targets::load(path: &Path, now_unix: u64, max_age_s: u64) -> Result<Targets, String>`, which refuses a file older than `max_age_s` (`generated_at`), a non-finite `usd`, or a route whose pools are equal. `Targets::pools(&self) -> BTreeSet<Address>`. `Targets::has(&self, weth_in: Address, other: Address) -> bool`. `Targets::bids_for(&self, weth_in: Address, other: Address) -> &[u128]` returns the route's own bids, else the top-level ones. `Targets::live_ready(&self) -> bool` = `ok && !routes.is_empty()`.
- Produces: `inventory::load(data_dir: &Path, filter: UniverseFilter, include: &BTreeSet<Address>) -> Result<Vec<PoolSpec>, InventoryError>`. A pool in `include` passes the depth floor (fee ceiling and venue rules still apply). `admission::admit_book(book, chain, read_block, filter, include)` behaves the same way.
- A target pool must be in the factory-built inventory (R30's refresh): the census's pool is admitted only through its inventory record, so the spec's factory check holds by construction. A target pool with no record is not admitted and is counted in the report's `armed.targets_missing`.
- Produces: config `armed.targets: Option<PathBuf>` and `armed.targets_max_age_s: u64` (default `172800`).
- Produces: at boot, the reactive frontier is built over the pools that pass the depth floor (unchanged behaviour). The armed engine's cycles are `frontier::build` over the whole book, kept when `targets.has(c.legs[0].pool, c.legs[1].pool)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_target_pool_below_the_floor_is_admitted_and_others_are_not() // inventory fixture: two pools at $5k, one listed in include; load returns only the listed one of the two
#[test]
fn routes_are_matched_weth_in_pool_first() // has(pa, pb) true, has(pb, pa) false
#[test]
fn a_route_without_twenty_bids_bids_against_everyone_s() // bids_for falls back to top-level
#[test]
fn a_stale_or_malformed_file_is_refused() // generated_at older than max_age_s → Err containing "stale"; equal pools → Err; usd NaN in the text → Err
#[test]
fn a_target_pool_without_an_inventory_record_is_counted_not_admitted() // include holds a pool absent from the fixture: load ignores it; shadow::targets_missing(&targets, &specs) == 1
#[test]
fn targets_absent_or_unfit_is_fatal_live_and_falls_back_in_shadow() // mode Live with targets None, or loaded with ok false or no routes → Err; Shadow → Ok(R26's cycle set) and `targets_loaded: false`
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -j 3 -p apex-runtime --test armed_targets`
Expected: FAIL (unresolved import `apex_runtime::armed::targets`)

- [ ] **Step 3: Implement `targets.rs`, the `include` parameter (callers that have no targets pass an empty set), and `pub fn armed_cycles(config: &ShadowConfig, all: &BTreeMap<RouteId, Cycle>, now_unix: u64) -> Result<(BTreeMap<RouteId, Cycle>, bool), String>` in `shadow/mod.rs`. The bool is whether targets were loaded. Wire both into the boot, with the reactive frontier over floor-passing pools only. The report's `armed` section gains `targets_loaded: bool`, `targeted_routes: u64`.**

- [ ] **Step 4: Run them to verify they pass, and the existing suites**

Run: `cargo test -j 3 -p apex-runtime --test armed_targets --test live_admission --test shadow_parts --test armed_engine`
Expected: all `test result: ok`

- [ ] **Step 5: Commit**

```bash
git add crates/apex-runtime/src/armed/targets.rs crates/apex-runtime/src/armed/mod.rs crates/apex-runtime/src/live/inventory.rs crates/apex-runtime/src/live/admission.rs crates/apex-runtime/src/shadow/mod.rs crates/apex-runtime/src/shadow/config.rs crates/apex-runtime/tests/armed_targets.rs
git commit -m "feat(armed): arm the census's routes, their pools admitted below the floor (Task 8.5 R36)"
```

---

### Task 4: Rungs close in

**Files:**
- Modify: `crates/apex-runtime/src/armed/threshold.rs:34` (`RUNG_OFFSETS`) and its doc comment
- Test: the existing threshold tests (`crates/apex-runtime/tests/armed_*.rs` that pin offsets)

**Interfaces:**
- Produces: `pub const RUNG_OFFSETS: [u32; 6] = [0, 1, 2, 4, 8, 16];`

- [ ] **Step 1: Change the test that pins R26's offsets to expect `[0, 1, 2, 4, 8, 16]`** (find it with `grep -rn "RUNG_OFFSETS\|50, 100, 200" crates/apex-runtime/tests`). Add a test `rungs_step_one_two_four_eight_sixteen_past_the_threshold`: on a fixture cycle whose threshold is step `h`, `rungs(…, 6)` returns steps `h, h+1, h+2, h+4, h+8, h+16`.
- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_engine` — Expected: FAIL on the step values.
- [ ] **Step 3: Change `RUNG_OFFSETS`.**
- [ ] **Step 4: Run them to verify they pass** — Expected: `test result: ok`.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): rungs one to sixteen steps past the threshold (Task 8.5 R36)"`

---

### Task 5: The bid, per gas, and fires that pay their tip

**Files:**
- Create: `crates/apex-runtime/src/armed/bid.rs`
- Modify: `crates/apex-runtime/src/armed/lines.rs` (`Arm`), `crates/apex-runtime/src/armed/engine.rs` (`EngineSettings`, `Engine::new`, ranking, `arm`), `crates/apex-runtime/src/armed/threshold.rs` (`band`'s hurdle), `crates/apex-runtime/src/shadow/config.rs`
- Test: `crates/apex-runtime/tests/armed_bid.rs`

**Interfaces:**
- Produces: `pub struct Bid { pub per_gas: u128, pub p_win: f64 }`.
- Produces: `pub fn best_bid(sorted_bids: &[u128], net_wei: U256, gas: u64, cap_wei_per_gas: u128) -> Bid`. It is the `b` maximising `F(b) × (net − b × gas)` over `b = 0` and every measured bid + 1 wei up to the cap, with `F(b)` the share of measured bids strictly below `b`. The tip is paid only on a win. A tie keeps the lower bid; `Bid { per_gas: 0, p_win: F(0) }` when nothing beats it. The comparison is integer: `below × (net − b × gas)` over the same `n`.
- Produces: `pub fn rank_value(p_trigger: f64, bid: &Bid, surplus_wei: U256, gas: u64) -> f64` = `p_trigger × p_win × (surplus − per_gas × gas)`.
- `Arm` gains `bid_wei_per_gas: String` and `p_win: f64`.
- `EngineSettings` gains `max_priority_wei_per_gas: u128`. `Engine::new` gains `bids: Arc<dyn Fn(Address, Address) -> Vec<u128> + Send + Sync>` (from `Targets::bids_for`; empty without targets).
- Each rung's band is computed with `Pricing { margin_wei: config margin + bid.per_gas × gas_expected, .. }`, so both corners clear the hurdle plus the arm's own tip.
- Produces: config `armed.max_priority_wei_per_gas` (default `20000000000`).

- [ ] **Step 1: Write the failing tests**

```rust
const BIDS: [u128; 4] = [0, 1_000_000, 10_000_000, 50_000_000];
#[test]
fn the_bid_maximises_the_chance_times_what_is_left() {
    // net 0.00005 ETH, 500k gas: b = 10_000_001 keeps 0.75 × (5e13 − 5e12) = 3.375e13, beating
    // 50_000_001 (1.0 × 2.5e13), 1_000_001 (0.5 × 4.95e13) and 1 (0.25 × 5e13).
    let b = best_bid(&BIDS, U256::from(50_000_000_000_000u64), 500_000, u128::MAX);
    assert_eq!((b.per_gas, b.p_win), (10_000_001, 0.75));
}
#[test]
fn the_cap_bounds_the_bid() { assert!(best_bid(&BIDS, U256::from(10u64.pow(17)), 500_000, 2_000_000).per_gas <= 2_000_000); }
#[test]
fn nothing_measured_bids_nothing() { assert_eq!(best_bid(&[], U256::from(10u64.pow(15)), 500_000, u128::MAX).per_gas, 0); }
#[test]
fn a_thin_net_bids_nothing() { assert_eq!(best_bid(&BIDS, U256::from(1_000u64), 500_000, u128::MAX).per_gas, 0); }
#[test]
fn more_gas_never_buys_a_higher_bid_for_the_same_net() // the same net at 300k and 600k gas: per_gas(600k) <= per_gas(300k)
#[test]
fn the_ranking_counts_the_chance_of_winning() // equal P_trigger and surplus; bids giving p_win 0.25 and 0.75: the 0.75 rung ranks first
#[test]
fn every_band_clears_the_hurdle_plus_its_tip() // armed_engine fixture: at both band corners the rung's plan surplus >= margin + per_gas × gas_expected
#[test]
fn every_arm_carries_its_bid() // Arm.bid_wei_per_gas == best_bid(route bids, surplus, gas_expected, cap).per_gas
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_bid` — Expected: FAIL (unresolved `apex_runtime::armed::bid`).
- [ ] **Step 3: Implement `bid.rs`.** In `Engine::reprice`, price each rung, take its bid from the route's bids, then compute its band at the hurdle plus that tip. In `tick`, rank with `rank_value`. In `Engine::arm`, write the bid onto the arm.
- [ ] **Step 4: Run them to verify they pass, with the engine suites** — Run: `cargo test -j 3 -p apex-runtime --test armed_bid --test armed_engine --test armed_predicate` — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): bid per gas against the winners; fires clear their own tip (Task 8.5 R36)"`

---

### Task 6: The validity submitter

**Files:**
- Create: `crates/apex-chain/src/base/validity.rs`; export from `crates/apex-chain/src/base/mod.rs`
- Modify: `crates/apex-chain/src/base/send.rs` (factor the answer reading out of `HttpSubmitter::send_raw`)
- Test: `crates/apex-chain/tests/validity_send.rs`

**Interfaces:**
- Produces: `pub fn validity_request(id: u64, raw: &[u8], validity: &serde_json::Value) -> serde_json::Value` returning `{"jsonrpc":"2.0","id":id,"method":"base_sendRawTransactionValidity","params":["0x…",{"validity": validity}]}`.
- Produces: `pub fn read_answer(raw: &[u8], status: u16, body: Option<serde_json::Value>) -> SendAnswer`, the same rules `HttpSubmitter` applies today: 4xx Refused, other non-2xx Silent, a JSON-RPC error Refused with `"{code}: {message}"`, our hash Accepted, another hash Silent.
- Produces: `pub struct ValiditySubmitter` with `new(url: &str, timeout: Duration) -> Result<Self, String>` and `async fn send(&self, raw: &[u8], validity: &serde_json::Value) -> SendAnswer`. Its `Debug` shows only the endpoint label.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_request_is_base_s_method_with_the_predicates_beside_the_transaction() // golden JSON for raw = [0x02, 0xab], validity = [{"type":"block_number","params":{"op":"<=","value":"0x10"}}]
#[test]
fn answers_read_as_the_raw_sender_reads_them() // read_answer: (200, result = keccak(raw)) Accepted; (200, error -32000 "replacement transaction underpriced") Refused("-32000: replacement transaction underpriced"); (503, None) Silent; (400, None) Refused
#[test]
fn the_raw_sender_still_reads_answers_the_same_way() // HttpSubmitter's existing behaviour through read_answer (the send_fallback suite stays green)
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-chain --test validity_send` — Expected: FAIL (unresolved import).
- [ ] **Step 3: Implement `validity.rs` and move `HttpSubmitter`'s answer reading into `read_answer`.**
- [ ] **Step 4: Run them to verify they pass** — Run: `cargo test -j 3 -p apex-chain --test validity_send --test send_fallback` — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(chain): base_sendRawTransactionValidity over HTTP (Task 8.5 R36)"`

---

### Task 7: The senders' keys

**Files:**
- Create: `crates/apex-runtime/src/armed/keys.rs`
- Modify: `crates/apex-runtime/src/main.rs` (`apex keys new`), `crates/apex-runtime/src/armed/mod.rs`
- Test: `crates/apex-runtime/tests/armed_keys.rs`

**Interfaces:**
- Produces: `pub fn generate(count: usize, path: &Path) -> Result<Vec<Address>, String>`. It writes one `0x`-hex secret per line, creating the file with mode `0o400` (`OpenOptions::mode`), refuses an existing path, and returns the addresses.
- Produces: `pub fn load(path: &Path) -> Result<Vec<LaneKey>, String>`. It refuses a file whose mode has any group or other bit (`mode & 0o077 != 0`), refuses an empty file, and the error never contains a line of the file.
- Produces: `apex keys new --count N --out PATH`, which prints one address per line and nothing else.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn generated_keys_load_back_to_the_same_addresses() // tempdir; generate(3) == load().map(address)
#[test]
fn a_file_others_can_read_is_refused() // chmod 0o440 → Err containing "readable"
#[test]
fn an_existing_file_is_never_overwritten() // generate twice → second Err
#[test]
fn no_error_shows_the_secret() // corrupt line → Err does not contain the line's text
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_keys` — Expected: FAIL (unresolved).
- [ ] **Step 3: Implement `keys.rs` (each key `ethers_signers::LocalWallet::new(&mut ethers_core::rand::thread_rng())`, written as `0x` + hex of `wallet.signer().to_bytes()`; loaded with `LaneKey::from_hex`) and the subcommand.**
- [ ] **Step 4: Run them to verify they pass** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): the senders' keys file, 0400, addresses only (Task 8.5 R36)"`

---

### Task 8: The live ledger: write-ahead, reserved exposure, the halt

**Files:**
- Create: `crates/apex-runtime/src/armed/live_ledger.rs`
- Test: `crates/apex-runtime/tests/armed_live_ledger.rs`

**Interfaces:**
- Produces: `pub enum LiveRecord` (serde `tag = "kind"`, snake_case; wei as decimal strings):
  - `Submitting { arm_id: u64, pools: [Address; 2], sender: Address, nonce: u64, tx: B256, to_block: u64, gas_limit: u64, max_fee_per_gas: u128, max_priority_fee_per_gas: u128, worst_case_wei: u128, at_ms: u64 }`, written and synced **before** the send;
  - `Answered { tx: B256, answer: String, accepted: Option<bool>, at_ms: u64 }`, where `accepted` is `None` for a `Silent` answer;
  - `Landed { arm_id: u64, pools: [Address; 2], sender: Address, nonce: u64, tx: B256, gross_wei: u128, paid_wei: u128, cost_wei: u128, net_wei: i128, at_ms: u64 }`;
  - `Expired { sender: Address, nonce: u64, at_ms: u64 }`: every submission at that nonce passed its bound and the nonce did not move;
  - `Skipped { arm_id: u64, sender: Address, reason: String, at_ms: u64 }`;
  - `Halted { reason: Halt, at_ms: u64 }` and `Reset { at_ms: u64 }`.
- Produces: `pub enum Halt { LossCap { day: String }, Tripwire { sender: Address, detail: String } }`.
- Produces: `pub struct PendingTx { pub arm_id: u64, pub pools: [Address; 2], pub tx: B256, pub to_block: u64, pub worst_case_wei: u128, pub accepted: Option<bool> }`.
- Produces: `pub struct LiveState { pub pending: BTreeMap<(Address, u64), Vec<PendingTx>>, pub day: String, pub day_net: i128, pub reserved_wei: u128, pub halted: Option<Halt> }`, from `LiveState::from_records(records: &[LiveRecord], now_ms: u64, budget_wei: u128) -> Self`.
  - A submission is pending until its `(sender, nonce)` is `Landed` or `Expired`.
  - A `Refused` answer (`accepted == Some(false)`) makes that one submission terminal.
  - `reserved_wei` is the sum of `worst_case_wei` over pending submissions, keeping only the largest per `(sender, nonce)`, because at most one transaction lands per nonce.
  - `day_net` is the UTC day's `Landed` net.
- Produces: `LiveState::admits(&self, worst_case_wei: u128, budget_wei: u128) -> bool` = `max(0, −day_net) + reserved + worst_case ≤ budget`.
- Produces: `pub struct LiveLedger` with `open(path) -> io::Result<(Self, Vec<LiveRecord>)>` and `append(&LiveRecord) -> io::Result<()>`. `append` calls `sync_data` for `Submitting`, `Landed`, `Expired` and `Halted`. `open` skips a torn last line and keeps every whole line before it.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn reservations_sum_the_worst_case_once_per_nonce() // two submissions at one (sender, nonce) of 3 and 5 units, one elsewhere of 4 → reserved 9
#[test]
fn a_refusal_releases_and_a_silence_does_not() // Answered{accepted: Some(false)} drops it; Answered{accepted: None} keeps it reserved
#[test]
fn a_landing_or_an_expiry_ends_the_nonce() // after Landed or Expired at (s, 7): nothing pending at (s, 7)
#[test]
fn the_budget_counts_losses_and_reservations_together() // day_net −4, reserved 3, budget 10: admits(3) true, admits(4) false
#[test]
fn the_cap_halts_until_the_next_utc_day_and_survives_a_restart() // landings net −0.011 ETH on 10-25, budget 0.01: halted at 23:59; reopened from the file, still halted; None at 00:00 10-26
#[test]
fn a_tripwire_holds_until_a_reset()
#[test]
fn a_torn_last_line_loses_only_itself() // file ending mid-record: open keeps the whole records before it
#[test]
fn records_round_trip()
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_live_ledger` — Expected: FAIL (unresolved).
- [ ] **Step 3: Implement `live_ledger.rs`** (modelled on `canary::ledger`).
- [ ] **Step 4: Run them to verify they pass** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): the live ledger: write-ahead, reserved exposure, the halt (Task 8.5 R36)"`

---

### Task 9: The live worker: submission

**Files:**
- Create: `crates/apex-runtime/src/armed/live.rs`
- Test: `crates/apex-runtime/tests/armed_live.rs`

**Interfaces:**
- Consumes: Task 5's `Arm.bid_wei_per_gas`; Task 6's `ValiditySubmitter`; Task 7's `LaneKey`s; Task 8's `LiveLedger`, `LiveState`; `sign::Eip1559Tx`; `armed::predicate::expiry`.
- Produces: `#[async_trait] pub trait ValiditySend: Send + Sync { async fn send(&self, raw: &[u8], validity: &serde_json::Value) -> SendAnswer; }`, implemented for `ValiditySubmitter`.
- Produces: `#[async_trait] pub trait LiveChain: Send + Sync { async fn head(&self) -> Result<u64, String>; async fn nonce(&self, a: Address) -> Result<u64, String>; async fn balance(&self, a: Address) -> Result<u128, String>; async fn receipt(&self, tx: B256) -> Result<Option<serde_json::Value>, String>; }`.
- Produces: `pub struct LivePolicy { pub budget_wei: u128, pub max_priority_wei_per_gas: u128, pub max_size_wei: U256, pub l1_fee_bound_wei: u128, pub sender_reserve_wei: u128, pub window_blocks: u64, pub kill_file: PathBuf }`.
- Produces: `pub struct ArmedLive` with `new(keys: Vec<LaneKey>, chain: Arc<dyn LiveChain>, submit: Arc<dyn ValiditySend>, ledger: LiveLedger, state: LiveState, policy: LivePolicy, executor: Address, recipient: Address, chain_id: u64)` and `async fn on_lines(&self, lines: &[ArmedLine], base_fee_wei: u128, now_ms: u64)`. It sends nothing until Task 10's `ready()` is true.
- Behaviour, in order, for each `Arm` and `Renew`:
  - nothing while not ready, while the kill file exists, or while `state.halted` is `Some`;
  - `Skipped{"size"}` when `size_wei > max_size_wei`;
  - it reads a fresh head and sets `to_block = head + window_blocks`. The predicates are the arm's, with the block bound replaced by `predicate::expiry(to_block)`;
  - it computes `max_priority = min(bid, cap)`, `max_fee = 2 × base_fee + max_priority`, and `worst_case = gas_limit × max_fee + l1_fee_bound_wei`;
  - `Skipped{"balance"}` when the sender's balance (refreshed after any landing, else at most every 60 s) is below `worst_case + sender_reserve_wei`;
  - `Skipped{"budget"}` when `!state.admits(worst_case, budget)`;
  - otherwise it signs `Eip1559Tx { chain_id, nonce: next_nonce[sender], max_priority, max_fee, gas_limit, to: executor, data }`, appends `Submitting` (synced), sends, then appends `Answered`;
  - `Accepted` means pending; `Silent` means pending and possibly sent; `Refused` ends that submission only;
  - an answer containing "underpriced" re-signs from the last submission's fees × 1125 / 1000, rounded up, at most twice. A bump above a cap is not signed, and the earlier transaction stays tracked;
  - "nonce too low" calls Task 10's `landing_check(sender)` at once, with no resend.

- [ ] **Step 1: Write the failing tests** (scripted `ValiditySend` and `LiveChain`, a tempdir ledger)

```rust
#[tokio::test] async fn nothing_is_sent_before_ready()
#[tokio::test] async fn an_arm_is_signed_from_its_slot_s_sender_with_a_bound_from_the_fresh_head() // head 100, window 5: last predicate == expiry(105), nonce 7 from keys[slot]
#[tokio::test] async fn the_submission_is_written_before_it_is_sent() // the scripted sender reads the ledger file and finds the Submitting record
#[tokio::test] async fn concurrent_worst_cases_never_exceed_the_budget() // 16 arms, worst case 0.003 ETH each, budget 0.01 ETH: exactly 3 sent, 13 Skipped{"budget"}
#[tokio::test] async fn a_silent_answer_is_a_possible_send() // Silent → still reserved; the next arm's admission counts it
#[tokio::test] async fn a_refused_replacement_keeps_the_earlier_one() // Accepted A at nonce 7, then two Refused("…underpriced…") bumps: A still pending, fees ×1.125 then ×1.265625 (integer, rounded up)
#[tokio::test] async fn no_bump_goes_over_the_cap() // cap 1_100_000, first fee 1_000_000: the 1_125_000 bump is not signed
#[tokio::test] async fn a_used_nonce_is_a_landing_to_find_not_a_retry() // Refused("nonce too low") → landing_check called, no resend
#[tokio::test] async fn a_sender_that_cannot_pay_its_worst_case_is_skipped() // balance 0.0006 ETH, worst case 0.0007 ETH → Skipped{"balance"}, nothing signed
#[tokio::test] async fn the_kill_file_and_a_halt_stop_new_submissions()
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_live` — Expected: FAIL (unresolved).
- [ ] **Step 3: Implement the submission half of `live.rs`**, with `ready()` and `landing_check` as stubs Task 10 fills (`ready()` true in this task's tests via a test constructor).
- [ ] **Step 4: Run them to verify they pass** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): the live worker reserves, signs, submits and renews (Task 8.5 R36)"`

---

### Task 10: The live worker: reconciliation, landings, the guards

**Files:**
- Modify: `crates/apex-runtime/src/armed/live.rs`
- Test: `crates/apex-runtime/tests/armed_live.rs`

**Interfaces:**
- Consumes: Task 9's `ArmedLive`, `LiveChain`; Task 8's `LiveState`; `canary::chain::read_receipt(v, WETH, executor, &[recipient])`.
- Produces: `ArmedLive::on_head(&self, head: u64, head_seen_ms: u64, now_ms: u64)`, `ArmedLive::landing_check(&self, sender: Address, now_ms: u64)`, and `ArmedLive::ready(&self) -> bool`.
- **Boot reconciliation.** `ready()` stays false until both hold:
  - every `(sender, nonce)` pending in the reopened ledger is settled by `landing_check`, once the head is past its largest `to_block`;
  - every sender's current nonce has been read and matches the ledger's view, or the tripwire has fired.
- **`landing_check(sender)`.** For each pending nonce of the sender, read the nonce.
  - If it advanced by one, the landing is whichever submitted hash at that nonce has a receipt. `read_receipt` gives `paid_wei`, the realised gross and the cost (`gasUsed × effectiveGasPrice + l1Fee`), and `net = paid − cost`. Record `Landed` (synced) and advance `next_nonce`. If `day_net ≤ −budget`, append `Halted{LossCap}` before anything else runs.
  - If the nonce did not move and the head is past every bound at that nonce, record `Expired`.
  - If it advanced with no receipt among our hashes, or by more than one, record `Halted{Tripwire}` and log ERROR.
- **Per head.** Run `landing_check` for every sender with a pending nonce whose largest `to_block ≤ head`.
- **The cadence guard.** Keep the last 20 head-arrival intervals. Live sends are allowed only while their median is within ±50% of `block_time_ms`. The report shows the measured and configured cadence.

- [ ] **Step 1: Write the failing tests** (each reconciliation case is a ledger prefix, one per crash boundary)

```rust
#[tokio::test] async fn boot_settles_every_pending_submission_before_any_send() // ledger: Submitting at nonce 7 (no Answered); head past its bound; nonce 8; receipt for its hash → Landed recorded, then ready()
#[tokio::test] async fn a_submission_that_expired_unseen_is_expired_not_lost() // Submitting + Answered{accepted: Some(true)}; nonce still 7 after the bound → Expired, reservation released
#[tokio::test] async fn a_landing_is_accounted_from_its_receipt() // nonce 7→8, a receipt with Profit and a WETH transfer to the recipient → Landed{net = paid − cost, pools}
#[tokio::test] async fn a_nonce_that_moved_without_our_transaction_trips_everything() // nonce 7→9, or 7→8 with no receipt among our hashes → Halted{Tripwire}; nothing sent after
#[tokio::test] async fn the_loss_cap_halts_before_the_next_send() // Landed net −0.011 ETH, budget 0.01 ETH → Halted{LossCap} written; the following on_lines signs nothing
#[tokio::test] async fn a_cadence_off_the_configuration_stops_sends() // block_time_ms 2000, heads every 200 ms → no sends; back to 2 s → sends resume
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_live` — Expected: the six new tests FAIL.
- [ ] **Step 3: Implement reconciliation, `landing_check`, `on_head` and the cadence guard.**
- [ ] **Step 4: Run them to verify they pass, with Task 9's** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): restart reconciliation, landings, the loss cap and the guards (Task 8.5 R36)"`

---

### Task 11: Wiring, boot checks and the report

**Files:**
- Modify: `crates/apex-runtime/src/shadow/config.rs` (`ArmedConfig`, `ArmedLiveConfig`), `crates/apex-runtime/src/shadow/mod.rs` (boot, `armed_loop`, a head hook, the report), `ops/canary.base.yaml`
- Test: `crates/apex-runtime/tests/shadow_parts.rs` (config), `crates/apex-runtime/tests/armed_live.rs` (boot checks)

**Interfaces:**
- **One policy.** Every cap lives in `armed:` and is read by both the engine and the live worker. `armed.live:` holds only where things are (`deny_unknown_fields`): `pub struct ArmedLiveConfig { keys_file: PathBuf, sequencer: String, ledger: PathBuf, kill_file: PathBuf }`.
- `ArmedConfig` gains, all integer wei: `daily_loss_budget_wei: u128`, `max_priority_wei_per_gas: u128`, `max_size_wei: u128`, `l1_fee_bound_wei: u128`, `sender_reserve_wei: u128`. It refuses zero or out-of-range values.
- `mode: live` refuses to boot when any of these holds:
  - `live` is absent;
  - the keys file's key count ≠ `max_arms`;
  - any key reads `executors(key) == false` on the executor;
  - targets are not loaded or not `ok`;
  - no profit recipient is configured.
  `ArmedLive`'s `recipient` is `canary.profit_recipient`, the address the executor pays.
- Produces: `pub fn executors_call(executor: Address, account: Address) -> serde_json::Value` (the `eth_call` request), and the boot check using it.
- Produces: the report's `armed.live: Option<LiveReport>`, where `LiveReport` carries:
  - the counts: `submitted`, `accepted`, `refused`, `silent`, `skipped` by reason, `landed`, `won`;
  - the money: `gross_wei`, `cost_wei`, `net_today_wei`, `reserved_wei`, `budget_wei`;
  - the state: `halted: Option<String>`, `cadence_ms_measured`, `cadence_ms_configured`, `expiry_horizon_blocks`;
  - `senders: Vec<SenderReport{address, balance_wei, nonce, worst_case_wei, days_left: Option<f64>}>`, where `days_left` is the balance over the last 24 h's spend from `Landed` costs (None with no spend).
- `ops/canary.base.yaml` `armed:` becomes:
  ```yaml
  armed:
    mode: shadow
    ledger: var/apex/armed.jsonl
    verdicts: var/apex/armed-verdicts.jsonl
    targets: var/apex/targets.json
    targets_max_age_s: 172800
    max_arms: 16
    rungs: 6
    window_s: 10
    block_time_ms: 2000            # 200 on the Denim day (ops/host/ARMED.md)
    margin_wei: 0
    band_cap_bps: 5
    liquidity_floor_bps: 9800
    max_priority_wei_per_gas: 20000000000
    daily_loss_budget_wei: 10000000000000000   # 0.01 ETH: realized loss plus everything pending
    max_size_wei: 5000000000000000000          # 5 WETH
    l1_fee_bound_wei: 50000000000000           # 0.00005 ETH
    sender_reserve_wei: 200000000000000        # 0.0002 ETH
    live:
      keys_file: /etc/apex/armed.keys
      sequencer: https://mainnet-sequencer.base.org
      ledger: var/apex/armed-live.jsonl
      kill_file: var/apex/armed.disable
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn live_mode_without_its_section_does_not_boot() // ShadowConfig::parse with mode: live and no live → Err naming "armed.live"
#[test] fn a_cap_in_the_live_section_is_refused() // armed.live.max_priority_wei_per_gas → Err (deny_unknown_fields)
#[test] fn the_executors_call_reads_the_mapping() // data[..4] == keccak256(b"executors(address)")[..4], then the account left-padded to 32 bytes
#[tokio::test] async fn a_key_the_executor_does_not_know_stops_the_boot() // scripted eth_call returning 0 → Err naming the address
#[test] fn the_report_carries_the_live_section()
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test shadow_parts --test armed_live` — Expected: FAIL.
- [ ] **Step 3: Implement the config, the boot checks, and the wiring.** `armed_loop` passes each pass's lines to `ArmedLive::on_lines` in live mode, and the head loop calls `on_head`. Add the report section and update the YAML.
- [ ] **Step 4: Run them to verify they pass, and the whole crate** — Run: `cargo test -j 3 -p apex-runtime --no-fail-fast --all-targets` — Expected: every `test result: ok`.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): live mode wired, one policy, its boot checks and its report (Task 8.5 R36)"`

---

### Task 12: The API probe

**Files:**
- Create: `crates/apex-runtime/src/validity_probe.rs`
- Modify: `crates/apex-runtime/src/main.rs`, `crates/apex-runtime/src/lib.rs`
- Test: `crates/apex-runtime/tests/validity_probe.rs`

**Interfaces:**
- Produces: `apex validity-probe --config PATH --sender I`, using key `I` of `armed.live.keys_file`, with the canary stopped (its nonce tripwire) and the armed lane in `shadow`. Every action ends in a reconciled terminal state: included (receipt), or expired (bound passed, nonce unmoved). "Accepted" alone is never a pass. One line is printed per action:
  1. A zero-value self-transfer whose predicate can never hold (`balance(sender) >= 2^128`) is not included by its bound (`head + 6`), and the nonce and balance do not move.
  2. The same with `block_number >= head` is included within 2 blocks.
  3. A never-true transaction bounded at `head + 30`, then a same-nonce transaction at the same fees. The answer is printed (accepted or underpriced), with `base_transactionStatus` of the first when the endpoint answers it. The first must still expire unincluded.
  4. After the bound, the same nonce at the same fees is accepted or not (printed).
  5. An 8-predicate transaction (2 trigger, 4 band, 1 liquidity, 1 block, all true on the probe's state) is accepted and included.
  6. Five same-nonce replacements of a never-true transaction, sent back to back with integer 12.5% bumps. Answers and latencies are printed.
  Cost: about two 21,000-gas inclusions.
- Produces: `pub fn probe_predicates(head: u64, sender: Address) -> Vec<(String, serde_json::Value)>` (the cases above, named), unit-tested as golden JSON.

- [ ] **Step 1: Write the failing test** `the_probe_s_predicates_are_the_cases` (golden JSON; the never-true case is `balance(sender) >= 0x1` followed by 32 zeros (2^128); the 8-predicate case has exactly 8 entries and one `block_number`).
- [ ] **Step 2: Run it to verify it fails** — Run: `cargo test -j 3 -p apex-runtime --test validity_probe` — Expected: FAIL.
- [ ] **Step 3: Implement the probe** (sign with `LaneKey`, send with `ValiditySubmitter`, read with the run's reads).
- [ ] **Step 4: Run it to verify it passes** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): apex validity-probe, the pilot's API probe (Task 8.5 R36)"`

---

### Task 13: Owner scripts and the operations page

**Files:**
- Create: `script/AuthorizeArmedSenders.s.sol`, `script/FundArmedSenders.s.sol`, `ops/armed-senders.example.json`, `ops/host/ARMED.md`

**Interfaces:**
- `ops/armed-senders.json` (written on the host after `apex keys new`; addresses only): `{"executor": "0xd25BD9A60113728a2ECb9FdC9392Cde8D906f2c5", "senders": ["0x…", …], "allowed": true, "amount_wei": "2000000000000000"}`.
- `AuthorizeArmedSenders.s.sol`: reads the file with `vm.parseJsonAddress`, `vm.parseJsonAddressArray` and `vm.parseJsonBool`, and calls `LeanExecutor(executor).setExecutor(s, allowed)` for each sender inside `vm.startBroadcast()`. With `allowed: false` it is the revocation.
- `FundArmedSenders.s.sol`: sends `amount_wei` to each sender.

- [ ] **Step 1: Write the scripts and the example file.**
- [ ] **Step 2: Dry-run both on a fork** (no broadcast): `forge clean && forge script script/AuthorizeArmedSenders.s.sol --fork-url "$BASE_RPC_URL" --sender <owner address>`, with the example file pointed at two throwaway addresses. Expected: the simulation lists 2 `setExecutor` calls and `executors(a) == true` afterwards. Ignore the 21 "failed to parse" warnings (known).
- [ ] **Step 3: Write `ops/host/ARMED.md`:** generating keys (`sudo -u apex /opt/apex/bin/apex keys new --count 16 --out /etc/apex/armed.keys`, then the addresses into `ops/armed-senders.json`); the operator's two broadcasts (authorize, fund) and the revocation; the kill file; the probe; reading the report's `armed.live`; resetting a halt (append `{"kind":"reset","at_ms":…}`); the daily census timer.
- [ ] **Step 4: Commit** — `git commit -m "feat(ops): armed senders' authorization, funding and operations (Task 8.5 R36)"`

---

### Task 14: The Denim switch

**Files:**
- Modify: `crates/apex-runtime/src/shadow/config.rs` (`FeedConfig { pending_logs: bool }` defaulting to `true`; `flashblocks: Option<FlashblocksConfig>`), `crates/apex-runtime/src/shadow/mod.rs:779` (the subscription list; the flashblock stream only when configured)
- Test: `crates/apex-runtime/tests/shadow_parts.rs`

**Interfaces:**
- Produces: `pub fn subscriptions(feed: &FeedConfig, filter: LogFilter) -> Vec<Subscription>`, giving `[NewHeads, PendingLogs(f), Logs(f)]` when `pending_logs`, else `[NewHeads, Logs(f)]`.

- [ ] **Step 1: Write the failing tests** `after_denim_the_feed_is_heads_and_canonical_logs` and `without_a_flashblocks_section_no_stream_is_opened` (config parse with no `flashblocks:` → `None`).
- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test shadow_parts` — Expected: FAIL.
- [ ] **Step 3: Implement; document in `ops/host/ARMED.md` the Denim day's edits: `feed.pending_logs: false`, `flashblocks` removed, `armed.block_time_ms: 200`.**
- [ ] **Step 4: Run them to verify they pass** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(runtime): the feed's Denim switch, by config (Task 8.5 R36)"`

---

### Task 15: Ship: gates, gate A′, the pilot

No code. Every step that touches the host or mainnet waits for the operator's approval at the time.

- [ ] **Step 1: All five gates on the branch** (`gates.sh`): expected red only `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh`; also `cd scripts/data && python3 -m unittest discover -s arb_census -t . -p "test_*.py"`. Verify the head commit in a clean worktree with its own `CARGO_TARGET_DIR`.
- [ ] **Step 2: PLAN.md's R36 entry; ask the operator to merge, push and deploy.**
- [ ] **Step 3: Deploy in `mode: shadow`.** On the host:
  - move R34's census files into `/opt/apex/var/census` (race days, `meta.json`, R33's gaps file);
  - run the targets job once by hand and check `targets.json` has `ok: true`, 50 routes and a calibration;
  - install and enable the timer;
  - deploy the revision and check the boot logs: `targets_loaded: true`, the targeted routes and missing pools, `unarmable` pools.
- [ ] **Step 4: Gate A′ after ≥ 1 day.** Report the numbers to the operator:
  - `apex armed-eval`: quote and simulation agree on ≥ 95% of checked fires, also weighted by value and listed by route, with the reasons for every disagreement;
  - no armed pool in `unarmable`;
  - request units under ~124M/day.
- [ ] **Step 5: The pilot's setup, each with the operator's approval.**
  - Generate the 16 keys on the host and write `ops/armed-senders.json`;
  - dry-run both owner scripts against mainnet state;
  - the operator broadcasts the authorization and the funding;
  - stop the canary, run `apex validity-probe --sender 0`, and report every line;
  - restart.
- [ ] **Step 6: `mode: live`** once the probe passes and the operator approves. Watch `armed.live` and the daily `armed-share.json` (net by day and route, our share of the market). Gate B: 7 days **and** at least 30 landings, realized net positive after every fee, included-revert fees < 20% of gross, zero tripwires.

