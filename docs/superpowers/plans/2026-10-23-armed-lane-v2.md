# Armed lane v2 Implementation Plan (Task 8.5 R36, phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Arm Cobalt validity transactions on the 50 routes where trigger-block arbitrage that our executor can run actually happens, fire them on ordinary moves, bid against what the winners pay, and run them live from 16 senders under a US$25/day loss cap.

**Architecture:** A daily census on the host (Python, `arb_census`) writes `targets.json`: the top routes and the winners' bid shares. The runtime admits those routes' pools to the book, arms only their cycles with R26's engine (rungs moved close in, a bid per arm), and in `mode: live` signs each arm from its own sender and submits it with `base_sendRawTransactionValidity`. A live ledger accounts every landing and enforces the caps; the feed can drop `pendingLogs` by config for Denim.

**Tech Stack:** Rust (apex-runtime, apex-chain; ethers-signers, alloy-primitives, serde, tokio), Python 3 stdlib (census), Foundry (owner scripts).

**Spec:** `docs/superpowers/specs/2026-10-23-armed-lane-v2-design.md` (approved 2026-10-23). R26's spec `docs/superpowers/specs/2026-10-13-armed-backruns-design.md` §4–6 defines the pilot pieces this plan builds; read both.

## Global Constraints

- Phase 1 only: two-hop, WETH-start cycles on our five venues (`UniswapV3`, `Slipstream`, `PancakeV3`, `SlipstreamV3`, `AerodromeV2`). No v4, no USDC start, no 3-hop.
- Targets: top **K = 50** routes by trailing **7-day** trigger-block value.
- Senders: **N = 16** (`max_arms: 16`), funded **0.002 ETH** each; a sender below **0.0005 ETH** is skipped and reported.
- Daily loss cap: **US$25**, configured as `daily_loss_cap_wei: 10000000000000000` (0.01 ETH at ~$2,450; ruling: the runtime prices in wei, as the canary's `loss_cap_wei`).
- Window **W = 10 s** as a `block_number` bound from `block_time_ms` (2000 now, 200 at Denim). **No `flashblock_index` predicates.**
- Rung offsets: **`[0, 1, 2, 4, 8, 16]`** steps past the threshold.
- Band cap 5 bps, liquidity floor 9800 bps (R26's, unchanged).
- Submit only to `https://mainnet-sequencer.base.org`, never BlockPI. Predicates on the wire as `{"type": …, "params": {…}}`.
- Replacement on an "underpriced" answer: both fees ×1.125, at most twice, then the slot is freed.
- Bid: share `f` maximising `F(f) × (1 − f)`, `F` the measured winners' priority shares of value; per-gas bid `f × surplus / gas_expected`, capped by `max_priority_wei_per_gas` and `max_bid_fraction`.
- Gate A′ (shadow, ≥ 1 day): quote and simulation agree on ≥ 95% of checked fires; every armed pool's storage words match; the API probe passed; request units under today's ~124M/day.
- Gate B (live, 7 days): realized net positive after every fee; included-revert fees < 20% of gross; zero tripwires.
- Secrets never printed, logged or on argv; keys file `/etc/apex/armed.keys`, owner `apex`, mode `0400`, refused if group- or world-readable; only addresses printed.
- Owner transactions are prepared and dry-run only; the operator broadcasts. Merges, pushes, deploys and every mainnet transaction need the operator's approval at the time.
- No new non-secret environment variables (config fields and files are fine). Python `urllib` calls carry a `User-Agent`. Never `cargo fmt`; build with `-j 3`; run the five gates (`gates.sh`) before pushing, known-red: `check_placeholder_endpoints.sh`, `cl_parity_sweep.sh`; verify each commit in a clean worktree.
- `ops/host/README.md` carries an uncommitted edit that is not ours: do not stage it; the pilot's operations go in a new `ops/host/ARMED.md`.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A restart with arms still pending at the sequencer.** Live submission must not start until one window after boot, and nonces are read then; otherwise a new arm collides with a pending one at the same nonce. Pinned in Task 9 (`no_submission_before_one_window_after_boot`).
2. **A renewal whose nonce has already been used** (the previous arm landed between the head and the renewal): the "nonce too low" answer must trigger the landing check, not a retry loop. Pinned in Task 9 (`a_used_nonce_is_a_landing_to_find_not_a_retry`), the check itself in Task 10.
3. **A missing, stale or corrupt `targets.json` at boot:** in `live` mode a boot error; in `shadow` mode the engine arms R26's universe and the report says so. Pinned in Task 3 (`targets_absent_is_fatal_live_and_falls_back_in_shadow`).
4. **A sender's balance falling below 0.0005 ETH mid-day:** skipped for new arms, reported, never signed for. Pinned in Task 9 (`a_sender_below_the_floor_is_skipped_and_reported`).
5. **The loss cap crossed by an included revert:** everything disarms at once, no submission until 00:00 UTC, and the halt survives a restart. Pinned in Task 8 (`the_cap_halts_until_the_next_utc_day_and_survives_a_restart`).

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
- Consumes: Task 1's `pools`, `weth_in`, `gain`; R34's `offset`, `usd`, `prio`, `gas`, and each day's `eth_usd`.
- Produces: `targets.json`:
  ```json
  {"generated_at": "2026-10-24T01:10:00Z", "days": ["2026-10-17", "…"], "top": 50,
   "routes": [{"pools": ["0x<weth-in pool>", "0x<other pool>"], "usd": 123.4, "arbs": 7, "shares": [0.12, …]}],
   "shares": [0.0, 0.03, …]}
  ```
  `routes` sorted by `usd` descending; a route's own `shares` only when it has ≥ 20; top-level `shares` sorted ascending over every selected arbitrage with `prio`.
- Produces: `targets.ours_vs_market(days, landings) -> list[dict]`: per route, the census's trigger-block value against our `Landed` records (Task 8's `armed-live.jsonl` lines, `kind == "landed"`, matched on `pools`) over the same days: `{"pools", "market_usd", "ours_landed", "ours_net_wei"}`. `main` writes it to `--share-out` when given.
- Produces: `targets.select(arb) -> bool` (two hops, every venue in `race.OURS`, `gain == [WETH]`, `weth_in` set, `offset == 0`), `targets.share(arb, eth_usd) -> float` (`prio × gas / 1e18 × eth_usd / usd`), `targets.build(days, top) -> dict`.

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
    def test_routes_ranked_by_value_weth_in_pool_first_cut_at_top(self):
        day = {"eth_usd": 2500.0, "arbs": [arb(usd=5.0), arb(pools=("pb", "pa"), weth_in="pa", usd=4.0), arb(pools=("pc", "pd"), weth_in="pd", usd=20.0)]}
        out = T.build([day], top=1)
        self.assertEqual([(r["pools"], r["usd"], r["arbs"]) for r in out["routes"]], [(["pd", "pc"], 20.0, 1)])

    def test_the_share_is_the_winners_priority_over_the_value(self):
        self.assertAlmostEqual(T.share(arb(usd=10.0, prio=10**9, gas=400_000), 2500.0), 10**9 * 400_000 / 1e18 * 2500 / 10)

    def test_a_route_carries_its_own_shares_only_from_twenty(self):
        rows = [arb(prio=10**8, gas=300_000) for _ in range(19)]
        out = T.build([{"eth_usd": 2500.0, "arbs": rows}], top=50)
        self.assertNotIn("shares", out["routes"][0])
        self.assertEqual(len(out["shares"]), 19)

    def test_our_landings_are_set_against_the_market_per_route(self):
        day = {"eth_usd": 2500.0, "arbs": [arb(usd=10.0), arb(usd=5.0)]}
        landed = [{"kind": "landed", "pools": ["pa", "pb"], "net_wei": "1000"}]
        self.assertEqual(T.ours_vs_market([day], landed), [{"pools": ["pa", "pb"], "market_usd": 15.0, "ours_landed": 1, "ours_net_wei": 1000}])

    def test_no_days_is_an_empty_file_not_a_crash(self):
        self.assertEqual(T.build([], top=50)["routes"], [])
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd scripts/data && python3 -m unittest arb_census.test_targets`
Expected: FAIL with `ModuleNotFoundError: No module named 'arb_census.targets'`

- [ ] **Step 3: Implement `targets.py`:** `select`, `share`, `build`, and `main()` with `--race DIR --days 7 --top 50 --out PATH --var /opt/apex/var/apex --meta PATH --gaps PATH`. `main` first runs `race.race_day` for each of the trailing days whose `race-*.json.gz` is missing and whose `flashblocks-*.jsonl` exists, or for any day after Denim (no ledger). It then writes the file atomically: write `PATH.tmp`, `os.replace`.

- [ ] **Step 4: Run them to verify they pass**

Run: `cd scripts/data && python3 -m unittest arb_census.test_targets arb_census.test_race`
Expected: OK

- [ ] **Step 5: Write the systemd units.** `apex-targets.service`: `Type=oneshot`, runs `runuser -u apex -- python3 -m arb_census.targets --race /opt/apex/var/census --days 7 --top 50 --out /opt/apex/var/apex/targets.json --meta /opt/apex/var/census/meta.json --gaps /opt/apex/var/census/r33-gaps.json` from `/opt/apex/scripts/data`, `nice -n 19`, then `ExecStartPost=/bin/systemctl restart apex-canary`. `apex-targets.timer`: `OnCalendar=*-*-* 00:20:00 UTC`, `Persistent=true`.

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
- Produces: `pub struct TargetRoute { pub pools: [Address; 2], pub usd: f64, pub arbs: u32, pub shares: Option<Vec<f64>> }`; `pub struct Targets { pub generated_at: String, pub routes: Vec<TargetRoute>, pub shares: Vec<f64> }`.
- Produces: `Targets::load(path: &Path, now_unix: u64, max_age_s: u64) -> Result<Targets, String>`, which refuses a file older than `max_age_s` (`generated_at`), shares outside `[0, ∞)`, or a route whose pools are equal. `Targets::pools(&self) -> BTreeSet<Address>`. `Targets::has(&self, weth_in: Address, other: Address) -> bool`. `Targets::shares_for(&self, weth_in: Address, other: Address) -> &[f64]` returns the route's own shares, else the top-level ones.
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
fn a_route_without_twenty_shares_bids_on_everyone_s() // shares_for falls back to top-level
#[test]
fn a_stale_or_malformed_file_is_refused() // generated_at older than max_age_s → Err containing "stale"; equal pools → Err; negative share → Err
#[test]
fn a_target_pool_without_an_inventory_record_is_counted_not_admitted() // include holds a pool absent from the fixture: load ignores it; shadow::targets_missing(&targets, &specs) == 1
#[test]
fn targets_absent_is_fatal_live_and_falls_back_in_shadow() // shadow::armed_cycles(config with mode Live, targets: None) → Err; Shadow → Ok(R26's cycle set) and a flag `targets_loaded: false`
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

### Task 5: The bid

**Files:**
- Create: `crates/apex-runtime/src/armed/bid.rs`
- Modify: `crates/apex-runtime/src/armed/lines.rs` (`Arm`), `crates/apex-runtime/src/armed/engine.rs` (`EngineSettings`, `Engine::new`, `arm`), `crates/apex-runtime/src/shadow/config.rs`
- Test: `crates/apex-runtime/tests/armed_bid.rs`

**Interfaces:**
- Produces: `pub fn best_fraction(sorted_shares: &[f64], max_fraction: f64) -> f64`. It returns the `f` maximising `F(f) × (1 − f)` over candidates `s + 1e-9` (each measured share just exceeded) with `f ≤ max_fraction`, where `F(f)` is the share of measurements below `f`. A tie keeps the lower; it returns 0 when nothing beats 0.
- Produces: `pub fn priority_per_gas(surplus_wei: U256, gas_expected: u64, fraction: f64, cap_wei_per_gas: u128) -> u128` = `min(cap, floor(fraction × surplus / gas))`, 0 when `gas_expected == 0` or the surplus is zero.
- Produces: `Arm.bid_wei_per_gas: String` (decimal) and `Arm.bid_fraction: f64`. `EngineSettings` gains `max_bid_fraction: f64`, `max_priority_wei_per_gas: u128`. `Engine::new` gains `shares: impl Fn(Address, Address) -> Vec<f64>` (from `Targets::shares_for`; empty in a run without targets).
- Produces: config `armed.max_bid_fraction` (default `0.5`), `armed.max_priority_wei_per_gas` (default `20000000000`, 20 gwei).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_fraction_balances_winning_against_keeping() {
    // shares 0.1, 0.2, 0.3, 0.9: candidates just above each.
    // F×(1−f): 0.25×0.9, 0.5×0.8, 0.75×0.7=0.525, 1.0×0.1 → just above 0.3.
    let f = best_fraction(&[0.1, 0.2, 0.3, 0.9], 1.0);
    assert!((f - 0.3).abs() < 1e-6 && f > 0.3);
}
#[test]
fn the_cap_bounds_the_fraction() { assert!(best_fraction(&[0.1, 0.2, 0.3, 0.9], 0.25) <= 0.25 + 1e-12); }
#[test]
fn nothing_measured_bids_nothing() { assert_eq!(best_fraction(&[], 0.5), 0.0); }
#[test]
fn the_per_gas_bid_is_the_fraction_of_the_surplus_capped() {
    assert_eq!(priority_per_gas(U256::from(1_000_000_000_000_000u64), 500_000, 0.2, u128::MAX), 400_000_000);
    assert_eq!(priority_per_gas(U256::from(1_000_000_000_000_000u64), 500_000, 0.2, 1_000), 1_000);
    assert_eq!(priority_per_gas(U256::zero(), 500_000, 0.2, u128::MAX), 0);
}
#[test]
fn the_ranking_is_net_of_the_bid() // two rungs, equal P and surplus, shares giving f 0.1 and 0.6: the f = 0.1 rung ranks first
#[test]
fn every_arm_carries_its_bid() // armed_engine fixture with shares [0.1, 0.2, 0.3, 0.9]: Arm.bid_fraction ≈ 0.3 and bid_wei_per_gas == priority_per_gas(surplus, gas_expected, f, cap)
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_bid` — Expected: FAIL (unresolved `apex_runtime::armed::bid`).
- [ ] **Step 3: Implement `bid.rs`; set `bid_fraction` and `bid_wei_per_gas` in `Engine::arm` from the route's shares and the point's `surplus` and `gas.expected`; rank by `P × surplus × (1 − f)` (the spec's "net at the rung after the expected bid") instead of `P × surplus`.**
- [ ] **Step 4: Run them to verify they pass, with the engine suites** — Run: `cargo test -j 3 -p apex-runtime --test armed_bid --test armed_engine --test armed_predicate` — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): bid each arm against the winners' measured shares (Task 8.5 R36)"`

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

### Task 8: The live ledger and its halt

**Files:**
- Create: `crates/apex-runtime/src/armed/live_ledger.rs`
- Test: `crates/apex-runtime/tests/armed_live_ledger.rs`

**Interfaces:**
- Produces: `pub enum LiveRecord` (serde `tag = "kind"`, snake_case):
  - `Submitted { arm_id: u64, sender: Address, nonce: u64, tx: B256, bid_wei_per_gas: u128, answer: String, at_ms: u64 }`;
  - `Landed { arm_id: u64, pools: [Address; 2], sender: Address, nonce: u64, tx: B256, gross_wei: u128, paid_wei: u128, cost_wei: u128, net_wei: i128, at_ms: u64 }` (`pools` WETH-in first, as in `targets.json`);
  - `Skipped { arm_id: u64, sender: Address, reason: String, at_ms: u64 }`;
  - `Halted { reason: Halt, at_ms: u64 }`;
  - `Reset { at_ms: u64 }`.
- Produces: `pub enum Halt { LossCap { day: String }, Tripwire { sender: Address, detail: String } }`.
- Produces: `pub struct LiveLedger` with `open(path) -> io::Result<(Self, Vec<LiveRecord>)>` and `append(&LiveRecord) -> io::Result<()>`, following `canary::ledger::Ledger`.
- Produces: `pub fn day_net(records: &[LiveRecord], day: &str) -> i128` (the `Landed` net of the UTC day `YYYY-MM-DD`). `pub fn halted(records: &[LiveRecord], now_ms: u64, cap_wei: u128) -> Option<Halt>`: a `Tripwire` halt holds until a `Reset`; a loss cap holds for the rest of its UTC day.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_day_s_net_counts_only_its_landings() // two landings on 2026-10-24, one on 10-23 → only the two
#[test]
fn the_cap_halts_until_the_next_utc_day_and_survives_a_restart() // landings net −0.011 ETH on 10-24 with cap 0.01 ETH: halted at 23:59 10-24; reopened from the file, still halted; None at 00:00 10-25
#[test]
fn a_tripwire_holds_until_a_reset() // Halted{Tripwire} then later records: halted; after Reset: None
#[test]
fn records_round_trip() // each variant serialises and reads back equal
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_live_ledger` — Expected: FAIL (unresolved).
- [ ] **Step 3: Implement `live_ledger.rs`.**
- [ ] **Step 4: Run them to verify they pass** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): the live ledger, the day's net and the halt (Task 8.5 R36)"`

---

### Task 9: The live worker: submission

**Files:**
- Create: `crates/apex-runtime/src/armed/live.rs`
- Test: `crates/apex-runtime/tests/armed_live.rs`

**Interfaces:**
- Consumes: Task 5's `Arm.bid_wei_per_gas`, `Arm.bid_fraction`; Task 6's `ValiditySubmitter`; Task 7's `LaneKey`s; Task 8's `LiveLedger`, `halted`; `sign::Eip1559Tx`; `armed::predicate::expiry`.
- Produces: `#[async_trait] pub trait ValiditySend: Send + Sync { async fn send(&self, raw: &[u8], validity: &serde_json::Value) -> SendAnswer; }`, implemented for `ValiditySubmitter`.
- Produces: `#[async_trait] pub trait LiveChain: Send + Sync { async fn nonce(&self, a: Address) -> Result<u64, String>; async fn balance(&self, a: Address) -> Result<u128, String>; async fn receipt(&self, tx: B256) -> Result<Option<serde_json::Value>, String>; }`.
- Produces: `pub struct LiveCaps { pub max_priority_wei_per_gas: u128, pub max_bid_fraction: f64, pub max_size_wei: U256, pub daily_loss_cap_wei: u128, pub min_balance_wei: u128, pub kill_file: PathBuf }`.
- Produces: `pub struct ArmedLive` with `new(keys: Vec<LaneKey>, chain: Arc<dyn LiveChain>, submit: Arc<dyn ValiditySend>, ledger: LiveLedger, caps: LiveCaps, executor: Address, recipient: Address, chain_id: u64, boot_ms: u64, window_ms: u64)` and `async fn on_lines(&self, lines: &[ArmedLine], head: u64, base_fee_wei: u128, now_ms: u64)`. It keeps each live arm by `arm_id`, with its sender slot, pools, predicates and the hashes signed at its nonce. It keeps `next_nonce` per sender, read once when submission starts.
- Behaviour, in order, for each `Arm` and `Renew`:
  - nothing is signed while `now_ms < boot_ms + window_ms`;
  - nothing while the kill file exists or `halted(…)` is `Some`;
  - a `Skipped` record (reason `"balance"`, `"size"` or `"bid"`) when the sender's balance (read at most every 60 s) is below `min_balance_wei`, `size_wei > max_size_wei`, or `bid_fraction > max_bid_fraction`;
  - otherwise it signs `Eip1559Tx { chain_id, nonce: next_nonce[sender], max_priority_fee_per_gas: min(bid, cap), max_fee_per_gas: 2 × base_fee + min(bid, cap), gas_limit: arm.gas_limit, to: executor, data: calldata }`;
  - it sends with the arm's predicates and records the answer. A `Renew` re-sends with its `calldata` and the predicates' block bound replaced by `predicate::expiry(to_block)`;
  - an answer containing "underpriced" means re-signing with both fees ×1.125, at most twice, then `Skipped{"underpriced"}`;
  - an answer containing "nonce too low" calls Task 10's landing check for that sender at once, with no resend.

- [ ] **Step 1: Write the failing tests** (scripted `ValiditySend` and `LiveChain`, a tempdir ledger)

```rust
#[tokio::test] async fn no_submission_before_one_window_after_boot()
#[tokio::test] async fn an_arm_is_signed_from_its_slot_s_sender_at_the_next_nonce_and_sent_with_its_predicates() // nonce 7 from keys[slot]; validity == arm.predicates
#[tokio::test] async fn a_renewal_moves_only_the_block_bound() // Renew{to_block: 120}: same nonce; predicates equal but the last == expiry(120)
#[tokio::test] async fn an_underpriced_replacement_bumps_both_fees_an_eighth_at_most_twice() // three Refused("…underpriced…") → sends at ×1, ×1.125, ×1.265625, then Skipped
#[tokio::test] async fn a_used_nonce_is_a_landing_to_find_not_a_retry() // Refused("nonce too low") → one receipt lookup, no resend
#[tokio::test] async fn a_sender_below_the_floor_is_skipped_and_reported() // balance 0.0004 ETH → Skipped{"balance"}, nothing signed
#[tokio::test] async fn the_kill_file_stops_new_submissions()
#[tokio::test] async fn a_halt_stops_new_submissions() // ledger holding Halted{Tripwire} → nothing signed
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_live` — Expected: FAIL (unresolved).
- [ ] **Step 3: Implement the submission half of `live.rs`, with `landing_check` as a stub that Task 10 fills (here it looks up the nonce's receipts and records nothing).**
- [ ] **Step 4: Run them to verify they pass** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): the live worker signs, submits and renews (Task 8.5 R36)"`

---

### Task 10: The live worker: landings, the loss cap, the tripwire

**Files:**
- Modify: `crates/apex-runtime/src/armed/live.rs`
- Test: `crates/apex-runtime/tests/armed_live.rs`

**Interfaces:**
- Consumes: Task 9's `ArmedLive`, `LiveChain`; Task 8's `day_net`; `canary::chain::read_receipt(v, WETH, executor, &[recipient])`.
- Produces: `ArmedLive::on_head(&self, head: u64, now_ms: u64)` and `ArmedLive::landing_check(&self, sender_slot: u16, now_ms: u64)`.
- Behaviour, per head: for each sender whose last arm's `to_block ≤ head`, read its nonce.
  - If it advanced by one, the landing is the one of the hashes signed at the old nonce whose receipt exists. `read_receipt` gives `paid_wei`, the realised gross and the cost (`gasUsed × effectiveGasPrice + l1Fee`), and `net = paid − cost`. It records `Landed{arm_id, pools, …}`, sets `next_nonce += 1`, and appends `Halted{LossCap}` when `day_net(today) ≤ −daily_loss_cap_wei`.
  - If it advanced with no receipt among our hashes, or by more than one, it records `Halted{Tripwire}` and logs ERROR.
  - Otherwise nothing. The nonce stays, and the next arm on that sender reuses it.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn a_landing_is_accounted_from_its_receipt() // nonce 7→8, a receipt with Profit and a WETH transfer to the recipient → Landed{net = paid − cost, pools}
#[tokio::test] async fn an_expired_arm_leaves_its_nonce_for_the_next() // nonce unchanged at to_block → no record; the next arm signs nonce 7 again
#[tokio::test] async fn a_nonce_that_moved_without_our_transaction_trips_everything() // nonce 7→9 → Halted{Tripwire}; the next arm is not signed
#[tokio::test] async fn the_loss_cap_halts_the_rest_of_the_day() // Landed net −0.011 ETH with cap 0.01 ETH → Halted{LossCap}; the next arm is not signed
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test armed_live` — Expected: the four new tests FAIL.
- [ ] **Step 3: Implement `on_head` and `landing_check`.**
- [ ] **Step 4: Run them to verify they pass, with Task 9's** — Expected: ok.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): landings accounted, the loss cap and the nonce tripwire (Task 8.5 R36)"`

---

### Task 11: Wiring, boot checks and the report

**Files:**
- Modify: `crates/apex-runtime/src/shadow/config.rs` (`ArmedConfig.live: Option<ArmedLiveConfig>`), `crates/apex-runtime/src/shadow/mod.rs` (boot, `armed_loop`, a head hook, the report), `ops/canary.base.yaml`
- Test: `crates/apex-runtime/tests/shadow_parts.rs` (config), `crates/apex-runtime/tests/armed_live.rs` (boot checks)

**Interfaces:**
- Produces: `pub struct ArmedLiveConfig { keys_file: PathBuf, sequencer: String, ledger: PathBuf, daily_loss_cap_wei: u128, max_priority_wei_per_gas: u128, max_bid_fraction: f64, max_size_weth: f64, min_balance_wei: u128, kill_file: PathBuf }` (`deny_unknown_fields`).
- `mode: live` refuses to boot when `live` is absent, when the keys file's key count ≠ `max_arms`, when any key reads `executors(key) == false` on the executor (selector of `executors(address)`), when targets are not loaded, or when no profit recipient is configured. `ArmedLive`'s `recipient` is `canary.profit_recipient`, the address the executor pays (the canary's boot already checks the executor pays there).
- Produces: `pub fn executors_call(executor: Address, account: Address) -> serde_json::Value` (the `eth_call` request), and the boot check using it.
- Produces: the report's `armed.live: Option<LiveReport>`, where `LiveReport { submitted, accepted, refused, skipped, landed, won, gross_wei, cost_wei, net_today_wei, halted: Option<String>, senders: Vec<SenderReport{address, balance_wei, nonce, days_left: Option<f64>}> }`, where `days_left` is the balance over the last 24 h's spend from `Landed` costs (None with no spend).
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
    block_time_ms: 2000
    margin_wei: 0
    band_cap_bps: 5
    liquidity_floor_bps: 9800
    max_bid_fraction: 0.5
    max_priority_wei_per_gas: 20000000000
    live:
      keys_file: /etc/apex/armed.keys
      sequencer: https://mainnet-sequencer.base.org
      ledger: var/apex/armed-live.jsonl
      daily_loss_cap_wei: 10000000000000000   # US$25 at ~$2,450/ETH
      max_priority_wei_per_gas: 20000000000
      max_bid_fraction: 0.5
      max_size_weth: 5.0
      min_balance_wei: 500000000000000        # 0.0005 ETH
      kill_file: var/apex/armed.disable
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn live_mode_without_its_section_does_not_boot() // ShadowConfig::parse with mode: live and no live → Err naming "armed.live"
#[test] fn the_executors_call_reads_the_mapping() // data[..4] == keccak256(b"executors(address)")[..4], then the account left-padded to 32 bytes
#[tokio::test] async fn a_key_the_executor_does_not_know_stops_the_boot() // scripted eth_call returning 0 → Err naming the address
#[test] fn the_report_carries_the_live_section() // serialise a LiveReport; fields present
```

- [ ] **Step 2: Run them to verify they fail** — Run: `cargo test -j 3 -p apex-runtime --test shadow_parts --test armed_live` — Expected: FAIL.
- [ ] **Step 3: Implement the config, the boot checks, and the wiring.** `armed_loop` passes each pass's lines to `ArmedLive::on_lines` in live mode, and the head loop calls `on_head`. Add the report section and update the YAML.
- [ ] **Step 4: Run them to verify they pass, and the whole crate** — Run: `cargo test -j 3 -p apex-runtime --no-fail-fast --all-targets` — Expected: every `test result: ok`.
- [ ] **Step 5: Commit** — `git commit -m "feat(armed): live mode wired, its boot checks and its report (Task 8.5 R36)"`

---

### Task 12: The API probe

**Files:**
- Create: `crates/apex-runtime/src/validity_probe.rs`
- Modify: `crates/apex-runtime/src/main.rs`, `crates/apex-runtime/src/lib.rs`
- Test: `crates/apex-runtime/tests/validity_probe.rs`

**Interfaces:**
- Produces: `apex validity-probe --config PATH --sender I`, using key `I` of `armed.live.keys_file`. It runs, in order, printing one line each: a zero-value self-transfer whose storage predicate is false (WETH's slot 0 `= 0x1`) is not included within 6 blocks and the nonce and balance do not move; the same with `block_number >= head` is included within 2 blocks; a false-predicate transaction bounded at `head + 1`, then the same nonce at the same fees after it expires (the answer is printed: accepted or underpriced); an 8-predicate transaction (2 trigger, 4 band, 1 liquidity, 1 block, all true) is accepted and included; five same-nonce replacements of a pending false-predicate transaction sent back to back with 12.5% bumps (answers and latencies printed). Cost: about two 21,000-gas inclusions.
- Produces: `pub fn probe_predicates(head: u64, weth: Address) -> [(String, serde_json::Value); 4]` (the four predicate sets above, named), unit-tested as golden JSON.

- [ ] **Step 1: Write the failing test** `the_probe_s_predicates_are_the_four_cases` (golden JSON per case; the false case reads WETH slot 0 `= 0x1`; the 8-predicate case has exactly 8 entries and one `block_number`).
- [ ] **Step 2: Run it to verify it fails** — Run: `cargo test -j 3 -p apex-runtime --test validity_probe` — Expected: FAIL.
- [ ] **Step 3: Implement the probe** (sign with `LaneKey`, send with `ValiditySubmitter`, read with the run's reads; stop the canary first because of its nonce tripwire, as `send-probe` documents).
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
  - run the targets job once by hand and check `targets.json` has 50 routes;
  - install and enable the timer;
  - deploy the revision and check the boot logs: `targets_loaded: true`, targeted routes, `unarmable` pools.
- [ ] **Step 4: Gate A′ after ≥ 1 day.**
  - `apex armed-eval`: quote and simulation agree on ≥ 95% of checked fires;
  - no armed pool in `unarmable`;
  - the report's RU counters under ~124M/day.
  Report the numbers to the operator.
- [ ] **Step 5: The pilot's setup, each with the operator's approval.**
  - Generate the 16 keys on the host and write `ops/armed-senders.json`;
  - dry-run both owner scripts against mainnet state;
  - the operator broadcasts the authorization and the funding;
  - stop the canary, run `apex validity-probe --sender 0`, and report each line;
  - restart.
- [ ] **Step 6: `mode: live`** once the probe passes and the operator approves. Watch the report's `armed.live` daily; gate B after 7 days.
