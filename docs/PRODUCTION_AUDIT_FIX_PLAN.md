# Arbot — Production Audit & Prioritised Fix Plan

**Audience:** the implementing agent. Each work item is self-contained: what, where,
why, exact fix, and acceptance criteria. Line numbers are accurate as of this audit
but **anchor by function/symbol name** — they will drift as edits land.

**Context:** self-hosted nodes exist for Base and are being prepared for Ethereum
mainnet. Base is the only production-wired chain today. Target: **$25k net/month
($833/day)**. Build compiles clean (`cargo check` exit 0, 26 dead-code warnings).

**Ground rules for the implementer:**
- Do **not** change on-chain economics/thresholds while refactoring; behaviour must be
  identical unless an item explicitly says otherwise.
- Every item lists an acceptance check. Run `cargo test` + `cargo clippy` after each
  P0/P1 item. Keep each item in its own commit.
- Never commit `.env` or secrets. Do not print private keys.

---

## Implementation status

Done (each its own commit; full suite green after each):
- **P0-1** JIT_LP_ENABLED flag parsing — `d68bcc3`
- **P0-2** unified block-out-of-range classifier — `58cd2ae`
- **P1-1** release profile + toolchain pin — `d9bbbba`
- **P1-2** concurrent load_token_decimals — `d66e58c`
- **P1-3** websocket-fed block head (+ `4332976` liveness guard from adversarial review) — `e102974`
- **P1-4** static chain_id + scan block number into sim — `bfd9494`
- **P1-5** grid retry-at-Latest — folded into **P2-2a** `4ebcb6e`
- **P2-1** config-loader + parse-helper dedup — `dc7cbde`
- **P2-2a/b** shared quote cores (CL + constant-product) — `4ebcb6e`, `fbb777c`
- **P2-3** graph cycle-economics dedup — `b5985c6`
- **P2-4** deleted dead k_best_cycles — `5f8a23f`
- **P2-8** build is now warning-free: superseded Fjord L1 model + import/lint noise
  removed (`851e782`), remaining scaffolding kept and `#[allow(dead_code)]`-annotated
  per product direction (`b7dddb5`)
- **P2-6 (hot-path slice)** per-block env tunables cached via OnceLock so the scan
  path no longer reads `std::env` each block — `0bae681`
- **P2-6 (env-helper migration)** typed helpers `env_parse_opt`/`env_u256_opt`/`env_flag`
  in util (`3a46e95`); 67 scattered inline env-read idioms in main.rs migrated onto them
  (`98d1d08`), verified behavior-preserving (env-var name set identical 116==116, build
  warning-free, full suite green).
- **P2-6 (Config struct)** the ~197-line tuning-resolution block extracted from
  launch_chain_runtime into a typed `RuntimeTuning` struct + `from_env(ops_inputs,
  chain_name)`, destructured back into the same local names (`1c1ac28`). Verified
  behavior-preserving by diff (moved block byte-identical bar `&cfg.name`→`chain_name`),
  env-var name set, build, and full suite. **P2-6 complete.**
- **P2-5** `update_float_ema` deduped into util (was copied in health.rs + main.rs); the
  OP-stack getL1Fee oracle address + calldata encoding shared between fees.rs and
  sim_revm.rs (`33a7cde`). No economics change.
- **P3-1** deleted 25 zero-byte paste-debris files from the repo root and added
  `__pycache__/`/`*.pyc` to .gitignore (`57d4d2d`).

Remaining:
- **P2-6 (nice-to-have)** the reads still inline in launch_chain_runtime are intentional
  `.context()?` fail-fast validation + four dynamic-name reads; a per-prefix-keyed cache
  for `classify_provider_for_chain` (the one param-dependent per-block reader left) is a
  minor follow-up.
- **P2-7** split the 13k-line `main.rs` — large, not started.
- **P3** items (junk-file purge, chain-config trim, token-universe trim, profit-funnel
  metrics) — not started.

---

## P0 — Correctness & safety (money-losing or unsafe; fix before any funded run)

### P0-1 — `JIT_LP_ENABLED` flag uses a stricter parser than every other flag
- **Where:** `src/main.rs` ~10055, `jit_enabled` binding.
- **Bug:** it uses `.to_lowercase() == "true"`, so `JIT_LP_ENABLED=1` or `=yes`
  **silently does not enable JIT**, while 15+ other flags accept `"1"|"true"|"yes"`.
- **Fix:** replace the inline parse with the existing helper `read_feature_flag`
  (`src/main.rs:215`): `let jit_enabled = read_feature_flag("JIT_LP_ENABLED", false);`
- **Accept:** `JIT_LP_ENABLED=1`, `=yes`, `=true` all enable JIT; add a unit test.

### P0-2 — Curve/Balancer RPC error classifier is a weaker copy than UniV3's
- **Where:** `is_block_out_of_range_error` in `src/quote_curve.rs:6`,
  `src/quote_balancer.rs:58` (match only `"blockoutofrangeerror"`) vs the complete
  version in `src/quote_univ3.rs:17` (also matches `"block out of range"`,
  `"header not found"`, `"requested was"`).
- **Bug:** on Curve/Balancer, a transient block-lag RPC error is misclassified as a
  hard quote failure → those edges get dropped instead of retried, losing real edges
  under node lag.
- **Fix:** create `src/quote_common.rs` (add `pub mod quote_common;` to `src/lib.rs`
  and `mod quote_common;` to `src/main.rs`), move the **complete** univ3 implementation
  there as `pub(crate) fn is_block_out_of_range_error(...)`, and have all three quote
  modules call it. Delete the two weak copies and the univ3 local copy.
- **Accept:** one definition remains (grep shows a single `fn is_block_out_of_range_error`);
  existing tests `detects_block_out_of_range_messages` / `ignores_non_block_range_messages`
  pass for all three modules against the full match set.

### P0-3 — Compromised plaintext private key on disk
- **Where:** `.env` (gitignored — not committed, good) contains a literal 64-hex
  `PRIVATE_KEY`; `docs/VALIDATION_RUN.md` flags it as previously exposed / compromised.
- **Fix (ops, not code):** generate a fresh operator key in a signer/KMS or hardware
  wallet; never let the old key hold funds. Confirm `.env` stays gitignored
  (`git check-ignore .env`). Route profits to a cold `SIPHON_TARGET_ADDRESS`
  (machinery already in `src/capital.rs`). Document key rotation in the runbook.
- **Accept:** funded hot wallet uses a key that has never appeared in any file;
  old key holds zero balance.

---

## P1 — Performance / latency on the hot path (gates fill rate → profit)

### P1-1 — No release optimisation profile; risk of running debug builds
- **Where:** `Cargo.toml` has **no `[profile.release]`**; `cargo check` reported
  `dev profile [unoptimized + debuginfo]`. No `rust-toolchain.toml` pin either.
- **Why:** this is a latency-race bot. Debug builds are multiples slower; even the
  default release profile omits LTO and `codegen-units=1`.
- **Fix:** add to `Cargo.toml`:
  ```toml
  [profile.release]
  opt-level = 3
  lto = "fat"
  codegen-units = 1
  panic = "abort"
  strip = true
  ```
  Add `rust-toolchain.toml` pinning the channel (docs say Rust 1.92+). Ensure all run
  scripts (`scripts/shadow/*.sh`, systemd units, docker) use `cargo run --release` /
  the release binary. Verify `panic = "abort"` is compatible with the supervised-worker
  design in `spawn_supervised` (it catches `JoinError::is_panic`; with `abort` a worker
  panic kills the process instead — **confirm this is desired**, or drop `panic=abort`
  and keep unwind so the supervisor can restart workers). Recommendation: **keep unwind**
  (drop `panic=abort`) so `spawn_supervised` restart semantics survive.
- **Accept:** `cargo build --release` produces an optimised binary; run scripts use it;
  supervised-worker restart still works (test by forcing a worker panic).

### P1-2 — Hot-path mutex held across sequential RPC calls (`load_token_decimals`)
- **Where:** `src/main.rs:3694` `load_token_decimals`. It takes
  `self.token_decimals.lock().await` and then, **while holding the lock**, loops over
  tokens calling `erc20_decimals(...).await` (RPC) with up to 3 retries and
  `sleep(200<<attempt)` backoffs.
- **Why:** any concurrent access to the decimals cache blocks for the full duration of N
  sequential RPC round-trips + sleeps. Serialises the scan pipeline under cold cache.
- **Fix:** snapshot missing tokens under a short lock, release it, fetch decimals
  **concurrently** (mirror the pattern already used in `load_native_prices`,
  `src/main.rs:3772`, which uses `buffer_unordered`), then re-lock briefly to insert.
  Never hold the lock across `.await` on RPC.
- **Accept:** no lock guard is alive across an `erc20_decimals`/`sleep` await; decimals
  fetch is concurrent; behaviour (values cached) unchanged.

### P1-3 — Redundant `get_block(Latest)` HTTP round-trip at the front of every scan
- **Where:** `src/main.rs:4854` inside `scan_once_with` — fetches latest block over
  HTTP even though the WS `block_head_monitor` already delivered the head (with
  `base_fee_per_gas`) via `block_head_rx`.
- **Fix:** carry the full block header (number + `base_fee_per_gas`) through the
  `block_head` watch channel (`block_head_channel`, `spawn_block_head_monitor` near
  `src/main.rs:11265`/11300) and consume it in `scan_once_with` instead of re-fetching.
  Keep the HTTP fetch only as a fallback when no WS head is available (the `else` branch
  where `block_head_rx` is `None`). Preserve the fail-closed behaviour on a zero/missing
  head.
- **Accept:** on the WS path, a scan triggered by a new head performs zero extra
  `get_block(Latest)` calls; fail-closed guards intact; tests pass.

### P1-4 — Per-candidate simulation is serial and quorum-verifies losers
- **Where:** `simulate_plan_execution` `src/main.rs:7482`. The `eth_call` fallback does
  `call` → `sim_quorum.verify` (N extra RPCs) → `estimate_gas` **sequentially per
  candidate**; the REVM path also fetches `get_block_number` + `get_chainid` per call.
- **Fix (two parts):**
  1. Only run `sim_quorum.verify` for the **single candidate about to be broadcast**,
     not for every ranked candidate. Move the quorum call out of the per-candidate loop
     to just before dispatch. (Quorum is a pre-broadcast safety gate, not a ranking tool.)
  2. Cache `chain_id` once at runtime startup (it is constant) instead of
     `get_chainid()` per sim; pass block number from P1-3's channel instead of
     `get_block_number()` per sim.
  3. Where multiple candidates are simulated, run the top-K REVM sims concurrently
     (bounded), then verify+broadcast the best.
- **Accept:** quorum RPCs scale with dispatches, not candidates; no `get_chainid` in the
  sim hot path; simulated economics unchanged; tests pass.
- **Implementation note (done, commit `bfd9494`):** parts 2 was implemented (static
  `chain_id` + scan block number threaded into the revm fork). Part 1/3 (quorum
  placement) was **intentionally not changed**: tracing the actual cascade showed it
  already breaks on the first fully-passing candidate, so `sim_quorum.verify` runs for
  the dispatch candidate, not every ranked one — moving it would cut zero calls while
  risking the try-next-on-quorum-failure behaviour. Concurrent top-K REVM sims (part 3)
  remain a future option if the cascade depth grows.

### P1-5 — `quote_path_grid` has no retry-at-Latest fallback (surfaced by P1-3)
- **Where:** `src/quote_univ3.rs` `quote_path_grid` (and the mirror in
  `src/quote_slipstream.rs`). Unlike `quote_path`, which retries at `Latest` when a
  pinned-block `eth_call` returns block-out-of-range, `quote_path_grid` has no such
  fallback and returns `Err`, dropping the batched multicall to the slow per-amount path
  (`venues.rs`).
- **Why (found by adversarial review of P1-3):** P1-3 sources `block_number` from the
  websocket feed. When the WS tip **leads** the HTTP quoting node (only possible when
  they are different endpoints — e.g. during failover to a public RPC, not on the
  self-hosted single-node target), grid quotes pin a block the quoting node hasn't seen,
  fail out-of-range, and degrade to the slow path. **Fail-safe** (trades still resolve
  correctly at the node's real latest; just slower), so low severity, but it makes a
  latency regression systematic during endpoint divergence.
- **Fix:** give `quote_path_grid` the same retry-at-`Latest` fallback `quote_path`
  already has (natural to fold into the P2-2 concentrated-liquidity core extraction).
- **Accept:** a grid quote pinned to a block ahead of the quoting node retries at
  `Latest` instead of failing to the per-amount path; edge `quote_block` reflects the
  block actually quoted.

---

## P2 — Architecture & duplication (safe iteration, correctness drift risk)

### P2-1 — Consolidate cross-file copy-pasted helpers
- **Config loaders duplicated `sandwich.rs` ⇄ `venues.rs`** (byte-identical except
  visibility): `env_var_with_fallback` (sandwich:407 / venues:905),
  `parse_pool_configs` (sandwich:414 / venues:912), `try_parse_pool_configs`
  (sandwich:430 / venues:928), `maybe_load_config_file` (sandwich:441 / venues:940).
  **Fix:** keep the `venues.rs` `pub(crate)` versions; delete the `sandwich.rs` copies;
  `use crate::venues::{...}` in sandwich.
- **`expand_env_vars` duplicated** `ops_inputs.rs:493` ⇄ `registry.rs:122`.
  **Fix:** move to `src/util.rs` as `pub fn expand_env_vars`; both call it. These parse
  the same config files, so a single implementation prevents address-resolution drift.
- **Parsing primitives scattered:** `parse_address` (registry:446, bridge:60,
  liquidations:1033, ops_inputs:2061), `parse_u256` (bridge:78, ops_inputs:2254,
  venues:983), `parse_selector` (bridge:64, liquidations:1057).
  **Fix:** centralise in `src/util.rs` (`parse_address`, `parse_u256`, `parse_selector`).
  Normalise signatures; update call sites. Where a variant adds context (e.g.
  liquidations’ `chain_name`), keep that as a thin wrapper.
- **Accept:** grep shows one definition per helper; all call sites compile; tests pass.

### P2-2 — Extract shared quoting cores
- **Concentrated liquidity duplicated** `quote_univ3.rs` ⇄ `quote_slipstream.rs`:
  `quote_path`, `quote_path_grid`, `pool_address`, `validate` are parallel
  implementations of the same tick math keyed differently (fee tier vs tick spacing).
- **Constant-product duplicated** `quote_univ2.rs` ⇄ `quote_solidly.rs`:
  `reserves_for`, `quote_exact_input_from_state`.
- **Fix:** in `src/quote_common.rs` (created in P0-2) add a `ConcentratedLiquidity`
  core parameterised by the pool key, and a `ConstantProduct` core; have the four
  modules delegate. This is also the enabler for wiring **Aerodrome Slipstream CL**
  (the documented open venue gap where much Base cross-venue arb lives).
- **Accept:** tick/CP math lives in one place; all quote tests pass; edge counts in a
  shadow run are unchanged for existing venues.

### P2-3 — Collapse duplicated cycle economics in `graph.rs`
- **Two profit-bps estimators, identical math tail:**
  `estimate_cycle_profit_bps_from_edges` (`src/graph.rs:1375`) and
  `estimate_cycle_profit_bps` (`src/graph.rs:1403`). Both live: `bellman_ford` calls the
  first, `.or_else` the second (`src/graph.rs:695`) — the fallback fires under partial
  search (load), running a *copy* of the slippage/profit math.
  **Fix:** make the node-path version resolve the path to edge indices (via the shared
  resolver below) then delegate to `_from_edges`, so the log-rate/profit computation
  exists once.
- **Two cycle-weight summers:** `cycle_weight` (`src/graph.rs:1354`) and
  `cycle_weight_for_node_path` (`src/graph.rs:1017`) both sum best-edge weights over a
  node path. **Fix:** keep one (`cycle_weight_for_node_path`, which already composes over
  `cycle_weight_from_edge_indices`); redirect callers.
- **"Best (min-weight) active edge per hop" reimplemented 3×:** canonical
  `edge_between` (`src/graph.rs:487`), plus inline copies in `cycle_weight_for_node_path`
  (1026) and `resolve_edge_path_for_cycle` fallback (1256). **Fix:** route both through
  `edge_between` (or a shared `best_edge_index(from,to)` helper).
- **Accept:** slippage/profit math and best-edge selection each have one definition;
  `graph.rs` tests (including `gas_penalties_can_remove_profitable_cycles`,
  `bellman_ford_preserves_parallel_edge_path`) pass unchanged.

### P2-4 — Decide the fate of the dead second search algorithm
- **Where:** `k_best_cycles` / `k_best_cycles_with_limits` (`src/graph.rs:512`/519,
  ~125 lines). Only test callers; production uses `bellman_ford` (main.rs:5277/5320).
- **Fix:** either (a) delete it and its tests, or (b) if intended as a fallback strategy,
  wire it behind a flag and make it share the P2-3 profit/weight helpers. Default
  recommendation: **delete** unless product wants a second strategy.
- **Accept:** no unreferenced-in-prod search path remains; build clean.

### P2-5 — Deduplicate the L1-fee oracle call and `update_float_ema`
- **L1 fee:** `fees.rs::estimate_op_stack_l1_fee` (async, ethers) and
  `sim_revm.rs::estimate_op_stack_l1_fee_blocking` (blocking reqwest) both call the same
  on-chain `GasPriceOracle.getL1Fee(bytes)` (`0x420…000F`). Results are consistent
  (oracle is authoritative — **no economics bug**), but the oracle address + selector +
  ABI encoding are duplicated. **Fix:** share the address/selector/encoding constants
  from one module (e.g. `quote_common` or a small `opstack` util); keep the two transport
  wrappers (one async, one blocking) but over shared encoding.
- **`update_float_ema`** is byte-identical in `src/main.rs:2718` and `src/health.rs:127`.
  **Fix:** keep the `health.rs` one as `pub(crate)`; delete the main.rs copy; call it.
- **Accept:** one EMA definition; one L1-fee encoding definition; sim/eth_call L1 fees
  still match.

### P2-6 — Introduce a typed `Config`, remove hot-path env reads
- **Where:** 130 `std::env::var` reads in `main.rs` (~73 inside the ~1,780-line
  `launch_chain_runtime`), plus **24 inside `venues.rs::populate_edges` which runs every
  block**, and 4 inside `scan_once_with`. The `matches!(raw..., "1"|"true"|"yes")` bool
  idiom is hand-inlined 16× despite `read_feature_flag` existing; the U256-from-env
  idiom is repeated 15× verbatim.
- **Fix:** add `src/config_runtime.rs` with a typed struct resolved **once at startup**
  from `ops/inputs.yaml` + `registry.json` + env (documented precedence), validated up
  front, passed by reference into the runtime and `populate_edges`. Add typed env
  helpers (`env_flag`, `env_u256`, `env_usize`, `env_f64`) in `src/util.rs`. Zero env
  reads in the per-block path.
- **Accept:** `grep 'std::env::var' src/venues.rs` → 0 in `populate_edges`;
  `scan_once_with` reads no env; startup fails fast on invalid config; a documented
  precedence table is added to the runbook.
- **Note:** large change — land it incrementally (env helpers first, then hoist reads
  out of `populate_edges`, then the struct), each behind its own test.

### P2-7 — Split the `main.rs` god-module (13,039 lines)
- **Fix:** carve into `runtime/` (loop + `spawn_supervised` + `ChainRuntimeHandle`),
  `pipeline/` (scan → populate → search → size → simulate → broadcast as testable
  stages), and `broadcast/` (relay/bundle: `send_bundle`, `send_private_rpc_bundle`,
  `select_broadcast_endpoint`, `competitive_priority_fee`, `apply_gas_parameters`).
  Move inline tests alongside their code.
- **Accept:** `main.rs` is orchestration only; each stage is unit-testable without RPC;
  behaviour unchanged. Land after P2-6 so config is already threaded.

### P2-8 — Remove dead / half-wired subsystems (26 warnings)
- **Where:** `cl_sim.rs` (`validate_pool`, `cl_quote_parity_enabled`,
  `log_cl_quote_parity`), `mempool.rs` (`decoded_swap_count`, `run`),
  `ingestion.rs` (`spawn_pending_tx_monitor`), `sim_revm.rs` (the dead Fjord model:
  `FJORD_INTERCEPT`, `FJORD_FASTLZ_COEF`, `FJORD_MIN_TX_SIZE`,
  `fjord_l1_fee_from_fastlz_size`, plus `simulate_typed_tx_revm`, `record_revm_metric`,
  `sim_revm_live_tests_enabled`), `graph.rs` `from_nodes`, `venues.rs` unread fields.
- **Fix:** delete, or finish-and-wire behind a flag if intended. The Fjord local
  estimator is superseded by the on-chain oracle call (P2-5) — delete it. Resolve each
  warning; get to a clean `cargo build`.
- **Accept:** `cargo build` emits 0 dead-code warnings; no behaviour change.

---

## P3 — Hygiene & ops (do alongside; low risk, high signal)

### P3-1 — Purge repo-root junk
- **Where:** ~26 zero-byte files in repo root from a botched terminal paste: `0`, `10,`,
  `260,`, `280,`, `320,`, `360,`, `420,`, `7,`, `8,`, `9,`, `None,`, `U256`, `arb,`,
  `backrun,`, `bundle,`, `configured,`, `extra,`, `liquidation,`, `private,`,
  `private_raw,`, `public,`, `threshold`, `{`, `self.filler_priority_fee,`,
  `self.searcher_priority_fee,`.
- **Fix:** delete them. Verify each is 0 bytes and untracked first (`git status`,
  `wc -c`). Add a `.gitignore` guard if useful. Also review the untracked
  `"Autonomous …Blueprint.pdf"` and `.claude/` — decide whether they belong in-repo.
- **Accept:** clean `git status` (only intended changes remain).

### P3-2 — Trim dead multi-chain config to what has real endpoints
- **Where:** `ops/inputs.yaml` configures 7 chains; only **base** has real RPCs + a
  deployed/allowlisted executor. `ethereum` → `http://127.0.0.1:8565` (add the real
  self-hosted node when ready), `arbitrum` → empty RPC lists, `optimism/linea/ink` →
  unset `${...}` placeholders, `linea` liquidation block is placeholder `0xCfDA…e90`
  repeated.
- **Fix:** move non-live chains to `configs/experimental/` not loaded by default; keep
  `base` (and `ethereum` once its node is live) in the active config. Add a startup
  guard that refuses to launch a chain whose RPC is empty/placeholder rather than
  silently pointing at localhost.
- **Accept:** default run starts only chains with valid endpoints; `CHAIN_LIST` with a
  placeholder chain fails fast with a clear error.

### P3-3 — Trim the Base token universe
- **Where:** `docs/VALIDATION_RUN.md` flags BALD (defunct memecoin), BSWAP, SPECTRA as
  thin-liquidity, revert-inducing noise. `ops/inputs.yaml` universe / `BASE_TOKENS` /
  `config/registry.json`.
- **Fix:** restrict to liquid assets (WETH/USDC/AERO/cbETH/cbBTC/wstETH-class). Config
  change only.
- **Accept:** thin tokens removed; shadow run shows fewer phantom cycles / reverts.

### P3-4 — Add profit-funnel instrumentation
- **Why:** the $25k/mo target has no funnel model. You cannot tell if you are
  RPC-limited, edge-limited, or capital-limited without it.
- **Fix:** add Prometheus counters (in `src/metrics.rs`) for: opportunities_detected →
  sim_passed → broadcast → included → net_positive, plus realised-vs-simulated net
  (slippage/L1-fee truth), revert rate, relay-reject rate, and per-stage latency
  (scan/quote/sim already partially logged at `src/main.rs:5166`). Surface on the Grafana
  dashboard under `docs/grafana`.
- **Accept:** dashboard shows the full funnel; a 48h shadow run yields a projected
  daily-net number to compare against $833.

---

## Suggested execution order (for the implementing agent)

1. **P0-1, P0-2** (behaviour bugs — small, isolated, high value). P0-3 is ops.
2. **P1-1** (release profile — one-line-ish, big latency win; decide unwind vs abort).
3. **P3-1** (purge junk — trivial, unblocks clean `git status`).
4. **P1-2, P1-3, P1-4** (hot-path latency; do P1-3 before P1-4 since P1-4 reuses the
   block header from the channel).
5. **P2-1, P2-2, P2-3, P2-4, P2-5, P2-8** (duplication + dead code; mechanical, test-guarded).
6. **P2-6** then **P2-7** (config struct, then module split — largest, land last, incremental).
7. **P3-2, P3-3, P3-4** (config trims + funnel metrics) alongside.

Run `cargo test` + `cargo clippy --all-targets` after each item. Do a Base shadow run
(`scripts/shadow/base_shadow_run.sh`) after P1 and again after P2 to confirm edge counts
and economics are unchanged.
