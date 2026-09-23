# APEX-MEV v4 — Definitive Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Do not skip a phase's exit gate.

**Goal:** Transform the existing `arbot-main2` repository into APEX-MEV v4 — an adaptive multi-chain, event-driven flash-loan arbitrage and MEV execution system with exact state, exact economics, capture assurance, and adversarial inclusion modeling — preserving every proven ARBOT implementation asset and replacing every incompatible architecture.

**Architecture:** The single 73k-LOC `arb-exec` binary crate becomes a Cargo workspace of deterministic, independently testable crates behind explicit data contracts. Proven exact mathematics (`cl_math`, `cl_swap`, `cl_ticks`, `quote_*`), state-trust machinery (`live_state`, `continuity`, `state_gate`, `cl_parity_gate`, `reconcile`), the Base fast path, REVM simulation, and the Foundry executor suite migrate forward largely intact. The `Runner` god-object orchestration, the env-var configuration plane, the unrestricted `_execGeneric` contract call, the single-signer nonce stream, and the excluded strategy modules (bridge, JIT, sandwich) do not.

**Tech Stack:** Rust 2021 (Cargo workspace, `tokio`, `ethers` 2.0.14 → migration-gated, `revm` 20, `rayon`, `dashmap`, `arc-swap`, `prometheus`), Solidity 0.8.21 + Foundry (`forge`, `via_ir`), JSON5/YAML configuration, Prometheus/Grafana observability.

---

## Global Constraints

Every task's requirements implicitly include this section. Values are normative.

| Constraint | Value | Source |
|---|---|---|
| Architectural authority | `APEX_MEV_v4_Final_Architect_Blueprint.md` wins over this PLAN; this PLAN wins over existing code; existing code wins over legacy ARBOT docs | Blueprint §58.2 |
| Repository baseline | Transform `arbot-main2` in place. Blank-repository rewrite is **prohibited**. Git history is preserved. No destructive reset. | Blueprint §58.1, §58.8 |
| Rust edition / toolchain | edition 2021; `rust-toolchain.toml` pinned (do not float) | `rust-toolchain.toml` |
| Solidity | `solc 0.8.21`, `optimizer_runs = 200`, `via_ir = true` | `foundry.toml` |
| Formatting | **Never run `cargo fmt` across the tree.** The repo has never been formatted; a tree-wide format destroys reviewability of this migration. Format only files you create, with `cargo fmt -- <path>`. | Repo history |
| Lint gate | `cargo clippy -D warnings` fails pre-existing on legacy modules. New crates MUST be clippy-clean; legacy crates are gated per-crate as they are migrated. | Repo history |
| Test invocation | `cargo test --workspace --all-targets`. Note: `cargo test --lib` alone misses binary-crate tests; `--all-targets` is mandatory. | Repo history |
| Profit metric | Realized net USD per hour. Never route count, candidate count, bps, or submission count. | Blueprint §1.1 |
| Economic target | Credible measured path to **> $25,000 net USD/month**. A measured capacity target, never a forecast. | Blueprint §38 |
| Numeraire | Execution decisions are valid in exact token units. USD marks are a bounded interval `[V_low, V_high]` used for ranking/reporting only and may never admit a trade on their own. | Blueprint §2.10, gate 32 |
| Capture guarantee | System capture assurance (internal) approaches 100%. Market capture (external) is stochastic and is never claimed as guaranteed. | Blueprint §2.4, §57.1.5 |
| Excluded from production | JIT liquidity, bridge arbitrage, cross-chain atomic execution, probabilistic spam, uncommitted probing, default public leakage, unbounded search, unbounded global NLP, unmodelled V4 hooks, unapproved arbitrary contract calls | Blueprint §42 |
| Next-dollar rule | When two tasks compete, pick the higher `E[incremental realized net USD] / (engineering + operating + risk cost)`. NEXT DOLLAR > NEXT FEATURE. | Blueprint §52 |
| Ticket invariant | Every admitted live ticket reaches exactly one terminal outcome: success/included/finalized, or `EXPLICIT_FAILURE(code, timestamp, state, cause)`. Silent expiry is zero-tolerance. | Blueprint §46.1, §57.1.1 |
| Hard zero counters | `ticket_drop_count`, `unexplained_pre_dispatch_expiry`, `nonce_reuse`, `wrong_chain_submission`, `commitment_mismatch`, `critical_state_gap_used_for_trade`, `unauthorized_adapter_call` must all read 0 in production. | Blueprint §29.6 |
| Rollback doctrine | No subsystem is replaced without a live-toggleable rollback path that survives until the replacement passes its production gate. | Blueprint §58.8 |
| Secrets | Private keys, RPC credentials, builder credentials, Fast Feed credentials never enter source, logs, metrics, or telemetry. | Blueprint §43 |
| Staging | **Never `git add -A` / `git add .`.** Stage named paths only. The tree carries parked config modifications, multi-MB logs and ~30 `.bak.<epoch>` files that `.gitignore` does not cover; the repository is public and history rewrites are forbidden (§35.4). | Repo state, §35.4 |
| Primitives | New `apex-*` crates use `alloy-primitives`. `arb-exec-legacy` keeps `ethers` until Phase 17. The single conversion boundary is `apex-types::compat`. | §2.2 C-10 |

---

# 1. Executive implementation strategy

## 1.1 What this repository actually is

`arbot-main2` is **not** a prototype. It is a 73,194-LOC Rust engine plus 2,144 LOC of Solidity that has been run against Base mainnet for months and has produced hard, negative, *useful* economic findings. Those findings are the most valuable asset in the repository and they shape this plan more than any single module does:

1. **Opportunity density is not the constraint.** A 4x universe expansion produced 4x the cycles and the identical ~96% `no_profitable_size` rejection rate.
2. **Latency was not the original constraint.** Measured edge persistence was ~35 s with zero decay — the quiet-block regime had no race to lose.
3. **More hops is strictly worse.** A ~1,200-sample census across 2/3/4-hop routes found zero profitable samples; deeper routes were monotonically worse.
4. **Fees were the constraint, and the cheap frontier is small.** Moving to the cheap cross-venue frontier collapsed the hurdle from 247 bps to 10 bps; the tradeable set is ~8 cross-venue pairs at 1.6–7 bps.
5. **The edge is event-triggered.** The first net-positive samples appeared only on swap-triggered evaluation; 88% of them came from swap triggers, not quiet blocks. On the cheap frontier, latency *became* the constraint.

### 1.1.1 A confound sits under all five findings — resolve it in Phase 2

All five were measured on a fast path that **does not price concentrated liquidity exactly**, and nothing in the repository records this. The mechanism is not the env flag — `.env:211` does set `ARBOT_CL_MULTI_TICK=1` — it is that the fast path carries no tick ladder:

```text
src/base_fast.rs:1668, :1776, :4274, :4983     tick_ladder: None
src/plan.rs:106-110   "multi-tick ON but this edge carries NO ladder; forced to single-tick"
src/plan.rs:186       → cl_sim::quote_exact_input_single_tick(...)   // constant-liquidity
src/plan.rs:26-32     cl_tick_buffer_bps() default = 50 bps haircut on single-tick CL quotes
```

`base_fast.rs:1570` states the intent — *"No cached tick ladder: sizing requotes on chain, and a stale ladder would be worse than none"* — so the design is single-tick for **ranking** and an on-chain re-quote for **sizing**. That is defensible. What is not recorded is which of the two produced each census verdict.

**Why this matters more than it looks:** the single-tick model holds liquidity constant, i.e. assumes infinite depth, so it is *systematically optimistic* — and `plan.rs`'s own comment says falling through to it "was the bug that made every candidate a phantom." Against that, the 50 bps haircut is **5× the 10 bps hurdle** the cheap frontier was measured against. The two errors push in opposite directions and neither is quantified.

So finding 4 (*"the tradeable set is ~8 cross-venue cheap pairs at 1.6–7 bps"*) and finding 5 (*"latency became the constraint"*) are **provisional**, and §36.2 scopes the first trade to exactly that set. This is not a reason to distrust the findings — the census's rejections were economic, not arithmetic — but the frontier must be re-measured under exact pricing before capital sizes against it. **Phase 2 does not exit until that re-measurement is done** (G-PRICE-2), and §36.2 is revised or confirmed by its result. Findings 1, 2 and 3 are unaffected: they are about density, decay and hop count, none of which the CL model's depth assumption can flip.

This is exactly the transition the v4 blueprint is designed for. ARBOT arrived empirically at the blueprint's central thesis — *fresh state + exact economics + event-triggered evaluation + capture* — and then ran out of architecture. The remaining gap between "first net-positive samples" and "realized net USD per hour" is **capture**: the ability to take an event-triggered, exactly-priced, thin-margin opportunity and deterministically get it dispatched inside its window. That is precisely what the v4 Capture Assurance Protocol specifies and what the current `Runner` architecture structurally cannot deliver.

## 1.2 The strategy in one paragraph

Keep the mathematics, keep the state-trust machinery, keep the Base fast path, keep the simulation and Foundry assets. Replace the orchestration, the configuration plane, the submission path, and the contract's unrestricted call surface. Carve the monolith into a workspace so that every v4 interface named in Blueprint §51 is a real, independently testable crate boundary. Build the Capture Assurance Controller early and run it in shadow long before it touches capital, so that by the time the first live dollar is at risk the ticket lifecycle has already been exercised millions of times. Reach the first safe profitable trade on Base only, on the measured cheap frontier, through the event-triggered fast path — then expand.

## 1.3 Sequencing thesis

```text
PHASE 0   workspace + contracts + inventory freeze  (no behaviour change)
PHASE 1   state correctness: versions, branches, fingerprints, feed continuity
PHASE 2   exact pricing engine + differential oracle (red/blue vs legacy)
PHASE 3   exact sizing + complete chain cost model
PHASE 4   simulation hierarchy T0–T2 + eth_simulateV1 on Base
PHASE 5   Solidity settlement correctness: allowlists, multi-asset invariant, commitment
PHASE 6   Capture Assurance Controller + signer pool + nonce lanes  (SHADOW)
PHASE 7   Base execution adapter: Flashblock eligibility, submission, ack ladder
PHASE 8   risk + observability + missed-opportunity + coverage auditor
──────────  FIRST PROFITABLE TRADE GATE  ──────────
PHASE 9   adversarial simulation + competitor model (Tier 3)
PHASE 10  parallel-pool allocation
PHASE 11  Uniswap V4 programmable execution
PHASE 12  joint allocation + shared-pool coupling + bounded packing
PHASE 13  event-driven backruns
PHASE 14  Ethereum execution
PHASE 15  BSC / Arbitrum / OP adapters + adaptive chain allocator
PHASE 16  liquidations + correlated dislocations
PHASE 17  legacy retirement + dead-code removal
```

Phases 0–8 are the profit path. Nothing in 9–17 may be started while a phase 0–8 exit gate is red.

## 1.4 What is explicitly *not* being done

- No rewrite of `cl_math.rs` / `cl_swap.rs` / `cl_ticks.rs`. They are ported, not reimplemented. (§5)
- No new graph algorithm. The blueprint's §52 example is explicit that adding graph algorithms to raise candidate count is *not* a priority. `cycle_index` + incremental Bellman-Ford is the candidate generator; Hermes stays out until §12.2's miss-rate instrumentation proves it earns its place.
- No cross-chain, bridge, JIT, or sandwich work at any phase. Those modules are REMOVE, not deferred. (§6)
- No tree-wide `cargo fmt`, no `git` history rewrite, no repository recreation.

---

# 2. Architectural authority and rules

## 2.1 Precedence (normative)

```text
1. APEX_MEV_v4_Final_Architect_Blueprint.md     ← architecture
2. PLAN.md  (this document)                     ← construction contract
3. Existing arbot-main2 implementation          ← evidence
4. Existing ARBOT docs / plans / assumptions    ← history, non-binding
```

Existing code is **evidence, not architecture**. Where code and blueprint conflict, the blueprint wins. Where this PLAN mistranslates the blueprint, this PLAN is corrected first and implementation waits.

## 2.2 Documented conflicts between blueprint and repository

These are recorded rather than silently compromised, per the task mandate. Each has a disposition and an owning phase.

| ID | Conflict | Blueprint position | Repository reality | Disposition |
|---|---|---|---|---|
| **C-01** | Arbitrary external call | §26.1: never expose `call(arbitraryTarget, arbitraryCalldata)` | `MultiVenueArbImplementation._execGeneric` decodes `(address target, bytes callData, …)` from plan data and executes `target.safeCall(callData)`. **No allowlist exists anywhere in `contracts/`.** | REMOVE `_execGeneric`; REBUILD as selector+target allowlisted adapters. Phase 5. Highest-severity finding in the repository. |
| **C-02** | Single-lane signer | §27.5: multi-lane `ExecutionSignerPool` mandatory; a single EOA nonce stream is "a preventable capture bottleneck" | One `LocalWallet`, one `NonceManager<C>` per chain (`main.rs:2686`), serialized through the `Runner` dispatch path | REBUILD as `SignerPool` + per-lane `NonceManager`. Phase 6. |
| **C-03** | Cross-chain / bridge | §42: bridge-dependent arbitrage and cross-chain atomic execution excluded from production | `src/bridge.rs` (430 LOC), `BridgeLib.sol`, `_execBridge`, `Op.BRIDGE`, `config/bridge_routes.example.json5`, `contracts/mocks/MockBridge.sol` | REMOVE from production path. Phase 5 (contract), Phase 17 (Rust). |
| **C-04** | JIT liquidity | §42: excluded | `Op.JIT_LP_ADD` / `Op.JIT_LP_REMOVE`, `jitPositions` mapping, `JitExecutor.sol`, `JitConfig` in `plan.rs` | REMOVE. Phase 5. |
| **C-05** | Sandwiching | Not in the v4 strategy stack (§18 A–F); §42 forbids nothing-but is silent — absence from the mandated stack is dispositive | `src/sandwich.rs` (408 LOC), `MevRole`, `process_sandwich_opportunities` | REMOVE. Phase 17. |
| **C-06** | Configuration plane | §2.4 forbids "late configuration lookup" on the dispatch path; §24.5 forbids any non-essential configuration lookup between AUTHORIZED and dispatch | 84 distinct `ARBOT_*` environment variables read via `read_feature_flag` / `OnceLock` *at call sites throughout the hot path*, plus `ops/inputs.yaml` (752 lines) and 14 JSON5 config files | REBUILD as an immutable, versioned, validated-at-boot `ApexConfig` snapshot. Phase 0/1. |
| **C-07** | State ownership | §5.3: "No global mutable state is used as shared truth between search workers. Workers receive immutable snapshots or versioned read handles." | `Published<T> = Arc<StdMutex<Option<Arc<T>>>>` (`base_fast.rs:2054`), plus `DashMap` live state, plus `Arc<RwLock<Vec<…>>>` hot-pool vectors mutated in place | ADAPT: `Published<T>` → `arc_swap::ArcSwapOption<T>` with a `StateVersion` stamp; hot-pool vectors → versioned snapshot. Phase 1. |
| **C-08** | Gas limit vs gas used | §23.4: keep separate; gas limit is a *scheduling* variable on Base | `FeeEstimate { gas_limit, gas_price, … }` conflates them; no Flashblock capacity model exists | REBUILD as `TotalExecutionCost` + `FlashblockScheduler`. Phase 3/7. |
| **C-09** | Acknowledgement ≠ inclusion | §24.8: seven distinct observable stages | `dispatch_call` treats a successful `send_raw_transaction` / relay response as the submission outcome; `classify_receipt` then jumps to the mined receipt | REBUILD as a staged `TransactionLifecycle`. Phase 6/7. |
| **C-10** | Ethers-rs | Blueprint is library-agnostic, but §51 requires independently testable modules and §24.6 requires `eth_simulateV1` on Base | `ethers` 2.0.14 is deprecated upstream and has **no typed support for `eth_simulateV1`** (verified: no `simulateV1` symbol in the vendored source). `alloy-primitives` 0.8.26 is **already in `Cargo.lock`**, pulled transitively by `revm` 20, so the tree already runs two primitive type families with conversion at the `sim_revm` boundary. | **Split the decision.** (a) New `apex-*` crates use `alloy-primitives` from Phase 0 — it costs nothing, it is already a transitive dependency, and Phase 4's `eth_simulateV1` backend needs it. (b) The ~310 legacy call sites are **not** ported ahead of the first dollar; `arb-exec-legacy` keeps `ethers`, and a single conversion boundary lives in `apex-types`. Ethers then dies crate by crate and is gone at Phase 17. |
| **C-11** | Venue address provenance | §6.3 requires verified address/venue/pair before admission | `base_venues_complete.yaml` (untracked) contains fabricated router/quoter addresses generated by `generate_base_venues.py` | REMOVE the file from any admission path; venue admission requires on-chain bytecode verification. Phase 2. |
| **C-12** | Chain priority | §3: dynamic `ChainScore`, no permanent ordering | Six Base-specific assumptions are hard-coded across `chain.rs`, `hot_pools.rs`, `base_fast.rs` and `ops_inputs.rs`; `token_seeds` is a global Base-only list | ADAPT to per-chain profiles in Phase 15; do **not** pay for this before Base is profitable (§52). |

## 2.3 Rules binding every task in this plan

1. **No task may leave a blueprint invariant unimplemented and untested.** Section 8 maps every invariant to code, test, monitor, failure response, and gate.
2. **No new module may read an environment variable at runtime.** Configuration is resolved once, validated, versioned, and handed down as an immutable value.
3. **Any component replacing a proven one ships behind a runtime switch with the legacy path intact** until its differential gate is green (§11, §35).
4. **A test that cannot fail is not a test.** Every TDD step here includes the observed failure mode before implementation.
5. **`UNKNOWN` classification blocks the production path.** A component with insufficient evidence may be compiled, shadowed, and measured — never dispatched from.

---

# 3. Existing repository assessment

## 3.1 Inventory

| Area | Size | Notes |
|---|---|---|
| Rust source | 67 files, 73,194 LOC, **1 crate** (`arb-exec`) | `src/main.rs` alone is 16,659 LOC |
| Binaries | 5 (`arb-exec`, `cl_parity`, `ingest`, `cycle_index_stats`, `ws_probe`) | Only 2 declared in `Cargo.toml`; 3 are implicit `src/bin/` targets |
| Solidity | 23 files, 2,144 LOC | `MultiVenueArbImplementation.sol` is 1,235 LOC of it |
| Foundry tests | 4 files, 2,017 LOC, ~60 test functions | `MultiVenueArbExecutor.t.sol` is 1,793 LOC |
| Rust integration tests | 2 files, 662 LOC | `tests/integration_smoke.rs`, `tests/abi_v2.rs` |
| Configuration | `ops/inputs.yaml` 752 lines + 14 JSON5 files + 84 `ARBOT_*` env vars + `.env` (14 KB) | Four independent configuration planes |
| Prometheus metrics | ~78 distinct series | Real, wired, scraped |
| `Cargo.lock` | 6,123 lines, 588 packages | Committed and current; the dependency set is large but pinned |
| `lib/` | `forge-std` only | A single Foundry submodule; no vendored Solidity dependencies |
| `broadcast/` | 5.9 MB, 77 `Deploy.s.sol` run records | Real deployment provenance — **keep**, it is the audit trail for every deployed executor address |
| **Benchmarks** | **None.** No `benches/` directory anywhere. | Nothing measures the latency this system competes on. Phase 0 adds `criterion` harnesses; §29.5 defines the budgets they enforce. |
| CI | **None.** No `.github/workflows`. | `Makefile` targets exist but nothing enforces them |
| Repo hygiene | 30+ `.bak.<epoch>` files tracked or untracked at root, `config/`, `ops/`, `data/`; a 119 MB Grafana tarball and a 105 MB Prometheus tarball committed in-tree; a 5.3 MB `arbot-live.log`; a file literally named `python3 Convert.py` | Material cleanup debt |

## 3.2 What already works (proven by implementation evidence)

**Exact concentrated-liquidity mathematics — `src/cl_math.rs` (513 LOC).** A faithful port of Uniswap v3-core `TickMath`/`SqrtPriceMath`/`SwapMath` for the exact-input path, with Solidity's `unchecked` replaced by explicit `Option` propagation, and a deliberate distinction between `mul_div_checked` (refuses to saturate — wrong price is worse than slow) and the ranking-path `math::mul_div` (saturates). This is exactly what Blueprint §9 demands and is the single most valuable asset in the repository.

**Multi-tick CL simulation — `src/cl_swap.rs`, `src/cl_ticks.rs`, `src/cl_sim.rs` (2,288 LOC).** Tick-bitmap word decoding, `liquidityNet` decoding, a `TickLadder` abstraction, and a multi-tick exact-input quote. Measured at 0 bps against the on-chain quoter across deep (>$60M) and thin (~$10k) pools at 1–7 tick crossings.

**Per-pool parity trust gate — `src/cl_parity_gate.rs` (349 LOC).** Earns trust in the local model *per pool* by measuring it against that pool's own quoter, caches verdicts with a TTL, and **fails closed**. It exists because of a real production discovery: pool `0xc211…b3f3` reports internally inconsistent `slot0`/`liquidity()` state — its own quoter saturates at 0.0012 WETH while its reported state implies 0.80 WETH absorbable in one tick, a 667x self-contradiction. No amount of correct mathematics detects that; only the pool's own quoter does. **This is production failure knowledge encoded in code and it must survive the migration verbatim.**

**State-trust separation — `src/state_gate.rs` + `src/state_validation.rs` + `src/reconcile.rs` + `src/validation_select.rs` (1,127 LOC).** A second, *distinct* gate validating log-derived *state* against fresh RPC reads, deliberately separated from the math gate because "a single verdict could not tell you which tripped." Validation runs entirely off the hot path; a slow RPC degrades trust by TTL expiry rather than stalling. Both gates fail closed.

**Ordering state machine — `src/continuity.rs` (189 LOC).** `Ordinal { block, tx_index, log_index }` with derived `Ord` giving exactly the lexicographic chain order, duplicate/backwards/reorg detection, and an explicit, documented refusal to infer missing logs from index gaps (the subscription is filtered, so gaps carry no information). The doc comment already anticipates flashblocks: "When flashblocks land this gains `payload_id` and `flashblock_index` ahead of `block`; the state machine below does not change." That is a v4-ready design.

**Base fast path — `src/base_fast.rs` (5,738 LOC).** Preconfirmed-log ingestion via `eth_subscribe("pendingLogs")` over the existing provider websocket, verified at 215 notifications in 30 s across 4 pools with first arrival at 576 ms. The dirty set drains as a single atomic swap so a log landing mid-drain is not lost. The feed is abstracted behind a `FlashFeed` enum specifically so Base's Denim migration is a variant change rather than a rewrite. The module deliberately does no quoting, RPC, or simulation — "a feed that falls behind is worse than no feed."

**REVM fork simulation — `src/sim_revm.rs` (1,577 LOC).** In-process EVM at a pinned block with lazy JSON-RPC state loading. Carries three fixed production bugs in its comments: a pinned fork must not borrow `latest` bytecode; a missing RPC field is not a zero; a failed state read must name the account and slot.

**Simulation quorum — `src/sim_quorum.rs` (354 LOC).** Re-runs the pre-broadcast `eth_call` against every *other* configured endpoint, **pinned to the block the primary simulated at**, and vetoes on contradiction. Three modes (`best_effort`/`strict`/`off`). This is a real defence against a compromised or stale primary fabricating profit.

**RPC failover — `src/rpc_failover.rs` (284 LOC).** Implements `JsonRpcClient` so it drops into the generic provider stack transparently: last-known-good affinity, rotate on error, bounded exponential backoff only after a full cycle. Written in response to ~40 days of `rpc_error` with zero trades caused by a single pinned endpoint.

**Executor contract + Foundry suite — `contracts/` + `test/` (4,161 LOC combined).** Clone-factory deployment, Permit2 handling, ERC-3156 / Aave / Balancer / UniV2-flashswap / UniV3-flash loan providers with callback-sender verification (`if (initiator != address(this)) revert`, `if (expectedVault != msg.sender) revert`, `ctxHash` binding), reentrancy guard, role separation (`owner` / `executor` / `configAdmin`), a circuit breaker, a runtime-size gate, and ~60 tests including adversarial revert cases.

**Prometheus surface — `src/metrics.rs` (1,101 LOC, ~78 series).** Live-state trust counters, divergence bps, continuity breaks, fast-state coverage, staged latency histograms, REVM success/fallback counters, per-outcome transaction counters.

**Accounting — `src/accounting.rs` (1,153 LOC).** CSV trade log, daily summary, event log, USD conversion.

## 3.3 What is partially implemented

| Component | State | Evidence |
|---|---|---|
| `src/venue_adapter.rs` | **Trait shell, 34 LOC, `#[allow(dead_code)]`, zero implementors.** The blueprint's `VenueAdapter` (§8.3) requires `identify_state_dependencies`, `quote_exact`, `simulate_call_graph`, `gas_model`, `classify_revert`, `encode_exact`. This trait has `name`/`kind`/`identify_pools`/`snapshot_edges`/`exact_quote`/`build_steps` and nothing implements it. | `grep` finds no `impl VenueAdapter` |
| `src/quote_univ4.rs` | **73 LOC fixed-price stub.** `quote_fixed_price_exact_input(sqrt_price_x96, amount_in, fee, zero_for_one)` — a constant-price approximation with a fee haircut. No `PoolKey`, no hook address, no hook permissions, no flash accounting, no tick crossing. Blueprint §10 makes this the largest single v3→v4 delta. | Full file read |
| `src/backrun_state.rs` | Scaffolding, `#[allow(dead_code)]`, single-tick post-state advance only | Module header: "Retained scaffolding for the in-progress backrun post-state simulation feature; some fields/helpers below are not wired into the scan path yet." |
| `src/convex.rs` (466 LOC) | Real convex-relaxation splitting over a cycle plus parallel pools, no solver dependency — but no improving-path certificate, no shared-pool coupling, no `PROVEN`/`HEURISTIC`/`INVALID_FOR_CERTIFICATION` labelling (Blueprint §16.2) | Full header read |
| `src/liquidations.rs` (1,122 LOC) | Aave-style monitor with health-factor decoding; no close factor, liquidation caps, isolation-mode handling, or unwind path costing (Blueprint §18.4) | Structure scan |
| `src/hot_path.rs` (300 LOC) | `ProfitabilitySnapshot` recency scoring — a *heuristic* precursor of the §46.3 precomputed route frontier, but keyed on `(token,token,fee)` rather than a route template with venue sequence, tick neighbourhood, hook fingerprint and gas class | Full header read |
| Feed continuity | `continuity.rs` covers ordering; **no `feed_sequence`/`gap_count`/`reconstruction_status` machinery**, and no rule that a gap blocks live tickets (Blueprint §5.6) | `grep` for `gap_count`/`reconstruction_status` returns nothing |

## 3.4 What is broken or structurally dangerous

| # | Finding | Severity | Evidence |
|---|---|---|---|
| **B-1** | **Unrestricted external call in the settlement contract.** `_execGeneric` decodes an arbitrary `target` and arbitrary `callData` from plan bytes and performs `target.safeCall(callData)`. There is no target allowlist, no selector allowlist, no pool allowlist, no token allowlist in `contracts/`. A compromised or buggy off-chain planner can make the executor call anything, including token `approve` to an attacker (`action == 1` path explicitly grants allowance to a decoded `target`). | **Critical** | `contracts/executor/MultiVenueArbImplementation.sol:797-808`; a search of `contracts/` for `allowlist`, `whitelist` or `approvedRouter` returns zero hits |
| **B-1b** | **Contract-size workaround adds indirection, not risk.** `_execModule` `delegatecall`s one of four module addresses selected by the step's `Op` enum. Those four are `immutable`, deployed by the constructor itself (`genericExecutorModule = address(new GenericExecutor())`), and each module is a trampoline that immediately calls back into the executor (`GenericExecutor.execute` → `moduleExecGeneric` → `_execGeneric`). The target is therefore **not settable, not storage-mutable, and not plan-controlled** — this is an EIP-170 size workaround and is *not* a vulnerability. It is recorded here because it is the mechanism by which `_execGeneric` is reached, and because the v4 adapter design removes the need for delegatecall-into-own-storage entirely. | Informational | `:283-297` (`immutable`, constructor-deployed), `:684-712` |
| **B-2** | **Single-asset profit invariant.** `_distributeProfit(token, profitDelta, minProfit)` enforces one scalar on one token; `_initiateLoanV2` hard-rejects multi-loan plans (`if (p.loans.length != 1) revert InvalidLoanCount()`). Blueprint §26.3 requires per-debt-asset `Balance_after,j ≥ Debt_j + FlashFee_j + RequiredReturn_j` plus explicit residue accounting. Unaccounted residue in a non-profit token is currently invisible. | High | `:1193-1195`, `:526` |
| **B-3** | **No CI.** No `.github/workflows`. `make lint` (`clippy -D warnings`) and `make fmt` both fail on the existing tree, so neither is enforced anywhere. Nothing prevents a regression in `cl_math` from merging. | High | Filesystem |
| **B-4** | **Dispatch path reads configuration.** `read_feature_flag` and `OnceLock`-cached `std::env::var` lookups are scattered through the candidate → dispatch path. Blueprint §2.4 names "late configuration lookup" as an engineering failure that loses opportunities. | High | 84 `ARBOT_*` vars; `main.rs:245` |
| **B-5** | **Single signer, single nonce stream.** All live execution serializes behind one `NonceManager`. Two simultaneously admissible opportunities cannot both be dispatched. | High | `main.rs:2686`, `:8908` |
| **B-6** | **No durable ticket state.** There is no persisted record of in-flight work. A process crash between sign and receipt leaves an unclassified outcome, which Blueprint §46.1 forbids ("Recovery code must reconcile all in-flight tickets from durable state before new live dispatch is re-enabled"). | High | `grep` for any ticket/journal persistence returns nothing |
| **B-7** | **Fabricated venue addresses.** `base_venues_complete.yaml` was machine-generated by `generate_base_venues.py` and invents router/quoter addresses that do not correspond to deployed contracts. | High | Recorded repository finding; file is untracked |
| **B-8** | **Pool inventories live outside git.** `data/**/pools.jsonl` is main-tree-only and unversioned, with 15+ `.bak` variants. Inventory content is a production input that decides what the engine trades. | Medium | `find data -type f` |
| **B-9** | **`Runner`/`RunnerConfig` god-object.** A single struct carries provider, quoters, factories, routers, validation configs, flash-loan provider sets and fee bps for five providers, per-venue hot-pool `Arc<RwLock<Vec<…>>>`, capital manager, depth cache, slippage/prune weights, and Bellman-Ford limits — ~120 fields. `scan_once_with`, `prepare_candidate`, `dispatch_call` and `simulate_plan_execution` are methods on it. No component boundary is independently testable. | High | `main.rs:3478-3960`, method list |
| **B-10** | **Acknowledgement conflated with submission.** No `transport_accepted` / `node_known` / `sequencer_received` / `preconfirmed` / `included` / `finalized` ladder. | Medium | `dispatch_call`, `classify_receipt` |
| **B-11** | **Binary targets undeclared.** `src/bin/ingest.rs`, `cycle_index_stats.rs`, `ws_probe.rs` are implicit targets; only `arb-exec` and `cl_parity` are declared. `cargo test --lib` therefore misses binary-crate tests (`sizing`, `hot_pools`). | Medium | `Cargo.toml` vs `src/bin/` |
| **B-14** | **Lane protection is undocumented, not absent.** Base's submission lanes use **provider-level MEV protection** rather than builder relays: the configured `https://base.blockpi.network/v1/rpc/{key}` is BlockPI's keyed endpoint, whose MEV protection is a per-endpoint toggle **enabled by default** (their public endpoint is the different `base.public.blockpi.network/...` host), and BlockPI additionally offers a **bundle service on Base**. The Flashbots/Titan/Beaver entries at `ops/inputs.yaml:79-81` belong to ethereum because Base has no builder market — a centralized sequencer instead — so a different protection architecture is correct here, not a misconfiguration. **What is genuinely missing is evidence, not protection:** nothing in the repo records what each lane guarantees, and §14.1's EV model prices lanes by policy. `ARCHITECTURE_PIVOT_HANDOFF.md §6.6` asserted these were unprotected; that assertion is **superseded**. | Low (documentation) | `.env:74`; [BlockPI MEV protection](https://blockpi.io/blog/new-features-mev-protection-is-live-263d6d8410a3/), [supported networks](https://docs.blockpi.io/build/supported-networks-and-advanced-features) |
| **B-13** | **`forge test` was not green, and the Phase 0 gate would have baked that in — two non-hermetic deploy tests.** *Diagnosed and FIXED in Phase 0 Task 0.2a; the original characterization below it was wrong and is corrected here.* Both failures had one root cause: **forge auto-loads `.env`, and the tests set unprefixed env keys while `.env` supplies prefixed ones that correctly outrank them.** `.env:32` sets `ETH_UNIV3_ROUTER` (one of 22 `ETH_*` keys; there are no `ETHEREUM_*` keys), so with `CHAIN=eth` the canonical key won and the alias-fallback branch was never reached — proved by commenting the line, which turned the test green. `.env:47` sets `BASE_EXECUTOR_OWNER` to exactly the address in the second failure message, and `.env:14` sets `CHAIN=base`. A compounding hazard: `DeployEnvPrefix.t.sol` writes `CHAIN` via `vm.setEnv`, and env writes persist for the whole run, so the resolved prefix depended on test order. **`Deploy.s.sol`'s precedence logic is correct** — prefixed beats unprefixed is the intended rule. **The suite is flaky, not red** — 8 unmodified runs gave a varying failing set (67/0, 65/2, 67/0, 66/1, 67/0, 67/0, 67/0, 66/1), so the "2 failures" at first freeze were a sample, not a fixed pair. Every affected test passes in isolation; they poison each other. Confined to the 15 `test/Deploy*.t.sol` tests — the other 52 are clean. Test-level fixes and `threads = 1` were tried and gave no measurable gain (11/4 vs 11/3 per-file), and were reverted. No `unsetEnv` cheatcode exists in forge 1.7. | ~~High~~ **Medium (test architecture), QUARANTINED** — operator decision 2026-09-22: excluded from the Phase 0 gate, fixed in Phase 5 Task 5.7 when `Deploy.s.sol` is rewritten for the new contract set anyway. Writing this test architecture twice is poor next-dollar allocation (§52). | `.env:14,32,47`; `docs/apex/BASELINE.md` |
| **B-12** | **200+ MB of build artifacts and logs in-tree.** `grafana-10.4.2.linux-amd64.tar.gz` (119 MB), `prometheus-2.52.0.linux-amd64.tar.gz` (105 MB), `arbot-live.log` (5.3 MB), `out/` (50 dirs), 30+ `.bak.<epoch>` files. | Medium | `ls -la` |

## 3.5 What is architecturally obsolete

- **The scan loop as the primary driver.** `scan_once` / `wait_for_scan_cadence` is a *polling* architecture. The measured result — 96% `no_profitable_size`, 4.20 s median end-to-end versus a 200 ms flashblock — is the direct consequence. v4 is event-driven (§46.3); the scan loop survives only as the slow path (§2.6).
- **`FeeEstimate` as the cost model.** `gas_limit × gas_price` plus an Arbitrum per-byte add-on. Blueprint §23.1 requires a nine-field `TotalExecutionCost` including L1 data fee, builder/sequencer payment, expected failure cost, compressed-size estimate and a gas-used *distribution*.
- **`EV = profit − gas` scalar economics.** No scenario tree, no `P(Π > 0) ≥ p_min`, no CVaR, no correlation between state validity / execution success / competition / inclusion (§2, §2.1).
- **Chain hard-coding.** `chain_hot_pool_base_cap(chain_name)`, `derive_chain_event_sampling_rate(chain_name)`, `derive_chain_time_budget_ms` — chain identity is a `&str` matched in `main.rs`, not a `ChainExecutionAdapter`.
- **`bridge.rs` / `sandwich.rs` / JIT ops.** Excluded by §42 and by the v4 strategy stack.
- **`MevRole` / `Strategy` / `BroadcastEndpoint` enums in `main.rs`.** Superseded by `ChainExecutionAdapter` + `SubmissionController` + strategy crates.

## 3.6 Build state (measured 2026-09-22)

```text
cargo check --all-targets  →  exit 0, 36.87s
  1 warning: src/venues.rs:633 `edge_capacity_from_cl_state` is never used
```

**The tree compiles cleanly.** This matters for the migration strategy: there is no build debt to pay down before restructuring, so the workspace split in Phase 0 is a pure move-and-rewire with a green baseline to diff against. The tree is separately known to fail `cargo fmt --check` and `cargo clippy -D warnings` pre-existing; those are *not* regressions and are gated per-crate as crates are extracted, never tree-wide.

## 3.7 Repository state caveat — this audit read a tree that is behind origin

```text
local HEAD    c5e4d44  feat(data): prune cheap pools too shallow …
origin/main   ae12d97  Delete docs/PRODUCTION_AUDIT_FIX_PLAN.md
git log origin/main..HEAD   -> (empty)      # local is strictly BEHIND, not diverged
git log HEAD..origin/main   -> 5 commits    # ae12d97, 118c37a, 7f059cb, e079d05, 580419d
```

Origin has already deleted `plan.md`, `HANDOFF.md` and `docs/PRODUCTION_AUDIT_FIX_PLAN.md`, and carries an "Add files via upload" commit whose contents this audit has **not** seen. Every file-level claim in §3, §4 and §34 was read against `c5e4d44`.

**Task 0.0 reconciles before any other work begins**, and any §4 disposition contradicted by the five unseen commits is re-derived rather than assumed. Recorded as risk **R-17**.

## 3.8 Harvest from the deleted audit documents (Task 0.0 Step 4)

`docs/PRODUCTION_AUDIT_FIX_PLAN.md` (382 lines) and `HANDOFF.md` (928 lines) were deleted on origin; `docs/ARCHITECTURE_PIVOT_HANDOFF.md` (342 lines) is still tracked there. Their **closed** items are history and go with them. These are the items still open, carried here so the deletion loses nothing:

| From | Open item | Disposition in this plan |
|---|---|---|
| Audit **P2-7** | "Split the 13k-line `main.rs` — large, not started." It was 13,039 lines at audit time and is **16,659 today — it grew 27.8% while flagged.** | Strengthens **B-9**. Phase 8 (`apex-runtime`). The growth rate is itself the argument for doing it rather than deferring again. |
| Audit **P0-3** | Plaintext `PRIVATE_KEY` on disk; rotate before funding; route profits to a cold siphon target. | **Residual open** — see §31.4. Verified: `.env` is gitignored, never tracked, never committed, and no `.env*` file is tracked, so this was never a repository leak. But a plaintext key still sits on disk and `SIPHON_TARGET_ADDRESS` is **unset** despite `capital.rs` implementing the machinery. |
| Audit **P3-2** | 7 chains configured, only Base live; placeholders and a silent `127.0.0.1:8565` fallback for ethereum. Wanted: a startup guard that refuses a chain with an empty/placeholder RPC. | Folded into **C-12** and `apex-config`'s fail-closed validation (Task 0.4). The startup guard is now a named acceptance criterion there. |
| Audit **P3-3** | Thin/defunct tokens (BALD, BSWAP, SPECTRA) generating phantom cycles and reverts. | §10.3 token admission (Phase 1 Task 1.5); the classifier plus measured thresholds subsume the manual trim. |
| Audit **P3-4** follow-up | "Realised-vs-simulated net — needs on-chain profit extraction from the executor receipt." | Already required by **INV-44** / §15.5 and Phase 7 Task 7.5. |
| Pivot **§6.3** | Local Base node — removes a ~240 ms RTT ceiling and "makes REVM viable". | Added to §32 infrastructure and **R-20**. Blueprint §31 permits it "where economically justified", and §30.1 governs the spend. |
| Pivot **§6.5** | `path_tokens` / `fee_tiers` / `block_number` are **declared in the candidate-log schema but never populated** at the post-sim call site. | Fourth instance of this repo's "written but never wired" pattern. Named as precedent in §27 and covered by **INV-40**'s exhaustive rejection-path test. |
| Pivot **§6.6** | "Base `private_relays` are public RPCs — `mode: private` buys no protection." | **SUPERSEDED — the claim was wrong.** BlockPI's keyed Base endpoint carries MEV protection on by default and supports bundles; Base uses provider-level protection because it has a sequencer, not a builder market. Retained as **B-14 (Low)**: record each lane's guarantee as evidence, since the EV model prices by policy. |
| Pivot **§7.0** | "Multi-tick CL simulator — highest-leverage single piece of math in the repo. Start here — the only item with no prerequisites." | It *was* built (`cl_swap.rs`, 0 bps vs the quoter) and then **never wired to the fast path** — which is exactly §1.1.1. Independent corroboration of **G-PRICE-2**, from a prior session that ranked it first. |
| Pivot **§8** | "No evidence was found that **$25k/month is achievable for a new entrant** in Base atomic arbitrage." | Strengthens **R-02** materially — see its revised entry. |
| Pivot **§8** | "Liquidations are **not** the answer on these venues: Aave's Chainlink SVR recaptures ~73% of liquidation MEV to the protocol and is extending to Base." | Recorded against Phase 16 and **R-21**. Phase 16's gate already demands positive standalone EV; this is prior evidence it will likely fail on Base, so the phase should not be started on hope. |

**Deliberately not re-raised** (recorded as already resolved): the handoff's §5 config-drift section and its §6.4 key-rotation item.

---

# 4. ARBOT → APEX migration matrix

Every significant component receives **exactly one** classification: `KEEP`, `ADAPT`, `REBUILD`, `REMOVE`, `UNKNOWN`. Classification is proved from implementation evidence, never from filename.

**Definitions** (Blueprint §58.3): `KEEP` = correct, tested, performant, secure, structurally v4-compatible. `ADAPT` = capability valuable and correct enough, but interfaces / ownership / data contracts / orchestration must change. `REBUILD` = capability required but implementation cannot meet v4 correctness, latency, state, economics, security or reliability; primitives preserved where practical. `REMOVE` = obsolete, duplicated, unsafe, economically inferior, or architecturally contradictory. `UNKNOWN` = insufficient evidence; **prohibited from the production execution path until verified**.

## 4.1 Exact mathematics and pricing

| Component | Current purpose | Current status | v4 requirement | Disposition | Reason | Dependencies | Replacement |
|---|---|---|---|---|---|---|---|
| `src/cl_math.rs` (513) | UniV3 `TickMath`/`SqrtPriceMath`/`SwapMath` exact-input port | Working; explicit `Option` overflow propagation; refuses to saturate | §9 exact integer rounding semantics | **KEEP** | Faithful port; the non-saturating `mul_div_checked` choice is exactly §1.3's "exact integer pricing" | `ethers::U256/U512` | → `apex-math/src/cl/fixed_point.rs`, verbatim |
| `src/cl_swap.rs` (550) | `TickLadder`, multi-tick exact-input quote | Working; measured 0 bps vs quoter over 1–7 crossings | §9 `amount_out`, `crossed_ticks`, `next_state` | **KEEP** | Measured exactness is the evidence §35.1 asks for | `cl_math`, `cl_ticks` | → `apex-math/src/cl/swap.rs` |
| `src/cl_ticks.rs` (1,181) | Tick-bitmap words, `liquidityNet` decode, RPC/cached tick sources | Working | §9 `initialized_ticks` | **KEEP** | Correct and already abstracted over source | `cl_math` | → `apex-math/src/cl/ticks.rs` |
| `src/cl_sim.rs` (757) | Local CL exact-input simulator over `slot0`/`liquidity`/ticks | Working, env-gated (`ARBOT_LOCAL_CL_QUOTES`) | §20 Tier 1 exact local state simulation | **ADAPT** | Logic keeps; the env gate becomes an `ApexConfig` field and the abigen provider coupling moves behind `TickSource` | `cl_math`, `cl_ticks`, provider | → `apex-math/src/cl/sim.rs` + `apex-venues` binding |
| `src/cl_parity_gate.rs` (349) | Per-pool trust verdict: local model vs pool's own quoter, TTL-cached, fails closed | Working; encodes the `0xc211…b3f3` self-inconsistent-pool discovery | §35.1 differential testing; §11 "not proven exact ⇒ candidate-only" | **KEEP** | Irreplaceable production failure knowledge. Migrate the doc comment verbatim. | quoter RPC | → `apex-math/src/parity_gate.rs` |
| `src/quote_cl.rs` (429) | Shared CL quoting core for UniV3 / Pancake / Slipstream (QuoterV2 `quoteExactInput`) | Working; deliberately de-duplicated so a fix lands for all CL venues at once | §8.3 `quote_exact` | **ADAPT** | Becomes the CL `VenueAdapter` quote backend | provider | → `apex-venues/src/cl/quoter.rs` |
| `src/quote_univ3.rs` (442) | UniV3 quoter + factory validation + fee tiers | Working | §8.3, §9 | **ADAPT** | Split: math → `apex-math`; adapter → `apex-venues/src/univ3.rs` | `quote_cl` | `UniV3Adapter` |
| `src/quote_slipstream.rs` (313) | Aerodrome Slipstream (tickSpacing factory ABI) | Working | §8.1 venue | **ADAPT** | Same split | `quote_cl` | `SlipstreamAdapter` |
| `src/quote_univ2.rs` (259) | CPMM exact-input from `UniV2PairState` | Working | §11 CPMM `quote_exact` | **KEEP** | Simple and exact | — | → `apex-math/src/cpmm.rs` |
| `src/quote_solidly.rs` (374) | Solidly/Aerodrome stable+volatile invariant | Working | §11 stable-swap | **KEEP** | Exact invariant implementation | — | → `apex-math/src/solidly.rs` |
| `src/quote_curve.rs` (138) | Curve stableswap quote | Partially — thin | §11 stable-swap exact + `next_state_exact` + `rounding_exact` | **ADAPT** | Missing `next_state`/rounding contract required by §11 | — | → `apex-math/src/curve.rs`, extended |
| `src/quote_balancer.rs` (250) | Balancer weighted/stable quote | Partially | §11 | **ADAPT** | Same gap | — | → `apex-math/src/balancer.rs`, extended |
| `src/quote_univ4.rs` (73) | **Fixed-price stub**: constant `sqrtPrice` × fee haircut | **Broken as a v4 pricer.** No `PoolKey`, hooks, dynamic fee, flash accounting, or tick crossing | §10 full programmable-pool exact engine incl. `PoolManager` lock/unlock, deltas, hook fingerprint | **REBUILD** | The single largest v3→v4 delta; current code would systematically misprice every v4 pool | `apex-math/cl`, new hook model | `apex-venues/src/univ4/` (Phase 11) |
| `src/quote_common.rs` (161) | Shared quote helpers | Working | — | **ADAPT** | Folded into `apex-math` prelude | — | `apex-math/src/lib.rs` |
| `src/math.rs` (51) | Saturating `mul_div` for ranking | Working | — | **KEEP** | Correctly scoped to the ranking path; the contrast with `cl_math::mul_div_checked` is documented and intentional | — | → `apex-math/src/ranking.rs` |
| `src/convex.rs` (466) | Convex-relaxation split over cycle + parallel pools, solver-free | Partially — no certificate, no shared-pool coupling, no validity labelling | §15, §16 KKT warm start + improving-path certificate + `PROVEN`/`HEURISTIC`/`INVALID_FOR_CERTIFICATION` | **ADAPT** | Core relaxation is sound and the solver-free choice is right for a 5–20-edge subgraph; the §16.1 validity predicate and §16.2 certificate must be added | `apex-math` | `apex-econ/src/allocation/` (Phase 10/12) |

## 4.2 State

| Component | Current purpose | Current status | v4 requirement | Disposition | Reason | Dependencies | Replacement |
|---|---|---|---|---|---|---|---|
| `src/live_state.rs` (1,977) | Log-derived pool state with a `TrustState` vocabulary | Working; `UnknownReason` is already exhaustive by design | §5.1 layered state, §5.2 fingerprint, §5.3 immutable snapshots | **ADAPT** | The trust vocabulary and `may_price_locally` are exactly right; what is missing is *versioning* — `DashMap` mutation is not an immutable snapshot with a fingerprint | `continuity`, `log_decode` | `apex-state/src/live.rs` + new `StateVersion` wrapper |
| `src/continuity.rs` (189) | Ordering state machine: duplicate / backwards / reorg | Working; documented refusal to infer missing logs | §5.6 feed continuity (partial) | **ADAPT** | `Ordinal` gains `payload_id` + `flashblock_index` ahead of `block` — the module's own doc comment already specifies this | — | `apex-state/src/continuity.rs` |
| `src/state_gate.rs` (341) | Per-pool trust verdict for log-derived *state* vs fresh RPC | Working; fails closed | §5.6, §40 gate 9 | **KEEP** | Correct, well-separated from `cl_parity_gate` for a documented reason | — | `apex-state/src/state_gate.rs` |
| `src/state_validation.rs` (492) | Off-hot-path background validation task | Working; TTL-expiry degradation, never stalls the searcher | §34 simulation fidelity feed | **KEEP** | The off-path design is precisely §29.4's rule that validation must not occupy capture capacity | `state_gate`, `reconcile`, `validation_select` | `apex-state/src/validation.rs` |
| `src/reconcile.rs` (256) | Pure local-vs-chain comparison | Working; pure, no provider | §34 calibration | **KEEP** | — | — | `apex-state/src/reconcile.rs` |
| `src/validation_select.rs` (260) | Which snapshots are validatable, at which block | Working; struct-argument design specifically to make omitting the source impossible (a bug the repo already shipped once) | §34 | **KEEP** | Encoded failure knowledge | `continuity` | `apex-state/src/validation_select.rs` |
| `src/log_decode.rs` (533) | `Sync`/`Swap`/`Mint`/`Burn` topic decode incl. Pancake's distinct topic0 | Working; `Mint`/`Burn` liquidity-delta decode exists (closes the former Phase-2b blocker) | §5.3 classify state mutation | **KEEP** | Complete for the admitted venue set | — | `apex-state/src/decode.rs` |
| `src/ingestion.rs` (2,392) | WS subscribe, stall detection, reconnect, poll refresh, `PoolMonitor` | Working | §5.6 sequence/gap/recovery semantics, §31 redundant transports | **ADAPT** | Transport machinery is sound; it lacks `feed_sequence`, `gap_count`, `reconstruction_status`, and the rule that a gap blocks live tickets | `live_state`, `metrics` | `apex-state/src/feed/` + new `FeedIntegrity` |
| `src/base_fast.rs` (5,738) | Base preconfirmed-log fast path: feed, dirty set, atomic drain, candidate handoff | Working and measured (215 notifications / 30 s, first at 576 ms) | §2.6 fast path, §22 Flashblocks | **ADAPT** | The feed and dirty-set drain are the v4 fast path. What must change: `Published<T> = Arc<Mutex<Option<Arc<T>>>>` → versioned `ArcSwap`; pricing/census helpers move out to `apex-econ`; the module splits along its own internal seams | `live_state`, `graph`, `metrics` | `apex-state/src/fast/` + `apex-chain/src/base/feed.rs` |
| `src/pool_store.rs` (406) | Pool record load/resolve | Working | §6.3 pool admissibility | **ADAPT** | Must carry the full §6.3 admissibility record (transfer semantics, revert profile, gas profile, update mapping) | — | `apex-state/src/pools.rs` |
| `src/liquidity_cache.rs` (524) | LRU depth cache with external TVL source | Working; the `hub_symbol`/`hub_usd_liquidity` coupling bug is fixed | §6.3 depth estimate | **ADAPT** | External TVL is a *signal*; §18.5 forbids it as execution truth. Must be marked non-authoritative. | `token_refresh` | `apex-state/src/depth.rs` |
| `src/hot_pools.rs` (1,773) | Rank and cap the per-venue hot pool set | Working; cheap-tier ranking landed | §29 compute economics | **ADAPT** | Ranking becomes an input to the §29.1 compute-priority scorer instead of a standalone cap | `pool_store`, `liquidity_cache` | `apex-econ/src/compute/pool_priority.rs` |
| `src/discovery.rs` (205) | `PairCreated` scan for low-liquidity pools | Working | §6.3 | **ADAPT** | Feeds venue admission (§8.2) rather than the graph directly | — | `apex-venues/src/discovery.rs` |
| `src/token_refresh.rs` (36) | Token list refresh | Working, thin | §7.1 token admission, §7.2 ERC-20 semantics classifier, §7.3 risk fingerprint | **REBUILD** | 36 LOC cannot express §7's classifier (fee-on-transfer, rebasing, blacklist, pauseable, non-standard approve, decimals anomalies, permit anomalies) which is a *correctness* requirement, not a nicety | — | `apex-state/src/tokens/` (Phase 2) |
| `Published<T>` (`base_fast.rs:2054`) | `Arc<StdMutex<Option<Arc<T>>>>` snapshot handoff | Working but is shared mutable truth | §5.3 "no global mutable state as shared truth" | **REBUILD** | Directly contradicts §5.3; a `Mutex` on the fast path is also a capture hazard | `arc-swap` (already a dependency) | `apex-state::Versioned<T>` over `ArcSwapOption` |

## 4.3 Search, sizing, allocation

| Component | Current purpose | Current status | v4 requirement | Disposition | Reason | Dependencies | Replacement |
|---|---|---|---|---|---|---|---|
| `src/graph.rs` (4,537) | Token graph, `VenueEdge`, Bellman-Ford negative-cycle search, hub-anchored cycles, incremental adjacency | Working; `refresh_incremental_adjacency` exists | §12.1 incremental negative-cycle search | **ADAPT** | The algorithm is right and §52 explicitly says adding graph algorithms for candidate count is *not* a priority. What changes: edges must carry `state_version`, and search must consume immutable snapshots | `apex-math`, `apex-state` | `apex-search/src/graph.rs` |
| `src/cycle_index.rs` (966) | Precomputed cycle set indexed by hop; `cycles_touching(pair)`; `structure_digest` refresh trigger | Working — separates near-static *structure* from per-block *state* | §46.3 precomputed route frontier | **ADAPT** | This is already 70% of the §46.3 frontier. It must additionally carry venue sequence, fee variants, tick neighbourhood, hook fingerprint, flash source and gas class | `graph` | `apex-search/src/frontier.rs` |
| `src/sizing.rs` (1,720) | 1-D size optimisation: CPMM Newton seeds + bracketed search, flash-provider aware | Working | §14.2 continuous warm start, §14.3 **discrete integer/wei refinement** | **ADAPT** | Newton/bracket warm start is §14.2 exactly. The missing half is §14.3: the final answer must be an exact integer-unit candidate verified by exact AMM evaluation, not a continuous optimum | `apex-math`, `flash_loan` | `apex-econ/src/sizing/` |
| `src/hot_path.rs` (300) | `ProfitabilitySnapshot` recency scoring on `(token,token,fee)` | Working heuristic | §46.3 route frontier | **ADAPT** | Merges into `apex-search/src/frontier.rs`; the recency signal survives as one frontier ranking input | — | folded into frontier |
| `src/backrun_state.rs` (168) | Single-tick post-victim state advance | Scaffolding, `#[allow(dead_code)]`, unwired | §12.5 predict target delta → exactly simulate target → reprice closure | **REBUILD** | Single-tick advance cannot express §12.5's "exactly simulate target"; the correct implementation runs the victim through the Tier-2 simulator and patches a state branch | `apex-sim`, `apex-state` | `apex-search/src/backrun/` (Phase 13) |
| `src/mempool.rs` (818) | Pending-tx decode → backrun hints; mined-swap monitor | Working for Base | §12.4 event templates, §12.5 target classification | **ADAPT** | Decode machinery keeps; it must emit typed `StateEvent`s into the event bus rather than ad-hoc `BackrunHint`s | `token_refresh`, `metrics` | `apex-search/src/events/` |
| `src/liquidations.rs` (1,122) | Aave-style health-factor monitor | Partial — missing close factor, caps, isolation mode, unwind costing | §18.4 full protocol model | **ADAPT** | Monitor and decode survive; the eligibility/unwind economics must be built | `graph` | `apex-strategy/src/liquidation/` (Phase 16) |
| `src/sandwich.rs` (408) | Sandwich opportunity detection | Working | **Not in the v4 strategy stack (§18 A–F)** | **REMOVE** | Absence from the mandated strategy stack is dispositive; it also consumes mempool and simulation capacity that §29.4 reserves for capture | `mempool` | none |
| `src/bridge.rs` (430) | Bridge route planning across chains | Working | §42: **excluded from production** | **REMOVE** | Explicitly excluded | `graph` | none |
| `src/capital.rs` (184) | Base amount, flash-loan bounds, siphon buffer | Working | §29 capital allocation, §3.4 portfolio controller | **ADAPT** | Becomes per-chain capital state under the chain portfolio controller | — | `apex-risk/src/capital.rs` |

## 4.4 Economics, simulation, execution

| Component | Current purpose | Current status | v4 requirement | Disposition | Reason | Dependencies | Replacement |
|---|---|---|---|---|---|---|---|
| `src/fees.rs` (803) | `FeeEstimate { gas_limit, gas_price }` + Arbitrum per-byte | Working but structurally insufficient | §23.1 nine-field `TotalExecutionCost`; §23.4 gas_limit vs gas_used separation; §23.2 OP Stack L1 data fee | **REBUILD** | §23 requires L1 data fee, builder/sequencer payment, expected failure cost, compressed-size estimate, and a gas-used *distribution*. The current struct cannot hold them and conflates the scheduling variable with the cost variable. | `apex-chain` | `apex-econ/src/cost/` (Phase 3) |
| `src/flash_loan.rs` (246) | Provider enum + fee bps + best-single-provider selection | Working; capacity bounding landed | §19 `FlashSourceQuote` with gas overhead, callback constraints, availability probability, reliability score; §19.3 `argmin(fee + gas + failure risk + availability penalty)` | **ADAPT** | Selection *shape* is right; the quote struct needs the five missing §19.1 fields and the §19.4 multi-source fallback | — | `apex-econ/src/flash/` |
| `src/risk_policy.rs` (324) | Per-chain enforceable limits from `ops/inputs.yaml`; revert-penalty premium | Working — written specifically to end "safety theater" | §28 hard gate, §28.1 graduated response | **ADAPT** | The enforcement discipline is exactly right; it must gain §28.1's `NORMAL → REDUCED → HIGH_EV_ONLY → STRATEGY_DISABLED → CHAIN_DISABLED → GLOBAL_HALT` ladder and §28.2 loss classification | `ops_inputs` | `apex-risk/src/policy.rs` |
| `CircuitBreaker` (`main.rs:2769-3076`) | Hourly/daily loss limits, consecutive-fail limit, revert-rate and RPC-error triggers, with tests | Working, well-tested | §28 triggers | **ADAPT** | Extract from `main.rs` unchanged in behaviour; extend the trigger set to §28's full list | `metrics` | `apex-risk/src/breaker.rs` |
| `src/sim_revm.rs` (1,577) | REVM 20 fork-at-block simulation with lazy RPC state | Working; three production bugs fixed and documented | §20 Tier 2 full EVM transaction simulation | **KEEP** | This *is* Tier 2. Keep the module; change only who calls it. | `revm`, RPC | `apex-sim/src/revm/` |
| `src/sim_quorum.rs` (354) | Independent cross-endpoint `eth_call` verification, block-pinned | Working | §20 Tier 2 fidelity; §43 operational security | **KEEP** | A real defence against a compromised primary; block-pinning is subtle and correct | `rpc_failover` | `apex-sim/src/quorum.rs` |
| `src/plan.rs` (2,472) | Cycle → executor `Plan`/`StepData` encoding, min-out derivation, JIT config | Working for the current contract ABI | §25 deterministic commitment hash; §26.2 route validator inputs | **REBUILD** | The encoding must produce an `ExecutionCommitment` that the contract independently verifies (§25), and must drop JIT (§42). The min-out/slippage logic is preserved. | `apex-math`, contract ABI | `apex-exec/src/encode/` (Phase 5) |
| `src/abi.rs` (1,349) | Generated executor bindings | Working | — | **ADAPT** | Regenerated against the v4 contract ABI by `scripts/gen_abi_rs.sh` | — | `apex-exec/src/abi.rs` |
| `NonceManager<C>` (`main.rs:2686-2760`) | Single pending-nonce allocator with gap recovery | Working for one lane; the "authoritative pending nonce" comment records a real historical bug | §27.2 per-chain nonce manager; §27.5 **multi-lane** | **ADAPT** | The per-lane logic is correct and battle-tested. It becomes *one lane* inside a `SignerPool`. | provider | `apex-capture/src/nonce.rs` |
| `dispatch_call` / relay stack (`main.rs:1905-2500`, `:8868-9335`) | Private relay bundles, parallel blast, public fallback, jitter, gas params | Working | §24 submission optimisation; §24.5 last-mile protocol; §24.8 ack ladder | **REBUILD** | No last-mile revalidation, no ticket, no ack ladder, acknowledgement conflated with submission, configuration read on the path | `apex-capture` | `apex-capture/src/dispatch/` + `apex-chain/*/submit.rs` |
| `Runner` / `RunnerConfig` (`main.rs`, ~120 fields) | Everything: scan loop, candidate prep, pricing, sizing, gas, flash, simulation, risk, dispatch, accounting | Working, untestable as components | §46 deterministic control plane with explicit inputs/outputs and minimal hidden state; §51 independently unit-testable modules | **REBUILD** | Directly contradicts §46/§51. This is the single largest structural change in the migration. | everything | `apex-runtime/src/` control plane (Phases 1–8) |
| `src/accounting.rs` (1,153) | CSV trade log, daily summary, USD conversion | Working | §32 economics telemetry; §33 missed-opportunity accounting; P&L attribution by route/venue/strategy/chain/layer | **ADAPT** | Schema must extend to the §45 objects and the §33 reason taxonomy; the CSV/daily machinery survives | — | `apex-obs/src/pnl/` |
| `src/metrics.rs` (1,101, ~78 series) | Prometheus registry and exporter | Working | §32 full surface incl. capture assurance, state branches, inclusion eligibility, chain economics | **ADAPT** | Keep the exporter and existing series; add the §32 capture-assurance and state-branch families | — | `apex-obs/src/metrics.rs` |
| `src/rpc_failover.rs` (284) | Multi-endpoint self-healing JSON-RPC | Working; solved a 40-day zero-trade outage | §31 redundant external RPC; §44 fail-open-to-alternatives | **KEEP** | Correct and proven | — | `apex-chain/src/rpc/failover.rs` |
| `src/health.rs` (179) | EMA reject/latency/success tracking | Working | §27.5 per-lane health score; §28 triggers | **ADAPT** | Becomes the per-signer-lane and per-endpoint health scorer | — | `apex-capture/src/health.rs` |
| `src/chain.rs` (2,782) | Per-chain config: RPC, quoter, factory, vault, aave pool, gas model | Working | §4 `ChainExecutionAdapter` with 10 methods | **REBUILD** | A config struct is not an adapter. Chain-specific *behaviour* (fee model, inclusion probability, submission optimisation, replacement policy, reconciliation) does not exist. | `ops_inputs`, `registry` | `apex-chain/src/adapter.rs` + per-chain impls |
| `src/ops_inputs.rs` (2,619) | `ops/inputs.yaml` deserialisation | Working | §45 schemas; immutable validated config | **ADAPT** | **Stays in place** (it imports `crate::util`; see Task 0.4's cycle note). Gains `to_apex_config()` so the fresh schema can be differentially verified against it. Retired in Phase 17. | `util` | `apex-config` built fresh alongside |
| `src/registry.rs` (872) | `config/registry.json` address book + env overrides | Working | §6.3 verified address/venue; §19.2 address-book preference | **ADAPT** | **Stays in place** — imports `crate::venues` (5,646 LOC). The bytecode-verification requirement (C-11/B-7) is implemented in the fresh `apex-config::registry`. Retired in Phase 17. | `util`, `venues` | `apex-config/src/registry.rs` built fresh |
| `src/config_validation.rs` (103) | Startup config checks | Thin | Boot-time validation of the whole `ApexConfig` | **REBUILD** | **Cannot move**: imports `bridge` (REMOVE), `chain`, `ops_inputs`, `registry`, `venues`. Replaced by a fresh fail-closed validator over the whole immutable snapshot. | `bridge`, `chain`, `venues` | `apex-config/src/validate.rs` built fresh |
| `src/util.rs` (1,217) | WS connect w/ fallbacks, slippage, path encode, weights, native price | Working, grab-bag | — | **ADAPT** | Split by owner: WS → `apex-chain`, slippage/path → `apex-exec`, weights → `apex-search`, native price → `apex-econ` | many | dissolved |
| `src/state_gate.rs` + `cl_parity_gate.rs` pair | Two independent fail-closed gates | Working | §40 gates 1–9 | **KEEP** (both) | The separation is deliberate and documented | — | migrated as-is |
| `src/venue_adapter.rs` (34) | Trait shell, zero implementors | **Unimplemented** | §8.3 six-method `VenueAdapter` | **REBUILD** | Wrong method set, no implementors, `#[allow(dead_code)]` | `apex-math`, `apex-state` | `apex-venues/src/adapter.rs` (Phase 2) |
| `src/integration_smoke.rs` (372) + `tests/integration_smoke.rs` (617) | Fork dry-run smoke path | Working | §35 integration/fork testing | **KEEP** | Real fork coverage; extend rather than replace | fork RPC | `tests/` retained |
| `src/bin/cl_parity.rs` (201) | CL parity sweep binary | Working | §35.1 differential harness | **KEEP** | Becomes part of the differential oracle | `cl_parity_gate` | `apex-math` bin |
| `src/bin/ingest.rs`, `cycle_index_stats.rs`, `ws_probe.rs` | Operational probes | Working; **undeclared in `Cargo.toml`** | operational tooling | **ADAPT** | Declared explicitly so `--all-targets` reaches them (fixes B-11) | — | `apex-tools/` |

## 4.5 Solidity

| Component | Current purpose | Current status | v4 requirement | Disposition | Reason | Dependencies | Replacement |
|---|---|---|---|---|---|---|---|
| `MultiVenueArbImplementation.sol` (1,235) | Flash-loan settlement executor, 5 providers, step VM | Working but **unsafe** (B-1) and single-asset (B-2) | §26 small typed executor with allowlists, §26.3 multi-asset invariant, §26.2 route validator, §25 commitment | **REBUILD** | `_execGeneric`'s arbitrary `target.call(callData)` is disqualifying on its own. Rebuild preserves: loan-provider callback verification, `ctxHash` binding, Permit2 handling, circuit breaker, role separation. | libraries, utils | `contracts/core/` + `contracts/adapters/` + `contracts/chains/BaseArbExecutor.sol` |
| `_execGeneric` | Arbitrary target + calldata + approve/transfer | **Dangerous** | §26.1 explicitly forbids | **REMOVE** | Critical security finding B-1 | — | typed adapters only |
| `_execModule` + `steps/*.sol` trampolines | Delegatecall to four `immutable`, constructor-deployed modules, selected by `Op` | Working; **safe** — target is not settable (B-1b) | §26 small typed executor | **REMOVE** | Not removed for safety: removed because the v4 adapter set replaces the size workaround with `call` to registry-resolved adapters, which is simpler and removes delegatecall-into-own-storage | — | `contracts/adapters/*` via `AdapterRegistry` |
| `Op.JIT_LP_ADD` / `JIT_LP_REMOVE`, `jitPositions`, `JitExecutor.sol` | JIT liquidity | Working | §42 **excluded** | **REMOVE** | Explicitly excluded from production | — | none |
| `Op.BRIDGE`, `BridgeLib.sol`, `_execBridge`, `BridgeExecutor.sol` | Cross-chain bridging | Working | §42 **excluded** | **REMOVE** | Explicitly excluded | — | none |
| `AccessController.sol` (63), `ReentrancyGuard.sol` (20) | Roles, reentrancy | Working | §26.5, §43 | **KEEP** | Correct primitives | — | `contracts/core/` |
| `FullMath.sol`, `TickMath.sol`, `LiquidityAmounts.sol` | Fixed-point libs | Working | §9 | **KEEP** | Standard, audited-lineage | — | `contracts/libraries/` |
| `DexLib.sol` (45) | Swap helpers | Working | §26 adapters | **ADAPT** | Absorbed into typed adapters | — | `contracts/adapters/` |
| `LiquidationLib.sol` (32) | Liquidation helper | Thin | §18.4 | **ADAPT** | Phase 16 | — | deferred |
| `ArbitrageCloneFactory.sol` (38) | EIP-1167 clone + atomic init | Working, tested (CREATE2 collision test exists) | deployment tooling | **KEEP** | — | — | `contracts/deploy/` |
| `BatchRouter.sol` (71) | Batch routing | Working | — | **UNKNOWN** | No evidence it is used by the production path; no test names it | — | audit in Phase 5, then KEEP or REMOVE |
| `contracts/mocks/*` (7 files) | Test doubles incl. `MockBridge` | Working | §35 testing | **KEEP** (minus `MockBridge`) | Real test assets. `MockBridge.sol` **REMOVE** with the bridge path. | — | `contracts/mocks/` |
| `test/MultiVenueArbExecutor.t.sol` (1,793, ~60 tests) | Executor suite incl. adversarial reverts | Working | §35.3 executor invariants | **ADAPT** | JIT/bridge tests removed with their features; all loan-provider, callback-sender, repayment, profit-split and encoding tests are retained and extended to the multi-asset invariant | forge-std | `test/` |
| `test/Deploy*.t.sol` (224) | Deployment validation | Working | deployment tooling | **KEEP** | — | — | `test/` |
| `script/Deploy.s.sol` | Foundry deploy script | Working. Its two red tests (B-13) were **test** defects, not script defects — diagnosed and fixed in Task 0.2a; the env-prefix precedence logic is correct. | §31 deployment; §26.2 executor address/version validation | **ADAPT** | Extended for the new contract set and per-chain executors. The four `chainId == 1` defaults (`:346` vault, `:354` Aave, `:362` Permit2, `:370` UniV3 router) are genuine fallbacks that only apply when the env supplies nothing — they do **not** override configuration. Revisit under per-chain profiles in Phase 15. | — | `script/` |
| `contracts/artifacts/**` (60+ JSON) | Committed Remix artifacts | Stale relative to `out/` | — | **REMOVE** | Duplicates `forge build` output and drifts silently | — | `out/` is the single source |

## 4.6 Configuration, data, tooling, docs

| Component | Current purpose | Current status | v4 requirement | Disposition | Reason | Dependencies | Replacement |
|---|---|---|---|---|---|---|---|
| `ops/inputs.yaml` (752) | Chains, universe, risk, features | Working, authoritative | immutable validated config | **ADAPT** | Remains the human-edited source; gains a schema version and boot-time full validation | `ops_inputs.rs` | `ops/inputs.yaml` + `apex-config` |
| 84 `ARBOT_*` environment variables | Runtime feature flags read at call sites | Working but forbidden on the dispatch path (§2.4) | boot-resolved immutable config | **REBUILD** | C-06 / B-4 | — | `ApexConfig` fields; env retained only for secrets and for `APEX_CONFIG_PATH` |
| `config/*.json5` (14 live + examples) | Per-venue pool/token configs | Working | §6.3 | **ADAPT** | Consolidated under a versioned config root with the `.bak.<epoch>` files removed | — | `config/` cleaned |
| `config/registry.json` | Address book | Working | §6.3 verified addresses | **ADAPT** | Bytecode verification required before production admission | `registry.rs` | `apex-config` |
| `base_venues_complete.yaml` + `generate_base_venues.py` | Machine-generated venue list | **Contains fabricated addresses** (B-7) | §6.3 | **REMOVE** | Cannot be trusted; regenerating requires on-chain verification | — | venue admission pipeline (Phase 2) |
| `data/**/pools.jsonl` | Pool inventories | Working; **unversioned, 15+ `.bak` variants** (B-8) | §6.3 admissibility record | **ADAPT** | Versioned, checksummed, verified on-chain at load, with a single canonical file per venue | `scripts/data/*` | `data/` + manifest |
| `scripts/data/*.py` (12 scripts) | Inventory build, ranking, census, verification | Working; `verify_pool_venues.py` checks pools against their deploying factory | §6.3, §2.7 coverage | **KEEP** | Real operational tooling; `verify_pool_venues.py` is the counter to B-7 | python3 | `scripts/data/` |
| `scripts/ci/*.sh` (4) | Panic check, placeholder-endpoint check, executor-size check, CL parity sweep | Working, **unenforced** (B-3) | production gates | **ADAPT** | Wired into real CI in Phase 0 | — | `.github/workflows/` |
| `scripts/fork/*.sh` (2) | Fork fixture + integration dry run | Working | §35 fork testing | **KEEP** | — | foundry | `scripts/fork/` |
| `scripts/shadow/*.sh` (4) | Base shadow run, address validation, config audit | Working | §39 Phase 0 shadow | **KEEP** | Directly serves the v4 shadow rollout | — | `scripts/shadow/` |
| `Makefile` | build / test / fmt / lint / checks | Working | — | **ADAPT** | Workspace-aware; `fmt` target changed to **never** run tree-wide (Global Constraints) | — | `Makefile` |
| `docs/superpowers/plans/*` (5), `docs/superpowers/specs/*` (2) | Live-state phase plans and designs | Historical, accurate | §58.9 traceability | **KEEP** | Operational value: they explain *why* `live_state`/`continuity`/`state_gate` look the way they do | — | `docs/superpowers/` |
| `docs/arbot_docs_pack/*` (9), `docs/*.md` (20+) | Runbooks, tuning, onboarding | Mixed currency | operational docs | **ADAPT** | Runbooks and venue-onboarding keep operational value; whitepaper-v3 and architecture docs are superseded and move to `docs/legacy/` | — | `docs/` restructured |
| `plan.md`, `HANDOFF.md`, `docs/ARCHITECTURE_PIVOT_HANDOFF.md`, `docs/PRODUCTION_AUDIT_FIX_PLAN.md` | Legacy ARBOT plans | **Deleted on `origin/main` already**; still tracked in the local `HEAD`, which is 5 commits behind (§3.7) | §20 of the task mandate: obsolete | **REMOVE** | Superseded completely by this PLAN. **Do not merge.** The local working-tree deletions duplicate work already on origin — reconcile first (Task 0.0), do not commit them as new deletions. **Before the deletion is final, move any still-open items from `PRODUCTION_AUDIT_FIX_PLAN.md` and `ARCHITECTURE_PIVOT_HANDOFF.md` into §39's risk register** — that is the only thing in them with operational value. | — | `PLAN.md` |
| `grafana-10.4.2.*.tar.gz` (119 MB), `prometheus-2.52.0.*.tar.gz` (105 MB), `grafana-v10.4.2/`, `prometheus-2.52.0.*/`, `eth-docker/`, `node/`, `out/`, `arbot-live.log`, `validation-*.log`, `*.bak.<epoch>`, `python3 Convert.py` | Vendored binaries, build output, logs, backups | In-tree, ~230 MB+ | repo hygiene | **REMOVE** | Not source. Replace with `.gitignore` entries and a documented install step. | — | `docs/apex/INFRA.md` |
| Prometheus/Grafana dashboards (`docs/grafana/`, `prometheus.yml`) | Observability config | Working | §32 | **KEEP** | Real config; only the vendored tarballs go | — | `ops/observability/` |
| `broadcast/` (77 run records, 5.9 MB) | Foundry deployment provenance | Working | §31 deployment; §26 executor address/version fingerprint | **KEEP** | The audit trail for every deployed executor address; `RouteValidator`'s `executor_version` check depends on knowing what was deployed when | foundry | `broadcast/` unchanged |
| `Cargo.lock` (588 packages) | Dependency pin | Current | reproducible builds | **ADAPT** | Regenerated once at the workspace split; thereafter committed and CI-verified unchanged by `cargo check --locked` | workspace | `Cargo.lock` |
| `lib/forge-std` | Foundry test stdlib | Working | §35 testing | **KEEP** | Only Solidity dependency; no vendored protocol code | foundry | `lib/forge-std` |
| **Benchmarks** (absent) | — | **None exist** | §30 latency architecture; §29.5 budgets | **REBUILD** (from nothing) | A system that competes on sub-200 ms capture has no latency measurement in-tree. `criterion` harnesses land in Phase 0 and every phase adds its stage. | criterion | `crates/*/benches/` |

## 4.7 `UNKNOWN` register

These may be compiled and shadowed but are **prohibited from the production execution path** until evidence is produced. Each has an owning phase that must resolve it to another classification.

| Component | Why UNKNOWN | Evidence needed | Resolving phase |
|---|---|---|---|
| ~~`contracts/executor/BatchRouter.sol`~~ | ~~No test references it~~ — **RESOLVED 2026-09-22, the claim was wrong.** `script/Deploy.s.sol:90` does `router = new BatchRouter(clone)` and then `initialise({_owner: address(router), …})`, so **the router owns the executor clone**, and `test/DeployOwnership.t.sol:22-25` asserts exactly that. It is load-bearing in the production deploy path. | — | **Reclassified KEEP**; folded into `contracts/core/ExecutionAuth.sol`'s ownership model in Phase 5 |
| ~~`src/quote_curve.rs` exactness~~ | ~~No differential test against a Curve pool exists~~ — **RESOLVED 2026-09-23: the question was mis-posed.** `quote_curve.rs` is an `abigen!` client for `get_dy`; it *is* the on-chain quoter. There is no local StableSwap implementation to differential against it, so "exactness" was never a property it could have. | — | **Reclassified**: `CurveAdapter` exists, returns `NotRepresentable` from `quote_exact`, and is `Exactness::Approximate` by construction. A local engine is new work, not a test. |
| ~~`src/quote_balancer.rs` exactness~~ | ~~Same~~ — **RESOLVED 2026-09-23, same finding.** `abigen!` client for `queryBatchSwap`, no local weighted-pool maths. | — | **Reclassified**: `BalancerAdapter`, same shape. |
| `data/base/uniswap_v4/pools.json` | V4 pools inventoried but the pricer is a stub; hook addresses unrecorded | Hook fingerprinting pass | Phase 11 |
| `src/liquidity_cache.rs` external TVL source | Third-party numbers used in ranking; §18.5 forbids external feeds as truth | **AUDITED 2026-09-23 — the confirmation FAILS.** See below. | **Reclassified ADAPT**, constraint recorded; the fix is Phase 3's |
| `config/registry.json` addresses | Not bytecode-verified | On-chain `extcodehash` verification of every production address | **PARTIAL**: `scripts/data/verify_registry_bytecode.py` exists and is self-tested against a stub; Optimism verified 11/11; Base and Ethereum unreachable from the dev environment (HTTP 403). Needs one run with egress. |
| `ops/inputs.yaml` `features:` block | Unknown how many flags still have live effect after the config rebuild | Enumerate and prove each flag's consumer | Phase 0 |

### Task 2.7 audit — `liquidity_cache` reaches execution, and the env manifest covered a prefix

**The `liquidity_cache` confirmation fails.** `PoolDepthCache` fetches token
liquidity from **DexScreener** (`api.dexscreener.com`) and
`main::compute_base_amounts` turns it into a `TradeSizing` per token, whose two
fields go to different places:

* `base_amount = depth / ARBOT_DEPTH_DIVISOR` (default 100, i.e. 1% of depth),
  clamped into the flash-loan band. This is the **probe size** for the search —
  ranking and filtering, which §18.5 permits.
* `slippage_tolerance_bps`, derived from `usage = probe / depth` and clamped to
  `[min(5, EDGE_SLIPPAGE_BPS), EDGE_SLIPPAGE_BPS]`. This one flows through
  `venues.rs` at **eight** sites into `Edge::tolerance_bps`, which `util.rs`
  documents as *"the EXECUTION min_out margin"* — the on-chain floor.

So a third-party API's liquidity number participates in setting the on-chain
`min_out`. It is bounded on both sides by operator configuration, and
`EDGE_SLIPPAGE_BPS` defaults to 30 with a floor of 5 — and `plan.rs`
records the measured tolerance on live Slipstream edges as **5**, the floor,
which means `usage` is currently so small that the external number is
saturating the clamp rather than discriminating. **The violation is latent, not
active.** It becomes active the moment probe size approaches depth, or the
moment DexScreener understates a pool.

Constraint recorded: **`tolerance_bps` must come from measured on-chain depth,
not from an external feed.** `venues.rs` has the real reserves at all eight
sites. Changing the min_out floor is a live-behaviour change that needs its own
measurement, so it belongs with Phase 3's cost model rather than here.

**The env migration manifest covered a prefix, not the configuration.** Task
0.5 accounted for "every legacy `ARBOT_*` variable" — 84 entries — and its test
scanned for that literal prefix. The code reads **192** distinct environment
variables. The 132 outside the prefix had no recorded destination, including
`PRIVATE_KEY`, `ALCHEMY_KEY`, `TAX_EXCHANGE_API_KEY`, and `EDGE_SLIPPAGE_BPS`
— the ceiling on the tolerance band above. A manifest that is complete over a
prefix and silent about everything else is worse than an obviously partial one,
because nothing reads as missing.

Now 250 entries with two new destinations: `Observability` (metrics, logs,
alerting, P&L accounting) and **`Secret`**, which is not a migration
destination at all but a prohibition — a credential never becomes a plain
`ApexConfig` field and never appears in a log, a metric label or a snapshot
(§43, INV-46). The scanner now unions two strategies, because neither is
sufficient alone: the call-form scan cannot see a variable read through a local
helper (`base_fast` has `f("ARBOT_COST_GAS_BPS", 5.0)`), and the prefix scan
cannot see anything outside the prefix.

## 4.8 Preserved engineering capital — explicit register

The following are **not** to be recreated from scratch. Any task that reimplements one of these without a documented v4 incompatibility is a plan violation.

| Capital | Where | Why it is capital |
|---|---|---|
| Exact CL mathematics | `cl_math.rs` | Faithful v3-core port with deliberate non-saturating arithmetic on the pricing path |
| Tick processing | `cl_ticks.rs`, `cl_swap.rs` | Bitmap word decode, `liquidityNet` decode, `TickLadder`; measured 0 bps over 1–7 crossings |
| Simulation primitives | `sim_revm.rs`, `sim_quorum.rs` | REVM 20 pinned-fork with three fixed production bugs; block-pinned cross-endpoint veto |
| Pool reconstruction | `live_state.rs`, `log_decode.rs`, `pool_store.rs` | Complete `Sync`/`Swap`/`Mint`/`Burn` decode incl. Pancake's distinct topic0 |
| State trust machinery | `state_gate.rs`, `cl_parity_gate.rs`, `state_validation.rs`, `reconcile.rs`, `validation_select.rs` | Two independent fail-closed gates + off-hot-path validation. Encodes the `0xc211…b3f3` discovery. |
| Ordering / reorg detection | `continuity.rs` | Correct lexicographic `Ordinal`; documented refusal to infer missing logs |
| ABI handling | `abi.rs`, `scripts/gen_abi_rs.sh`, `tests/abi_v2.rs` | Generated bindings with a round-trip encoding test |
| Chain/RPC infrastructure | `rpc_failover.rs`, `util::connect_ws_provider_with_fallbacks`, `ingestion.rs` | Solved a measured 40-day zero-trade outage |
| Base fast-path infrastructure | `base_fast.rs` | Verified preconfirmed feed; atomic dirty-set drain; `FlashFeed` abstraction for the Denim migration |
| Accounting | `accounting.rs` | CSV trade log / daily summary / event log with USD conversion |
| Deployment tooling | `ArbitrageCloneFactory.sol`, `script/Deploy.s.sol`, `scripts/check_deploy_env.sh`, `test/Deploy*.t.sol` | Atomic deploy-and-init with CREATE2 collision coverage |
| Foundry tests | `test/MultiVenueArbExecutor.t.sol` (~60 tests) | Adversarial callback-sender, repayment, profit-split and encoding coverage |
| Operational tooling | `scripts/shadow/*`, `scripts/fork/*`, `scripts/data/*`, `src/bin/*` | Shadow runs, fork fixtures, inventory verification, parity sweeps |
| Prometheus / metrics | `metrics.rs` (~78 series), `docs/grafana/`, `prometheus.yml` | Live, scraped, dashboarded |
| Venue adapters (as quoters) | `quote_univ3/slipstream/univ2/solidly/cl` | Working exact or near-exact venue pricing for the admitted Base set |
| Known-good fixtures | `tests/integration_smoke.rs`, `scripts/fork/create_arbitrage_fixture.sh`, `contracts/mocks/*` | Real fork and unit fixtures |
| Production lessons in code | comments in `cl_parity_gate.rs`, `rpc_failover.rs`, `sim_revm.rs`, `sim_quorum.rs`, `validation_select.rs`, `continuity.rs`, `main.rs:2717` | Each documents a specific production failure. **Migrate the comments with the code.** |
| Production lessons in measurement | §1.1 findings 1–5 | Shape the entire phase order of this plan |

## 4.9 Legacy architecture that must die — explicit register

| Legacy architecture | Verdict | When it becomes unreachable | When it may be deleted |
|---|---|---|---|
| **Old orchestration** — `Runner`/`RunnerConfig` god-object, `scan_once` as primary driver, `wait_for_scan_cadence` | **REPLACED** by the `apex-runtime` control plane | Phase 8, when the event-driven path carries 100% of live tickets | Phase 17 |
| **Obsolete planning assumptions** — hop-count-as-complexity, `EV = profit − gas`, infinitesimal-rate-implies-profit | **REPLACED** by §13 `ComplexityCost`, §2 scenario-conditioned `J(a\|I)`, §12.3 finite-size search | Phase 3 (cost), Phase 9 (scenario EV) | Phase 17 |
| **Superseded chain priorities** — `chain_hot_pool_base_cap(&str)`, `derive_chain_event_sampling_rate(&str)`, Base-only `token_seeds` | **ADAPTED** into `ChainExecutionAdapter` + per-chain profiles | Phase 15 | Phase 17 |
| **Outdated execution paths** — `plan.rs` legacy `Plan` encoding, `start`/`startLegacy` contract entry points | **REPLACED** by commitment-verified `PlanV3` | Phase 5 (contract), Phase 7 (off-chain) | Phase 17, after one full canary cycle on the new path |
| **Unsafe submission paths** — public-mempool default, jitter, parallel relay blast without an EV justification, acknowledgement-as-inclusion | **REPLACED** by §24 `SubmissionController` (private-first, §24.4 no probabilistic spam, §24.8 ack ladder) | Phase 7 | Phase 17 |
| **Global mutable state** — `Published<T> = Arc<Mutex<Option<Arc<T>>>>`, `DashMap` live state as shared truth, `Arc<RwLock<Vec<…>>>` hot-pool vectors | **REPLACED** by `Versioned<T>` over `ArcSwap` + immutable snapshots (§5.3) | Phase 1 | Phase 2 |
| **Serial bottlenecks** — single signer/nonce stream, serialized `prepare_candidate → simulate → dispatch`, FIFO candidate handling | **REPLACED** by `SignerPool` multi-lane + §46.2 concurrent workers + §29.4 priority scheduling (FIFO forbidden on the capture path) | Phase 6 | Phase 17 |
| **Stale economic assumptions** — `FeeEstimate` as total cost, gas_limit ≡ gas_used, flat min-profit thresholds, USD marks admitting trades | **REPLACED** by §23 `TotalExecutionCost`, §23.4 separation, §37 dynamic thresholds, §2.10 USD isolation | Phase 3 | Phase 17 |
| **Obsolete strategy modules** — `bridge.rs`, `sandwich.rs`, JIT ops, `MevRole` | **REMOVED** (§42 and the §18 strategy stack) | Phase 5 (contract ops), Phase 17 (Rust) | Phase 17 |
| **Duplicate systems** — `contracts/artifacts/**` vs `out/`, 30+ `.bak.<epoch>` config/ops files, 15+ `data/**/*.bak*`, 3 undeclared binaries, `src/integration_smoke.rs` vs `tests/integration_smoke.rs` | **REMOVED** / consolidated | Phase 0 | Phase 0 |
| **Configuration plane** — 84 runtime `ARBOT_*` lookups | **REPLACED** by immutable `ApexConfig` (§2.4 forbids late configuration lookup) | Phase 1 | Phase 17 |

---

# 5. Target repository architecture

## 5.1 Workspace layout

The repository becomes a Cargo workspace. The root `Cargo.toml` becomes a virtual manifest; `src/` is emptied into crates over Phases 0–8. **Git history is preserved by using `git mv` for every relocation** so that `git log --follow` continues to work on migrated files.

```text
arbot-main2/
├── Cargo.toml                      # [workspace] virtual manifest
├── PLAN.md                         # this document
├── APEX_MEV_v4_Final_Architect_Blueprint.md
├── Makefile                        # workspace-aware
├── foundry.toml
├── rust-toolchain.toml
├── .github/workflows/ci.yml        # NEW — gates from scripts/ci/*
│
├── crates/
│   ├── apex-types/                 # shared types; zero I/O; no async
│   ├── apex-config/                # immutable validated configuration
│   ├── apex-math/                  # exact AMM mathematics; zero I/O; no async
│   ├── apex-state/                 # state model, branches, ingestion, patching, trust
│   ├── apex-venues/                # VenueAdapter trait + per-venue adapters
│   ├── apex-search/                # candidate generation + route frontier
│   ├── apex-econ/                  # sizing, allocation, cost model, EV, flash routing
│   ├── apex-sim/                   # simulation tiers 0–3
│   ├── apex-risk/                  # risk engine, circuit breakers, capital
│   ├── apex-capture/               # Capture Assurance Controller, tickets, signers, dispatch
│   ├── apex-exec/                  # transaction construction, commitment, ABI encoding
│   ├── apex-chain/                 # ChainExecutionAdapter trait + per-chain impls + RPC
│   ├── apex-obs/                   # metrics, missed-opportunity, coverage audit, P&L
│   ├── apex-strategy/              # strategy modules A–F
│   ├── apex-runtime/               # control plane wiring  [bin: apex]
│   └── apex-tools/                 # operational binaries
│
├── contracts/
│   ├── core/                       # ExecutionAuth, RouteValidator, ProfitInvariant, FlashSourceRouter
│   ├── adapters/                   # Aave, Morpho, UniV3, UniV4, Aerodrome, Slipstream, Pancake
│   ├── chains/                     # BaseArbExecutor, EthereumArbExecutor, …
│   ├── libraries/                  # FullMath, TickMath, LiquidityAmounts  (KEEP)
│   ├── utils/                      # AccessController, ReentrancyGuard     (KEEP)
│   ├── deploy/                     # ArbitrageCloneFactory                 (KEEP)
│   └── mocks/                      # test doubles (minus MockBridge)
│
├── test/                           # Foundry tests
├── tests/                          # Rust workspace integration tests
├── script/                         # Foundry deploy scripts
├── scripts/{ci,fork,shadow,data}/  # KEEP
├── config/                         # cleaned JSON5 + registry
├── ops/
│   ├── inputs.yaml                 # single human-edited source
│   └── observability/              # prometheus.yml, grafana dashboards
├── data/                           # versioned, checksummed pool inventories + manifest
└── docs/
    ├── apex/                       # v4 docs: BASELINE, INVARIANTS, GATES, INFRA, RUNBOOK
    ├── superpowers/                # KEEP — explains why state modules look as they do
    └── legacy/                     # superseded ARBOT docs, retained read-only
```

## 5.2 Crate responsibilities, and which existing modules survive into each

### `apex-types` — shared vocabulary
No I/O, no async, no provider types. Everything below is `Clone + Debug + Serialize + Deserialize` and independently unit-testable (§51).

**Contains (all NEW):** `ChainId`, `StateVersion`, `StateFingerprint`, `StateBranchId`, `PoolId`, `VenueId`, `TokenId`, `RouteCommitment`, `Candidate`, `OpportunityTicket`, `TicketStatus`, `TerminalFailure`, `ExecutionCommitment`, `TotalExecutionCost`, `SimulationResult`, `SimulationTier`, `RiskDecision`, `SubmissionDecision`, `FlashSourceQuote`, `CertificateStatus`, `MissReason`, `TransactionLifecycleStage`, `PnlAttribution`.

**Absorbs:** nothing. This crate is written first precisely so every other crate depends on a stable vocabulary rather than on `main.rs` structs.

### `apex-config` — immutable configuration
**New responsibility:** parse `ops/inputs.yaml` + `config/*.json5` + `config/registry.json` + secrets-only env, validate completely at boot, and emit one immutable `Arc<ApexConfig>` stamped with a `config_version`. **After boot, no code reads the environment or the filesystem for configuration.** This is the mechanism that eliminates C-06/B-4.

**Absorbs:** `ops_inputs.rs` (ADAPT), `registry.rs` (ADAPT), `config_validation.rs` (ADAPT), the 84 `ARBOT_*` lookups (REBUILD into typed fields).

### `apex-math` — exact mathematics
No I/O, no async, no provider. Every engine implements the §11 contract: `quote_exact()`, `next_state_exact()`, `fee_exact()`, `rounding_exact()`, `revert_conditions()`, `state_dependencies()`.

**Absorbs (KEEP):** `cl_math.rs`, `cl_swap.rs`, `cl_ticks.rs`, `quote_univ2.rs`, `quote_solidly.rs`, `math.rs`, `cl_parity_gate.rs`.
**Absorbs (ADAPT):** `cl_sim.rs`, `quote_curve.rs`, `quote_balancer.rs`, `quote_common.rs`.

### `apex-state` — canonical + speculative state
**New responsibility:** the §5 state architecture — `CanonicalState`, `SpeculativeStateTree`, versioned immutable snapshots, the patch engine, dependency indexes, feed integrity and gap recovery.

**Absorbs (KEEP):** `state_gate.rs`, `state_validation.rs`, `reconcile.rs`, `validation_select.rs`, `log_decode.rs`.
**Absorbs (ADAPT):** `live_state.rs`, `continuity.rs`, `ingestion.rs`, `base_fast.rs` (feed + dirty set), `pool_store.rs`, `liquidity_cache.rs`.
**Absorbs (REBUILD):** `Published<T>` → `Versioned<T>`; `token_refresh.rs` → `tokens/` classifier.

### `apex-venues` — venue adapters
**New responsibility:** the §8.3 `VenueAdapter` trait and one implementation per admitted venue, each owning its state dependencies, exact quote, call graph, gas model, revert taxonomy and encoding. Each adapter has an independent circuit breaker (§8.2) and independent fuzz/differential suites (§43).

**Absorbs (ADAPT):** `quote_univ3.rs`, `quote_slipstream.rs`, `quote_cl.rs`, `discovery.rs`.
**Absorbs (REBUILD):** `venue_adapter.rs` (wrong trait, no implementors), `quote_univ4.rs` (stub → full §10 engine).

### `apex-search` — candidate generation
**New responsibility:** the §46.3 precomputed route frontier as the primary structure, with §12's four production engines (A incremental negative-cycle, C finite-size, D event templates, E backrun prediction) feeding it. Engine B (Hermes) is **not built** until §12.2 miss-rate instrumentation proves a material recall gap — §52 forbids paying for candidate count.

**Absorbs (ADAPT):** `graph.rs`, `cycle_index.rs`, `hot_path.rs`, `mempool.rs` (→ typed `StateEvent`s).
**Absorbs (REBUILD):** `backrun_state.rs`.

### `apex-econ` — economics
**New responsibility:** §14 exact sizing with mandatory discrete refinement, §15/§16 allocation with validity labelling, §17 conflict graph and bounded packing, §19 flash-source routing, §23 `TotalExecutionCost`, §2 scenario-conditioned EV, §29 compute economics.

**Absorbs (ADAPT):** `sizing.rs`, `convex.rs`, `flash_loan.rs`, `hot_pools.rs` (→ compute priority), `util::NativePrice`.
**Absorbs (REBUILD):** `fees.rs` → `cost/`.

### `apex-sim` — simulation
**New responsibility:** the §20 tier hierarchy behind one `Simulator` trait, plus §36 adversarial perturbation harnesses and §34 fidelity scoring.

**Absorbs (KEEP):** `sim_revm.rs`, `sim_quorum.rs`.
**New:** Tier 0 analytic filter, Base `eth_simulateV1` backend (§24.6), Tier 3 adversarial, `SimulationFidelity`.

### `apex-risk` — risk engine
**Absorbs (ADAPT):** `risk_policy.rs`, `CircuitBreaker` (extracted from `main.rs`), `capital.rs`.
**New:** §28.1 graduated response ladder, §28.2 loss classification, §44 failure containment.

### `apex-capture` — Capture Assurance Controller
**New responsibility, and the heart of v4.** §2.5 ticket lifecycle, §2.8 resource reservation, §27.5 `SignerPool` with independent lanes, §29.4 priority scheduling and preemption, §24.5 last-mile dispatch, §24.6 revalidation, §24.8 acknowledgement ladder, §25 duplicate suppression, §46.1 recovery and terminal reconciliation, durable ticket journal.

**Absorbs (ADAPT):** `NonceManager` (→ one lane), `health.rs` (→ per-lane health).
**Absorbs (REBUILD):** the entire `dispatch_call` relay stack.

### `apex-exec` — transaction construction
**New responsibility:** §25 deterministic `ExecutionCommitment`, executor calldata encoding, §26.2 route-validator input assembly, calldata-size optimisation for §23.3.

**Absorbs (ADAPT):** `abi.rs`, `util::{apply_slippage, encode_univ3_path}`.
**Absorbs (REBUILD):** `plan.rs`.

### `apex-chain` — chain execution adapters
**New responsibility:** the §4 ten-method `ChainExecutionAdapter`, with Base (§4.1, §22), Ethereum (§4.2), BSC (§4.5), Arbitrum (§4.3, runtime ordering-mode discovery), OP (§4.4) implementations.

**Absorbs (KEEP):** `rpc_failover.rs`.
**Absorbs (ADAPT):** `util::connect_ws_provider_with_fallbacks`, `base_fast.rs` Base-specific feed pieces.
**Absorbs (REBUILD):** `chain.rs` (config struct → adapter).

### `apex-obs` — observability
**Absorbs (ADAPT):** `metrics.rs`, `accounting.rs`.
**New:** §33 missed-opportunity ledger with the full reason taxonomy, §2.7 `CoverageAuditor`, §32 capture-assurance metric family, P&L attribution by route/venue/strategy/chain/optimization-layer.

### `apex-strategy` — strategy modules
Strategy A (triangular), B (short multi-hop), C (event-driven backrun), D (liquidation), E (correlated), F (finite-size inventory mismatch), each behind a common trait with its own resource budget (§18.4) and circuit breaker.

**Absorbs (ADAPT):** `liquidations.rs`.
**REMOVED, not absorbed:** `sandwich.rs`, `bridge.rs`.

### `apex-runtime` — control plane  `[[bin]] apex`
The §46 deterministic state machine: event bus, worker pools, the §46.2 concurrency graph, supervision (the existing `spawn_supervised` + `panic = unwind` requirement is preserved and its rationale comment migrated), graceful shutdown with ticket reconciliation.

**Absorbs (REBUILD):** `main.rs`.

### `apex-tools` — operational binaries
`cl_parity`, `ingest`, `cycle_index_stats`, `ws_probe`, plus new `coverage_audit` and `ticket_journal_inspect`. **All declared explicitly** in the crate manifest (fixes B-11).

## 5.3 Solidity target

```text
contracts/
├── core/
│   ├── ExecutionAuth.sol          # roles: executor / configAdmin / owner / treasury   (from AccessController)
│   ├── RouteValidator.sol         # §26.2 pre-call validation + §25 commitment check   NEW
│   ├── ProfitInvariant.sol        # §26.3 multi-asset invariant + §26.4 residue policy NEW
│   ├── AdapterRegistry.sol        # §26.1 target/selector/pool/token allowlists        NEW
│   └── FlashSourceRouter.sol      # §19 approved providers only                        NEW
├── adapters/
│   ├── AaveAdapter.sol            MorphoAdapter.sol
│   ├── UniswapV3Adapter.sol       UniswapV4Adapter.sol
│   ├── AerodromeAdapter.sol       SlipstreamAdapter.sol
│   ├── PancakeAdapter.sol         BalancerAdapter.sol
├── chains/
│   ├── BaseArbExecutor.sol        # the only chain executor until Phase 14
│   └── EthereumArbExecutor.sol    # Phase 14
├── libraries/ utils/ deploy/ mocks/   # KEEP
```

**Deleted:** `_execGeneric`, `_execModule`, `Op.JIT_LP_ADD`, `Op.JIT_LP_REMOVE`, `Op.BRIDGE`, `BridgeLib.sol`, `JitExecutor.sol`, `BridgeExecutor.sol`, `MockBridge.sol`, `contracts/artifacts/**`.
**Under review:** `BatchRouter.sol` (UNKNOWN).

## 5.4 Traits, state models, execution models

| Concept | Trait / type | Crate | Blueprint |
|---|---|---|---|
| Venue behaviour | `trait VenueAdapter` (6 methods) | `apex-venues` | §8.3 |
| Chain behaviour | `trait ChainExecutionAdapter` (10 methods) | `apex-chain` | §4 |
| Pricing | `trait ExactPricingEngine` (6 methods) | `apex-math` | §11 |
| Simulation | `trait Simulator` + `enum SimulationTier` | `apex-sim` | §20 |
| Flash liquidity | `trait FlashSource` + `struct FlashSourceRouter` | `apex-econ` | §19 |
| Strategy | `trait Strategy` | `apex-strategy` | §18 |
| Submission | `trait SubmissionLane` + `struct SubmissionController` | `apex-capture` | §24 |
| State | `StateVersion` → `StateBranch` → `SpeculativeStateTree` | `apex-state` | §5.1 |
| Execution | `Candidate` → `OpportunityTicket` → `ExecutionCommitment` → `TransactionLifecycle` | `apex-types` / `apex-capture` | §2.5, §25, §27.3 |

## 5.5 Persistence

v4 introduces exactly one piece of mandatory durable state: the **ticket journal**, required by §46.1 ("Recovery code must reconcile all in-flight tickets from durable state before new live dispatch is re-enabled").

- **Format:** append-only newline-delimited JSON at `var/tickets/<chain>/<utc-date>.jsonl`, `fsync` on every state transition that crosses `AUTHORIZED`.
- **Why not a database:** a database on the dispatch path is a §2.4 "avoidable serialization" and a new failure mode. An append-only local file with `fsync` is sufficient for reconciliation and adds ~µs.
- **Recovery:** on boot, `apex-capture` replays the journal, queries each non-terminal ticket's chain outcome, closes it with a terminal code, and only then enables live dispatch.
- Accounting CSVs (`accounting.rs`) and the missed-opportunity ledger are durable but **not** on the dispatch path.

## 5.6 New v4 components — register

Every component the v4 architecture requires that does **not** exist in the repository today, with its target home, the phase that builds it, and its traceability identifier. The left column uses the canonical names from Blueprint §51 and the task mandate.

| v4 component | Exists today? | Crate / file | Phase | BP |
|---|---|---|---|---|
| `StateVersion` | No | `apex-types/src/state.rs` | 0 | BP-170 |
| `StateBranch` / `StateBranchId` | No | `apex-types/src/state.rs`, `apex-state/src/branch.rs` | 0, 1 | BP-037 |
| `StateFingerprint` | No | `apex-types/src/state.rs` | 0 | BP-038 |
| `SpeculativeStateEngine` (realized as `SpeculativeStateTree`) | No | `apex-state/src/branch.rs` | 1 | BP-002, BP-041 |
| `ChainExecutionAdapter` | No — `chain.rs` is a config struct, not an adapter | `apex-chain/src/adapter.rs` | 7 | BP-030 |
| `VenueAdapter` | No — `venue_adapter.rs` is a 34-LOC shell with zero implementors and the wrong method set | `apex-venues/src/adapter.rs` | 2 | BP-053 |
| `ExactPricingEngine` | Partially — the mathematics exists, the contract does not | `apex-math/src/engine.rs` | 2 | BP-059 |
| `FlashSourceRouter` (off-chain) | Partially — `flash_loan.rs` selects, but without the §19.1 quote fields | `apex-econ/src/flash/router.rs` | 3 | BP-087 |
| `FlashSourceRouter.sol` (on-chain) | No | `contracts/core/FlashSourceRouter.sol` | 5 | BP-088 |
| `ScenarioConditionedEV` | No — today's EV is a scalar `profit − gas` | `apex-econ/src/ev/scenario.rs` | 3, 9 | BP-007, BP-013, BP-014 |
| `CostModel` (`TotalExecutionCost`) | No — `FeeEstimate` is `gas_limit × gas_price` | `apex-types/src/cost.rs`, `apex-econ/src/cost/` | 3 | BP-102…105 |
| `Opportunity` (`Candidate`) | Partially — an ad-hoc `SizedCandidate` inside `main.rs` | `apex-types/src/candidate.rs` | 0 | BP-168 |
| `OpportunityTicket` | No | `apex-types/src/ticket.rs` | 0, 6 | BP-020, BP-021 |
| `CaptureAssuranceController` | No | `apex-capture/src/{protocol,registry,scheduler}.rs` | 6 | BP-018, BP-172, BP-173 |
| `ExecutionCommitment` | No | `apex-types/src/commitment.rs`, `apex-exec/src/commitment.rs`, `contracts/core/RouteValidator.sol` | 0, 5 | BP-116 |
| `SignerPool` | No — one `LocalWallet`, one nonce stream | `apex-capture/src/signer/pool.rs` | 6 | BP-127, BP-128 |
| `NonceManager` | Partially — exists for a single lane inside `main.rs` | `apex-capture/src/nonce.rs` | 6 | BP-124 |
| `SubmissionController` | No — `dispatch_call` is an inline relay stack | `apex-capture/src/dispatch/router.rs` | 6, 7 | BP-106…111 |
| `CoverageAuditor` | No | `apex-obs/src/coverage.rs` | 8 | BP-023 |
| `CompetitorModel` | No — `CompetitionSnapshot` in `main.rs` tracks EMA win rate only | `apex-sim/src/competitor/` | 9 | BP-094…097 |
| `RiskDecision` | Partially — `risk_policy.rs` returns booleans, not a typed decision | `apex-types/src/risk.rs`, `apex-risk/src/policy.rs` | 0, 6 | BP-129 |
| `TransactionLifecycle` | No — acknowledgement is conflated with submission | `apex-capture/src/dispatch/ack.rs` | 6, 7 | BP-115, BP-125 |
| `P&LAttribution` | Partially — `accounting.rs` records trades but attributes to nothing | `apex-types/src/pnl.rs`, `apex-obs/src/attribution.rs` | 0, 8 | BP-149 |
| `AdaptiveChainAllocator` | No | `apex-econ/src/allocator/{mod,chain_score}.rs` | 15 | BP-001, BP-028, BP-029, BP-178 |
| **Additional components the blueprint requires beyond the mandate's list** | | | | |
| `FeedIntegrity` / `FeedArbiter` | No | `apex-state/src/feed/{integrity,arbiter}.rs` | 1 | BP-043, BP-044 |
| `Versioned<T>` | No — `Published<T>` is a `Mutex` | `apex-state/src/versioned.rs` | 1 | BP-040 |
| `TokenSemanticsClassifier` / `TokenRiskFingerprint` | No | `apex-state/src/tokens/` | 1 | BP-049, BP-050 |
| `RouteTemplate` frontier | Partially — `cycle_index` + `hot_path` carry two of eight attributes | `apex-search/src/frontier.rs` | 2 | BP-176 |
| `DiscreteSize` | No | `apex-econ/src/sizing/discrete.rs` | 3 | BP-068 |
| `CertificateStatus` / improving-path check | No | `apex-econ/src/allocation/{certificate,improving_path}.rs` | 12 | BP-072, BP-073 |
| `SharedPoolCluster` | No | `apex-econ/src/allocation/cluster.rs` | 10 | BP-071 |
| `ConflictGraph` / bounded packing | No | `apex-econ/src/packing/` | 12 | BP-076…078 |
| `FlashblockScheduler` / `MeasuredCapacityModel` | No | `apex-chain/src/base/flashblock.rs` | 7 | BP-098…100, BP-114 |
| `AdapterRegistry.sol` / `RouteValidator.sol` / `ProfitInvariant.sol` | No | `contracts/core/` | 5 | BP-118…121 |
| `MissLedger` / `MissRecord` | No | `apex-obs/src/miss.rs` | 8 | BP-150 |
| `SimulationFidelity` scorer | No | `apex-sim/src/fidelity.rs` | 4 | BP-151 |
| `RiskPosture` ladder / `LossClass` | No | `apex-risk/src/{posture,loss}.rs` | 6 | BP-130, BP-131 |
| Ticket journal (durable state) | No | `apex-capture/src/journal.rs` | 6 | BP-174 |
| `ApexConfig` (immutable, versioned) | No — 84 runtime env reads | `apex-config/src/lib.rs` | 0 | BP-166 |
| `Secret<T>` | No | `apex-config/src/secret.rs` | 0 | BP-166 |

## 5.7 Configuration, observability, testing, deployment

- **Configuration:** `ops/inputs.yaml` (human-edited) + `config/` (venue/registry) + env (secrets only) → validated → `Arc<ApexConfig>` with `config_version`. A config change requires a process restart or an explicit, audited hot-reload that produces a *new* immutable snapshot; in-flight tickets keep the snapshot they were admitted under.
- **Observability:** Prometheus exporter (existing), the §32 metric families, structured JSON events, the missed-opportunity ledger, and P&L attribution. Grafana dashboards move to `ops/observability/`.
- **Testing:** per §15 of the task mandate and §35/§36 of the blueprint — unit, property, fuzz, differential, integration, fork, simulation, adversarial, failure-injection, performance, canary. Detailed in §29.
- **Deployment:** Foundry clone-factory deploy (existing, tested) extended for the new contract set; a single `apex` binary per host; configuration and secrets injected at start; no vendored infrastructure tarballs in-tree.

---

# 6. Dependency graph

## 6.1 True build order

Derived from actual dependencies, **not** blueprint section order. Arrows are "must exist before".

```text
                      apex-types
                          │
          ┌───────────────┼───────────────┐
          ▼               ▼               ▼
     apex-config     apex-math       apex-obs(metrics core)
          │               │               │
          └───────┬───────┘               │
                  ▼                       │
            apex-state  ◄─────────────────┘
        (ingest → continuity → patch → branch → fingerprint → dep index)
                  │
        ┌─────────┴─────────┐
        ▼                   ▼
   apex-venues          apex-chain(rpc, adapter trait)
        │                   │
        └─────────┬─────────┘
                  ▼
            apex-search  (frontier, negative cycle, finite-size, events)
                  │
                  ▼
             apex-econ   (sizing → allocation → cost → flash → EV)
                  │
                  ▼
             apex-sim    (T0 → T1 → T2 → T3)
                  │
                  ▼
             apex-risk   (gate, breaker, capital)
                  │
                  ▼
            apex-exec    (commitment, encoding)
                  │
                  ▼
          apex-capture   (ticket → reserve → revalidate → sign → dispatch → ack → reconcile)
                  │
                  ▼
         apex-chain(base impl)  ──►  apex-strategy
                  │                        │
                  └───────────┬────────────┘
                              ▼
                        apex-runtime
                              │
                              ▼
                         apex-tools
```

Cycle-free by construction. `apex-obs` is depended on by nearly everything but depends only on `apex-types`, so it never creates a cycle.

## 6.2 Runtime data-flow order (the capture path)

This is the §57.1 mandatory capture protocol expressed as the hot path. Every arrow is an observable, idempotent transition (§46.3).

```text
MARKET STATE EVENT                    apex-state/feed
      ↓
STATE VERSION VERIFIED                apex-state   (fingerprint + reconstruction_status=VERIFIED)
      ↓
DEPENDENCY INVALIDATION               apex-state/dep_index
      ↓
AFFECTED ROUTE FRONTIER REVALUATED    apex-search/frontier      ← fast path
      ↓
ANALYTIC FILTER (Tier 0)              apex-sim/tier0
      ↓
EXACT PRICING                         apex-math via apex-venues
      ↓
FINITE-SIZE EXACT SIZING              apex-econ/sizing  (continuous warm start → discrete wei)
      ↓
ALLOCATION / SPLITTING                apex-econ/allocation      (Phase 10+)
      ↓
FLASH SOURCE ROUTING                  apex-econ/flash
      ↓
TOTAL COST MODEL                      apex-econ/cost + apex-chain adapter
      ↓
EXACT SIMULATION (Tier 1 → Tier 2)    apex-sim
      ↓
ADVERSARIAL ROBUSTNESS (Tier 3)       apex-sim/adversarial      (Phase 9+)
      ↓
SCENARIO-CONDITIONED EV / RISK GATE   apex-econ/ev + apex-risk
      ↓
OPPORTUNITY TICKET CREATED            apex-capture               ◄── journal fsync
      ↓
EXECUTION RESOURCES RESERVED          apex-capture/reserve  (signer lane, nonce, sim slot, dispatch lane, gas, flash feasibility, authorization)
      ↓
LAST-MILE REVALIDATION                apex-capture/revalidate
      ↓
SIGNER LANE ASSIGNED                  apex-capture/signer
      ↓
SIGNED COMMITMENT                     apex-exec + apex-capture
      ↓
PARALLEL APPROVED DISPATCH            apex-capture/dispatch → apex-chain/<chain>/submit
      ↓
TRANSPORT ACK → NODE KNOWN → SEQUENCER/BUILDER OBSERVED
      ↓
PRECONFIRMATION / INCLUSION           apex-chain/<chain>/observe
      ↓
FINAL RECONCILIATION                  apex-capture/reconcile     ◄── journal close
      ↓
REALIZED NET P&L                      apex-obs/pnl
      ↓
CAPTURE + MISS ATTRIBUTION            apex-obs/coverage + apex-obs/miss
      ↓
MODEL / RESOURCE RECALIBRATION        apex-econ/compute + apex-chain/allocator
```

## 6.3 Where this ordering differs from the blueprint's section order, and why

| Change | Reason |
|---|---|
| `apex-config` is built before `apex-state` | The env-var configuration plane (C-06) must die before any new hot-path module is written, or every new module inherits the anti-pattern. |
| Cost model (§23) moves **before** simulation (§20) | Tier 0's analytic filter needs a real cost estimate to reject; a Tier 0 built on `FeeEstimate` would have to be rewritten. |
| Capture assurance (§46) is built **before** the Base adapter's submission path (§24) | The controller owns the dispatch contract; building submission first would produce a dispatch path that then has to be retrofitted into tickets. |
| Risk (§28) precedes `apex-exec` | The risk gate is what promotes a candidate to a ticket; encoding a candidate that risk would have rejected is wasted capture capacity (§29.4). |
| Hermes (§12.2) is **not** in the graph | §52: "If adding another graph algorithm only increases candidate count, do not prioritize it." Gated behind measured recall loss (Phase 8 coverage auditor). |
| V4 exact engine (§10) after first profitable trade | The measured tradeable set (§1.1 finding 4) is 8 cross-venue cheap pairs on venues that are already exactly priced. V4 unlocks surface but does not unblock the first dollar. |
| Adversarial simulation (§21, §36) after first profitable trade | Tier 3 raises *margin quality*, not *capture ability*. The first trade is gated on a conservative robustness margin instead, which is cheaper and available immediately. |

---

# 7. Critical data contracts

These are the exact types later phases reference by name. Defined in `crates/apex-types/src/`. All are `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]` unless noted. No type here performs I/O.

**Primitive types are `alloy-primitives`** (`B256`, `Address`, `U256`, `I256`) — see §2.2 C-10. `alloy-primitives` 0.8.26 is already in `Cargo.lock` via `revm` 20, so this adds no dependency. `apex-types::compat` holds the **only** `ethers` ↔ `alloy` conversion boundary in the workspace, and Task 0.3a round-trip property-tests it. `arb-exec-legacy` keeps `ethers` until Phase 17.

## 7.1 Identity and state

```rust
// apex-types/src/ids.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChainId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PoolId { pub chain: ChainId, pub address: Address }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VenueId(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenId { pub chain: ChainId, pub address: Address }
```

```rust
// apex-types/src/state.rs   — Blueprint §5.2, §45 "State version"
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StateVersion(pub u64);            // monotonic within a process

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StateBranchId(pub u32);           // 0 = canonical

/// Blueprint §5.2. Every field is REQUIRED; there is no `Default`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StateFingerprint {
    pub chain_id: ChainId,
    pub parent_block_hash: B256,
    pub confirmed_block_number: u64,
    pub preconf_sequence: Option<u64>,
    pub flashblock_index: Option<u32>,
    pub state_root_or_equivalent: Option<B256>,
    pub block_hash_if_available: Option<B256>,
    pub state_delta_hash: B256,
    pub venue_state_version: BTreeMap<VenueId, u64>,
    pub external_dependency_fingerprint: Option<B256>,
}

/// Blueprint §5.6 — the provenance every usable state version carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateProvenance {
    pub parent_state_id: Option<StateVersion>,
    pub fingerprint: StateFingerprint,
    pub feed_source_id: FeedSourceId,
    pub sequence_range: (u64, u64),
    pub first_seen_ns: u64,
    pub last_seen_ns: u64,
    pub canonicality: Canonicality,
    pub reconstruction: ReconstructionStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Canonicality { Confirmed, Preconfirmed, Speculative, Orphaned }

/// §5.6: only `Verified` may authorize a live ticket. No `Default`, no `Unknown`
/// that silently reads as safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconstructionStatus { Verified, Rebuilding, Unsafe }
```

## 7.2 Candidate and opportunity

```rust
// apex-types/src/candidate.rs   — Blueprint §45 "Candidate"
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub candidate_id: CandidateId,
    pub chain_id: ChainId,
    pub strategy: StrategyId,
    pub venue_set: Vec<VenueId>,
    pub route: RouteCommitment,
    pub state_fingerprint: StateFingerprint,
    pub state_age: Duration,
    pub flash_source: Option<FlashSourceQuote>,
    pub input_amount: U256,
    pub expected_output: U256,
    pub gross_profit: U256,
    pub dex_fees: U256,
    pub flash_fee: U256,
    pub total_execution_cost: TotalExecutionCost,
    pub expected_net_profit: I256,            // signed: a candidate may be negative
    pub robust_ev: I256,
    pub certificate_status: CertificateStatus,
    pub simulation_tier: SimulationTier,
    pub capture_probability: f64,
    pub robustness_margin: f64,
    pub deadline: Instant,
    pub submission_policy: SubmissionPolicy,
}

/// §16.2 — never silently promote a heuristic allocation to "optimal".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CertificateStatus { Proven, Heuristic, InvalidForCertification }

/// §13 — hop count is NOT the complexity metric.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteCommitment {
    pub hops: Vec<RouteHop>,
    pub complexity_cost: ComplexityCost,
    pub route_hash: B256,                     // keccak over the normalized hop list
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComplexityCost {
    pub hops: u8,
    pub external_calls: u16,
    pub calldata_bytes: u32,
    pub state_deps: u16,
    pub tick_crossings: u32,
    pub hooks: u8,
    pub gas_estimate: u64,
    pub failure_surface: f64,
}
```

## 7.3 Opportunity Ticket — the capture-assurance object

```rust
// apex-types/src/ticket.rs   — Blueprint §2.5
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpportunityTicket {
    pub ticket_id: TicketId,
    pub chain_id: ChainId,
    pub strategy: StrategyId,
    pub state_fingerprint: StateFingerprint,
    pub route_commitment: RouteCommitment,
    pub exact_input: U256,
    pub expected_net_ev: I256,
    pub robustness_margin: f64,
    pub validity_start: Instant,
    pub dispatch_deadline: Instant,
    pub target_execution_window: ExecutionWindow,
    pub signer_lane: Option<SignerLaneId>,
    pub nonce: Option<u64>,
    pub flash_source: Option<FlashSourceQuote>,
    pub simulation_result_hash: B256,
    pub submission_policy: SubmissionPolicy,
    pub required_gas_limit: u64,
    pub created_at: SystemTime,
    pub status: TicketStatus,
}

/// §2.5 — MONOTONIC. `TicketStatus::advance` is the only mutator and it refuses
/// any transition that is not forward along this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TicketStatus {
    Observed, Reserved, Exacting, Simulated, Authorized, Signed,
    Dispatching, Acknowledged, Preconfirmed, Included, Finalized, Reconciled,
}

/// §2.5 terminal loss states. Every one carries the cause — §46.1 forbids an
/// unclassified outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TerminalFailure {
    Stale { observed_age: Duration },
    StateChanged { expected: StateFingerprint, actual: StateFingerprint },
    EvCollapsed { admitted: I256, revalidated: I256 },
    RiskRejected { rule: &'static str },
    DispatchTimeout { deadline: Instant, elapsed: Duration },
    NonceUnavailable { lane: SignerLaneId },
    SignerUnavailable,
    SubmissionRejected { lane: SubmissionLaneId, detail: String },
    CompetitorWon { observed_tx: Option<B256> },
    Reverted { revert_class: RevertClass, data: Bytes },
    Diverged { branch: StateBranchId },
}

/// The closed-world outcome. §46.1: exactly one of these, always.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TicketOutcome {
    Success { stage: TicketStatus, realized: PnlAttribution },
    ExplicitFailure { code: TerminalFailure, at: SystemTime,
                      state: StateFingerprint, cause: String },
}
```

## 7.4 Execution commitment

```rust
// apex-types/src/commitment.rs   — Blueprint §25
/// keccak256 over ALL critical trade parameters. The executor recomputes this
/// on-chain and reverts on mismatch; the off-chain signer never signs a payload
/// whose recomputed commitment differs from the ticket's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionCommitment {
    pub chain_id: ChainId,
    pub executor_address: Address,
    pub executor_version: u32,
    pub venue_fingerprints: BTreeMap<VenueId, B256>,
    pub flash_source: FlashProviderId,
    pub state_fingerprint_hash: B256,
    pub route_hash: B256,
    pub exact_inputs: Vec<U256>,
    pub min_profit: U256,
    pub slippage_constraints: Vec<u32>,       // bps per hop
    pub deadline: u64,                        // unix seconds
    pub submission_policy: SubmissionPolicy,
}

impl ExecutionCommitment {
    pub fn hash(&self) -> B256;               // keccak256(abi.encode(...))
}
```

## 7.5 Cost

```rust
// apex-types/src/cost.rs   — Blueprint §23.1
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TotalExecutionCost {
    pub l2_execution_fee: U256,
    pub l1_data_fee: U256,
    pub priority_fee: U256,
    pub builder_payment: U256,
    pub sequencer_payment: U256,
    pub flash_fee: U256,
    pub dex_fees: U256,
    pub expected_failure_cost: U256,
    pub calldata_bytes: u32,
    pub compressed_data_estimate: u32,
    /// §23.4 — the SCHEDULING variable. Never used as a cost.
    pub gas_limit: u64,
    /// §23.4 — the COST variable, as a distribution, never a point estimate.
    pub gas_used_distribution: GasDistribution,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GasDistribution { pub p50: u64, pub p90: u64, pub p99: u64, pub max_observed: u64 }

impl TotalExecutionCost {
    /// Conservative total used by the risk gate: p99 gas, full failure cost.
    pub fn conservative_total(&self, gas_price: U256) -> U256;
}
```

## 7.6 Flash liquidity

```rust
// apex-types/src/flash.rs   — Blueprint §19.1
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlashSourceQuote {
    pub provider: FlashProviderId,
    pub asset: TokenId,
    pub amount: U256,
    pub premium: U256,
    pub gas_overhead: u64,
    pub callback_constraints: CallbackConstraints,
    pub availability_probability: f64,
    pub state_dependencies: Vec<PoolId>,
    pub reliability_score: f64,
}
```

## 7.7 Simulation and risk

```rust
// apex-types/src/sim.rs   — Blueprint §20
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SimulationTier { Tier0Analytic, Tier1LocalExact, Tier2FullEvm, Tier3Adversarial, Tier4Canary }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimulationResult {
    pub tier: SimulationTier,
    pub success: bool,
    pub revert: Option<(RevertClass, Bytes)>,
    pub gas_used: u64,
    pub balance_deltas: BTreeMap<TokenId, I256>,
    pub loan_repaid: bool,
    pub profit_invariant_held: bool,
    pub token_residues: BTreeMap<TokenId, U256>,
    pub state_after: StateFingerprint,
    pub simulated_at_state: StateFingerprint,
    pub result_hash: B256,
    pub elapsed: Duration,
}

// apex-types/src/risk.rs   — Blueprint §28
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RiskDecision {
    Admit { size_multiplier: f64 },
    AdmitReduced { size_multiplier: f64, reason: &'static str },
    Reject { reason: MissReason, rule: &'static str },
}

/// §28.1 graduated response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskPosture { Normal, ReducedSize, HighEvOnly, StrategyDisabled, ChainDisabled, GlobalHalt }
```

## 7.8 Missed-opportunity taxonomy

```rust
// apex-types/src/miss.rs   — Blueprint §33. EXHAUSTIVE: no catch-all variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MissReason {
    LowEv, StaleState, TooSlow, CompetitorWon, SimFail, RiskFail, GasFail,
    L1DataCostFail, NoFlashLiquidity, VenueDisabled, ConflictRejected,
    PackingNotWorthwhile, EarliestFlashblockTooLate, HookModelIncomplete,
    BuilderRejected, SequencerRejected, NonceUnavailable,
}

/// §33 record. Written for EVERY economically attractive but unexecuted candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MissRecord {
    pub candidate_id: CandidateId,
    pub state_fingerprint: StateFingerprint,
    pub simulated_ev: I256,
    pub estimated_capture_probability: f64,
    pub reason: MissReason,
    pub submission_policy: SubmissionPolicy,
    pub later_realized_outcome: Option<ObservedOutcome>,
}
```

## 7.9 P&L attribution

```rust
// apex-types/src/pnl.rs   — Blueprint §32 "Attribution"
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PnlAttribution {
    pub ticket_id: TicketId,
    pub chain: ChainId,
    pub strategy: StrategyId,
    pub venues: Vec<VenueId>,
    pub route_hash: B256,
    pub optimization_layers: Vec<OptimizationLayer>,   // which layers touched this trade
    pub gross_profit: I256,
    pub realized_cost: TotalExecutionCost,
    pub net_profit_token: I256,
    pub net_profit_usd_bounds: (f64, f64),             // §2.10 [V_low, V_high]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptimizationLayer {
    SinglePath, ParallelSplit, JointAllocation, CrossCyclePacking, EventDriven,
    Liquidation, Correlated, V4Route, ChainSubmissionOpt, ComputeScheduling,
}
```

## 7.10 Contract-side data contract (Solidity)

```solidity
// contracts/core/Types.sol
struct PlanV3 {
    uint256  chainId;
    uint32   executorVersion;
    bytes32  commitment;        // §25 — recomputed and checked on entry
    Loan[]   loans;             // §26.3 multi-asset
    Step[]   steps;
    Debt[]   debts;             // per-asset repayment requirement
    uint256  minProfit;
    address  profitToken;
    uint64   deadline;
    uint16   cycleSlippageBps;
}

struct Debt { address asset; uint256 principal; uint256 flashFee; uint256 requiredReturn; }
struct Step { uint8 adapterId; bytes32 poolKey; bytes payload; }   // NO free-form target/calldata
```

`Step` carries an `adapterId` resolved through `AdapterRegistry`, never a raw address+calldata pair. This is the structural fix for **B-1/C-01**.

---

# 8. Critical invariants

Every invariant below is mapped to implementation, test, monitor, failure response and acceptance gate. **No invariant may exist only in documentation** (task mandate §14). `docs/apex/INVARIANTS.md` is generated from this table in Phase 0 and CI fails if an invariant here has no matching test name in the workspace.

## 8.1 Capture assurance (zero tolerance)

| ID | Invariant | Implementation | Test | Monitor | Failure response | Gate |
|---|---|---|---|---|---|---|
| **INV-01** | Every admitted live ticket reaches exactly one terminal outcome (success or `ExplicitFailure`). Silent expiry is forbidden. (§2.4, §46.1, §57.1.1, gates 26/36) | `apex-capture::TicketRegistry` + journal; `Drop` on a non-terminal ticket panics in debug and records `ExplicitFailure{cause:"dropped"}` in release | `types::ticket_status_is_monotonic` + `types::the_full_lifecycle_walks_forward` (type-level, Phase 0 ✅); `capture::ticket_always_terminates` (property, Phase 6) | `tickets_admitted` − (`tickets_terminal_success` + `tickets_terminal_failure`) must be 0 | Halt new live tickets on the affected chain; reconcile | G-CAP-1 |
| **INV-02** | `ticket_drop_count == 0` (§29.6) | Registry accounting; no ticket removal path except terminal close | `capture::no_drop_under_preemption` (failure injection) | counter | Global halt on that chain | G-CAP-1 |
| **INV-03** | `unexplained_pre_dispatch_expiry == 0` (§29.6) | Deadline scheduler closes expiring tickets with `DispatchTimeout{deadline, elapsed}` before the deadline passes | `capture::expiry_is_always_explained` | counter | Expand signer lanes / shed load | G-CAP-1 |
| **INV-04** | `nonce_reuse == 0` (§29.6) | Per-lane `NonceManager` with exclusive reservation; a nonce is released only on terminal close | `capture::nonce_never_reused` (loom model over the lane state machine) | counter | Quarantine the lane, reconcile its tickets | G-SIGN-1 |
| **INV-05** | `wrong_chain_submission == 0` (§29.6) | `chain_id` in `ExecutionCommitment`, checked in last-mile revalidation *and* on-chain | `capture::chain_id_mismatch_refuses_to_sign`; `forge` `testWrongChainReverts` | counter | Global halt | G-SIGN-1 |
| **INV-06** | `commitment_mismatch == 0` (§29.6, §25) | Signer recomputes `ExecutionCommitment::hash()` from the payload it is about to sign and compares to the ticket | `exec::commitment_is_stable_under_reencode` (property); `forge` `testCommitmentMismatchReverts` | counter | Refuse to sign; `EvCollapsed`/`StateChanged` close | G-SOL-1 |
| **INV-07** | `unauthorized_adapter_call == 0` (§29.6, §26.1) | `AdapterRegistry` allowlist of (adapter, target, selector, pool, token); no free-form call path exists in the contract | `forge` `testUnknownAdapterReverts`, `testUnlistedTargetReverts`, `testUnlistedSelectorReverts`, `testNoArbitraryCallSurfaceExists` (bytecode scan for `CALL` outside adapters) | counter + on-chain event | Emergency pause | G-SOL-1 |
| **INV-08** | `critical_state_gap_used_for_trade == 0` (§5.6, §29.6, gate 33) | A ticket may be admitted only from a state with `ReconstructionStatus::Verified`; the type system enforces it (`VerifiedState` newtype is the only admission input) | `types::only_verified_state_may_authorize_a_live_ticket` (type-level, Phase 0 ✅); `state::gap_blocks_ticket_admission` (runtime, Phase 1) (compile-fail test) | counter | Stop dependent tickets, backfill, verify, resume | G-STATE-1 |
| **INV-09** | No FIFO on the capture path; an authorized live ticket is never preempted (§29.4, §57.1.2) | Priority scheduler with the §57.1.2 preemption order; `Authorized` and later statuses are exempt from preemption | `types::authorized_and_later_are_never_preemptible` (type-level, Phase 0 ✅); `capture::authorized_ticket_never_preempted` (runtime property, Phase 6) | `capture_path_utilization`, `signer_lane_utilization` | Shed research → slow search → low-confidence sims → low-EV | G-CAP-2 |
| **INV-10** | No duplicate economically distinct attempts for one opportunity unless the submission model proves positive incremental EV and the nonce/replacement policy accounts for it (§46.1) | Transport redundancy sends the *same signed bytes*; economically distinct duplicates require an explicit policy flag | `capture::redundant_transport_sends_identical_bytes` | `submission_failover_rate`, duplicate counter | Disable the offending lane | G-SUB-1 |

## 8.2 State correctness

| ID | Invariant | Implementation | Test | Monitor | Failure response | Gate |
|---|---|---|---|---|---|---|
| **INV-11** | No global mutable state is shared truth between search workers; workers receive immutable snapshots or versioned read handles (§5.3) | `Versioned<T>` over `ArcSwapOption`; `#![forbid]` lint + a CI grep that fails on `Mutex<Option<Arc<` and on `&mut` state crossing a worker boundary | `state::snapshot_is_immutable` (compile-fail); CI `scripts/ci/no_shared_mutable_state.sh` | — | Build fails | G-STATE-1 |
| **INV-12** | A feed gap is never silently converted to "probably unchanged" (§5.6) | `FeedIntegrity` tracks `expected_sequence`; a gap sets `ReconstructionStatus::Unsafe` on the affected branch | `state::sequence_gap_marks_unsafe`; `state::duplicate_and_out_of_order_are_distinguished` | `gap_count`, `out_of_order_count`, `duplicate_count`, `recovery_time_ms` | Mark UNSAFE → stop tickets → backfill → verify → resume | G-STATE-1 |
| **INV-13** | Two disagreeing feeds are resolved by parentage/sequence/reconciliation/rebuild — **never** by majority vote (§5.6) | `FeedArbiter::resolve` has no vote path; it returns `Rebuild` when parentage is ambiguous | `state::contradicting_feeds_never_vote` | `state_fingerprint_mismatch` | Full state rebuild | G-STATE-1 |
| **INV-14** | Speculative branches that diverge from the final block are rolled back and impacted candidates re-evaluated (§5.4) | `SpeculativeStateTree::commit_or_rollback` on each confirmed block | `state::divergent_branch_rolls_back`; `state::rollback_invalidates_dependent_candidates` | `preconf_to_final_divergence_rate`, `state_branch_reorg_count`, `rollback_cost_ms`, `candidate_invalidated_by_divergence` | Reduce reliance on preconf state; raise margin | G-STATE-2 |
| **INV-15** | A pool with no trust verdict is not trusted (both gates fail closed) (existing design; §40 gate 9) | `cl_parity_gate` + `state_gate`, unchanged behaviour | existing tests, migrated; `math::cold_start_trusts_nothing` | `live_state_trusted_pools` / `untrusted_pools` | Fall back to on-chain quoting | G-PRICE-1 |

## 8.3 Pricing and economics

| ID | Invariant | Implementation | Test | Monitor | Failure response | Gate |
|---|---|---|---|---|---|---|
| **INV-16** | No router quote is authoritative; the exact integer state machine is (§9, §10.3) | `ExactPricingEngine::quote_exact` is the only pricing input to sizing; quoter calls exist only inside `cl_parity_gate`/validation | CI grep: no quoter call reachable from `apex-econ` | — | Build fails | G-PRICE-1 |
| **INV-17** | An engine not proven exact produces **candidate-only** output (§11) | `ExactQuote { exactness: Proven / Approximate }`; the risk gate rejects `Approximate` for live dispatch | `math::approximate_engine_cannot_authorize`; per-venue differential suites | `venue_exactness` gauge | Venue → shadow-only | G-PRICE-1 |
| **INV-18** | The final size is an exact integer/wei candidate verified by exact AMM evaluation; a continuous optimum is never an execution dependency (§14.3) | `sizing::refine_discrete` is mandatory and returns `DiscreteSize`, the only type `Candidate::input_amount` accepts | `types::discrete_size_requires_a_refinement_witness` (type-level, Phase 0 ✅); `econ::discrete_refinement_never_worse_than_continuous` (property, Phase 3) | `continuous_size` vs `final_discrete_size` | — | G-ECON-1 |
| **INV-19** | Gas limit (scheduling) and gas used (cost) are never conflated (§23.4) | Separate fields with distinct types: `GasLimit(u64)` and `GasUsed(u64)`; no `From` conversion between them | `types::gas_limit_and_gas_used_are_distinct_types` + `scripts/ci/no_gas_conversion.sh` (Phase 0 ✅) | both exported | Build fails | G-ECON-1 |
| **INV-20** | USD marks may never admit a trade on their own; execution validity holds in token units under the conservative bound (§2.10, gate 32) | EV gate takes token-denominated profit; USD enters only `PnlAttribution` and resource ranking, always as `(low, high)` | `types::usd_bounds_expose_the_conservative_end_by_name` (type-level, Phase 0 ✅); `econ::usd_mark_cannot_flip_admission` (property, Phase 3) | `net_profit_usd` bounds | — | G-ECON-1 |
| **INV-21** | A heuristic allocation is never promoted to "optimal" (§16.2) | `CertificateStatus` is set by `allocation::certify`, which returns `InvalidForCertification` when §16.1's validity predicate fails | `econ::shared_pool_coupling_invalidates_certificate`; `econ::improving_move_denies_proven` | `certificate_status` by count | Fall back to single-path | G-ALLOC-1 |
| **INV-22** | Two routes consuming the same pool are never independently optimized and summed (§15.3) | `SharedPoolCluster` detection is mandatory before any multi-route allocation | `econ::shared_pool_forces_joint_transition` | `active_pools`, cluster count | Split into separate tickets | G-ALLOC-1 |
| **INV-23** | Candidate eligibility requires the full §2.3 conjunction, not just `EV > 0` | `EligibilityGate::evaluate` returns `Reject` with the failing clause named | `econ::eligibility_requires_every_clause` (table test over all 9 clauses) | rejection histogram by clause | — | G-ECON-1 |

## 8.4 Solidity settlement

| ID | Invariant | Implementation | Test | Monitor | Failure response | Gate |
|---|---|---|---|---|---|---|
| **INV-24** | Only the authorized caller can execute (§35.3) | `ExecutionAuth.onlyExecutor` | `forge` `testOnlyExecutorCanStart` | on-chain event | — | G-SOL-1 |
| **INV-25** | Only an approved provider can call back (§35.3) | Per-provider callback-sender check + `ctxHash` binding (**existing, KEEP**) | `forge` `testErc3156RevertsWhenCallbackSenderMismatch` and siblings (existing) | — | — | G-SOL-1 |
| **INV-26** | Only approved venues/pools/tokens/selectors can be called (§26.1, §26.2) | `AdapterRegistry` + `RouteValidator` | `forge` `testUnlisted{Target,Selector,Pool,Token}Reverts` | `unauthorized_adapter_call` | Emergency pause | G-SOL-1 |
| **INV-27** | Per-debt-asset repayment: `Balance_after,j ≥ Debt_j + FlashFee_j + RequiredReturn_j` (§26.3) | `ProfitInvariant.assertMultiAsset(Debt[] memory)` | `forge` `testMultiAssetInvariantHolds`, `testPartialRepaymentReverts` (fuzz over asset count and amounts) | revert-class counter | — | G-SOL-1 |
| **INV-28** | `Profit_realized ≥ MinimumProfit` in the designated profit asset (§26.3) | `ProfitInvariant.assertProfit` | `forge` `testProfitBelowMinimumReverts` (existing, extended) | — | — | G-SOL-1 |
| **INV-29** | Unaccounted residue cannot create silent loss (§26.3, §26.4) | Terminal state must be all-debt-repaid + required-profit + **exactly zero** allowed residue, or a declared residue path with deterministic accounting | `forge` `testUnaccountedResidueReverts`, `testDeclaredResiduePathAccountsExactly` (fuzz) | residue counter | — | G-SOL-1 |
| **INV-30** | Wrong chain cannot execute (§35.3) | `if (p.chainId != block.chainid) revert` | `forge` `testWrongChainReverts` | — | — | G-SOL-1 |
| **INV-31** | Expired routes revert (§35.3) | `if (block.timestamp > p.deadline) revert` | `forge` `testExpiredDeadlineReverts` | — | — | G-SOL-1 |
| **INV-32** | Loan is always repaid or the entire transaction reverts (§35.3) | Provider callback path has no success exit without repayment | `forge` `testRepaymentShortfallReverts` (existing, extended to multi-asset) | — | — | G-SOL-1 |
| **INV-33** | No arbitrary-call surface exists in deployed bytecode (§26.1) | `_execGeneric`/`_execModule` deleted; adapters have fixed targets from the registry | `forge` `testNoArbitraryCallSurfaceExists` + `scripts/ci/check_no_generic_call.sh` (source + bytecode scan) | build gate | Build fails | G-SOL-1 |

## 8.5 Submission and lifecycle

| ID | Invariant | Implementation | Test | Monitor | Failure response | Gate |
|---|---|---|---|---|---|---|
| **INV-34** | Acknowledgement is not inclusion; all seven stages are separately observable (§24.8, gate 34) | `TransactionLifecycleStage` enum with per-stage timeout and escalation | `capture::ack_does_not_imply_inclusion`; `capture::each_stage_has_a_timeout` (table) | per-stage counters and latencies | Escalate per stage | G-SUB-1 |
| **INV-35** | Last-mile revalidation runs immediately before signing and covers the §24.6 list (chain id, executor fingerprint, critical pool state, hook fingerprint, nonce ownership, fee ceiling, gas eligibility, signer balance, flash availability, deadline, min profit) | `revalidate::last_mile` returns `Revalidated` (a token required by `sign`) | `capture::sign_requires_revalidation_token` (compile-fail); `capture::each_revalidation_check_can_reject` (table over all 11) | revalidation rejection histogram | Re-simulate or invalidate — **never** blind dispatch | G-SUB-1 |
| **INV-36** | No public leakage by default; no probabilistic spam; no uncommitted gas-burning probes (§24.4) | `SubmissionPolicy::Public` requires an explicit per-route configuration opt-in; no probe path exists | `capture::public_is_not_default`; CI grep for probe patterns | `submission_policy` distribution | — | G-SUB-1 |
| **INV-37** | Gas limit is the smallest value `≥ G_safe = Q_gas(simulated) + headroom` that remains valid and fits the earliest economically valuable window; if none exists the ticket is rejected **before signing** (§24.7) | `gas::choose_limit` returns `Option<GasLimit>`; `None` closes the ticket with `EarliestFlashblockTooLate` | `chain::base_gas_limit_minimization`; `chain::no_safe_limit_rejects_pre_sign` | `earliest_eligible_flashblock` vs `actual_flashblock_index` | Reject pre-sign | G-BASE-1 |
| **INV-38** | Once a Flashblock is built its ordering is locked; no "pay more later, land earlier" logic exists (§22.3) | The Base scheduler only ever computes a *future* eligible index | `chain::no_retroactive_flashblock_entry` (property) | — | — | G-BASE-1 |
| **INV-39** | On restart/reconnect/replacement/provider failure, every outstanding transaction is reconciled before new live dispatch (§57.1.4) | Boot-time journal replay gate | `capture::boot_blocks_dispatch_until_reconciled` (integration, kill -9 mid-flight) | `inflight_unreconciled` gauge | Dispatch stays disabled | G-CAP-1 |

## 8.6 Risk, coverage, observability

| ID | Invariant | Implementation | Test | Monitor | Failure response | Gate |
|---|---|---|---|---|---|---|
| **INV-40** | Every economically attractive but unexecuted candidate receives a machine-readable reason code (§33, gate 36) | `MissLedger::record` is called on every rejection path; `MissReason` has no catch-all | `types::miss_reason_is_exhaustive_and_labelled` (taxonomy, Phase 0 ✅); `obs::every_rejection_path_records_a_miss` (runtime, Phase 8) | miss counts by reason | — | G-OBS-1 |
| **INV-41** | Hot-path discovery recall is continuously measured against a delayed independent broad-search oracle (§2.7, gate 30) | `CoverageAuditor` running on reserved slow-path resources | `obs::auditor_detects_injected_miss` (inject a known opportunity the fast path cannot see) | `hot_path_recall`, `high_EV_miss_rate`, `route_class_miss_rate`, `coverage_audit_lag` | Expand templates / raise slow-path budget / disable the class | G-OBS-1 |
| **INV-42** | A risk trigger produces a graduated response, never an unhandled continue (§28.1) | `RiskPosture` ladder with an exhaustive transition table | `types::risk_posture_ladder_orders_by_severity_and_gates_live_tickets` (type-level, Phase 0 ✅); `risk::every_trigger_maps_to_a_posture` (Phase 6) | `risk_posture` gauge | ladder | G-RISK-1 |
| **INV-43** | Every loss is classified into one of the §28.2 classes | `LossClass` enum, exhaustive | `types::loss_class_is_exhaustive` (taxonomy, Phase 0 ✅); `risk::every_loss_is_classified` (runtime, Phase 6) | loss counts by class | Class over-frequency tightens its gate | G-RISK-1 |
| **INV-44** | Simulation fidelity is scored per strategy/venue and out-of-band scores force reduce-size / raise-tier / disable (§34) | `FidelityScorer` fed by every executed trade and credible canary | `sim::fidelity_breach_triggers_action` | `F_sim` per strategy×venue | reduce → raise tier → disable | G-SIM-1 |
| **INV-45** | No strategy receives live trust from historical backtests alone (§34) | Promotion requires measured realized P&L in the go/no-go checklist; backtest results are not an accepted input | process gate in `docs/apex/GATES.md` + a checklist item in the promotion script | — | — | G-PROD-1 |
| **INV-46** | Private keys and credentials never enter logs, metrics or telemetry (§43) | `Secret<T>` newtype whose `Debug`/`Display`/`Serialize` redact; the `sim_rpc_url` credential-carrying field keeps its existing "keep out of logs" discipline | `secret::serialize_redacts` + `secret::debug_redacts` + `secret::display_redacts` (Phase 0 ✅); `scripts/ci/secret_scan.sh` in CI | — | Build fails | G-SEC-1 |

---

# 9. State architecture

Blueprint §5. Crate: `apex-state`. This is the first thing built after types and config because everything downstream is only as correct as the state it reads.

## 9.1 Layers

```text
CanonicalState (branch 0)
   ├── ConfirmedState        — finalized blocks
   └── SpeculativeStateTree
          ├── Flashblock / pending branch 0..N   (Base)
          ├── Target-event branch                (backrun victim applied)
          └── Candidate-local execution branch   (our own trade applied)
```

Each branch is a persistent (structurally shared) map from `PoolId` to `PoolState`, built on immutable snapshots. A branch never mutates a parent; it records deltas. `SpeculativeStateTree::commit_or_rollback(confirmed_fingerprint)` compares the arriving confirmed block against each speculative branch and either promotes or discards it (§5.4).

## 9.2 Patch engine (§5.3)

```text
incoming tx / log / flashblock
        ↓ classify state mutation        (log_decode — KEEP)
        ↓ identify pools/contracts       (pool index)
        ↓ apply delta to immutable snapshot
        ↓ update affected dependency closure
        ↓ publish new StateVersion       (Versioned<T> swap, not Mutex)
```

The existing `base_fast` dirty set becomes the trigger for "identify pools"; its atomic-swap drain property is preserved and tested (`state::drain_is_lossless_under_concurrent_append`, adapting the existing `loom-model` feature).

## 9.3 What changes versus today

| Today | v4 |
|---|---|
| `DashMap<Address, PoolState>` mutated in place | Immutable snapshot per `StateVersion`; readers hold an `Arc` |
| `Published<T> = Arc<StdMutex<Option<Arc<T>>>>` | `Versioned<T>` over `ArcSwapOption<T>` carrying `(StateVersion, StateProvenance)` |
| `Ordinal { block, tx_index, log_index }` | `Ordinal { payload_id, flashblock_index, block, tx_index, log_index }` — the field order *is* the comparison order, exactly as `continuity.rs` already documents |
| Trust is per-pool (`state_gate`, `cl_parity_gate`) | Per-pool trust **plus** per-branch `ReconstructionStatus` |
| No sequence accounting | `FeedIntegrity { feed_sequence, last, expected, gap_count, out_of_order_count, duplicate_count, reconnect_count, recovery_time_ms, feed_freshness_ms }` |
| A gap is invisible | A gap sets the branch `Unsafe`; `VerifiedState` cannot be constructed from it (INV-08) |

## 9.4 Dependency indexes (§5.5)

Maintained incrementally, never rebuilt per scan:

```text
pool → token pair | routes | candidate cycles | state variables | venue adapter | active branch versions
token → pools | cycles
cycle → pools | loan assets | conflict resources
candidate → state fingerprint | simulation state | submission state | execution commitment
```

`cycle_index.rs`'s `cycles_touching(pair)` is the existing implementation of `pool → candidate cycles` and is retained.

## 9.5 Feed integrity and gap recovery (§5.6)

```text
sequence gap detected
      ↓ mark affected branch ReconstructionStatus::Unsafe
      ↓ stop admitting new tickets dependent on that branch
      ↓ parallel backfill / state reconstruction
      ↓ verify state fingerprint
      ↓ ReconstructionStatus::Verified → resume
```

Two independent transports per capture-critical chain where available. **Redundancy repairs transport failure; it never merges contradictory states.** Disagreement is resolved by parentage, sequence continuity, explicit reconciliation, or full rebuild — never majority vote (INV-13).

## 9.6 Red/blue migration for state reconstruction

State reconstruction is high-consequence, so it follows the §11 red/blue pattern:

1. **Blue** = existing `live_state` + `state_gate` path, unchanged, still authoritative.
2. **Red** = new `apex-state` versioned path, running in parallel on the same feed.
3. **Differential** = `state::differential_harness` compares, per pool per block, blue's snapshot against red's snapshot and records divergence bps + tick delta into `docs/apex/reports/state-diff-<date>.csv`.
4. **Verification gate** = 72 h continuous run, zero unexplained divergences, red's `gap_count`/`recovery_time_ms` within band.
5. **Traffic migration** = `state.authority = red` in `ApexConfig`; blue keeps running as the differential oracle.
6. **Retirement** = blue removed in Phase 17 after 14 days of green.

---

# 10. Market graph / venue architecture

Blueprint §6, §7, §8. Crates: `apex-search` (graph), `apex-venues` (adapters), `apex-state` (tokens).

## 10.1 The graph is a candidate generator, not the execution model (§6)

Edge weight `w_uv = −ln(r_uv^eff)` and negative-cycle extraction remain exactly as `graph.rs` implements them. The **finite-size warning (§6.2) is made structural**: a `NegativeCycle` type cannot be converted into a `Candidate` except through `apex-econ::sizing`, which returns `None` when no profitable size exists. This is the type-level encoding of the repository's own measured finding that 96% of infinitesimally-negative cycles have no profitable size.

## 10.2 Pool admissibility (§6.3)

`PoolRecord` is extended to carry the full §6.3 record. A pool missing any field is **not admitted**:

```rust
pub struct PoolAdmission {
    pub address: Address,                      // verified: extcodehash non-empty
    pub venue: VenueId,                        // verified: deployed by the venue's factory
    pub tokens: (TokenId, TokenId),            // verified: read from the pool
    pub decimals: (u8, u8),                    // verified: read from the tokens
    pub fee_behavior: FeeBehavior,             // Static(ppm) | Dynamic{source} | Hook{address}
    pub reconstruction: ReconstructionMethod,  // which logs rebuild this pool's state
    pub depth_estimate: DepthEstimate,         // non-authoritative external TVL is marked as such
    pub gas_profile: GasProfile,               // measured, not assumed
    pub revert_profile: RevertProfile,
    pub transfer_semantics: (TransferSemantics, TransferSemantics),
    pub update_mapping: Vec<B256>,             // topic0s that mutate this pool
}
```

`scripts/data/verify_pool_venues.py` (existing) already checks pool-against-factory and becomes a mandatory step in the inventory pipeline. **Low-liquidity rejection is a configurable economic threshold, never a magic number** (§6.3) — the measured depth floor from the existing sweep becomes the initial value with its provenance recorded.

## 10.3 Token admission and ERC-20 semantics (§7)

`token_refresh.rs` (36 LOC) is REBUILT into `apex-state/src/tokens/`:

- **`classifier.rs`** — classifies each token as standard / fee-on-transfer / rebasing / blacklist-enabled / pauseable / non-standard-approve / decimals-anomalous / permit-anomalous, by on-chain probing on a fork plus bytecode heuristics.
- **`fingerprint.rs`** — `TokenRiskFingerprint { address, code_hash, proxy_implementation, transfer_semantics, allowance_semantics, decimals, pause_state, admin_metadata }`. A material code or behaviour change invalidates every affected route (§7.3).
- **Rule (§7.2):** for non-standard tokens, expected transfer output is derived from **actual balance deltas in simulation**, never nominal transfer amounts. Enforced by `SimulationResult::balance_deltas` being the only accepted output source for such tokens.

Seed universe is the v3 Base list (WETH, USDC, cbBTC, cbETH, wstETH, AERO, DAI, USDS, EURC) and remains dynamic via §7.1 measured thresholds.

## 10.4 Venue adapter contract (§8.3)

```rust
// apex-venues/src/adapter.rs
pub trait VenueAdapter: Send + Sync {
    fn venue_id(&self) -> VenueId;
    fn identify_state_dependencies(&self, pool: &PoolId) -> StateDeps;
    fn quote_exact(&self, state: &StateSnapshot, order: &Order) -> Result<ExactQuote>;
    fn simulate_call_graph(&self, candidate: &Candidate) -> Result<CallGraph>;
    fn gas_model(&self, candidate: &Candidate) -> GasModel;
    fn classify_revert(&self, data: &[u8]) -> RevertClass;
    fn encode_exact(&self, candidate: &Candidate) -> Result<EncodedAction>;
}
```

`ExactQuote` carries `exactness: Exactness::{Proven, Approximate}` (INV-17). **No adapter may hide a meaningful economic assumption from the core engine** (§8.3) — enforced by `gas_model` and `classify_revert` being required methods with no default implementation.

## 10.5 Venue admission gate (§8.2)

A venue reaches production only after **all** of: deployment verified (extcodehash) → ABI verified → pool discovery verified → state reconstruction verified → exact pricing differential-tested → gas profile measured → revert taxonomy implemented → adapter fuzz-tested → multi-day shadow → micro-canary where economically justified. Every venue gets an **independent circuit breaker**.

Initial Base universe (§8.1): Aerodrome, Aerodrome Slipstream, Uniswap V3, Uniswap V4 (Phase 11), PancakeSwap, Aave V3 (flash liquidity). Existing quoters cover all but V4.

## 10.6 Route topology policy (§13)

Base/default: 3 hops preferred, 4 allowed, >4 exceptional. Ethereum: 3 preferred, 4 allowed, 5 exceptional. **But hop count is not the complexity metric** — `ComplexityCost` (§7.2) is, and the admission comparison is on `ComplexityCost`, not `hops`. This matters concretely here: the repository's census found 3- and 4-hop routes strictly worse than 2-hop on the cheap frontier, and `ComplexityCost` is the structure that lets the engine learn that from measurement instead of from a constant.

---

# 11. Exact pricing implementation

Blueprint §9, §10, §11. Crate: `apex-math`. This is where the repository's best asset lives, so the plan's job here is mostly *not to break it*.

## 11.1 The `ExactPricingEngine` contract (§11)

```rust
pub trait ExactPricingEngine {
    type State: Clone;
    fn quote_exact(&self, s: &Self::State, o: &Order) -> Result<ExactQuote>;
    fn next_state_exact(&self, s: &Self::State, o: &Order) -> Result<Self::State>;
    fn fee_exact(&self, s: &Self::State, o: &Order) -> Result<U256>;
    fn rounding_exact(&self) -> RoundingMode;
    fn revert_conditions(&self, s: &Self::State, o: &Order) -> Vec<RevertCondition>;
    fn state_dependencies(&self, s: &Self::State) -> StateDeps;
}
```

Implementors: `UniV3Engine` (KEEP, from `cl_math`/`cl_swap`/`cl_ticks`), `SlipstreamEngine`, `PancakeV3Engine`, `CpmmEngine` (from `quote_univ2`), `SolidlyEngine` (from `quote_solidly`), `CurveEngine` (ADAPT), `BalancerEngine` (ADAPT), `UniV4Engine` (REBUILD, Phase 11).

## 11.2 Uniswap V3-family (§9) — KEEP

Required state: `sqrtPriceX96`, `liquidity`, `current_tick`, `initialized_ticks`, `fee_tier`, fee-growth variables. Outputs: `amount_out`, `fee`, `price_impact`, `crossed_ticks`, `next_state`, `state_dependencies`, `estimated_gas`, `revert_risk`. The existing port reproduces `SqrtPriceMath` integer rounding. **No router quote is authoritative** (INV-16).

## 11.3 Uniswap V4 (§10) — REBUILD, Phase 11

The current 73-LOC fixed-price stub is replaced by a real programmable-pool engine. Required state (§10.1): `PoolKey`, `currency0/1`, fee mode, hook address, hook permissions, pool state, liquidity, initialized ticks, hook code hash, hook-controlled state dependencies, external oracle state if consulted, custom accounting behaviour.

Execution model (§10.2): `beforeSwap → fee modification → core swap → afterSwap → custom accounting / return deltas → final settlement`. `PoolManager` lock/unlock and flash accounting (§10.2, §17 ref) must be reproduced exactly — **intermediate deltas are never mistaken for settled balances**.

Isolation (§10.4): a production V4 route carries hook address, hook fingerprint, hook simulation tier, hook dependency freshness, custom-accounting flag, and native/ERC-20 settlement mode. **If a hook is effectively unmodelled, the route is shadow-only** — enforced by `Exactness::Approximate` + INV-17.

## 11.4 Red/blue migration for exact pricing

Pricing is the highest-consequence replacement after the contract, so:

1. **Blue** = today's call path (`quote_univ3` → quoter RPC with `cl_parity_gate` deciding local vs RPC).
2. **Red** = `apex-math::ExactPricingEngine` on the same state.
3. **Differential** = `crates/apex-math/tests/differential.rs` plus the existing `src/bin/cl_parity.rs` sweep, run over: every admitted pool × {dust, 0.1×depth, 1×depth, 3×depth} × both directions × 0–20 tick crossings. **Three-way**: reference (on-chain quoter) vs Rust exact model vs forked EVM execution (§35.1). Compare amounts, fees, state transition, rounding, reverts.
4. **Gate** = 0 bps divergence on every pool with a `cl_parity_gate` trusted verdict; every divergent pool either explained (like `0xc211…b3f3`) or the venue is shadow-only.
5. **Traffic migration** = `pricing.authority = red`.
6. **Retirement** = blue's RPC quoter path is retained **permanently** as the parity oracle — it is not retired, because `cl_parity_gate` depends on it. This is a deliberate exception to §58.7's retirement step and is recorded as such.

## 11.5 Fuzzing (§35.2)

`crates/apex-math/fuzz/` targets: token amounts (full `U256` range with rejection sampling), tick boundaries (`MIN_TICK`/`MAX_TICK` ±3, every initialized tick ±1), liquidity extremes (0, 1, `u128::MAX`), fee edges (0, 1, 999_999, 1_000_000), rounding boundaries (amounts where `mul_div` remainder is 0 or `denom−1`), hook return values within their legal constraints, adversarial callback ordering.

---

# 12. Search architecture

Blueprint §12, §46.3. Crate: `apex-search`.

## 12.1 The frontier is primary (§46.3)

The system keeps a **resident frontier of executable route templates** for the most active pool neighbourhoods. A market event triggers **revaluation of known routes first**, then broader discovery. This — not another graph algorithm — is the primary mechanism for reducing capture latency.

```rust
pub struct RouteTemplate {
    pub topology: Vec<TokenId>,
    pub venue_sequence: Vec<VenueId>,
    pub fee_variants: Vec<FeeVariant>,
    pub tick_neighborhood: TickNeighborhood,
    pub hook_fingerprint: Option<B256>,
    pub flash_source: FlashProviderId,
    pub expected_gas_class: GasClass,
    pub last_profitable: Option<Instant>,     // from hot_path.rs's recency signal
}
```

`cycle_index.rs` supplies topology + `cycles_touching`; `hot_path.rs` supplies the recency signal; the remaining fields are new.

## 12.2 Engines

| Engine | §12 | Status | Phase |
|---|---|---|---|
| **A** incremental negative-cycle | §12.1 | ADAPT `graph.rs` — incremental Bellman-Ford, Top-K extraction. The graph layer is *allowed* to be approximate because it only proposes. | 2 |
| **B** Hermes structure-aware | §12.2 | **NOT BUILT.** Gated behind measured recall loss from the Phase 8 coverage auditor. §52: adding a graph algorithm that only raises candidate count is explicitly deprioritized. If built, it carries mandatory `Hermes_rank` vs `full_search_rank` vs certificate-outcome instrumentation and loses authority automatically on repeated material misses. | gated |
| **C** finite-size route search | §12.3 | NEW. Searches directly for finite-size improvement: pairwise cross-venue mismatch, k-shortest simple routes, k-shortest cycles, same-pair split, event-targeted, backrun templates, liquidation routes, stable/correlated dislocations. **This is the engine the repository's measurements most demand** — the cheap frontier is a finite-size phenomenon that infinitesimal rates do not express. | 2 |
| **D** event templates | §12.4 | ADAPT `mempool.rs` → typed `StateEvent`. Targets: large swap, liquidity removal/addition, liquidation, oracle-sensitive mutation, stablecoin dislocation, tick transition, hook state mutation, fee-tier/dynamic-fee change. | 2 |
| **E** backrun prediction | §12.5 | REBUILD `backrun_state.rs`. Target tx → predict delta → **exactly simulate target** (Tier 2) → reprice affected closure → search successors. The target tx is never assumed final until the execution regime says it is. | 13 |

## 12.3 Fast path / slow path separation (§2.6)

```text
FAST PATH (never delayed by the slow path)
  pre-materialized route templates → incremental state patch →
  exact affected-pool repricing → finite-size warm starts →
  immediate simulation → immediate signing/dispatch

SLOW PATH (reserved resources, separate worker pool)
  broad graph search → structure rebuilds →
  exhaustive finite-size discovery → counterfactual coverage audit → research
```

Enforced by separate `tokio` runtimes with distinct thread pools and by INV-09's priority scheduler. `state::fast_path_never_blocks_on_slow_path` is a failure-injection test that saturates the slow path and asserts fast-path p99 latency is unchanged.

---

# 13. Optimization and allocation

Blueprint §14, §15, §16, §17. Crate: `apex-econ`.

## 13.1 Exact route sizing (§14)

Objective: `max_{x≥0} [ f_p(x) − Cost_p(x) ]` subject to flash liquidity, pool depth, slippage bounds, loan availability, execution gas, chain fee constraints, deadline and profit floor.

```text
continuous optimum     ← Newton (CPMM, closed form) / Brent (1-D unimodal) / bracketed
       ↓                  [existing sizing.rs machinery — ADAPT]
local neighborhood
       ↓
integer / wei candidates
       ↓
exact AMM evaluation    ← apex-math
       ↓
exact encoded EVM simulation
       ↓
maximum robust EV
```

**INV-18 is the load-bearing change here:** `DiscreteSize` is the only type `Candidate::input_amount` accepts, so a continuous optimum cannot become an execution dependency. This is enforced at compile time, not by review.

## 13.2 Parallel-pool splitting (§15) — Phase 10

`Σ x_i = X`, maximize `Σ f_i(x_i) − C_multi(x)`. KKT `f_i'(x_i) = λ` on genuinely concave segments gives the warm start (existing `convex.rs`). Concentrated-liquidity boundaries are handled by segmenting at tick boundaries: `continuous allocation → tick-boundary discovery → piecewise candidate set → discrete refinement → exact simulation`.

**Shared-pool coupling (§15.3) is mandatory, not optional:** when multiple candidate routes materially consume the same pool state, a `SharedPoolCluster` is formed, inputs are aggregated, and one exact joint state transition is computed. Independently optimizing two paths against the same pool and summing is forbidden (INV-22).

## 13.3 Joint allocation and certification (§16) — Phase 12

Convex formulation is valid **only** when: objective concave in allocation variables ∧ constraints convex ∧ fixed activation costs absent or separately enumerated ∧ shared-pool coupling explicitly represented ∧ no unmodelled discrete hooks/ticks changing the domain.

```rust
pub fn certify(alloc: &Allocation, domain: &Domain) -> CertificateStatus {
    if !domain.satisfies_convexity_preconditions() { return CertificateStatus::InvalidForCertification; }
    match improving_path::search(alloc, domain) {
        Some(_move) => CertificateStatus::Heuristic,     // an improving move exists
        None        => CertificateStatus::Proven,
    }
}
```

Gas-aware rejection (§16.3): a mathematically superior split is rejected when `ΔOutputValue ≤ ΔDEXFees + ΔGas + ΔL1DataFee + ΔFailureCost + ΔInclusionCost`.

## 13.4 Cross-cycle portfolio and bounded packing (§17) — Phase 12

Conflict graph nodes are candidate actions; edges are conflicts: same pool, materially shared token balance, incompatible loan asset, shared liquidity cap, ordering dependency, overlapping state mutation, excessive calldata/gas, **same signer/nonce bottleneck**, **same Flashblock capacity bottleneck**.

Packing rule (§17.3) — **not** `EV_packed > EV_single`, but:

```text
EV_packed^risk-adjusted  >  EV_best_alternative + Margin
```

accounting for increased gas limit, delayed earliest eligibility on Base, larger calldata/L1 data fee, larger revert surface, additional state dependencies, and larger capital/flash requirements.

Search bound (§17.4): top few candidates, small compatible subsets, 2–3 orderings where needed, exact simulation. **No unrestricted global integer/nonlinear program in the hot path** — enforced by a hard subset-size cap and a CI test asserting the enumerated subset count stays below it.

## 13.5 Compute economics (§29) — Phase 8

`Priority(q) = E[Incremental NetUSD(q)] / (EstimatedCPU_ms(q) + RPCCost(q))`, subject to deadlines. Overload sheds in the §29.2 order: protect state ingestion → protect exact simulation for high-EV → protect submission → shed exotic searches → shed low-confidence routes → shed expensive low-hit-rate strategies. No strategy may create an unbounded queue (§29.3): queue deadline, candidate EV floor, max outstanding simulations, max outstanding RPC calls, per-class CPU budget.

---

# 14. Economic engine

Blueprint §2, §19, §23, §37, §38. Crate: `apex-econ`.

## 14.1 Scenario-conditioned EV (§2)

Replaces `EV = P_land · P_state · P_exec · P_net − C_failure`, which wrongly implies independence.

```rust
pub struct Scenario { pub kind: ScenarioKind, pub probability: f64, pub profit: I256 }

pub enum ScenarioKind {
    SameStateImmediate, SameStateOneFlashblockLater, CompetingSamePoolSwapFirst,
    CompetingArbitrageFirst, TargetBackrunStateChanged, DelayedBeyondValidity,
    RouteExecutionSuccess, VenueRevert, FlashLiquidityFailure,
    CostAboveForecast, InclusionRejected, PreconfirmationDivergence,
}

/// J(a|I) = Σ_s P(s|I,a)·Π(a,s) − C_irrecoverable(a)
pub fn scenario_ev(scenarios: &[Scenario], c_irrecoverable: U256) -> I256;
```

Robust gate (§2.1): admit only when `J(a) > 0` **and** `Pr(Π(a,s) > 0) ≥ p_min` **and** the downside distribution is acceptable. High-risk classes additionally require `CVaR_α(Loss(a)) ≤ L_max`.

Until the competitor model exists (Phase 9), the scenario set is the conservative subset {SameStateImmediate, SameStateOneFlashblockLater, CompetingSamePoolSwapFirst, VenueRevert} with pessimistic fixed priors recorded in config and flagged `prior=unmeasured` in telemetry. This is honest and shippable; it is replaced by measured distributions in Phase 9.

## 14.2 Complete cost model (§2.2, §23)

```text
NetProfit = GrossReturn − DEXFees − FlashFee − L2ExecutionFee − L1DataFee
          − PriorityFee − BuilderOrSequencerPayment − FailureCost − ExternalExecutionCost
```

plus opportunity cost of occupying CPU, RPC simulation slots, private-builder capacity and signing/submission slots when candidates compete.

Per-chain fee models (§23.2): OP Stack chains (Base, OP) require explicit L1 data-fee modelling from compressed transaction size and Ethereum base/blob fee conditions — `gasUsed × gasPrice` is insufficient. Arbitrum requires its own data-fee model. The **calldata optimizer** (§23.3) makes calldata layout economically relevant: `route encoding → calldata size → compression estimate → L1 fee → net EV`.

## 14.3 Flash-liquidity routing (§19)

```text
FlashSource* = argmin ( FlashFee + GasOverhead + FailureRiskCost + AvailabilityPenalty )
               subject to required asset and amount
```

Sources: Aave V3, Morpho, venue-native mechanisms where exact and audited, future approved providers. **Addresses come from the canonical address book, not hand-maintained constants** (§19.2). Multi-source fallback prevents a single point of failure (§19.4), but the execution contract supports **only explicitly approved and tested providers** — the `FlashSourceRouter.sol` allowlist.

The existing `flash_loan.rs` capacity bounding (each provider bounded by measured capacity, not cycle appetite) is preserved.

## 14.4 Dynamic opportunity surface (§37)

Rolling distributions per `{chain, strategy, venue, route class, hour, volatility regime, state-age bucket, latency bucket, execution mode}` of candidate EV, capture probability, realized net, failure probability, gas, flash premium, competitor intensity. These set `minimum EV`, `minimum robustness margin`, `maximum state age`, `maximum route complexity` **dynamically** — replacing today's static thresholds.

## 14.5 Profit target framework (§38)

`MonthlyNet = ΣRealizedNet − ΣFailureCosts − InfrastructureCosts`, target `> $25,000`. A credible path requires, across multiple market regimes: sufficient opportunity density, sufficient capture probability, positive realized net P&L, stable simulation fidelity, acceptable failure cost, acceptable infrastructure cost. **No assumed trade count, no assumed average profit, no single hero trade is accepted as evidence.**

---

# 15. Simulation architecture

Blueprint §20, §21, §34, §36. Crate: `apex-sim`.

## 15.1 Tier hierarchy (§20)

| Tier | Purpose | Implementation | Phase |
|---|---|---|---|
| **0 Analytic filter** | Cheap exact/near-exact AMM math, fee checks, coarse cost, rough EV. Reject obvious losers. | NEW `apex-sim/src/tier0.rs` over `apex-math` + `apex-econ::cost` | 4 |
| **1 Local exact** | Versioned reconstructed state + exact integer venue math | `cl_sim.rs` (ADAPT) + `apex-math` | 4 |
| **2 Full EVM** | Exact encoded call graph against the relevant fork/preconf state. Checks success, revert data, gas, balances, loan repayment, profit invariant, token residues, state changes. | **`sim_revm.rs` (KEEP)** + Base `eth_simulateV1` backend + `sim_quorum.rs` (KEEP) | 4 |
| **3 Adversarial** | Perturb future state per empirically observed competitor behaviour | NEW `apex-sim/src/adversarial.rs` | 9 |
| **4 Controlled production validation** | New adapter classes / strategy families / uncertain paths only, where expected information gain justifies cost. **Never a latency technique, never a substitute for simulation.** | canary harness | 8+ |

## 15.2 Base simulation correctness (§24.6)

Blueprint is explicit: do **not** assume a generic `eth_call` against `pending` gives a correct current block-context triple on Base — block-context properties can reflect cached historical context. Capture-critical simulation prefers **`eth_simulateV1` with explicit state/block controls and validation enabled**. `apex-sim/src/backends/base_simulate_v1.rs` is therefore a Phase 4 deliverable, not an optimization, and the existing `eth_call` path becomes the fallback.

## 15.3 Adversarial perturbation harness (§36) — Phase 9

Per strategy class: `+ one competing swap`, `+ larger competing swap`, `+ opposing swap`, `+ target state mutation`, `+ one Flashblock delay`, `+ two Flashblocks delay`, `+ gas estimate error`, `+ L1 fee shock`, `+ different builder outcome`, `+ different PGA round outcome`. **A candidate whose profitability disappears under a very small realistic perturbation is treated as fragile and requires a higher margin** — implemented as `robustness_margin` scaling, not as a binary reject.

## 15.4 Competitor model (§21) — Phase 9

```rust
pub struct CompetitorModel {
    pub arrival_lag_distribution: Histogram,
    pub observed_sizes: Histogram,
    pub opportunity_class_win_rate: BTreeMap<RouteClass, f64>,
    pub state_age_to_capture_curve: Curve,
    pub private_flow_intensity: f64,
    pub builder_acceptance_profile: BTreeMap<BuilderId, f64>,
    pub sequencer_acceptance_profile: f64,
    pub historical_bid_curve: Curve,
    pub ordering_mode: OrderingMode,
    pub observed_reaction_to_target_events: BTreeMap<EventClass, Reaction>,
}
```

Latency buckets (§21.2): 0–20, 20–40, 40–80, 80–120, 120–200, 200–400, 400+ ms — **measurement buckets, not promises**.

**Outcome censoring (§21.3) is mandatory:** a missed trade does not reveal the competitor's action. The model supports `Observation::Censored { opportunity_existed: true, submission_lost: true, competitor_action: None }` and **never fabricates competitor size from missing observations**. Enforced by `CompetitorSize` being `Option` with no default and a test `sim::censored_observation_does_not_impute_size`.

## 15.5 Simulation fidelity and calibration (§34)

For every executed trade or credible canary, record predicted vs realized for gas, amount out, profit, and revert classification, plus state drift and inclusion outcome. `F_sim = g(err_gas, err_balance, err_profit, err_success)` per strategy×venue. Out of band → reduce size → increase simulation tier → disable the affected venue/strategy (INV-44).

## 15.6 Red/blue migration for simulation

**Blue** = `eth_call` through the failover client with `sim_quorum` verification (today). **Red** = `eth_simulateV1` (Base) / REVM Tier 2. **Differential** = both run on every candidate for 7 days; compare success, gas, balance deltas, revert class. **Gate** = agreement on ≥99.9% of candidates with every disagreement explained. **Migration** = `sim.authority = red`. **Retirement** = blue retained as the quorum verifier (deliberate exception, like the pricing oracle).

---

# 16. Capture Assurance Controller

Blueprint §2.4–2.9, §29.4–29.6, §46.1, §57. Crate: `apex-capture`. **This is a core dependency, not a later enhancement.** It is built in Phase 6, in shadow, before any live dispatch exists in the v4 path.

## 16.1 Position in the control plane

```text
EV / RISK GATE
    ↓
OPPORTUNITY TICKET          ← journal fsync; ticket is now an ASSET with a deadline
    ↓
RESOURCE RESERVATION        ← signer lane, nonce, sim slot, dispatch lane, gas reserve,
    ↓                          flash feasibility, executor authorization, state-read capacity
LAST-MILE REVALIDATION
    ↓
SIGNER LANE
    ↓
DISPATCH ROUTER             ← parallel across economically approved lanes
    ↓
ACK MONITOR                 ← transport → node → sequencer/builder
    ↓
PRECONF / INCLUSION MONITOR
    ↓
OUTCOME / EXPLICIT FAILURE  ← journal close; exactly one terminal state
```

The controller **owns the invariant that no admitted ticket can disappear silently** (INV-01).

## 16.2 The mandatory capture protocol (§46.1)

Implemented literally as an 11-step state machine in `apex-capture/src/protocol.rs`:

```text
 1. LOCK opportunity commitment
 2. RESERVE signer + nonce lane
 3. RESERVE simulation / RPC / dispatch capacity
 4. REVALIDATE only critical mutable state
 5. SIGN exact committed payload
 6. DISPATCH through all economically approved lanes in parallel
 7. OBSERVE transport / node / sequencer / builder acknowledgement
 8. OBSERVE preconfirmation / inclusion
 9. IF state changes before inclusion: reprice or explicitly abandon
10. RECONCILE receipt, balances, debt and realized P&L
11. CLOSE ticket with success or explicit failure code
```

Each step is a distinct function returning a typed token consumed by the next, so a step cannot be skipped (the type system, not review, enforces the order). Step 5 requires a `Revalidated` token produced only by step 4 (INV-35).

## 16.3 Resource reservation (§2.8)

A live ticket reserves, **before signing**:

| Resource | Reservation | Failure |
|---|---|---|
| Signer / nonce lane | exclusive, per-lane | `SignerUnavailable` / `NonceUnavailable` |
| Simulation completion slot | counted semaphore with deadline | `DispatchTimeout` |
| Submission lane | counted semaphore per lane | `SubmissionRejected` |
| Minimum native gas balance | per-lane balance check against `gas_limit × fee_cap` | `SignerUnavailable` |
| Flash-source feasibility | `FlashSourceQuote.availability_probability` ≥ floor | `NoFlashLiquidity` |
| Executor authorization | cached fingerprint, checked in revalidation | `RiskRejected` |
| State-read capacity | reserved read handle on the branch | `StaleState` |

Reservations are released **only** on terminal close. This prevents the classic failure mode in which the system discovers more profitable opportunities than its execution infrastructure can physically dispatch (§2.8).

## 16.4 Priority scheduling and preemption (§29.4, §57.1.2)

```text
1. already-authorized live tickets        ← NEVER PREEMPTED
2. high-EV candidates inside capture window
3. exact simulations likely to become live tickets
4. route discovery
5. slow research / coverage auditing
```

Preemption when capacity becomes scarce goes in the reverse order: research/coverage → slow search → low-confidence simulations → low-EV candidates → high-EV candidates → **AUTHORIZED LIVE TICKETS: never**.

**FIFO is forbidden on the final capture path** (§46.1). Implemented as a binary-heap priority queue keyed on `(status_rank, deadline, −expected_net_ev)`. `capture::authorized_ticket_never_preempted` is a property test over synthetic overload (INV-09).

## 16.5 Capture-path utilization SLO (§29.5)

```text
U_capture = tickets dispatched before deadline / tickets admitted for live dispatch
```

`U_capture` is an infrastructure SLO with an alert band. Any material decline automatically triggers — in order — load shedding, signer-lane expansion, RPC failover, or a reduction in candidate admission thresholds, **before** it becomes a P&L leak.

## 16.6 Zero-tolerance and hard-budget controls (§29.6)

Zero tolerance (breach disables new live tickets on the affected path until recovery is verified): `ticket_drop_count`, `unexplained_pre_dispatch_expiry`, `nonce_reuse`, `wrong_chain_submission`, `commitment_mismatch`, `critical_state_gap_used_for_trade`, `unauthorized_adapter_call`.

Hard budgets (strategy/chain specific, configured): max ticket age, max queue residency, max signer-lane utilization, max simulation wait, max feed recovery time, max submission acknowledgement time, max stale-state probability.

## 16.7 Two metrics, never conflated (§2.9)

```text
SYSTEM_CAPTURE_ASSURANCE = dispatched within deadline / admitted     ← engineering; target ≈ 100%
MARKET_CAPTURE           = landed / economically available            ← competitive; optimized, never guaranteed
```

Exported as distinct Prometheus series with distinct dashboards. **Any claim of deterministic 100% market capture is architecturally unsound** (§57.1.5) and is forbidden in documentation, dashboards and alerts.

## 16.8 Recovery (§57.1.4, INV-39)

On boot, reconnect, replacement or provider failure:

```text
replay ticket journal
  → for each non-terminal ticket: query chain outcome (receipt / nonce / balance)
  → close with a terminal code
  → only when zero unreconciled tickets remain: enable live dispatch
```

`capture::boot_blocks_dispatch_until_reconciled` kills the process with `SIGKILL` mid-flight and asserts the restarted process refuses to dispatch until reconciliation completes.

---

# 17. Opportunity Ticket lifecycle

Blueprint §2.5, §27.3. Crate: `apex-capture`.

## 17.1 Monotonic status machine

```text
OBSERVED → RESERVED → EXACTING → SIMULATED → AUTHORIZED → SIGNED
        → DISPATCHING → ACKNOWLEDGED → PRECONFIRMED/INCLUDED → FINALIZED → RECONCILED
```

`TicketStatus::advance(&mut self, to: TicketStatus) -> Result<(), MonotonicityError>` is the only mutator and refuses any non-forward transition. The journal records every transition with a timestamp; a transition that is not recorded did not happen.

## 17.2 Terminal loss states

`STALE`, `STATE_CHANGED`, `EV_COLLAPSED`, `RISK_REJECTED`, `DISPATCH_TIMEOUT`, `NONCE_UNAVAILABLE`, `SIGNER_UNAVAILABLE`, `SUBMISSION_REJECTED`, `COMPETITOR_WON`, `REVERTED`, `DIVERGED` — each carrying its cause payload (§7.3).

## 17.3 TTL and dispatch deadline

A ticket's TTL derives from (a) the opportunity's estimated decay from the §37 opportunity surface, and (b) the chain inclusion window from the `ChainExecutionAdapter`. **No ticket may remain queued past `dispatch_deadline`** — a deadline-wheel timer closes it with `DispatchTimeout{deadline, elapsed}` *before* the deadline passes, so expiry is always explained (INV-03).

On Base the inclusion window is derived from the Flashblock cadence and the computed `earliest_eligible_flashblock`; a ticket whose earliest eligible Flashblock lies beyond its validity window is closed with `EarliestFlashblockTooLate` **before** any signing work.

## 17.4 Duplicate suppression (§25)

`ExecutionCommitment::hash()` is the deduplication key. An in-flight ticket with an identical commitment hash suppresses a new one. **Transport redundancy sends identical signed bytes** (INV-10); economically distinct duplicate attempts require an explicit policy flag plus a proven positive incremental EV from the submission model, and the nonce/replacement policy must account for the interaction. Otherwise duplicate gas expenditure is prohibited.

## 17.5 Durability

Journal at `var/tickets/<chain>/<utc-date>.jsonl`, `fsync` on every transition at or past `AUTHORIZED`. Pre-`AUTHORIZED` transitions are buffered (they carry no capital risk). Journal entries are `TicketJournalEntry { ticket_id, from, to, at_ns, detail }` and the full `OpportunityTicket` is written once at `AUTHORIZED`.

---

# 18. Signer / nonce architecture

Blueprint §27. Crate: `apex-capture`. Fixes **C-02 / B-5**.

## 18.1 Signer roles (§27.1)

```text
ExecutionSigner   — signs arbitrage transactions only; holds gas, nothing else
EmergencyAdmin    — pause / circuit trip; never signs trades
Treasury          — receives profit; never signs trades
Observer          — read-only
```

**No general-purpose wallet holds unrestricted operational authority.** Separation is enforced on-chain by `ExecutionAuth` roles (existing `AccessController` provides the primitive) and off-chain by separate key material with separate storage.

## 18.2 Multi-lane signer pool (§27.5)

```text
ExecutionSignerPool (per chain)
├── Lane 0 → NonceManager → executor
├── Lane 1 → NonceManager → executor
├── Lane 2 → NonceManager → executor
└── …
```

Hard requirements, each with a test:

| Requirement | Test |
|---|---|
| Independent nonce streams | `signer::lanes_have_independent_nonce_streams` |
| Independent pending-state tracking | `signer::lane_pending_state_is_isolated` |
| Pre-funded gas reserve | `signer::lane_without_reserve_is_not_assigned` |
| Shared immutable executor authorization | `signer::all_lanes_share_executor_auth` |
| Per-lane health score | `signer::unhealthy_lane_leaves_hot_pool` |
| Per-lane circuit breaker | `signer::lane_breaker_does_not_stop_chain` |
| No cross-lane nonce reuse | `signer::no_cross_lane_nonce_reuse` (loom) |

The scheduler assigns a ticket to **the healthiest currently-free lane with sufficient gas reserve and the required chain/contract authorization**. A slow or conflicted lane is removed from the hot pool **without stopping the chain**.

## 18.3 Nonce manager (§27.2)

Per lane: `confirmed_nonce`, `pending_nonce`, `reserved_nonce`, `submitted_nonce`, `replacement_set`.

The existing `NonceManager` logic is **ADAPTed, not rewritten** — its authoritative-pending-nonce start point and gap-recovery path encode a real historical bug fix (`main.rs:2717-2754`), and that comment migrates with the code. What changes: it becomes per-lane, and nonce release is tied to ticket terminal close rather than to an ad-hoc `mark_failed`/`mark_confirmed` pair.

## 18.4 Pool sizing and rotation (§27.6)

```text
N_signers ≥ Q_P99(concurrent live tickets) + reserve margin
```

with an operational cap from measured gas-management and security constraints. The pool **expands before saturation becomes a capture bottleneck** and contracts when sustained utilization falls. Initial value: 4 lanes on Base (chosen as `Q_P99 = 2` from the existing candidate log plus a 2-lane margin), re-derived weekly from measurement and recorded in `docs/apex/reports/signer-sizing-<date>.md`.

Rotation must preserve contract authorization and cannot create nonce ambiguity. A failed signer is **quarantined**, its outstanding tickets are reconciled, and only then are its resources returned to the pool.

Keys are isolated from the general application process as far as latency/security permit: Phase 6 ships in-process keys with a `Secret<LocalWallet>` wrapper (INV-46); an out-of-process signer is a Phase 15+ option gated on measured latency cost.

## 18.5 Transaction state machine (§27.3) and replacement (§27.4)

```text
CANDIDATE → PRECHECK → SIMULATED → AUTHORIZED → SIGNED → SUBMITTED
         → SEEN → PRECONFIRMED/INCLUDED → FINALIZED → RECONCILED
failure: STALE | REPLACED | REJECTED | REVERTED | DIVERGED | TIMEOUT
```

**Replacement is allowed only while the opportunity's remaining EV exceeds the incremental replacement cost. No blind gas escalation** (§27.4) — `replacement::should_replace` takes the current EV and the replacement cost and returns `false` by default; there is no unconditional escalation path.

---

# 19. Solidity implementation

Blueprint §25, §26, §35.3, §43. Directory: `contracts/`. Fixes **C-01 / B-1 / B-2**.

## 19.1 Target contract set

```text
core/
  ExecutionAuth.sol        roles; emergency admin separate from routine execution (§26.5)
  AdapterRegistry.sol      allowlists: adapterId → (target, selector set, pool set, token set)
  RouteValidator.sol       §26.2 pre-call validation + §25 commitment verification
  ProfitInvariant.sol      §26.3 multi-asset invariant + §26.4 residue policy
  FlashSourceRouter.sol    §19 approved providers only
adapters/
  AaveAdapter MorphoAdapter UniswapV3Adapter UniswapV4Adapter
  AerodromeAdapter SlipstreamAdapter PancakeAdapter BalancerAdapter
chains/
  BaseArbExecutor.sol      (EthereumArbExecutor.sol in Phase 14)
```

## 19.2 No unrestricted calls (§26.1)

This is the single most important contract change. **Deleted:** `_execGeneric` — which decodes an arbitrary `target` and arbitrary `callData` from plan bytes and executes `target.safeCall(callData)`, with an `action == 1` branch that grants that decoded target an ERC-20 allowance. This is the critical finding. The `_execModule` delegatecall trampolines go with it, not because they are unsafe (their targets are `immutable` and constructor-deployed) but because the adapter set replaces the contract-size workaround they exist to serve.

**Replaced by:** `Step { uint8 adapterId; bytes32 poolKey; bytes payload }`. `RouteValidator` resolves `adapterId` through `AdapterRegistry` to a fixed adapter address, verifies the pool and tokens are listed, and the adapter itself constructs the call with a **compile-time-fixed selector**. There is no code path from plan data to an arbitrary target.

Enforced three ways: (a) `forge` test `testNoArbitraryCallSurfaceExists`; (b) `scripts/ci/check_no_generic_call.sh` greps source for a `.call(` or `delegatecall` whose target is not a registry-resolved address, and fails the build; (c) a bytecode scan asserting every `CALL`/`DELEGATECALL` site in the deployed runtime traces to a registry-resolved address.

## 19.3 Route validator (§26.2)

Validates **before any external call**: chain id, executor address/version, caller authorization, flash provider, venue, pool, input token, output token, route topology, deadline, minimum profit, minimum output, maximum input, hook fingerprint where applicable, **and the §25 commitment hash**.

## 19.4 Multi-asset profit invariant (§26.3, §26.4)

```solidity
// For each debt asset j:
//   Balance_after[j] >= Debt[j].principal + Debt[j].flashFee + Debt[j].requiredReturn
// For the designated profit asset:
//   Profit_realized >= MinimumProfit
// Residue: all expected debt repaid AND required profit received AND allowed residue
//   exactly zero — or an explicitly declared residue path with deterministic accounting.
//   Unaccounted residue is a FAILURE (revert).
```

This replaces the current single-scalar `_distributeProfit` and removes the `loans.length != 1` restriction, enabling the multi-asset routes §26.3 assumes.

## 19.5 What is preserved from the existing contract

Callback-sender verification for every provider (`initiator != address(this)` / `expectedVault != msg.sender` / `ctxHash` binding), the `ActiveLoanContext` pattern, Permit2 allowance handling, `ReentrancyGuard`, role separation, the circuit breaker, the clone-factory deployment path, and the runtime-size gate. All of the ~60 existing Foundry tests that cover these survive; only the JIT and bridge tests are deleted with their features.

## 19.6 Formal verification focus (§35)

"Formal verification effort focuses first on the small Solidity execution core, because that is the final capital-control boundary." Targets, in order: `ProfitInvariant` (multi-asset arithmetic cannot under-repay), `AdapterRegistry` (no unlisted target is reachable), `RouteValidator` (commitment equality implies parameter equality). Phase 5 ships invariant fuzzing (`forge` `invariant_` suites); a formal-methods pass is a Phase 17 item with its own go/no-go.

## 19.7 Red/blue migration for execution encoding and settlement

**Blue** = deployed `MultiVenueArbImplementation` (current). **Red** = `BaseArbExecutor` + adapters, deployed to a *different address*. **Differential** = `apex-exec` encodes every candidate for both and `apex-sim` simulates both against the same fork state, comparing success, gas, balance deltas and profit. **Gate** = identical economic outcome on ≥99.9% of candidates, every difference explained, plus a clean external review of the new contract set. **Migration** = `executor.authority = red` flips which address is dispatched to; capital moves only after 100 canary trades. **Rollback** = flip the config field back; blue remains deployed and funded until Phase 17.

---

# 20. Chain execution adapters

Blueprint §4. Crate: `apex-chain`. Fixes **C-12** (deferred) and replaces `chain.rs`'s config-struct-as-chain-model.

```rust
#[async_trait]
pub trait ChainExecutionAdapter: Send + Sync {
    fn chain_id(&self) -> ChainId;
    async fn state_feed(&self) -> Result<StateFeedHandle>;
    async fn pending_state(&self) -> Result<VerifiedState>;
    async fn simulate(&self, tx: &Committed) -> Result<SimulationResult>;
    fn estimate_total_fee(&self, c: &Candidate) -> Result<TotalExecutionCost>;
    fn estimate_inclusion_probability(&self, c: &Candidate, at: Instant) -> f64;
    fn optimize_submission_cost(&self, c: &Candidate) -> SubmissionDecision;
    async fn submit(&self, signed: &SignedPayload, lane: SubmissionLaneId) -> Result<Ack>;
    fn replacement_policy(&self) -> ReplacementPolicy;
    async fn observe_outcome(&self, h: TxHash) -> Result<ObservedOutcome>;
    async fn reconcile_final_state(&self, h: TxHash) -> Result<PnlAttribution>;
}
```

**A universal interface with chain-specific economics.** Every chain-conditional branch currently living in `main.rs` (`chain_hot_pool_base_cap(&str)`, `derive_chain_event_sampling_rate(&str)`, `derive_chain_time_budget_ms`) becomes an adapter method or an adapter-owned config value. A CI grep fails the build on `match chain_name` / `if chain == "base"` outside `apex-chain`.

## 20.1 Runtime regime discovery

Adapters **discover** their active execution regime at startup and periodically thereafter rather than hard-coding a historical assumption (§4.3, §4.5). `ChainRegime { ordering_mode, priority_fee_semantics, round_length, fast_feed_available, private_feed_available, replacement_rules, gas_and_data_fee_model }`. A chain whose regime cannot be discovered is **not admitted to live trading**.

---

# 21. Base execution

Blueprint §4.1, §22, §24.1, §24.5, §24.6, §24.7. This is the primary battlefield and the only live chain through Phase 13.

## 21.1 Base is a preconfirmation-aware sequencing market

Interfaces used: Flashblocks transaction/log/full-block streams, `pending`, `base_transactionStatus`, `eth_simulateV1`, `newFlashblockTransactions`.

**Feed policy (existing, preserved):** the application connects through a Flashblocks-aware *RPC provider* endpoint, not the raw node-operator infrastructure stream. `base_fast.rs` already documents and implements exactly this via `eth_subscribe("pendingLogs")`, and the `FlashFeed` enum already anticipates the Denim migration. Blueprint §22 endorses this choice.

## 21.2 Flashblock scheduler (§22.1, §22.2) — the critical v4 addition

```rust
pub struct FlashblockSchedulerState {
    pub current_flashblock_index: u32,
    pub current_block_number: u64,
    pub current_block_gas_limit: u64,
    pub current_flashblock_gas_budget: u64,
    pub residual_gas_capacity: u64,
    pub candidate_gas_limit: u64,
    pub earliest_eligible_flashblock: u32,
    pub fee_rank_estimate: f64,
    pub state_validity_window: Duration,
}

/// §22.2: k_eligible = min{ k : G_t <= Q(k) }
/// Q(k) is the MEASURED allocation policy, never a hard-coded one-tenth rule.
pub fn earliest_eligible(g_t: u64, q: &MeasuredCapacityModel) -> Option<u32>;
```

`Q(k)` is learned from observed Flashblock gas budgets and stored with a confidence interval. The blueprint is explicit that a fixed fractional rule must **not** be hard-coded because chain parameters may change.

Submission decision (§4.1):

```text
current Flashblock index → estimated residual gas capacity → transaction gas limit
  → earliest eligible Flashblock → fee rank within eligible capacity
  → state validity at that time → submit / reject
```

**Ordering lock (§22.3):** once a Flashblock is built its ordering is fixed. The scheduler only ever computes a *future* eligible index; there is no "pay more later and still land earlier" path (INV-38).

## 21.3 Gas-limit minimization as a capture control (§24.7)

```text
G_safe = Q_gas(simulated gas) + headroom
choose smallest gas_limit >= G_safe that remains valid AND fits the earliest
  economically valuable inclusion window
if none exists → REJECT THE TICKET BEFORE SIGNING
```

On Base this is capture, not accounting: a smaller gas limit can mean an earlier Flashblock (INV-37).

## 21.4 Simulation (§24.6)

Capture-critical simulation uses **`eth_simulateV1` with explicit state/block controls and validation enabled**, because Base documents that block-context properties under a generic `eth_call` against `pending` can reflect cached historical context. The existing `eth_call` path is the fallback and the quorum verifier.

## 21.5 Dispatch and acknowledgement (§24.5, §24.8)

```text
AUTHORIZED → reserve nonce/signing lane → cheap last-mile validation
          → sign exactly committed payload → primary Flashblocks-aware RPC endpoint
          → base_transactionStatus / newFlashblockTransactions acknowledgement check
          → controlled fallback if acknowledgement misses deadline
```

Controlled RPC redundancy uses **the same signed transaction** — redundancy is transport reliability, not economically distinct attempts. If the opportunity has gone stale, the fallback is **cancelled before dispatch**.

`base_transactionStatus = Known` is evidence the preconfirmation node **received** the transaction — **not** evidence it has been placed in a Flashblock (§24.8). The ack ladder records `transport_accepted → node_known → sequencer_received → preconfirmed → included → finalized`, each with its own timeout and escalation rule.

## 21.6 Backrun timing (§22.4) — Phase 13

A target transaction arriving in Flashblock *i* creates a successor-state candidate whose earliest possible execution is constrained by the remaining sequencer process. The backrun engine evaluates target Flashblock index, current residual capacity, next eligible slot, next-block transition, competition and state decay.

---

# 22. Ethereum execution

Blueprint §4.2, §24.2, §24.5. Phase 14. Not before Base is profitable.

- **Regime:** private-builder / relay relationships with an **empirical bid-to-inclusion model**, not a static "percentage of profit" rule. `EV(b) = P_land(b, builder, state, time) × Profit(b) − Cost(b)`.
- **Multiplexing:** multiple builder destinations only when incremental inclusion probability exceeds leakage/complexity cost. Builder multiplexing is an **inclusion-optimization problem, not blind duplication**.
- **Bundles:** Flashbots atomic bundles, block validity windows, registered-builder targeting, replacement/cancellation.
- **Acknowledgement:** builder/relay acknowledgement is evidence of acceptance **at that layer**, not proof a proposer selected the bundle. Inclusion remains an observed outcome.
- **Route topology:** 3 preferred / 4 allowed / 5 exceptional (§13).
- **Existing assets:** the private-relay bundle plumbing in `main.rs:1905-2280` (`send_bundle_transaction`, `build_send_bundle_request`, relay connection) is real working code. It is **ADAPTed** into `apex-chain/src/ethereum/submit.rs` as a `SubmissionLane` implementation — but the `parallel_private_relay_blast` behaviour is gated behind a measured positive incremental EV (§24.2, INV-10), not enabled by default.

---

# 23. Other chain execution

All Phase 15, all gated on `ChainScore` (§3.3), none before Base is profitable.

## 23.1 BSC (§4.5)
First-class **economic candidate**, no privileged status without measured capture. The adapter discovers: active public/private ordering options, RPC propagation behaviour, fee/priority semantics, available flash liquidity, dominant DEX inventory, pool update cadence, competitor intensity. **Never assume Ethereum-style public mempool behaviour without measuring it.**

## 23.2 Arbitrum (§4.3, §24.3)
Ordering regime is **runtime state**, not a constant. At startup and periodically: `ordering_mode ∈ {TIMEBOOST, PGA, PGA+FAST_FEED, OTHER}`, priority-fee semantics, PGA round length, Fast Feed availability, private sequencer feed availability, replacement rules, current gas/data fee model. When PGA is active, dispatch timing and priority fee become first-class high-frequency variables. The existing `ArbitrumFeeConfig` (per-byte data fee) in `fees.rs` is the seed for the data-fee model.

## 23.3 OP Mainnet (§4.4)
OP Stack fee model: `L2 execution fee + L1 data fee + priority fee + failure cost`, with the L1 component depending on compressed transaction size and Ethereum base/blob fee conditions. **Calldata size is therefore an economic optimization variable** and feeds the §23.3 calldata optimizer. Existing `scripts/data/build_optimism_core_pools.py` is a seed asset.

## 23.4 Unichain (§4.6)
Shadow / specialized only until measured opportunity density justifies live capacity.

## 23.5 Chain portfolio controller (§3.3, §3.4) — Phase 15

```text
ChainScore_c = E[NetUSD/hour | c] / (InfraCost_c + MarginalComputeCost_c + RiskAdjustedFailureCost_c)
```

with minimum observation and reliability requirements. Allocates state ingestion capacity, candidate-generation CPU, exact simulation capacity, dedicated node/RPC connections, signer capacity, flash-source evaluation and engineering effort. A constrained online allocator with **hard live-risk floors**; exploration happens preferentially in shadow or low-notional canary; live trading stays exploitative once safety gates are met.

**A chain with more volume but poor capture ranks below a lower-volume chain with a specialized execution edge.** DEX volume is a prior, never proof of arbitrage EV.

---

# 24. Submission architecture

Blueprint §24, §25. Crate: `apex-capture` (controller) + `apex-chain` (lanes).

## 24.1 Submission controller

```rust
pub struct SubmissionController { /* lanes, policies, health, ack monitors */ }

pub trait SubmissionLane: Send + Sync {
    fn lane_id(&self) -> SubmissionLaneId;
    fn policy(&self) -> SubmissionPolicy;
    async fn send(&self, signed: &SignedPayload) -> Result<Ack>;
    async fn observe(&self, h: TxHash) -> Result<TransactionLifecycleStage>;
    fn health(&self) -> LaneHealth;
}
```

## 24.2 Policy rules (§24.4)

- **No public leakage by default.** Public submission is the *fallback*, not the default, wherever private sequencing relationships materially increase EV.
- **No probabilistic spam.**
- **No uncommitted gas-burning probes.**

Enforced by `SubmissionPolicy::Public` requiring an explicit per-route configuration opt-in and by CI greps for probe patterns (INV-36). The existing `apply_public_mempool_jitter` is removed — it is a leakage-shaping heuristic with no EV justification in the v4 model.

## 24.3 Acknowledgement ladder (§24.8)

```text
transport_accepted → node_known → sequencer_received → builder_acknowledged
                   → preconfirmed → included → finalized
```

Each stage has its own timeout and escalation rule. **An RPC success response proves only that an endpoint accepted the request** — not network receipt, ordering, preconfirmation or inclusion (INV-34).

## 24.4 Bundle-capable lanes on Base

BlockPI exposes a **bundle service on Base** alongside its MEV-protected endpoint. The plan had not accounted for this, and it matters in two places:

- **§17 bounded packing (Phase 12).** A bundle lane is what makes cross-cycle packing executable at all on a sequencer chain — without one, "packing" can only mean a single fatter transaction, which §17.3 penalises via gas limit and Flashblock eligibility. With a bundle lane, the conflict-graph work has a real execution target.
- **§24.3 acknowledgement ladder.** A bundle has its own acceptance semantics distinct from a single transaction's, so `TransactionLifecycleStage` must model bundle acknowledgement separately rather than reusing the single-tx path.

**Not adopted before the first trade.** §36.3 defers packing, and a bundle lane has no value until there is something to pack. Phase 7 records the capability in `PrivacyEvidence`/lane metadata; Phase 12 is where it earns its place, gated on §17.3's risk-adjusted margin like every other packing decision.

## 24.5 Commitment and duplication control (§25)

Every candidate gets a deterministic `commit = keccak256(chain_id, executor_version, venue/version fingerprints, flash_source, state_fingerprint, route_hash, exact input sizes, min profit, slippage constraints, deadline, submission policy)`. **The executor and the off-chain signer must agree on the commitment** — the contract recomputes and reverts on mismatch.

This prevents: accidental route mutation after authorization, stale candidate submission, duplicate attempts, wrong-chain execution, unexpected venue substitution.

---

# 25. Risk architecture

Blueprint §28, §44. Crate: `apex-risk`. **Risk is a hard execution gate**, not advice.

## 25.1 Triggers (§28)

`state staleness`, `preconf divergence`, `simulation divergence`, `revert spike`, `unexpected callback`, `venue invariant violation`, `profit shortfall`, `fee anomaly`, `gas anomaly`, `competitor intensity anomaly`, `sequencer/builder acceptance collapse`, `node desynchronization`, `contract code fingerprint change`, `flash-source reliability collapse`.

Each maps to a posture transition in an exhaustive table (INV-42). The existing `CircuitBreaker` already implements revert-spike, RPC-error-burst, consecutive-failure and loss-window triggers with tests; those are extracted and the remaining triggers added.

## 25.2 Graduated response (§28.1)

```text
NORMAL → REDUCED SIZE → HIGH-EV ONLY → STRATEGY DISABLED → CHAIN DISABLED → GLOBAL HALT
```

Transitions are automatic on trigger and require an explicit operator action (or a measured recovery window) to step back down. `RiskPosture` is a Prometheus gauge and the primary operator display.

## 25.3 Loss-event containment (§28.2)

Every loss is classified: `pricing error | state error | simulation error | venue error | inclusion error | fee-model error | contract error | operator/config error | external protocol behavior`. **A loss class exceeding its expected frequency automatically tightens its gate or disables the affected module** (INV-43).

## 25.4 Failure containment (§44)

Every external boundary has a bounded failure model: node unavailable, feed stalled, state divergence, RPC timeout, simulation timeout, builder unavailable, sequencer unavailable, flash source unavailable, venue revert, signing failure, nonce conflict.

**Fail closed for capital safety; fail open to alternative providers where the alternative itself passes health/risk gates.** Worked example from the blueprint, implemented literally:

```text
Base Flashblocks feed stale
  → stop Flashblocks-dependent candidates
  → continue only routes whose state can be reconstructed safely
  → reduce size / halt according to policy
```

## 25.5 Risk policy source

`ops/inputs.yaml` `risk:` block (existing, 90 lines) remains the declaration. `risk_policy.rs` already resolves it into enforceable on-path limits — min net profit (USD and native), max gas units per tx, max fee-per-gas cap, max slippage bps, max price-impact bps, must-simulate-before-send, revert-penalty model. That enforcement discipline is preserved; the posture ladder and loss classification are added around it.

---

# 26. Observability

Blueprint §32. Crate: `apex-obs`. The existing ~78-series Prometheus surface is the base; the families below are added.

| Family | Series |
|---|---|
| **State** | `state_age_ms`, `flashblock_index`, `state_patch_ms`, `state_rebuild_ms`, `state_branch_count`, `rollback_count`, `preconf_final_divergence_rate`, `state_fingerprint_mismatch` |
| **Feed integrity** | `feed_sequence`, `gap_count`, `out_of_order_count`, `duplicate_count`, `reconnect_count`, `recovery_time_ms`, `feed_freshness_ms`, `state_rebuild_after_gap` |
| **Search** | `candidates_per_sec`, `profitable_candidates_per_sec`, `candidate_ev_quantiles`, `hermes_rank`, `full_search_rank`, `improving_path_miss_rate`, `finite_size_discovery_rate` |
| **Optimization** | `continuous_size`, `final_discrete_size`, `split_ratio`, `active_pools`, `single_route_ev`, `split_route_ev`, `joint_ev`, `packed_ev`, `certificate_status` |
| **Execution** | `signal_to_submit_ms`, `simulation_ms`, `signing_ms`, `submission_ms`, `earliest_eligible_flashblock`, `actual_flashblock_index`, `inclusion_latency`, `revert_rate`, `replacement_rate` |
| **Capture assurance** | `tickets_admitted`, `tickets_dispatched`, `tickets_acknowledged`, `tickets_expired_pre_dispatch`, `ticket_drop_count`, `dispatch_success_rate`, `capture_path_utilization`, `signer_lane_utilization`, `nonce_conflict_rate`, `submission_failover_rate`, `hot_path_recall`, `high_ev_miss_rate` |
| **Competition** | `capture_rate`, `competitor_win_rate`, `arrival_lag`, `builder_accept_rate`, `sequencer_accept_rate`, `bid_to_inclusion_curve`, `pga_round_win_rate` |
| **Economics** | `gross_profit`, `flash_fee`, `dex_fees`, `l2_execution_fee`, `l1_data_fee`, `priority_payment`, `builder_sequencer_payment`, `failure_cost`, `net_profit`, `ev`, `profit_per_hour` |
| **Attribution** | incremental P&L by `OptimizationLayer` (single path, parallel split, joint allocation, packing, event-driven, liquidation, correlated, V4 route, chain submission opt, compute scheduling) |

**Latency decomposition (§30):** `T_signal→submit = T_ingest + T_patch + T_generate + T_price + T_size + T_simulate + T_sign + T_submit`, each with p50/p90/p99. The existing `stage_latency_ms{stage}` histogram already provides the mechanism.

**Latency optimization is approved only when** `ΔCaptureEV > ΔInfrastructureCost + ΔComplexityRisk` (§30.1). A faster code path that does not improve realized capture is not a priority — this is recorded as a review rule in `docs/apex/GATES.md`.

---

# 27. Missed-opportunity accounting

Blueprint §33. Crate: `apex-obs`.

Every economically attractive but unexecuted candidate gets a `MissRecord` (§7.8) with an exhaustive `MissReason`. Stored: candidate id, state fingerprint, simulated EV, estimated capture probability, rejection reason, submission policy, and **later realized outcome where observable**.

**The resulting counterfactual dataset is a core engineering dataset**, not a log. It is:
- written to `var/miss/<chain>/<utc-date>.jsonl` (off the dispatch path),
- aggregated into the §37 opportunity surface,
- the input that decides which subsystem gets the next engineering dollar (§52).

`obs::every_rejection_path_records_a_miss` is an exhaustive test: every `return Reject` / `None` path in the candidate pipeline is enumerated and asserted to produce a `MissRecord` (INV-40). New rejection paths that omit it fail the test.

**Precedent this test exists to prevent.** `ARCHITECTURE_PIVOT_HANDOFF.md §6.5` records that `path_tokens`, `fee_tiers` and `block_number` were **declared in the candidate-log schema and never populated** at the post-sim call site. That is the fourth instance of this repository's "written in one phase, never wired" pattern — after `break_continuity`, the `metrics: None` construction bug, and `anchor_cl`/`anchor_v2`. A declared-but-empty field reads exactly like a measured zero. INV-40's test therefore asserts **population**, not merely presence: a `MissRecord` whose required fields are defaulted fails.

The repository's existing candidate log already carries this shape — including the operational subtlety that it mixes fast-path and scan-path rejections and must be filtered on `edges_scanned == 0` for fast-path-only analysis. The v4 `MissRecord` carries an explicit `path: SearchPath` field so that filter becomes unnecessary.

---

# 28. Coverage auditing

Blueprint §2.7. Crate: `apex-obs`. Phase 8.

**A fast path can only capture what it discovers.** The `CoverageAuditor` performs delayed, broader, more expensive discovery against historical/speculative state on **reserved slow-path resources** and compares its best executable candidates against what the hot path produced.

Tracked: `hot_path_recall`, `high_EV_miss_rate`, `route_class_miss_rate`, `venue_class_miss_rate`, state-trigger miss rate, `finite_size_miss_rate`, `coverage_audit_lag`.

**Automatic response** to a material unexplained hot-path miss rate, in order:
1. expand route templates in the frontier;
2. raise slow-path resource allocation;
3. disable the affected strategy class.

The purpose is **not** to pretend the auditor is omniscient — it is to prevent the engine from silently losing known opportunity classes. `obs::auditor_detects_injected_miss` injects a synthetic opportunity the frontier provably cannot see and asserts the auditor reports it (INV-41).

**This is also the gate on Hermes (§12.2).** Engine B is built only if the auditor shows a material recall gap that template expansion does not close.

---

# 29. Testing strategy

Blueprint §35, §36; task mandate §15. **Every implementation unit has its validation strategy defined before coding begins.**

## 29.1 Test classes and where each lives

| Class | Scope | Location | Runs |
|---|---|---|---|
| **Unit** | one function/type | `#[cfg(test)]` in each crate | every commit |
| **Property** | invariants over generated inputs (`proptest`) | `crates/*/tests/prop_*.rs` | every commit |
| **Fuzz** | adversarial input spaces (`cargo-fuzz`) | `crates/apex-math/fuzz/`, `crates/apex-venues/fuzz/` | nightly + pre-gate |
| **Differential** | reference vs Rust exact vs forked EVM (§35.1) | `crates/apex-math/tests/differential.rs`, `src/bin/cl_parity.rs` | pre-gate + weekly sweep |
| **Integration** | multi-crate wiring, no chain | `tests/` (workspace) | every commit |
| **Fork** | against real chain state | `tests/fork_*.rs`, `scripts/fork/` | pre-gate |
| **Simulation** | full candidate→ticket→dispatch against a simulated chain | `tests/sim_chain_*.rs` | every commit |
| **Adversarial** | §36 perturbation harness | `crates/apex-sim/tests/adversarial.rs` | pre-gate |
| **Failure injection** | kill -9, feed gap, RPC blackhole, signer loss, slow path saturation | `tests/chaos_*.rs` | pre-gate |
| **Performance** | latency budgets per stage | `crates/*/benches/` (criterion) | pre-gate, tracked over time |
| **Canary** | Tier 4 controlled production validation | `scripts/canary/` | per promotion |
| **Concurrency model** | `loom` exhaustive interleaving (existing `loom-model` feature) | `crates/apex-state`, `crates/apex-capture` | pre-gate |

## 29.2 Differential testing (§35.1) — the three-way comparison

For every exact venue engine:

```text
reference implementation (on-chain quoter / protocol contract)
            vs
Rust exact model (apex-math)
            vs
forked EVM execution (REVM at the same block)
```

compared on **amounts, fees, state transition, rounding, reverts**. A venue that does not pass is `Exactness::Approximate` and therefore candidate-only (INV-17).

## 29.3 Executor invariant tests (§35.3)

Each is a named `forge` test mapped in §8.4: only authorized caller executes; only approved provider can call back; only approved venues/pools/tokens can be called; loan always repaid or the whole transaction reverts; minimum profit invariant enforced; unexpected residue cannot create silent loss; wrong chain cannot execute; expired routes revert.

## 29.4 Failure-injection scenarios (mandatory before first live trade)

| Scenario | Assertion |
|---|---|
| `SIGKILL` between sign and receipt | restart reconciles the ticket to a terminal state before enabling dispatch (INV-39) |
| Feed sequence gap | branch marked `Unsafe`; no ticket admitted from it; recovery verified before resume (INV-12) |
| Two feeds disagree | no majority vote; parentage/rebuild path taken (INV-13) |
| Primary RPC blackhole | failover rotates; no ticket dropped (INV-02) |
| Signer lane loses gas mid-flight | lane quarantined, tickets reconciled, chain keeps trading (§27.6) |
| Slow path saturated | fast-path p99 unchanged (§2.6) |
| Simulation slot starvation | tickets close with explicit `DispatchTimeout`, never silently (INV-03) |
| Nonce collision injected | detected, lane quarantined, zero reuse (INV-04) |
| Clock skew | deadlines still honoured (monotonic clock only on the capture path) |

## 29.5 Performance budgets (measured, enforced in CI)

| Stage | Budget (Base, p99) | Rationale |
|---|---|---|
| `T_ingest` | ≤ 5 ms | feed → decoded log |
| `T_patch` | ≤ 2 ms | delta → published `StateVersion` |
| `T_generate` | ≤ 10 ms | frontier revaluation for the dirty closure |
| `T_price` | ≤ 15 ms | exact repricing of affected hops |
| `T_size` | ≤ 20 ms | continuous warm start + discrete refinement |
| `T_simulate` | ≤ 60 ms | Tier 0 + Tier 1; Tier 2 runs concurrently with reservation |
| `T_sign` | ≤ 3 ms | includes last-mile revalidation |
| `T_submit` | ≤ 20 ms | to transport ack |
| **`T_signal→submit`** | **≤ 135 ms** | **< one Flashblock (200 ms)** |

These are initial budgets derived from the 200 ms Flashblock cadence. They are re-derived from measurement after Phase 8 and any change is recorded with its justification.

## 29.6 Test-first discipline

Every task in §33 is written as: failing test → run it and observe the specific failure → minimal implementation → run it and observe the pass → commit. A task whose test cannot be made to fail first is a task whose test is wrong.

---

# 30. Performance strategy

Blueprint §29, §30, §31.

- **Latency decomposition** is measured per stage (§26), never as a single number.
- **A latency improvement is approved only if** `ΔCaptureEV > ΔInfrastructureCost + ΔComplexityRisk` (§30.1).
- **Compute is economically allocated** (§29.1): `Priority(q) = E[Incremental NetUSD(q)] / (CPU_ms(q) + RPCCost(q))`.
- **Concurrency, not serialization** (§46.2): on a state event, exact repricing / finite-size sizing / competitor scenarios / cost refresh run concurrently and join; top candidates race their simulations. No artificial serialization where tasks are independent.
- **Release profile is preserved** (`opt-level=3`, `lto="fat"`, `codegen-units=1`, `strip="debuginfo"`) with its existing rationale comment — including the explicit requirement that `panic` stays at `unwind` because `spawn_supervised` relies on `JoinError::is_panic()` to restart workers. **`panic = "abort"` must never be set.**
- **Infrastructure baseline (§31):** high-frequency multi-core CPU, 32 GB+ RAM, NVMe, low-jitter networking, local chain node(s) where economically justified, redundant external RPC, redundant WS feeds, clock synchronization, process isolation. Co-location only when measured capture improvement justifies cost.

---

# 31. Security strategy

Blueprint §43.

## 31.1 Contract level
Reentrancy defense, callback sender verification, role separation, approved provider/venue/pool/token lists, profit invariant, deadline/slippage enforcement, emergency pause, chain-id validation. **The single largest security deliverable in this plan is removing the unrestricted call surface (B-1).**

## 31.2 Adapter level
Every adapter has independent configuration, deployment record, circuit breaker, fuzz suite, differential suite and shadow metrics.

## 31.3 Key management
Never combine trading signer, admin signer and treasury signer. Keys wrapped in `Secret<T>` with redacting `Debug`/`Display`/`Serialize` (INV-46).

## 31.4 Key-hygiene residual carried from audit P0-3

Verified 2026-09-22 (presence checks only; no values read or logged):

```text
.env gitignored                 YES
.env tracked in HEAD            no
.env ever committed             no          -> this was never a repository leak
any .env* file tracked          0
PRIVATE_KEY present in .env     yes         -> plaintext operator key on disk
SIPHON_TARGET_ADDRESS           UNSET       -> capital.rs siphon machinery is dormant
```

Two items stay open into Phase 6, both ops rather than code:

1. **A plaintext key on disk cannot satisfy P0-3's acceptance** ("a key that has never appeared in any file"). Phase 6 ships `Secret<LocalWallet>` (INV-46), which stops it reaching logs and telemetry but does not remove it from disk. Blueprint §27.6 wants keys "isolated from the general application process as far as latency/security requirements permit"; an out-of-process signer is the real fix and is a Phase 15+ option gated on measured latency cost.
2. **`SIPHON_TARGET_ADDRESS` is unset**, so profit has nowhere cold to go. Blueprint §26.5 and §43 require treasury separation from the trading signer. Set it before the Phase 8 canary, not after.

`.env` additionally carries provider API keys inline (including one embedded directly in a URL). They are correctly gitignored, but they are credentials under §43 and belong in the same rotation discipline as the operator key.

## 31.5 Operational security
Credentials externalized from source. Private keys never in logs or telemetry. RPC, builder and Fast Feed credentials treated as secrets. `scripts/secret_scan.sh` (existing) runs in CI. The existing `sim_rpc_url` field's "keep this field out of logs: it carries the provider credential" discipline is generalized to the `Secret<T>` type.

## 31.6 Security review gates
- Phase 5 exit requires an external review of the new contract set before any mainnet deployment.
- Phase 8 exit requires a dependency audit (`cargo audit`, `cargo deny`).
- Any change to `contracts/core/` requires two reviewers and a fresh invariant-fuzz run.

---

# 32. Infrastructure strategy

- **Repository hygiene (Phase 0):** remove the 119 MB Grafana tarball, the 105 MB Prometheus tarball, `grafana-v10.4.2/`, `prometheus-2.52.0.*/`, `out/`, `node/`, `eth-docker/`, `arbot-live.log`, `validation-*.log`, all `*.bak.<epoch>` files, `contracts/artifacts/**`, and `python3 Convert.py`. Replace with `.gitignore` entries and an install step in `docs/apex/INFRA.md`. **Use `git rm --cached` for tracked files so history is preserved** (no history rewrite).
- **Local Base node (R-20).** `ARCHITECTURE_PIVOT_HANDOFF.md §6.3` records a ~240 ms RPC RTT ceiling and argues a local node is what "makes REVM viable" for Tier 2. Blueprint §31 permits local nodes "where economically justified" and §30.1 requires `ΔCaptureEV > ΔInfrastructureCost + ΔComplexityRisk`. **Decide after Phase 8**, with the measured §29.5 latency decomposition in hand — if RTT is not the dominant term in `T_signal→submit` p99, the node does not earn its cost.
- **Observability config** moves to `ops/observability/` (`prometheus.yml`, Grafana dashboards) and stays in-tree — it is source, the tarballs are not.
- **CI (Phase 0):** `.github/workflows/ci.yml` running `cargo check --workspace --all-targets`, `cargo test --workspace --all-targets`, `forge build`, `forge test`, plus the existing `scripts/ci/*.sh` gates (`no_runtime_panics.sh`, `check_placeholder_endpoints.sh`, `check_executor_size.sh`, `secret_scan.sh`) and the new `check_no_generic_call.sh` and `no_shared_mutable_state.sh`. Clippy runs **per-crate** on migrated crates only, expanding as crates move.
- **Pool inventories (Phase 2):** `data/` becomes versioned with a `manifest.json` carrying a content hash and a provenance record per venue file; loading verifies the hash and (for production) re-verifies addresses on-chain.
- **Config cleanliness:** one live file per venue per chain; `.bak.<epoch>` files deleted; `ops/inputs.yaml` gains a `schema_version`.

---

# 33. Detailed implementation phases

Each phase specifies: objective, files, dependencies, implementation tasks (as bite-sized TDD steps), tests, benchmarks, acceptance criteria, failure criteria, exit gate. **"Code compiles" is never sufficient.**

---

## PHASE 0 — Repository archaeology, workspace, and CI

**Objective:** Establish the workspace, the shared vocabulary, the immutable configuration plane, and enforceable CI — with **zero behaviour change** to the running engine.

**Dependencies:** none.

**Files:**
- CREATE `Cargo.toml` (workspace virtual manifest), `crates/apex-types/{Cargo.toml,src/lib.rs,src/ids.rs,src/state.rs,src/candidate.rs,src/ticket.rs,src/commitment.rs,src/cost.rs,src/flash.rs,src/sim.rs,src/risk.rs,src/miss.rs,src/pnl.rs}`
- CREATE `crates/apex-config/{Cargo.toml,src/lib.rs,src/ops.rs,src/registry.rs,src/validate.rs,src/secret.rs}`
- CREATE `.github/workflows/ci.yml`, `scripts/ci/check_no_generic_call.sh`, `scripts/ci/no_shared_mutable_state.sh`, `scripts/ci/invariant_coverage.sh`
- CREATE `docs/apex/{BASELINE.md,INVARIANTS.md,GATES.md,INFRA.md}`
- MODIFY `Makefile` (workspace-aware; `fmt` target refuses tree-wide formatting)
- MODIFY `.gitignore`
- **DO NOT MOVE** `src/ops_inputs.rs`, `src/registry.rs`, `src/config_validation.rs` — see Task 0.4's cycle note. They stay in `arb-exec-legacy` and are retired in Phase 17.
- REMOVE (`git rm --cached` + `.gitignore`) the artifacts listed in §32
- REMOVE `plan.md`, `HANDOFF.md`, `docs/ARCHITECTURE_PIVOT_HANDOFF.md`, `docs/PRODUCTION_AUDIT_FIX_PLAN.md` (already deleted in the working tree — commit the deletions)

### Task 0.0 — Reconcile with origin BEFORE anything else

**This runs first. No other task may start until it is green.** §3.7: the audit behind this plan read `c5e4d44`, which is 5 commits behind `origin/main`.

- [x] **Step 1: See what this audit did not.** *Run 2026-09-22.* Five commits: `580419d` (one-line `agents.md` edit), `e079d05` (delete `plan.md`), `7f059cb` ("Add files via upload"), `118c37a` (delete `HANDOFF.md`), `ae12d97` (delete `docs/PRODUCTION_AUDIT_FIX_PLAN.md`).

```bash
git fetch origin && git log --oneline --stat HEAD..origin/main
```

- [x] **Step 2: `7f059cb` resolved — it is the blueprint itself.** 4,053 insertions of `APEX_MEV_v4_Final_Architect_Blueprint.md`, verified **byte-identical** to the file this plan was written against (`sha256 0f32b5a3a8be485a…`, 4,053 lines / 113,992 bytes). **No source file is touched by any of the five commits**, so no §4 disposition is affected. R-17 closed.
- [x] **Step 3: Reconciled 2026-09-22.** Fast-forwarded `c5e4d44 → ae12d97`. The three deletions origin had already made were restored locally first so the ff applied them cleanly; the untracked blueprint was moved aside and verified byte-identical to the tracked copy that arrived with `7f059cb`. The parked config work was confirmed deliberate and committed (`a358ca9`), along with a real `grafana-v10.4.2` path fix. **Two working-tree changes were deliberately left unstaged** — `docs/arbot_docs_pack/arbot_master_runbook.md` (whitespace only) and `docs/superpowers/plans/2026-09-01-anchor-path.md` (a markdown table-separator reformat) — because committing trailing-whitespace noise into a never-formatted repo pollutes blame for no gain. They remain in the working tree for the operator to decide.
- [x] **Step 4: Open items harvested → §3.8**, with new findings promoted to **B-14** (Base "private" relays are public RPCs) and risks **R-19/R-20/R-21**, plus the §31.4 key-hygiene residual. The handoff's §5 config drift and §6.4 key rotation were **not** re-raised — both already resolved.
- [x] **Step 5: `PLAN.md` amended and committed** (`8b3bcfc`), on a reconciled tree. No disposition changed (Step 2). `docs/ARCHITECTURE_PIVOT_HANDOFF.md` was retired in the same commit, after §3.8 harvested its open items.

> **Task 0.0 is complete.** R-17 is closed. Phase 0 may proceed at Task 0.1.

### Task 0.1 — Baseline capture

**Interfaces:** Produces `docs/apex/BASELINE.md` consumed by every later differential gate.

- [ ] **Step 1:** Record the current build, test and metric baseline.

```bash
{
  echo "# APEX-MEV v4 baseline — $(date -u +%F)"
  echo '## cargo check --all-targets'; cargo check --all-targets 2>&1 | tail -20
  # grep, not tail: the tree compiles a DUAL module tree (lib.rs declares 42 modules,
  # main.rs declares 58 — shared files compile twice), so there are two separate test
  # summaries and `tail` can truncate one. Record BOTH counts.
  echo '## cargo test --all-targets'; cargo test --all-targets 2>&1 | grep -E 'test result|^running|^error'
  echo '## forge test'; forge test 2>&1 | grep -E 'Suite result|FAIL|test result'
  echo '## rust LOC'; find src -name '*.rs' -exec wc -l {} + | tail -1
  echo '## sol LOC'; find contracts -name '*.sol' -exec wc -l {} + | tail -1
  echo '## env vars'; grep -rno 'ARBOT_[A-Z0-9_]*' src/ | awk -F: '{print $3}' | sort -u
} > docs/apex/BASELINE.md
```

- [ ] **Step 2:** Commit.

```bash
git add docs/apex/BASELINE.md && git commit -m "docs(apex): record the pre-migration baseline"
```

### Task 0.2 — Workspace skeleton

- [ ] **Step 1: Write the failing test** — `crates/apex-types/tests/workspace.rs`:

```rust
#[test]
fn workspace_exposes_apex_types() {
    // Fails until the crate exists and the workspace resolves it.
    let _ = apex_types::VERSION;
}
```

- [ ] **Step 2: Run it and observe the failure.** `cargo test -p apex-types` → `error: package ID specification 'apex-types' did not match any packages`.
- [ ] **Step 3: Create the workspace manifest and the crate.** Root `Cargo.toml` becomes `[workspace] members = ["crates/*"]` plus `[workspace.dependencies]` hoisting every shared dependency at its current version. The existing `arb-exec` package moves to `crates/arb-exec-legacy/` via `git mv` with its `Cargo.toml` retaining the `arb-exec` package name and both `[[bin]]` entries, **plus explicit `[[bin]]` entries for `ingest`, `cycle_index_stats` and `ws_probe`** (fixes B-11). `crates/apex-types/src/lib.rs` defines `pub const VERSION: &str = env!("CARGO_PKG_VERSION");`.
- [ ] **Step 4: Run and observe the pass.** `cargo test -p apex-types` → 1 passed. Then `cargo check --workspace --all-targets` → matches the Task 0.1 baseline (same single `edge_capacity_from_cl_state` warning, no new errors).
- [ ] **Step 5: Commit.**

```bash
git add Cargo.toml crates/ && git commit -m "build(workspace): split into a Cargo workspace, preserving history via git mv"
```

**Never `git add -A` or `git add .` in this repository, in any task.** The working tree carries seven parked modifications (including `config/balancer.base.json5` and `config/univ2.base.json5`, which decide what the engine trades), a 5.3 MB `arbot-live.log`, the pool census outputs, `python3 Convert.py`, and ~30 `.bak.<epoch>` files — **and `.gitignore` covers none of them**. This is a public repository and §35.4 forbids history rewrites, so a bad `add -A` is not cheaply reversible. Stage named paths only. This rule is added to Global Constraints.

### Task 0.2a — Make `forge test` green before freezing the baseline (B-13)

Phase 0's exit criterion is "`forge test` result matches baseline". If the baseline is captured while two tests fail, the gate permanently blesses two real faults. Fix them first.

- [ ] **Step 1: Observe the failures.**

```bash
forge test 2>&1 | grep -E 'FAIL|Suite result'
```

Expected, as measured 2026-09-22:
```text
[FAIL: 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45 != 0x…3001] testEnvAddressLookupAcceptsEthereumLongPrefixAlias()
[FAIL: router owner should be configured owner: 0xD7A4… != 0xad54…] testDeployTransfersRouterOwnershipToConfiguredExecutorOwner()
```

- [x] **Step 2: Diagnosed — both are the same fault, and it is in the TESTS, not the script.** forge auto-loads `.env`; the tests set *unprefixed* keys while `.env` supplies *prefixed* ones that correctly outrank them (`.env:32 ETH_UNIV3_ROUTER`, `.env:47 BASE_EXECUTOR_OWNER`, `.env:14 CHAIN=base`). Proved by commenting `.env:32`, which turned the alias test green. Compounding hazard: `vm.setEnv("CHAIN", …)` in `DeployEnvPrefix.t.sol` persists for the whole run, so the resolved prefix was order-dependent. There is no `unsetEnv` cheatcode in forge 1.7.
- [x] **Step 3: Attempted a test-level fix. It did not work, and was reverted.** Synthetic probe keys, an alias/precedence split, a pinned `CHAIN`, both key forms in the ownership test, and `threads = 1` in `foundry.toml`. Every individual test passes **alone**. None of it moved the per-file number: 11 passed/4 failed with the fixes versus 11/3 for the originals — no measurable gain, so the churn was reverted rather than kept. `foundry.toml` and both test files are back at `HEAD`. See BASELINE.md for the full "what was tried and did not work" table.
- [x] **Step 4: Measured the real problem instead.** The suite is **flaky, not red**: 8 unmodified runs gave 67/0, 65/2, 67/0, 66/1, 67/0, 67/0, 67/0, 66/1, with a varying failing set. A flaky gate cannot certify anything, so "make forge green" was the wrong framing for this task.
- [ ] **Step 5: BLOCKED — needs an operator decision.** The fix is a test-architecture change (stop configuring the deploy script through process-global env), and `script/Deploy.s.sol` is already scheduled for rewrite in Phase 5 for the new contract set. Rewriting its tests now and again in Phase 5 is poor next-dollar allocation, so the options are quarantine-now / fix-now / fix-in-Phase-5. Recorded as **B-13** and as an amendment to the Phase 0 exit gate.

### Task 0.3 — Shared vocabulary (`apex-types`)

- [ ] **Step 1: Write the failing tests** — one per invariant that the type system is supposed to carry:

```rust
// crates/apex-types/tests/ticket_monotonic.rs
use apex_types::{TicketStatus, MonotonicityError};

#[test]
fn ticket_status_refuses_to_go_backwards() {
    let mut s = TicketStatus::Authorized;
    assert!(matches!(s.advance(TicketStatus::Simulated), Err(MonotonicityError { .. })));
    assert_eq!(s, TicketStatus::Authorized);
}

#[test]
fn ticket_status_advances_forward() {
    let mut s = TicketStatus::Authorized;
    s.advance(TicketStatus::Signed).unwrap();
    assert_eq!(s, TicketStatus::Signed);
}
```

```rust
// crates/apex-types/tests/miss_exhaustive.rs
use apex_types::MissReason;

#[test]
fn miss_reason_has_no_catch_all() {
    // Compile-time proof: adding a variant without updating this match fails the build.
    for r in MissReason::ALL {
        let _label: &'static str = match r {
            MissReason::LowEv => "LOW_EV",
            MissReason::StaleState => "STALE_STATE",
            MissReason::TooSlow => "TOO_SLOW",
            MissReason::CompetitorWon => "COMPETITOR_WON",
            MissReason::SimFail => "SIM_FAIL",
            MissReason::RiskFail => "RISK_FAIL",
            MissReason::GasFail => "GAS_FAIL",
            MissReason::L1DataCostFail => "L1_DATA_COST_FAIL",
            MissReason::NoFlashLiquidity => "NO_FLASH_LIQUIDITY",
            MissReason::VenueDisabled => "VENUE_DISABLED",
            MissReason::ConflictRejected => "CONFLICT_REJECTED",
            MissReason::PackingNotWorthwhile => "PACKING_NOT_WORTHWHILE",
            MissReason::EarliestFlashblockTooLate => "EARLIEST_FLASHBLOCK_TOO_LATE",
            MissReason::HookModelIncomplete => "HOOK_MODEL_INCOMPLETE",
            MissReason::BuilderRejected => "BUILDER_REJECTED",
            MissReason::SequencerRejected => "SEQUENCER_REJECTED",
            MissReason::NonceUnavailable => "NONCE_UNAVAILABLE",
        };
    }
    assert_eq!(MissReason::ALL.len(), 17);
}
```

```rust
// crates/apex-types/tests/gas_types.rs
// INV-19: gas limit is a scheduling variable, gas used is a cost variable.
// This test is a COMPILE-FAIL test under trybuild; the .rs fixture attempts
// `let _: GasUsed = gas_limit.into();` and must not compile.
#[test]
fn gas_limit_does_not_convert_to_gas_used() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/gas_limit_into_gas_used.rs");
}
```

- [ ] **Step 2: Run and observe the failures.** All three fail to compile (`cannot find type ... in crate apex_types`).
- [ ] **Step 3: Implement the types** exactly as specified in §7, plus `MissReason::ALL`, `TicketStatus::advance`, and the `GasLimit`/`GasUsed` newtypes with no `From` between them.
- [ ] **Step 4: Run and observe the passes.** `cargo test -p apex-types` → 3 test files pass, including the `trybuild` compile-fail.
- [ ] **Step 5: Commit.**

```bash
git add crates/apex-types && git commit -m "feat(types): the v4 shared vocabulary, with the invariants in the type system"
```

### Task 0.4 — Immutable configuration (`apex-config`)

**Interfaces:** Produces `Arc<ApexConfig>`; consumed by every later crate. `ApexConfig` has a public field for every currently-live `ARBOT_*` variable, typed.

> **Cycle note — why the three legacy config modules are NOT moved.** The obvious move (`ops_inputs.rs`, `registry.rs`, `config_validation.rs` → `apex-config`) creates a Cargo dependency cycle, because those modules import *upward* into the rest of the legacy crate:
>
> ```text
> src/ops_inputs.rs:3          use crate::util::expand_env_vars;
> src/registry.rs:14,15        use crate::util::…;  use crate::venues::{BalPoolCfg, CurvePoolCfg, UniV2PoolCfg};
> src/config_validation.rs:3-7 use crate::bridge::…; use crate::chain::…; use crate::ops_inputs::…;
>                              use crate::registry::…; use crate::venues::…;
> ```
>
> `venues.rs` alone is 5,646 LOC and pulls `graph`, `hot_path`, `metrics`, `pool_store` and every `quote_*`; `bridge` is classified REMOVE. Since `arb-exec-legacy` must depend on `apex-config`, moving these makes `apex-config` depend back on `arb-exec-legacy` and Cargo rejects the cycle.
>
> **Therefore:** `apex-config` is built **fresh** as a typed schema with no legacy imports. `ops_inputs.rs` is *adapted in place* to additionally emit an `ApexConfig` (a pure serialization step with no new dependency direction), and the legacy modules are retired with the rest of `arb-exec-legacy` in Phase 17. This is the general rule for the whole migration: **never move a module that has upward dependencies — build its replacement fresh and adapt the old one to feed it.**

- [ ] **Step 1: Write the failing test:**

```rust
// crates/apex-config/tests/immutable.rs
#[test]
fn config_is_resolved_once_and_carries_a_version() {
    let cfg = apex_config::ApexConfig::load_from(
        "ops/inputs.yaml", "config/", &apex_config::Env::empty_but_secrets()
    ).expect("boot config must validate");
    assert!(cfg.config_version > 0);
    // INV: no runtime env reads. The loader records every key it consulted.
    assert_eq!(cfg.consulted_env_keys().iter().filter(|k| !k.starts_with("APEX_SECRET_")).count(), 0);
}

#[test]
fn missing_required_field_fails_closed() {
    let err = apex_config::ApexConfig::load_from(
        "tests/fixtures/inputs_missing_risk.yaml", "config/", &apex_config::Env::empty_but_secrets()
    ).unwrap_err();
    assert!(err.to_string().contains("risk"), "error must name the missing section: {err}");
}
```

- [ ] **Step 2: Run and observe the failure** — `apex_config` does not exist.
- [ ] **Step 3: Implement.** Write `apex-config` **fresh**, with no `arb-exec-legacy` dependency: `ApexConfig::load_from` parses `ops/inputs.yaml` + `config/` + the registry with its own `serde` types, validates **every** section (fail closed on any missing required field), stamps a `config_version` (monotonic counter + content hash), and records consulted env keys. Add `Secret<T>` with redacting `Debug`/`Display`/`Serialize`. Then add one function to the legacy `ops_inputs.rs` that emits an `ApexConfig` from the already-parsed legacy structs, so both planes are proven to agree (Task 0.4a).
- [ ] **Step 4: Run and observe the passes.**
- [ ] **Step 5: Commit.**

### Task 0.4a — Config differential (legacy vs `apex-config`)

- [ ] **Step 1: Write the failing test** — `crates/apex-config/tests/differential.rs` loads `ops/inputs.yaml` through both planes and asserts field-by-field equality:

```rust
#[test]
fn both_config_planes_agree_on_every_field() {
    let fresh = apex_config::ApexConfig::load_from("ops/inputs.yaml", "config/", &Env::secrets_only()).unwrap();
    let legacy = arb_exec::ops_inputs::load_ops_inputs("ops/inputs.yaml").unwrap().to_apex_config();
    assert_eq!(fresh, legacy, "the two config planes must not drift");
}
```

- [ ] **Step 2: Run and observe the failure** — it lists every field where the fresh schema and the legacy parser disagree. **Each disagreement is a finding**: either the fresh schema is wrong, or the legacy parser has a latent bug.
- [ ] **Step 3: Resolve every disagreement**, recording any legacy bug found in `docs/apex/reports/config-diff-<date>.md`.
- [ ] **Step 4: Run and observe the pass.**
- [ ] **Step 5: Commit** (named paths only).

### Task 0.5 — Env-var inventory and typed migration

- [ ] **Step 1: Write the failing test** — `crates/apex-config/tests/env_coverage.rs` asserting that every `ARBOT_*` key found in the legacy crate has a corresponding typed field in `ApexConfig` (or an explicit entry in a `RETIRED` list with a reason):

```rust
#[test]
fn every_legacy_env_var_is_accounted_for() {
    let found = apex_config::testing::scan_legacy_env_keys("crates/arb-exec-legacy/src");
    let unaccounted: Vec<_> = found.iter()
        .filter(|k| !apex_config::ApexConfig::has_field_for(k) && !apex_config::RETIRED.contains(&k.as_str()))
        .collect();
    assert!(unaccounted.is_empty(), "unaccounted env vars: {unaccounted:?}");
}
```

- [ ] **Step 2: Run and observe the failure** — it lists all 84 keys.
- [ ] **Step 3: Implement** the typed fields and the `RETIRED` list, resolving the `ops/inputs.yaml` `features:` UNKNOWN in §4.7 by proving each flag's consumer.
- [ ] **Step 4: Run and observe the pass.**
- [ ] **Step 5: Commit.**

### Task 0.6 — Manifest-relative paths survive the move

Moving `src/` to `crates/arb-exec-legacy/src/` puts every manifest-relative path two directories deeper. Four sites break, and one of them breaks **silently**, which would violate Phase 0's zero-behaviour-change promise.

| Site | Current | Breakage |
|---|---|---|
| `src/chain.rs:2230` | `include_str!("../scripts/fork/run_integration_dry_run.sh")` | **Compile error** under `--all-targets` (it is inside a `#[test]`) |
| `src/config_validation.rs:19` | `Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)` | Stops finding its files |
| `src/hot_pools.rs:1535` | `Path::new(env!("CARGO_MANIFEST_DIR"))…` | Stops finding its files |
| `src/main.rs:14812` | `.env` fallback from `CARGO_MANIFEST_DIR` | **Silent.** Resolves to `crates/arb-exec-legacy/.env`. Masked when launched from the repo root because `dotenvy` searches the CWD first — so anything launched from elsewhere starts *without its configuration* and nothing says so. |

`src/venues.rs:5095`'s `include_str!("venues.rs")` is self-relative and survives unchanged.

- [ ] **Step 1: Write the failing test** — `crates/arb-exec-legacy/tests/manifest_paths.rs`:

```rust
#[test]
fn every_manifest_relative_path_resolves_after_the_move() {
    // WORKSPACE_ROOT is set by build.rs from CARGO_WORKSPACE_DIR, not CARGO_MANIFEST_DIR.
    for rel in ["scripts/fork/run_integration_dry_run.sh", "ops/inputs.yaml", "config/registry.json"] {
        let p = arb_exec::workspace_path(rel);
        assert!(p.exists(), "{rel} does not resolve to {}", p.display());
    }
}

#[test]
fn dotenv_fallback_resolves_to_the_workspace_root_not_the_crate() {
    assert_eq!(arb_exec::dotenv_fallback_path(), arb_exec::workspace_root().join(".env"));
}
```

- [ ] **Step 2: Run and observe the failures** — `workspace_path` does not exist; before it does, the `include_str!` is already a hard compile error, so this test cannot even build. Fix the `include_str!` first (it is the loudest), then the rest.
- [ ] **Step 3: Implement** a `build.rs` in `arb-exec-legacy` that exports `WORKSPACE_ROOT` from `CARGO_WORKSPACE_DIR`, plus `workspace_path()` / `workspace_root()` / `dotenv_fallback_path()` helpers, and repoint all four sites at them.
- [ ] **Step 4: Run and observe the passes**, then run the binary from `/tmp` and confirm it loads `.env` — the silent case.
- [ ] **Step 5: Commit** (named paths only).

### Task 0.7 — CI and repository hygiene

- [ ] **Step 1: Write the failing check** — `scripts/ci/invariant_coverage.sh` parses `PLAN.md` §8 for invariant IDs and their named tests, then greps the workspace for each test name; exits non-zero on any missing one.
- [ ] **Step 2: Run it and observe the failure** — every INV-* is missing (expected at Phase 0; the script takes a `--allow-missing-until-phase N` argument so it ratchets).
- [ ] **Step 3: Implement** `.github/workflows/ci.yml` with the jobs in §32, `check_no_generic_call.sh`, `no_shared_mutable_state.sh`, and the hygiene removals (`git rm --cached` + `.gitignore`).
- [ ] **Step 4: Run and observe** CI green on the ratcheted invariant set; repository size drops by ~230 MB.
- [ ] **Step 5: Commit.**

**Tests:** `apex-types` (3 files incl. trybuild), `apex-config` (3 files), `scripts/ci/*` self-tests.
**Benchmarks:** none (no hot-path code).
**Acceptance criteria:**
1. `cargo check --workspace --all-targets` matches the Task 0.1 baseline exactly (no new warnings or errors).
2. `cargo test --workspace --all-targets` passes, including every pre-existing test.
3. `forge test --no-match-path 'test/Deploy*'` is **52/0, deterministically**, verified over 5 consecutive runs. The 15 `test/Deploy*.t.sol` tests are **quarantined** under B-13 and excluded from this gate until Phase 5 Task 5.7. Note the amendment: the original wording was "fully green", written before the suite was known to be *flaky* rather than merely red — determinism is the real requirement, since a gate that passes 5 runs in 8 certifies nothing.
4. CI runs on push and enforces the gates listed in §32.
5. `git log --follow` works on `crates/apex-config/src/ops.rs` (history preserved).
6. Repository working-tree size reduced by ≥ 200 MB with no tracked source lost.
7. Every one of the 84 `ARBOT_*` keys is either a typed `ApexConfig` field or a `RETIRED` entry with a reason.
8. Zero behaviour change: the `arb-exec` binary produces byte-identical output on `scripts/shadow/base_shadow_run.sh` against a recorded fixture.

**Phase 0 follow-ups (recorded, not blocking):** three CI gates are written but not wired, each red on arrival for a reason that wiring would not fix:
- `check_no_generic_call.sh` — `_execGeneric` still exists (B-1); lands with its fix in Phase 5 Task 5.1.
- `check_placeholder_endpoints.sh` — `config/registry.json:363` carries `${ALCHEMY_KEY}` for `abstract`, one of the six non-live chains. Same root cause as `apex-config` refusing to load fail-closed; both clear when the default chain set is trimmed.
- `scripts/secret_scan.sh` — fires only on test fixtures (`/v2/test-key`, `/v2/test`, `/v2/abc123`) and one doc comment. No real credential is exposed; it needs a placeholder filter before it can gate.

A gate that is red on arrival gets ignored, so each is listed here rather than wired and muted.

**Failure criteria:** any new compiler error/warning; any pre-existing test regressing; history lost on any moved file; behaviour change in the shadow run.
**Exit gate:** **G-0** — baseline recorded, workspace green, CI enforcing, hygiene done, zero behaviour change.

---

## PHASE 1 — State architecture

**Objective:** Versioned, branchable, fingerprinted state with feed integrity, running red/blue against the existing path.

**Dependencies:** Phase 0.

> **Scope refinement, recorded 2026-09-22.** §6.1's dependency graph puts `apex-math` *before* `apex-state`, but this phase order puts state first. The conflict is real: `live_state` imports `quote_univ2`, and `reconcile`/`ingestion` follow it. Measured dependencies for the whole cluster — `continuity`, `state_gate`, `log_decode` and `pool_store` import nothing; `live_state`, `reconcile`, `validation_select` and `ingestion` import only each other plus `quote_univ*`.
>
> So Phase 1 delivers the **new state primitives**, which are pure and need no pricing: `Versioned<T>`, the extended `Ordinal`, `FeedIntegrity`/`FeedArbiter`, and the speculative branch tree. The **module moves** (`live_state`, `ingestion`, `reconcile`, `validation_select`) and the red/blue differential that depends on them move to Phase 2, landing with `apex-math`. Tasks 1.5 (token classifier, needs on-chain probing) and 1.6 (differential) go with them. This respects the dependency graph rather than forcing a premature `apex-math` stub.

**Files:**
- CREATE `crates/apex-state/{Cargo.toml,src/lib.rs,src/versioned.rs,src/branch.rs,src/ordinal.rs,src/feed/{mod.rs,integrity.rs,arbiter.rs}}`
- MOVE `src/live_state.rs` → `crates/apex-state/src/live.rs`; `src/continuity.rs` → `src/continuity.rs`; `src/state_gate.rs`, `src/state_validation.rs`, `src/reconcile.rs`, `src/validation_select.rs`, `src/log_decode.rs` → `crates/apex-state/src/`
- MOVE `src/ingestion.rs` → `crates/apex-state/src/feed/ingestion.rs`; `src/pool_store.rs` → `src/pools.rs`; `src/liquidity_cache.rs` → `src/depth.rs`
- CREATE `crates/apex-state/src/tokens/{mod.rs,classifier.rs,fingerprint.rs}` (REBUILD of `token_refresh.rs`)
- MODIFY `crates/arb-exec-legacy/src/main.rs` — depend on `apex-state` for the red path only; blue path unchanged

### Task 1.1 — `Versioned<T>` replaces `Published<T>` (INV-11)

- [ ] **Step 1: Write the failing test:**

```rust
// crates/apex-state/tests/versioned.rs
use apex_state::Versioned;

#[test]
fn readers_never_block_writers() {
    let v = Versioned::new(vec![1u32, 2, 3]);
    let r1 = v.load();                       // Arc snapshot
    v.store(vec![4, 5, 6]);                  // must not block on r1
    assert_eq!(*r1, vec![1, 2, 3]);          // r1 still sees its snapshot
    assert_eq!(*v.load(), vec![4, 5, 6]);
}

#[test]
fn every_snapshot_carries_a_version_and_provenance() {
    let v = Versioned::new(vec![1u32]);
    let (ver, prov, _val) = v.load_full();
    assert!(ver.0 > 0);
    assert_eq!(prov.reconstruction, apex_types::ReconstructionStatus::Verified);
}
```

- [ ] **Step 2: Run and observe the failure** — `Versioned` does not exist.
- [ ] **Step 3: Implement** `Versioned<T>` over `arc_swap::ArcSwapOption<(StateVersion, StateProvenance, T)>`.
- [ ] **Step 4: Run and observe the pass.** Then add `scripts/ci/no_shared_mutable_state.sh` matching `Mutex<Option<Arc<` inside `crates/apex-*` and confirm it passes.
- [ ] **Step 5: Commit.**

### Task 1.2 — `Ordinal` gains flashblock ordering

- [ ] **Step 1: Write the failing test:**

```rust
// crates/apex-state/tests/ordinal.rs
use apex_state::continuity::Ordinal;

#[test]
fn flashblock_index_orders_ahead_of_block() {
    let a = Ordinal { payload_id: 7, flashblock_index: 1, block: 100, tx_index: 5, log_index: 0 };
    let b = Ordinal { payload_id: 7, flashblock_index: 2, block: 100, tx_index: 0, log_index: 0 };
    assert!(a < b, "a later flashblock must order after an earlier one regardless of tx_index");
}

#[test]
fn existing_block_ordering_is_unchanged_when_flashblocks_are_absent() {
    let a = Ordinal { payload_id: 0, flashblock_index: 0, block: 100, tx_index: 5, log_index: 1 };
    let b = Ordinal { payload_id: 0, flashblock_index: 0, block: 100, tx_index: 5, log_index: 2 };
    assert!(a < b);
}
```

- [ ] **Step 2: Run and observe the failure** — the struct has no `payload_id`/`flashblock_index`.
- [ ] **Step 3: Implement** by adding the two fields ahead of `block`, exactly as `continuity.rs`'s own doc comment specifies. **The state machine below does not change.**
- [ ] **Step 4: Run and observe the pass**, plus every existing `continuity` test still green.
- [ ] **Step 5: Commit.**

### Task 1.3 — Feed integrity and gap blocking (INV-08, INV-12, INV-13)

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-state/tests/feed_integrity.rs
#[test]
fn sequence_gap_marks_branch_unsafe() {
    let mut f = FeedIntegrity::new(FeedSourceId(1));
    f.observe(100); f.observe(101); f.observe(104);           // gap: 102, 103
    assert_eq!(f.gap_count(), 1);
    assert_eq!(f.status(), ReconstructionStatus::Unsafe);
}

#[test]
fn unsafe_state_cannot_authorize_a_ticket() {
    let branch = branch_with_status(ReconstructionStatus::Unsafe);
    assert!(VerifiedState::try_from(&branch).is_err(),
            "INV-08: a gapped branch must be unconstructible as VerifiedState");
}

#[test]
fn contradicting_feeds_are_never_resolved_by_vote() {
    let a = fp_with_parent(B256::repeat_byte(1));
    let b = fp_with_parent(B256::repeat_byte(2));
    let c = fp_with_parent(B256::repeat_byte(2));            // two "votes" for b's parent
    match FeedArbiter::resolve(&[a, b, c]) {
        Resolution::Rebuild | Resolution::ByParentage(_) => {}
        Resolution::Majority(_) => panic!("INV-13: majority vote is forbidden"),
    }
}
```

- [ ] **Step 2: Run and observe the failures.**
- [ ] **Step 3: Implement** `FeedIntegrity`, `VerifiedState` (a newtype constructible only from a `Verified` branch), and `FeedArbiter::resolve` with no `Majority` construction path (the variant exists only so the test can assert it is unreachable — it is `#[doc(hidden)]` and never returned).
- [ ] **Step 4: Run and observe the passes.**
- [ ] **Step 5: Commit.**

### Task 1.4 — Speculative branch tree and rollback (INV-14)

- [ ] **Step 1: Write the failing tests** for `commit_or_rollback`: a matching fingerprint promotes the branch; a differing one discards it and returns the list of invalidated candidate ids.
- [ ] **Step 2: Run and observe the failures.**
- [ ] **Step 3: Implement** `SpeculativeStateTree` with persistent per-branch delta maps over a shared confirmed base.
- [ ] **Step 4: Run and observe the passes**, plus `rollback_cost_ms` under the Phase 29.5 budget.
- [ ] **Step 5: Commit.**

### Task 1.5 — Token semantics classifier (§7.2, §7.3)

- [ ] **Step 1: Write the failing tests** — a fork-based table test over known Base tokens asserting the classification of at least one standard, one fee-on-transfer and one pausable token, and that a non-standard token's expected output is derived from balance deltas rather than nominal amounts.
- [ ] **Step 2: Run and observe the failures.**
- [ ] **Step 3: Implement** `classifier.rs` and `fingerprint.rs`.
- [ ] **Step 4: Run and observe the passes.**
- [ ] **Step 5: Commit.**

### Task 1.6 — Red/blue differential harness

- [ ] **Step 1: Write the failing test** — `state::differential_harness_reports_divergence` feeds both paths a recorded log stream containing a known divergence and asserts it is reported with the pool, block and bps.
- [ ] **Step 2: Run and observe the failure.**
- [ ] **Step 3: Implement** `differential.rs` writing `docs/apex/reports/state-diff-<date>.csv`.
- [ ] **Step 4: Run and observe the pass**, then start a 72 h live shadow run.
- [ ] **Step 5: Commit.**

**Tests:** unit + property for `Versioned`, `Ordinal`, `FeedIntegrity`, branch tree; `loom` model for the dirty-set drain (adapting the existing `loom-model` feature); fork tests for the token classifier; the differential harness.
**Benchmarks:** `state_patch_ms` p99 ≤ 2 ms; `rollback_cost_ms` p99 ≤ 20 ms; `Versioned::load` ≤ 50 ns.
**Acceptance criteria:**
1. 72 h continuous red/blue shadow with **zero unexplained divergences**; every divergence traced to a named cause.
2. `gap_count` > 0 observed at least once in the run **and** the branch correctly marked `Unsafe` with verified recovery.
3. INV-08, INV-11, INV-12, INV-13, INV-14 tests green in CI.
4. Fast-path p99 latency unchanged versus the Phase 0 baseline.
5. Token classifier correctly classifies the full admitted Base token set, with the classification recorded in `data/tokens/manifest.json`.

**Failure criteria:** any unexplained divergence; any `Unsafe` branch producing a `VerifiedState`; fast-path p99 regression > 10%.
**Exit gate:** **G-STATE-1** (feed integrity + no shared mutable state + gap blocking) and **G-STATE-2** (branch rollback correctness).

---

## PHASE 2 — Exact pricing engine and venue adapters

**Objective:** One `ExactPricingEngine` trait, one `VenueAdapter` trait, differential-proven exactness for every admitted venue, and a verified pool/venue inventory.

**Dependencies:** Phases 0, 1.

### Scope correction, recorded 2026-09-22 — the file allocation below made a crate cycle

**The allocation this phase originally specified cannot be built.** Measured
import edges in `crates/arb-exec-legacy/src`:

```
math            -> (nothing)          cl_swap    -> cl_math cl_sim
cl_math         -> math               cl_ticks   -> cl_math cl_sim cl_swap quote_cl
quote_common    -> math               cl_sim     -> cl_ticks quote_cl quote_univ3 util
quote_solidly   -> math quote_common  quote_cl   -> quote_common quote_slipstream quote_univ3 util
quote_curve     -> quote_common       quote_univ3-> quote_cl quote_common util
quote_balancer  -> quote_common       quote_slip.-> quote_cl quote_univ3 util
cl_parity_gate  -> cl_sim cl_swap util  quote_univ2 -> quote_cl quote_common
                                      discovery  -> quote_univ2 util
```

`cl_sim`, `cl_ticks` and `quote_univ2` were assigned to `apex-math`; they all
import `quote_cl`, which was assigned to `apex-venues`; and `quote_cl` imports
`quote_common`, which was assigned to `apex-math`. §6.1 has `apex-math` below
`apex-venues`. That is a cycle, and cargo rejects it. This is the same failure
that Task 0.4 caught for `apex-config`, found the same way — measure the edges
before moving the file.

**The seam is purity, not venue family.** Classified by I/O evidence
(`abigen!` / `Provider` / `.await` / `async fn` counts per file):

| Pure — no provider, no async | Has network I/O |
|---|---|
| `math`, `cl_math`, `quote_common`, `cl_swap`, `quote_solidly`, `cl_parity_gate` | `cl_sim`, `cl_ticks`, `quote_cl`, `quote_univ2`, `quote_univ3`, `quote_slipstream`, `quote_curve`, `quote_balancer`, `discovery` |

Two artefacts, not real dependencies, are what tie the pure files to the I/O
files. Both are fixed by moving a symbol, not by redrawing the crate boundary:

1. **`ClPoolState`** is declared in `cl_sim` (which fetches it) but is the state
   the pure swap loop operates on. It is the entire `cl_swap -> cl_sim` edge and
   half the `cl_ticks -> cl_sim` edge. It belongs in `apex-math`.
2. **`multicall3_aggregate3`** is declared in `quote_cl` but is generic JSON-RPC
   batching with nothing CL about it. It is the entire `quote_univ2 -> quote_cl`
   edge and both `cl_ticks`/`cl_sim` -> `quote_cl` edges. Six modules call it,
   including `util` and `hot_pools`, which stay in the legacy crate. It belongs
   in `apex-venues::transport`.

**Also recorded:** `quote_curve` and `quote_balancer` are `abigen!` RPC clients,
not local pricing engines. There is no local Curve or Balancer math in this
repository to differential against the chain. §4.7's two `UNKNOWN` entries for
them are therefore mis-stated — see Task 2.7.

**Corrected files:**
- CREATE `crates/apex-math/{Cargo.toml,src/lib.rs,src/engine.rs,src/cl_state.rs}`; MOVE `math.rs`, `cl_math.rs`, `quote_common.rs`, `cl_swap.rs`, `quote_solidly.rs` into `crates/apex-math/src/` unchanged
- EXTRACT the pure half of `cl_sim.rs` (`ClPoolState`, `quote_exact_input_single_tick`, `validate_pool`, the decode helpers) into `crates/apex-math/src/cl_state.rs`; the loader half stays behind and moves to `apex-venues`
- MOVE `cl_parity_gate.rs` into `crates/apex-math/src/` **after** its three `util::env_parse_opt` reads and `cl_sim::cl_max_ticks_crossed()` read become injected config — it is pure of I/O but not of env, and `apex-math` must not read the environment (§5.2). Lands with Task 2.6, which is where that env plane is dismantled anyway.
- CREATE `crates/apex-venues/{Cargo.toml,src/lib.rs,src/adapter.rs,src/transport.rs,src/univ3.rs,src/slipstream.rs,src/pancake.rs,src/aerodrome.rs,src/balancer.rs,src/curve.rs,src/discovery.rs,src/registry.rs,src/breaker.rs}`
- MOVE `cl_sim.rs` (loader half), `cl_ticks.rs`, `quote_cl.rs`, `quote_univ2.rs`, `quote_univ3.rs`, `quote_slipstream.rs`, `quote_curve.rs`, `quote_balancer.rs`, `discovery.rs` into `crates/apex-venues/src/`
- REMOVE `src/venue_adapter.rs` (wrong trait, zero implementors)

**Files:**
- (superseded by the corrected list above)
- CREATE `crates/apex-math/tests/differential.rs`, `crates/apex-math/fuzz/`
- CREATE `scripts/data/verify_registry_bytecode.py`
- REMOVE `base_venues_complete.yaml`, `generate_base_venues.py` (B-7)

### Three defects the Phase 2 migration surfaced, recorded 2026-09-23

None of these were caused by the migration. All three were found because moving
a file forces you to look at what it depends on, and because accounting for a
test count forced the first build of a *commit* rather than of the working
directory.

**1. `.gitignore`'s `*secret*` pattern swallowed three source files.**
`crates/apex-config/src/secret.rs` is declared by `mod secret;` and matches the
deny rule that keeps credentials out of a public repository, so it was never
added. Its test file and `scripts/secret_scan.sh` went the same way.
**`apex-config` did not compile from a clean checkout of any commit in Phase 0
or Phase 1.** Nothing caught it because every check — `cargo check`, `cargo
test`, clippy, the gates — ran against the working tree, where the files exist.
CI would have caught it on the first push, but this branch has not been pushed
since the plan commit. Fixed with per-path negations; the deny rule stands.
`scripts/ci/no_ignored_sources.sh` now fails any build where a source file is
gitignored and untracked, and it runs first in the gates job because it answers
the question every other check assumes.

**2. The detection haircut was read from a process-global inside the
relaxation loop.** `util::detection_haircut_bps()` latches a `OnceLock` from
`DETECTION_HAIRCUT_BPS` and was called once per edge at three sites in
`graph.rs` — a late configuration lookup on the scanner's hottest path (§2.4
forbids it). Because the value latches for the whole process, one test that set
the variable fixed it for the entire test binary: nineteen sibling tests failed
or passed depending on which ran first. The test that set it already carried a
comment admitting this and wrapped its own assertion in `if
detection_haircut_bps() == 25`, so **that assertion had never actually run** —
and when finally made to run it failed, because `apply_slippage` truncates and
a 25 bps haircut on a numerator of 1000 is 30 bps. The haircut is now a field
on `Graph`, resolved once at construction; tests set it on the graph instead of
shouting it at the process.

**3. `apex-venues` did not build standalone.** Its `quote_cl`/`quote_univ3`
tests drive the TTL cache with `tokio::time::pause`, which needs tokio's
`test-util` feature. Inside the workspace, feature unification borrowed it from
`arb-exec`; `cargo clippy -p apex-venues` alone did not. Per-crate clippy in CI
is what caught it, which is an argument for keeping the gate per-crate rather
than running it once over the workspace.

**Also recorded:** `cl_load::log_cl_quote_parity` has no callers and never did,
and neither did the `ARBOT_CL_QUOTE_PARITY` flag that gated it. It is carried
forward rather than deleted because it is exactly the local-vs-quoter
comparison Task 2.2's three-way differential needs. Its internal env check is
gone — `apex-venues` does not read the environment, and gating is the caller's
job.

### Task 2.1 — `ExactPricingEngine` trait and the CL implementors

- [ ] **Step 1: Write the failing test:**

```rust
// crates/apex-math/tests/engine_contract.rs
#[test]
fn every_engine_implements_the_full_contract() {
    fn assert_engine<E: ExactPricingEngine>() {}
    assert_engine::<UniV3Engine>();
    assert_engine::<SlipstreamEngine>();
    assert_engine::<PancakeV3Engine>();
    assert_engine::<CpmmEngine>();
    assert_engine::<SolidlyEngine>();
    assert_engine::<CurveEngine>();
    assert_engine::<BalancerEngine>();
}

#[test]
fn next_state_exact_round_trips_through_quote_exact() {
    // Quoting X then quoting Y against next_state must equal quoting X+Y in one call,
    // to the wei, for every CPMM and CL engine.
    proptest!(|(x in 1u128..1e21 as u128, y in 1u128..1e21 as u128)| {
        let s0 = fixture_cl_state();
        let q1 = UniV3Engine.quote_exact(&s0, &order(x))?;
        let s1 = UniV3Engine.next_state_exact(&s0, &order(x))?;
        let q2 = UniV3Engine.quote_exact(&s1, &order(y))?;
        let qc = UniV3Engine.quote_exact(&s0, &order(x + y))?;
        prop_assert_eq!(q1.amount_out + q2.amount_out, qc.amount_out);
    });
}
```

**Correction, recorded 2026-09-22 — the equality above is wrong for V2, and the
implemented test asserts `<=` with the gap measured.** Splitting a swap can
never produce *more* than doing it at once; whether it produces *less* depends
on where the venue keeps its fee, and the two families differ:

| amount in | CL ticks crossed | CL split gap | CPMM split gap |
|---|---|---|---|
| 1e12 | 0 | 0 wei | 2 wei |
| 1e15 | 0 | 1 wei | 1,495,497 wei |
| 1e18 | 0 | 0 wei | 1,491,776,539,545 wei |
| 1e19 | 1 | 1 wei | 145,882,581,575,060 wei |
| 2e19 | 1 | 0 wei | 569,345,526,271,364 wei |

Uniswap V3 holds fees **outside** the swappable curve — `computeSwapStep`
returns `feeAmount` separately, the price moves only by the post-fee input, and
the fee accrues to `feeGrowthGlobal`. So V3 composition telescopes exactly and
the only gap is per-step rounding: 0–1 wei, including across a tick crossing.
Uniswap V2 adds the fee straight to reserves, so the second half of a split
trades against a pool the fee has already moved and pays fee-on-fee — a real,
quadratic, second-order loss (0.14 bps at 2e19 against 1000/2000 reserves).

The assertion that carries the weight is the **direction**, not the magnitude.
`q1 + q2 > qc` would mean the model believes chopping an order against a single
pool creates value out of nothing, and since ranking maximises gross the
searcher would chase it on every block — the same shape as the `liquidity()`
overstatement that made one Base pool the most profitable edge on the chain.

**Two gaps this task surfaced, both recorded rather than papered over:**

1. **`cl_math` had no `get_tick_at_sqrt_ratio`.** `next_state_exact` needs it:
   a swap that stops mid-range leaves the price between ticks, and the state it
   produces must carry the tick that price sits in or the next swap navigates
   the ladder from the wrong side of a boundary. Implemented as a binary search
   over `get_sqrt_ratio_at_tick` rather than a port of v3-core's `log2`
   assembly — searching the function being inverted cannot disagree with it,
   whereas the assembly agrees only because its magic constants were chosen to,
   and a transcription slip there is wrong on a narrow band of prices and right
   everywhere else. The exhaustive monotonicity test that makes the round trip
   a proof runs at every one of the 1,774,545 ticks, in release, in CI.
2. **The multi-tick loop leaves the post-swap state undetermined at a tick
   boundary.** When a swap comes to rest exactly on an initialized tick,
   v3-core crosses eagerly and this port deliberately does not (crossing there
   can mislabel a complete quote as exhausted). The quote is unaffected; the
   *liquidity* on the far side is not determined. `next_state_exact` returns
   `NotRepresentable` rather than publishing pre-crossing liquidity.

**Also corrected:** ladder exhaustion is `NotRepresentable`, **not** a
`RevertCondition`. The pool would very likely fill the trade; we cannot see far
enough to say by how much. Recording it as a revert would tell the simulator
the chain refused a trade the chain never saw, and every revert statistic built
on that is then wrong.

- [ ] **Step 2: Run and observe the failures** — the trait does not exist; the round-trip will additionally expose any rounding drift in `next_state_exact`.
- [ ] **Step 3: Implement** the trait and wire each existing quoter to it. `cl_math`/`cl_swap`/`cl_ticks` are moved unchanged; only the trait impl is new.
- [ ] **Step 4: Run and observe the passes.**
- [ ] **Step 5: Commit.**

### Task 2.2 — Three-way differential harness (§35.1)

- [ ] **Step 1: Write the failing test** — `differential::three_way_agreement` over the admitted pool set × {dust, 0.1×depth, 1×depth, 3×depth} × both directions, comparing on-chain quoter vs Rust exact vs REVM fork on amounts, fees, state transition, rounding and reverts. Seed it with the known-bad pool `0xc211e1f853a898bd1302385ccde55f33a8c4b3f3` and assert it is **reported as divergent**, not silently passed.
- [ ] **Step 2: Run and observe the failure.**
- [ ] **Step 3: Implement** the harness, reusing `src/bin/cl_parity.rs`'s sweep logic.
- [ ] **Step 4: Run and observe the pass** — including the known-bad pool being flagged and the `cl_parity_gate` verdict for it being `untrusted`.
- [ ] **Step 5: Commit.**

### Task 2.3 — `VenueAdapter` and per-venue circuit breakers

- [x] **Step 1: Write the failing tests** — every admitted venue has an adapter implementing all six methods; `gas_model` and `classify_revert` have no default impls; each adapter has an independent breaker that can trip without affecting others.
- [x] **Step 2: Run and observe the failures.**
- [x] **Step 3: Implement.** (`src/venue_adapter.rs` is deleted in Task 2.7 with the rest of the legacy pricing surface — it has zero implementors, so nothing depends on the order.)
- [x] **Step 4: Run and observe the passes.**
- [x] **Step 5: Commit.**

**Delivered 2026-09-23**, with four decisions worth recording.

**Five methods now, two later, none by default.** §10.4 sketches seven.
`simulate_call_graph` and `encode_exact` are typed over `CallGraph` and
`EncodedAction`, which belong to Phases 4 and 5; inventing those shapes now
would mean guessing at two phases of design and then contradicting the guess.
They are not declared. The risk in leaving them out is the obvious one — when
Phase 4 adds `simulate_call_graph`, the path of least resistance is a default
body so existing adapters keep compiling, which is exactly the failure the
no-defaults rule guards against. `scripts/ci/no_adapter_defaults.sh` reads the
trait block and fails if any method signature ends in `{` instead of `;`, so
the rule holds for methods that do not exist yet. Mutation-tested.

**Not one venue gas figure has ever been measured.** The six constants in
`venues.rs` (`ESTIMATED_GAS_UNIV3 = 140_000` and friends) are round numbers
with no recorded source, and gas is a first-order term in the profit decision —
an unmeasured gas number is precisely the "hidden economic assumption" §8.3
forbids. `GasModel` now carries `GasProvenance`, and every adapter returns
`UnmeasuredLegacyConstant`. A test asserts that, and is expected to fail in
Phase 3 when the measurements land: the way to satisfy it is to measure, not to
edit a comment.

**Curve and Balancer have adapters that refuse.** Both are `abigen!` clients
with no local curve implementation, so `quote_exact` returns
`NotRepresentable`. They get adapters anyway so the venues are visible to the
registry, the breaker and the metrics, and so the absence of local maths is a
value the caller receives rather than a venue that silently is not there. A
quote that forwarded the on-chain quoter's number and called itself exact would
make INV-16 — *"no router quote is authoritative"* — a dead letter.

**Losing a race is not a venue fault.** The breaker's real judgement is what
counts as a failure, and `counts_against_venue` is exhaustive over
`RevertClass` with no catch-all so a new class forces the decision.
`MinOutNotMet` does not count: it means someone moved the pool between quote
and execution, it happens most on the venues with the most flow, and counting
it would trip exactly the venues worth trading on. `OutOfGas` does not count
either — a gas ceiling is ours to set, and tripping the venue hides the fix.
`Unknown` does count: an unexplained failure on one venue is when to back off.

Two things the implementation caught that the specification did not:

* **A dropped probe permit would have wedged a venue permanently.** The
  half-open state admits exactly one caller, claimed by compare-and-swap. If
  that caller panicked or returned early on `?` before reporting, the claim was
  never released and `try_admit` refused every caller for the life of the
  process — a permanent outage caused by an error path, on the one venue that
  had just started recovering. `Permit` is now RAII: `Drop` resolves an
  unreported probe as a failure, which is the conservative reading of "we
  admitted a call and never heard back".
* **Empty revert data is not evidence of out-of-gas.** It is what an
  out-of-gas frame returns, and also a bare `revert()`, a call to an address
  with no code, and several assembly paths. `RevertClass::OutOfGas` has to come
  from gas accounting, which the classifier does not see. Guessing would tell
  the risk engine to raise gas limits in response to a venue rejecting us.

### Task 2.4 — Pool admissibility and registry verification (§6.3, C-11/B-7)

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-venues/tests/admission.rs
#[test]
fn a_pool_missing_any_admissibility_field_is_rejected() {
    for missing in PoolAdmission::REQUIRED_FIELDS {
        let rec = admission_fixture_without(missing);
        assert!(VenueRegistry::admit(rec).is_err(), "must reject when {missing} is absent");
    }
}

#[test]
fn an_address_with_no_bytecode_is_never_admitted() {
    assert!(VenueRegistry::admit(admission_with_empty_code()).is_err());
}
```

- [ ] **Step 2: Run and observe the failures.**
- [ ] **Step 3: Implement** `PoolAdmission`, `VenueRegistry::admit`, and `scripts/data/verify_registry_bytecode.py` which checks `extcodehash` for every address in `config/registry.json` and every pool in `data/**/pools.jsonl`. Delete `base_venues_complete.yaml` and `generate_base_venues.py`.
- [x] **Step 4: Run and observe the passes**, and run the verification script over the whole registry — recording any fabricated address it finds in `docs/apex/reports/registry-verification-<date>.md`.
- [x] **Step 5: Commit.**

**Delivered 2026-09-23.** `apex-venues::admission` with 10 tests, and
`scripts/data/verify_registry_bytecode.py` with an offline self-test that
exercises the real RPC path against a localhost stub.

**The admissible universe is 232 pools, not 558,095.** The inventory holds
558,095 pool records; **232** carry `hub_usd_liquidity ≥ $100k`, the measured
floor. Every one of the other 557,863 would be refused by `VenueRegistry::admit`
on depth alone. This sharpens §36.2 and [[inventory-is-the-constraint]]: the
thing being searched is three orders of magnitude smaller than the thing being
stored, and the verification script scopes to the admissible band for exactly
that reason — verifying a pool that cannot be admitted proves nothing about
what the engine would trade.

**G-VENUE-1 is NOT closed. Base and Ethereum could not be reached.** Both the
public and the keyed Base endpoints return HTTP 403 from this environment, and
the Ethereum endpoint refuses the connection. Optimism answered and **11 of 11
addresses hold code**. The report records 402 addresses as *unread*, and unread
exits non-zero — "we could not check" is not "it is fine". The gate needs one
run from a host with egress to Base.

Two false-reassurance bugs in the report generator, both caught by reading the
output rather than the code:

* It printed *"every address on this chain holds code"* beside the unread
  count, for a chain where nothing had been read.
* A chain with **no RPC configured at all** returned `(no missing, 0 unread)`
  and so rendered as clean. "No endpoint configured" is a reason the check did
  not happen, not a result.

Both are the same shape as the defect the script exists to find, which is worth
noting: a verification report that reassures about work it did not do is more
dangerous than no report.

**Also measured:** `mainnet.optimism.io` answers a single `eth_blockNumber` and
then refuses an eleven-request JSON-RPC batch with HTTP 413. Batching is an
optimisation; the script now falls back to single calls, because being unable
to batch must not be indistinguishable from being unable to verify.

**On deleting the fabricated files.** `base_venues_complete.yaml` and
`generate_base_venues.py` are **untracked**, so they cannot arrive through a
clone and deleting them is unrecoverable. Nothing in the tracked tree reads
either one — verified — and `scripts/ci/no_fabricated_venue_sources.sh` now
fails the build if anything ever does, scanning tracked files with docstring
and doc-comment awareness so the modules that exist *because* of this hazard
can still name it. The files remain on disk for their owner to remove; they are
inert.

### Task 2.5 — Finite-size candidate generation (Engine C, §12.3)

- [x] **Step 1: Write the failing test** — a fixture where infinitesimal rates show no negative cycle but a finite size of 0.4 WETH is profitable across two venues; assert Engine C finds it and Engine A does not.
- [x] **Step 2: Run and observe the failure.** **The fixture does not exist.** See below.
- [x] **Step 3: Implement** pairwise cross-venue mismatch search over the cheap frontier.
- [x] **Step 4: Run and observe the pass.**
- [x] **Step 5: Commit.**

### Correction, recorded 2026-09-23 — the specified fixture is impossible, and Engine C's value runs the other way

**No fixture exists where infinitesimal rates show no negative cycle and a
finite size is profitable.** Every venue this repository prices — constant
product, the Solidly curves, concentrated liquidity — has an output that is
concave in its input and zero at zero. For any such function the average rate
over `[0, x]` is at most the marginal rate at 0, and a cycle's finite-size
gross is the product of its hops' average rates. So the finite-size gross can
never exceed the marginal gross. "Infinitesimal rates say no, finite size says
yes" describes a **convex** market. Asserted as a property test over random
spreads, fees and sizes, not just the six hand-picked fixtures.

**Engine C earns its place by refusing, not by finding.** `graph.rs`'s edge
weights are rate-only — `compute_edge_weight` is `-ln(rate)` and nothing adds a
gas term, whatever the stale comment on `Edge::weight` said (now corrected);
`venues.rs` depends on exactly that when it reuses a cached edge set across a
gas-price move. So Engine A calls any cycle with a marginal gross above parity
a negative cycle, **regardless of whether any size pays for the transaction**.
Measured on a two-pool fixture with gas at 0.00002 WETH (~1 cent, this
repository's own figure):

| spread | marginal gross | best gross over all sizes | clears gas |
|---|---|---|---|
| 60.5 bps | +0.000228% | +0.00000006 WETH | **no — 300× short** |
| 61 bps | +0.000725% | +0.00000066 WETH | **no** |
| 63 bps | +0.002713% | +0.00000921 WETH | **no** |
| 65 bps | +0.004701% | +0.00002765 WETH | yes |
| 80 bps | +0.019611% | +0.00048050 WETH | yes |
| 200 bps | +0.138890% | +0.02381796 WETH | yes |

Engine A accepts all six. Three cannot be traded at any size. That band is not
a corner case — it is exactly where the cheap frontier sits, and it is
consistent with the census finding **0 of 750** candidates net-positive and
with 15.2% of prep-sent candidates already carrying `local_gross_bps < 0`.

**And it answers a question Engine A cannot ask.** The optimum on that fixture
is 0.120 WETH at 65 bps and 0.490 WETH at 80 bps — so the plan's *"0.4 WETH"*
was the right order of magnitude for the wrong reason. A rate-only search
cannot express a quantity at all.

**Two things measured on the way:**

* **A dust probe measures rounding, not a rate.** At 1e-6 WETH the round trip
  reads −21 bps on a fixture whose rate is −5, because the middle leg is
  6-decimal USDC and integer division throws away a meaningful fraction of
  2,000 raw units. It errs pessimistic — the safe direction — but the marginal
  rate is not measurable that way. 1e-3 WETH puts truncation below 0.01 bps.
* **A basis point is too coarse to express the disagreement.** At a 61 bps
  spread the gross is +0.7 *thousandths* of a basis point. `marginal_gross_bps`
  rounds that to zero, so the test asks Engine A's actual question
  (`output > input`) rather than a rounded report of it.

**Not delivered:** the other Engine C modes §12.2 lists — k-shortest simple
routes, k-shortest cycles, same-pair split, event-targeted, backrun and
liquidation templates. Pairwise cross-venue mismatch is the one the cheap
frontier needs, and the rest are search *topologies* over the same finite-size
evaluator.

### Task 2.6 — Close the pricing provenance (§1.1.1)

- [ ] **Step 1: Write the failing test:**

```rust
// crates/apex-venues/tests/fast_path_exactness.rs
#[test]
fn no_fast_path_cl_edge_is_silently_single_tick() {
    let edges = fast_path_edges_for(&fixture_frontier());
    for e in edges.iter().filter(|e| e.is_concentrated_liquidity()) {
        assert!(
            e.tick_ladder.is_some() || e.exactness == Exactness::Approximate,
            "CL edge {:?} prices single-tick but claims to be exact", e.pool
        );
    }
}

#[test]
fn exactly_priced_edges_carry_no_haircut() {
    // plan.rs:26 — "Set 0 once the simulator is multi-tick."
    let e = exact_cl_edge();
    assert_eq!(tick_buffer_bps_for(&e), 0);
}
```

- [x] **Step 2: Run and observe the failures** — today every fast-path CL edge has `tick_ladder: None` and a 50 bps haircut.
- [x] **Step 3 (partial): make the approximation visible.** See the correction below — ladder attachment is NOT the right fix, and criterion (b) was mis-stated.
- [ ] **Step 4: Re-run the event-triggered census under exact pricing** and diff it against the recorded run. **Blocked: no egress to Base from the development environment** (HTTP 403 on both the public and keyed endpoints).
- [ ] **Step 5: Record** the result in `docs/apex/reports/frontier-exact-<date>.md` and **revise or confirm §36.2**.

### Correction, recorded 2026-09-23 — (b) was wrong, and (a)'s fix is the second branch

**Criterion (b) as written — *"`cl_tick_buffer_bps` is 0 on exactly-priced
edges"* — conflates two different things, and the first is already true.**
`plan::hop_expected_out` already branches on whether the quote actually crossed
ticks: `used_multi_tick == true` takes `cl_exec_buffer_bps` (75) and
`false` takes `cl_tick_buffer_bps` (50). A test already guards against those
arms being inverted. So the tick buffer is *not* applied to a modelled
crossing, and has not been.

What an exactly-priced edge *does* still carry is the 75 bps **execution**
buffer, and that is not a modelling fudge: it is a measured model-versus-router
gap. Slipstream pool `0xdbc6998296caa1652a810dc8d3baf4a8294330f1`, 4340.955932
USDC → WETH through the deployed router with the planner's own path bytes — the
model said 1.891636 WETH, the router paid 1.885833, a ~31 bps overstatement,
observed on 100% of candidates across 57k+ records and invariant to trade size,
tick buffer and pricing model. Driving that to zero means closing the gap to
the router, not deleting the buffer. Criterion (b) is therefore restated:
**the tick buffer must not be applied to a modelled crossing** (already true,
now cross-checked), and the execution buffer stays until the router gap is
measured to zero.

**Criterion (a)'s fix is the second branch, not the first.** The plan offered
"every fast-path CL edge carries a tick ladder, **or** the edge is explicitly
marked `Exactness::Approximate`". Attaching ladders on the fast path is the
wrong move, and `base_fast.rs:1570` says why: *"No cached tick ladder: sizing
requotes on chain, and a stale ladder would be worse than none."* That design —
rank cheaply, requote exactly — is sound. What was unsound is that nothing
recorded which number was which: `cl_hop_out` logged *"multi-tick ON but this
edge carries NO ladder; forced to single-tick"* at **debug** level, and the
ranking then compared that number against exactly-priced ones as if they were
commensurable.

Delivered instead:

* `Edge::exactness()` — derived from what was actually modelled, not from the
  venue name. A CL edge is `Proven` only with **both** a ladder to cross
  against and the pool state to cross from; three of those four combinations
  are `Approximate`. Constant product and Solidly are `Proven` (the reserves
  are the whole state). Curve, Balancer and V4 are `Approximate` and cannot
  become otherwise — there is no local implementation of either curve, and
  INV-16 says a router quote is not authoritative.
* `Edge::may_authorize_live_dispatch()` — INV-17's predicate, per hop.
* A **cross-check in the pricing path**, so the method is load-bearing rather
  than a fact nobody reads. `used_multi_tick` ("the quote crossed ticks") and
  `edge.exactness()` ("the edge carries what crossing needs") are derived
  independently and must agree. When a multi-tick quote appears on an edge that
  is not exactly priced, the conservative arm wins and the larger single-tick
  buffer is charged; the reverse disagreement warns, because an edge claiming
  exactness that fell through to single-tick has a claim that does not hold at
  that size.
* Tests pinning **both** halves: that not one fast-path CL edge is exactly
  priced today, and that the constant-product half of the same path *is* — a
  blanket "the fast path is approximate" would have been the easy answer and
  the wrong one.

**Still open for G-PRICE-2:** step 4's census re-run needs egress to Base. And
the candidate log does not yet carry exactness, so the *proportion* of
candidates priced approximately is not measurable from the logs. That belongs
with Phase 8's observability work rather than here: `log_candidate_stage`
already takes sixteen positional arguments with two adjacent booleans
(`bridge`, `liquidation`) and is called from 35 sites, so adding a
seventeenth is the wrong move — it wants a parameters struct, and §26/§27
restructure that record anyway.

### Task 2.7 — Resolve the `UNKNOWN` register entries owned by this phase

- [ ] **Step 1:** Run the differential harness against Curve (`get_dy`) and Balancer (`queryBatchSwap`); record results.
- [ ] **Step 2:** Audit every `liquidity_cache` call site and assert non-authoritative use.
- [ ] **Step 3:** Update §4.7 in `PLAN.md` with the resolved classifications and commit.

**Tests:** engine contract, property round-trip, three-way differential, fuzz (`apex-math/fuzz` targets from §11.5), venue adapter suite, admission table test, Engine C fixture.
**Benchmarks:** `T_price` p99 ≤ 15 ms for a 4-hop closure; local CL quote ≤ 40 µs.
**Acceptance criteria:**
1. Three-way differential shows **0 bps divergence** on every pool holding a `cl_parity_gate` trusted verdict.
2. Every divergent pool is either explained or its venue is marked `Exactness::Approximate` and shadow-only.
3. 24 h fuzz run on `apex-math` with zero panics and zero `None` returns on inputs that should be representable.
4. Every production address in `config/registry.json` and `data/**` has verified bytecode; the report lists zero unverified addresses.
5. `base_venues_complete.yaml` and `generate_base_venues.py` are gone.
6. Curve and Balancer exactness resolved from `UNKNOWN` to a definite classification.
7. **Pricing provenance closed (§1.1.1).** All of: (a) every fast-path CL edge carries a tick ladder, or the edge is explicitly marked `Exactness::Approximate`; (b) `cl_tick_buffer_bps` is 0 on exactly-priced edges — the constant's own comment says *"Set 0 once the simulator is multi-tick"*; (c) the cheap frontier is re-measured end-to-end under exact pricing; (d) §36.2's route-surface row is confirmed or revised against that measurement, and the result is recorded in `docs/apex/reports/frontier-exact-<date>.md`.

**Failure criteria:** any unexplained pricing divergence; any unverified address admitted; a fuzz panic; **shipping Phase 3 sizing against the provisional frontier**.
**Exit gate:** **G-PRICE-1** (exactness proven or venue demoted), **G-PRICE-2** (pricing provenance closed and the frontier re-measured), **G-VENUE-1** (admission gate enforced).

---

## PHASE 3 — Exact sizing and the complete cost model

**Objective:** Discrete integer sizing that is structurally impossible to bypass, and a `TotalExecutionCost` that models Base's real economics.

**Dependencies:** Phases 0–2.

**Files:**
- CREATE `crates/apex-econ/{Cargo.toml,src/lib.rs,src/sizing/{mod.rs,continuous.rs,discrete.rs},src/cost/{mod.rs,l1_data.rs,failure.rs,calldata.rs},src/flash/{mod.rs,router.rs},src/ev/{mod.rs,scenario.rs},src/eligibility.rs}`
- MOVE `src/sizing.rs` → `crates/apex-econ/src/sizing/continuous.rs`; `src/flash_loan.rs` → `src/flash/mod.rs`; `src/convex.rs` → `src/allocation/convex.rs` (dormant until Phase 10)
- REMOVE `src/fees.rs` (REBUILD) — its Arbitrum per-byte logic moves to `crates/apex-chain/src/arbitrum/fee.rs` in Phase 15
- MODIFY `crates/apex-types/src/cost.rs`

### Task 3.1 — `DiscreteSize` makes INV-18 structural

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-econ/tests/discrete.rs
#[test]
fn continuous_result_cannot_become_a_candidate_input() {
    let t = trybuild::TestCases::new();
    // fixture attempts: candidate.input_amount = continuous_optimum;   // f64 / U256
    t.compile_fail("tests/compile_fail/continuous_into_candidate.rs");
}

#[test]
fn discrete_refinement_is_never_worse_than_the_continuous_optimum() {
    proptest!(|(seed in any::<u64>())| {
        let route = random_route(seed);
        let c = continuous::optimize(&route)?;
        let d = discrete::refine(&route, c)?;
        prop_assert!(exact_profit(&route, d.get()) >= exact_profit_at_nearest_integer(&route, c),
                     "refinement must not regress");
    });
}

#[test]
fn no_profitable_size_returns_none_not_zero() {
    let route = route_with_hurdle_above_best_edge();
    assert!(discrete::refine(&route, continuous::optimize(&route).unwrap()).is_none());
}
```

- [x] **Step 2: Run and observe the failures.**
- [x] **Step 3: Implement** `DiscreteSize(U256)` with a private field and a single constructor `discrete::refine(...) -> Option<DiscreteSize>`; change `Candidate::input_amount` to `DiscreteSize`.
- [x] **Step 4: Run and observe the passes.**
- [x] **Step 5: Commit.**

**Delivered 2026-09-23.** `apex-econ` with `sizing::{continuous, discrete}`,
12 tests including two property tests.

**`compile_fail` doctests, not `trybuild`.** `trybuild` compares full stderr
against a recorded file, pinning the test to a rustc version and turning a
diagnostic reword into a red build. `compile_fail` asserts only that the code
does not compile, which is the actual claim. Its known weakness — it also
passes when a snippet fails for an unrelated reason — is answered by pairing
each forbidden case with a twin that differs *only* in the forbidden step and
does compile. **Mutation-verified:** making `DiscreteSize`'s field `pub` turns
the first case green and the test goes red.

**`--all-targets` does not run doctests.** It expands to `--lib --bins --tests
--benches --examples`. The INV-18 guards live in doc comments, so CI would
never have executed them — a type-level proof that was decorative from the
moment it was written. A `cargo test --workspace --doc` step now runs them.

**The guarantee holds by construction, not by cleverness.** The refined size is
never worse than the nearest integer to the continuous optimum because the
climb *starts* there and only ever moves to a strictly better point. The
alternative framing — "search the range and trust it beats the warm start" — is
a claim about the optimiser, and optimisers on a lattice with truncating
integer arithmetic are where quiet regressions live. Net profit is concave so
the climb also finds the global integer optimum, but the guarantee does not
depend on that.

**Three reasons an f64 optimum is not a size**, recorded on `continuous.rs`
because only the first is obvious: no AMM accepts a fractional wei; `f64` has
53 bits of mantissa and a wei-denominated size routinely exceeds 2^53, so the
continuous stage cannot *represent* every integer in its own search range
(tested — two adjacent wei above 2^60 collapse to the same `f64`); and the two
stages evaluate the objective in different arithmetic, with integer truncation
always rounding against the trader.

**`apex_types::compat` landed here, not in Phase 1.** `lib.rs` reserved it for
*"Phase 1, when the first crate actually has to cross it"*. Phase 1's state
primitives never did. `discrete::refine` is the first thing that does: it reads
a size from `apex-math`'s route evaluation (ethers) and mints an
`apex_types::DiscreteSize` (alloy). The conversion is total and exact in both
directions, tested at every boundary value including `U256::MAX` and 2^255.

### Task 3.2 — `TotalExecutionCost` and gas-limit/gas-used separation (INV-19)

- [ ] **Step 1: Write the failing tests** — all nine cost components present and non-defaultable; `GasLimit`/`GasUsed` non-convertible (trybuild); `conservative_total` uses p99 gas and full failure cost; an OP Stack L1 data fee derived from compressed size reproduces a recorded real receipt's `l1Fee` within 1%.
- [x] **Step 2: Run and observe the failures.**
- [x] **Step 3: Implement** `cost/`, with `l1_data.rs` computing the OP Stack L1 fee from compressed calldata size and Ethereum base/blob fee conditions, ~~validated against recorded Base receipts in `tests/fixtures/base_receipts.json`~~ — **the validation is NOT done.** See below.
- [x] **Step 4: Run and observe the passes.**
- [x] **Step 5: Commit.**

**Delivered 2026-09-23**, minus the validation, which is blocked.

**Acceptance criterion 2 is not met and cannot be met here.** *"L1 data fee
reproduces recorded Base receipts within 1% across ≥ 50 receipts"* needs Base
receipts. There are none in this repository — `broadcast/` holds Ethereum
deploy artifacts, which carry no `l1Fee` — and the development environment has
no egress to Base. The Fjord arithmetic is implemented from the published
constants and exercised by its own algebra; **whether those constants match
what Base's oracle currently holds is unverified.**

That is carried in the type, not in a comment. `L1FeeModel` reports a
`Validation`, whose only constructible value today is
`FromPublishedConstantsOnly`, and `may_price_a_live_dispatch()` is false for
it. `CompressedSize` is separately `Measured` or `Estimated`, and a fee is
authoritative only when both halves are — a validated model fed an estimated
size is still an estimate. `the_l1_model_has_not_been_checked_against_a_receipt`
exists to **fail** when someone validates it, forcing the evidence into the
type. Same device as `no_venue_gas_figure_has_been_measured_yet`.

**Why model it locally at all**, given the oracle answers exactly: the legacy
path calls `GasPriceOracle.getL1Fee(bytes)` once per estimate, which is a round
trip on the hot path and, more decisively, makes Task 3.3 impossible — choosing
between encodings means pricing several of them.

**Three things the implementation surfaced:**

* **The Fjord intercept is negative** (−42,585,600), so the linear term goes
  below zero for transactions under ~51 compressed bytes and the
  `minTransactionSize` clamp is the *operative branch*, not a safety rail.
  Computing that in unsigned arithmetic wraps; the near-miss version —
  saturating at zero — would quietly charge every small transaction the floor
  for the wrong reason. Tested both ways.
* **A reverted transaction pays the whole L1 data fee.** The calldata reached
  Ethereum the moment the transaction was included; the revert is an L2
  detail L1 never learns. So the failure cost is not a fraction of the success
  cost — its L1 half is identical and only its L2 half is smaller. On a Base-
  shaped fixture the naive "scale the total by the gas ratio" model understates
  the failure cost by **over 50%**, in the direction that makes marginal trades
  look acceptable. Measured in `scaling_the_total_by_a_gas_ratio_understates_the_failure_cost`.
* **The fee is linear in the L1 prices only up to a wei of truncation**, and
  the truncation rounds *against the trader* — `floor(2a) ≥ 2·floor(a)` — so a
  cost model built on it cannot understate.

**INV-19 now has an in-language proof.** §23.4's `GasLimit`/`GasUsed`
separation was guarded only by `scripts/ci/no_gas_conversion.sh`, with the
recorded reason that Rust has no negative trait bounds. `compile_fail`
doctests are that proof, version-independent and paired with a compiling twin;
**mutation-verified** by adding a `From<GasLimit> for GasUsed` impl, which
turns one case green and the test red. The grep guard stays: it also catches
`as`-casts and field-level conversions that a trait-based test cannot see.

### Task 3.3 — Calldata optimizer (§23.3)

- [x] **Step 1: Write the failing test** — two encodings of the same route with different calldata sizes produce different `l1_data_fee` and the optimizer picks the smaller when the outputs are economically identical.
- [x] **Step 2: Run and observe the failure.** **Step 3: Implement.** **Step 4: Observe the pass.** **Step 5: Commit.**

**Delivered 2026-09-23, with the task's premise corrected.** *"Picks the
smaller"* is the wrong rule. Fjord prices the **FastLZ-compressed** size, and
ABI encoding pads everything to 32-byte words — so a "wasteful" padded encoding
is mostly zero bytes in a long repeating pattern, which is what a compressor
removes. Measured on a fixture: a 384-byte padded encoding costs **less** than
a 200-byte packed one. The optimizer therefore ranks by **modelled fee** and
`cheapest` is deliberately unable to see a byte count at all.

**The estimator is a proxy and says so.** Fjord's input is
`flzCompressLen(tx_bytes)`; this crate does **not** implement FastLZ. A port
written from memory would produce an authoritative-looking number that cannot
be checked against Base from here — the same shape of mistake as a fabricated
venue address, in a different costume. `byte_class_size` is the pre-Ecotone
Bedrock measure (`4·zeros + 16·nonzeros`) used as a compressibility proxy:
right about what dominates here, wrong about everything a real compressor does
with repetition.

That is enough for ranking and not enough for a fee, which is the reason
`CompressedSize` carries its provenance. **Ranking needs the estimator to be
monotone in the truth; quoting a fee needs it to be equal to the truth.** One
number, two jobs, two accuracy requirements — and the test asserts the
monotonicity it actually depends on rather than an accuracy it does not have.

### Task 3.4 — Flash-source router (§19)

**Delivered 2026-09-23.**

**§19's objective is missing an input, and the omission matters.** The rule is
`argmin(premium + gas_overhead·gasPrice + failure_risk_cost + availability_penalty)`.
Three terms are properties of the provider. The fourth is not: an availability
penalty is `P(unavailable) × (what the outage costs)`, and what an outage costs
is the **opportunity**, which belongs to the trade. A provider that is 1% flaky
is fine for a trade worth a cent and unacceptable for one worth ten thousand
dollars. So `select` takes the opportunity's value rather than pretending a
quote can be ranked alone, and a test pins the crossover: with a 0.002 ETH
premium gap and a 2% outage rate the two providers swap at an opportunity of
**0.1 ETH**, exactly `gap / outage_rate`.

**Probabilities cross into wei exactly once, and pessimistically.**
`availability_probability` and `reliability_score` are `f64`; every cost is
wei. `to_ppm` converts once and rounds **down**, treating a provider as
slightly less available than it claims — rounding the other way would shave the
penalty on precisely the providers whose availability is least certain. A NaN
availability becomes zero, which *excludes* the provider rather than scoring it
well.

**Exclusion is not a bad score.** Insufficient capacity, a zero availability
and a wrong asset remove a provider from the ranking and are reported with the
reason, which is what makes §19.4's outage requirement checkable at all. Every
provider excluded returns `None` — a different fact from "the cheapest one is
expensive", and reported as such rather than by a sentinel.

### Task 3.4 — Flash-source router (§19)

- [ ] **Step 1: Write the failing tests** — `FlashSourceQuote` carries all nine §19.1 fields; selection is `argmin(premium + gas_overhead·gasPrice + failure_risk_cost + availability_penalty)`; a provider whose measured capacity is below the required amount is excluded (preserving the existing capacity-bounding behaviour); a single provider outage does not block selection (§19.4).
- [ ] **Step 2: Run and observe the failures.** **Step 3: Implement.** **Step 4: Observe the passes.** **Step 5: Commit.**

### Cycle rotation is not determined, recorded 2026-09-23

`bellman_ford_inner` searches every candidate start vertex under
`rayon::par_iter` with a **shared `abort` flag**, so whichever worker finishes
first decides which rotation of a cycle is reported. The same arbitrage entered
at A and entered at B are the same cycle and different `edge_indices`.

Found when `graph::tests::bellman_ford_preserves_parallel_edge_path` went red
on a GitHub runner after 65 consecutive green local runs — including 25 under
deliberate CPU contention and across `RAYON_NUM_THREADS` of 1, 2, 4 and 8. It
reported `[2, 1]`: the same cycle, entered from B. The property the test
existed for — that the *better* of two parallel edges is chosen — held.

The test now asserts that property and not the rotation. The finding is
larger than the test, though: **anything downstream that needs a canonical
rotation has to canonicalise it**, because the search does not. `cycle_start`
decides a candidate's start token, and therefore its flash-loan asset and its
profit denomination. This is recorded rather than fixed: making the search
deterministic means giving up the abort optimisation, and choosing between
those is a Phase 12 question (§13 joint allocation) rather than a Phase 3 one.

### Task 3.5 — Scenario-conditioned EV and the eligibility gate (§2, INV-20, INV-23)

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-econ/tests/ev.rs
#[test]
fn eligibility_requires_every_clause() {
    for clause in EligibilityGate::CLAUSES {           // all 9 from §2.3
        let ctx = passing_context_but_failing(clause);
        match EligibilityGate::evaluate(&ctx) {
            Decision::Reject { rule, .. } => assert_eq!(rule, clause),
            other => panic!("clause {clause} did not gate: {other:?}"),
        }
    }
}

#[test]
fn usd_mark_cannot_flip_admission() {
    proptest!(|(mult in 0.5f64..1.5)| {
        let base = admitted_set(&candidates(), UsdMark::exact(1.0));
        let perturbed = admitted_set(&candidates(), UsdMark::exact(mult));
        prop_assert_eq!(base, perturbed, "INV-20: USD marks must not change admission");
    });
}

#[test]
fn scenario_ev_is_not_a_product_of_independent_probabilities() {
    // A correlated scenario set must produce a different J(a) than the naive product.
    let naive = p_land * p_state * p_exec * p_net * profit - c_fail;
    let j = scenario_ev(&correlated_scenarios(), c_fail);
    assert_ne!(j, naive);
}
```

- [x] **Step 2: Run and observe the failures.** **Step 3: Implement** `scenario.rs` with the conservative Phase-3 scenario subset and `prior=unmeasured` telemetry flags. **Step 4: Observe the passes.** **Step 5: Commit.**

**Delivered 2026-09-23, with a correction to the clause count.**

**§2.3 has EIGHT clauses, not nine.** The test above and acceptance criterion 3
both say *"all 9 from §2.3"*; the blueprint lists eight, joined by seven
`AND`s. The ninth clause is real but belongs to **§2.1's robust gate** —
`Pr(Π(a,s) > 0) ≥ p_min` — and is implemented and attributed there. `Clause`
carries a `source()` and a test asserts exactly eight clauses come from §2.3,
so the miscount cannot quietly return.

**INV-20 is structural before it is tested.** *"A USD mark may never admit a
trade"* is usually guarded by perturbing the mark and comparing admitted sets.
That test exists, over both the stated ±50% and arbitrary marks from 1e-9 to
1e9. But it is the weaker half: **`EligibilityContext` has no USD field**, so
there is nothing for a clause to read. USD lives in `apex_types::pnl::UsdBounds`
on the reporting path and the gate cannot name it. The property test confirms
no USD leaks in by another route; the struct is what makes it impossible.

**Why the product form was wrong, measured.** `EV = P_land·P_state·P_exec·P_net
− C_failure` asserts independence, and these are not independent: the same
competitor that takes the pool is why the state changed, why execution reverts,
and why the net came in under forecast. On the conservative fixture,
`J = 505,500` against a naive `418,750` — the product **understates by 17% of
the expected value**. The direction is the informative part: four probabilities
below one compound, so the product is pessimistic in a benign case and
*optimistic* under correlated failure, where the factors are mostly redescribing
one event. That asymmetry is why a product cannot be repaired by tuning its
inputs.

**A partial scenario set is refused, never normalised.** A set summing to 0.9
is missing a tenth of the outcome space, and the missing tenth is exactly where
the unmodelled disasters live. Normalising would redistribute that mass across
the scenarios somebody did think of.

**The irrecoverable cost sits outside the sum**, because it is not conditional
on a scenario — the L1 data fee of an included-and-reverted transaction is paid
in every world. Folding it into each scenario's profit would make it look like
something the distribution could avoid.

**One rejection, not five.** `evaluate` returns the *first* failing clause, so
the §27 histogram counts what stopped a candidate rather than everything wrong
with it. Because the clauses are evaluated in a fixed order, breaking exactly
one and observing that clause reported is also what proves each is reachable
rather than shadowed by an earlier one.

**Tests:** as above, plus property tests on cost monotonicity and a fork test reproducing three recorded Base receipts' total cost within 1%.
**Benchmarks:** `T_size` p99 ≤ 20 ms; cost model evaluation ≤ 200 µs.
**Acceptance criteria:**
1. Continuous-to-candidate conversion is a compile error.
2. L1 data fee reproduces recorded Base receipts within 1% across ≥ 50 receipts.
3. All eligibility clauses independently gate — **eight from §2.3** plus §2.1's probability-of-profit gate, which the original "nine from §2.3" miscounted as one of them.
4. USD perturbation ±50% leaves the admitted set unchanged over a 24 h replay.
5. Flash selection matches a hand-computed optimum on a 20-case table.

**Failure criteria:** any cost component defaultable; any USD-driven admission change; sizing returning a non-integer or an unverified size.
**Exit gate:** **G-ECON-1**.

---

## PHASE 4 — Simulation hierarchy

**Objective:** Tiers 0–2 behind one trait, with Base `eth_simulateV1` as the capture-critical backend and the existing REVM + quorum assets preserved.

**Dependencies:** Phases 0–3.

**Files:**
- CREATE `crates/apex-sim/{Cargo.toml,src/lib.rs,src/tier0.rs,src/tier1.rs,src/tier2.rs,src/backends/{mod.rs,revm.rs,eth_call.rs,base_simulate_v1.rs},src/fidelity.rs}`
- MOVE `src/sim_revm.rs` → `crates/apex-sim/src/backends/revm.rs`; `src/sim_quorum.rs` → `crates/apex-sim/src/quorum.rs`
- MODIFY `crates/apex-types/src/sim.rs`

### Task 4.1 — `Simulator` trait and Tier 0

- [x] **Step 1: Write the failing test** — Tier 0 rejects a candidate whose gross profit is below `TotalExecutionCost::conservative_total`, in ≤ 50 µs, without any RPC call (assert the mock provider recorded zero requests).
- [x] **Step 2: Run and observe the failure.** **Step 3: Implement.** **Step 4: Observe the pass.** **Step 5: Commit.**

**Delivered 2026-09-23.**

**"Assert the mock provider recorded zero requests" is the weaker half, and
the test says so.** The mock exists and records zero — because **there is no
way to give it to `screen`**, which is a synchronous `fn` taking a
`&Tier0Input` and nothing else. A mock that *could* have been called and
happened not to be would be a weaker result than a signature with nothing to
call. `scripts/ci/tier0_is_pure.sh` keeps it true as the module changes: it
fails the build if `tier0.rs` names a provider type, becomes `async`, or
awaits. Mutation-tested.

**The 50 µs budget is measured amortised, not once.** A single timing on a
loaded runner measures the scheduler. Ten thousand screenings over a rotating
set of candidates, after a warm-up, with `black_box` so the work is not
optimised away. The real figure is tens of nanoseconds — this is arithmetic on
a struct — so the assertion has three orders of magnitude of headroom and
exists to catch a Tier 0 that *starts doing something*, not to benchmark one
that does not.

**The ladder is one-directional, and that is the whole safety argument.** A
lower tier may only reject, because nothing runs after it to catch what it
admits. So Tier 0 screens against the **conservative** total — p99 gas, full
failure cost — and a test pins that: the same candidate priced at p50 would
have cleared. Breaking even exactly is a rejection, because a trade that nets
zero has still consumed a simulation slot, a signer, and a block's worth of
attention.

### Task 4.2 — Base `eth_simulateV1` backend (§24.6)

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-sim/tests/base_simulate_v1.rs
#[test]
fn simulate_v1_is_preferred_over_eth_call_on_base() {
    let sim = Simulator::for_chain(ChainId(8453), &cfg);
    assert_eq!(sim.capture_critical_backend(), BackendKind::EthSimulateV1);
}

#[test]
fn simulate_v1_sends_explicit_block_and_state_context() {
    let (_, req) = record_request(|s| s.tier2(&committed));
    assert!(req["params"][0]["blockStateCalls"].is_array());
    assert_eq!(req["params"][0]["validation"], serde_json::json!(true));
    // Base documents that eth_call against `pending` may return cached block context.
    assert_ne!(req["method"], "eth_call");
}
```

- [x] **Step 2: Run and observe the failures.** **Step 3: Implement** the backend; `eth_call` becomes the fallback and the quorum verifier. **Step 4: Observe the passes.** **Step 5: Commit.**

**Delivered 2026-09-23 — the request builders, not the transport.**

**Request construction is separated from sending, and that is what made this
task possible at all here.** The capture-critical decisions live in the request
*shape*: `validation: true`, an explicit block rather than `pending`, state
overrides pinning what was simulated against. A backend that builds and sends
in one `async fn` can only be tested by mocking the transport, which tests the
mock. A builder returning `serde_json::Value` can be asserted on exactly — with
no node, which this environment does not have.

**Two properties worth naming beyond the specified ones:**

* **Neither backend may address a tag**, checked on both paths rather than only
  the primary. A fallback that silently used `pending` would make the quorum's
  *agreement* meaningless precisely when the primary was right.
* **An unchecked chain falls back rather than assuming.** `eth_simulateV1` is
  not universally available; claiming it for a chain nobody verified produces a
  backend that fails at runtime on the one path that must not fail.

**`validation: true` is the difference between two questions.** With it off the
node reports what happens when the calls execute. With it on it also applies
nonce, balance, intrinsic-gas and fee-cap checks — the ones that decide whether
the transaction would be *accepted*. Simulating without it answers "would this
succeed if it ran" when the question is "would this run".

### Task 4.3 — Tier 2 result completeness

- [ ] **Step 1: Write the failing test** — `SimulationResult` from every backend carries success, revert class + data, gas used, per-token balance deltas, loan-repaid flag, profit-invariant flag, token residues, state-after fingerprint, the state it simulated at, and a stable `result_hash`.
- [ ] **Step 2–5:** as usual.

### Task 4.4 — Simulation fidelity scorer (§34, INV-44)

- [ ] **Step 1: Write the failing test** — feeding a stream of (predicted, realized) pairs whose gas error exceeds the band produces, in order, `ReduceSize`, then `RaiseTier`, then `Disable` for that strategy×venue.
- [ ] **Step 2–5:** as usual.

### Task 4.5 — Red/blue for simulation

- [ ] **Step 1:** Run both backends on every candidate for 7 days; write `docs/apex/reports/sim-diff-<date>.csv`.
- [ ] **Step 2:** Assert ≥ 99.9% agreement with every disagreement explained.
- [ ] **Step 3:** Flip `sim.authority = red`; keep blue as the quorum verifier.

**Tests:** trait conformance, backend request-shape tests, fidelity table test, quorum contradiction test (existing, migrated), fork tests against recorded Base state.
**Benchmarks:** Tier 0 ≤ 50 µs; Tier 1 ≤ 5 ms; Tier 2 (`eth_simulateV1`) p99 ≤ 60 ms.
**Acceptance criteria:**
1. `eth_simulateV1` is the capture-critical Base backend with explicit state/block controls and validation enabled.
2. 7-day red/blue agreement ≥ 99.9%, all disagreements explained.
3. Fidelity scorer demonstrably drives the three responses in a controlled test.
4. Quorum still vetoes a contradicting endpoint, block-pinned (existing behaviour preserved).

**Failure criteria:** Tier 2 exceeding its budget at p99; any unexplained backend disagreement; fidelity breach not triggering a response.
**Exit gate:** **G-SIM-1**.

---

## PHASE 5 — Solidity settlement correctness

**Objective:** A settlement contract with **no arbitrary-call surface**, a multi-asset profit invariant, and on-chain commitment verification. This is the highest-severity fix in the plan.

**Dependencies:** Phases 0–4 (for the encoder and simulator that exercise it).

**Files:**
- CREATE `contracts/core/{ExecutionAuth.sol,AdapterRegistry.sol,RouteValidator.sol,ProfitInvariant.sol,FlashSourceRouter.sol,Types.sol}`
- CREATE `contracts/adapters/{AaveAdapter.sol,UniswapV3Adapter.sol,AerodromeAdapter.sol,SlipstreamAdapter.sol,PancakeAdapter.sol,BalancerAdapter.sol}`
- CREATE `contracts/chains/BaseArbExecutor.sol`
- REMOVE `contracts/executor/MultiVenueArbImplementation.sol` `_execGeneric`, `_execModule`, `Op.JIT_LP_ADD`, `Op.JIT_LP_REMOVE`, `Op.BRIDGE`, `jitPositions`
- REMOVE `contracts/libraries/BridgeLib.sol`, `contracts/executor/steps/{JitExecutor,BridgeExecutor}.sol`, `contracts/mocks/MockBridge.sol`, `contracts/artifacts/**`
- MODIFY `test/MultiVenueArbExecutor.t.sol` (drop JIT/bridge tests; keep and extend the rest)
- CREATE `test/{AdapterRegistry.t.sol,RouteValidator.t.sol,ProfitInvariant.t.sol,NoArbitraryCall.t.sol}`, `test/invariant/{ProfitInvariant.invariant.sol,AdapterRegistry.invariant.sol}`
- CREATE `crates/apex-exec/{Cargo.toml,src/lib.rs,src/commitment.rs,src/encode/{mod.rs,steps.rs},src/abi.rs}`; MOVE `src/plan.rs` → `crates/apex-exec/src/encode/legacy_plan.rs` (blue path), `src/abi.rs` → `crates/apex-exec/src/abi.rs`
- CREATE `scripts/ci/check_no_generic_call.sh`

### Task 5.1 — Prove the hole exists, then close it (INV-07, INV-33)

- [ ] **Step 1: Write the failing test** — `test/NoArbitraryCall.t.sol`:

```solidity
function testGenericStepCanCallAnyTarget() external {
    // Demonstrates B-1 against the CURRENT contract. Must PASS before the fix
    // (proving the hole) and then be inverted to testNoArbitraryCallSurfaceExists.
    Attacker atk = new Attacker();
    bytes memory payload = abi.encode(address(atk), abi.encodeCall(Attacker.pwn, ()), uint256(0), address(0), uint256(0));
    vm.prank(executor);
    impl.startV2(planWithGenericStep(payload));
    assertTrue(atk.pwned(), "arbitrary call surface is reachable");
}
```

- [ ] **Step 2: Run it and observe it PASS** against the current contract — the vulnerability is now demonstrated and recorded in `docs/apex/reports/security-B1.md`.
- [ ] **Step 3: Implement** `AdapterRegistry` + typed `Step { adapterId, poolKey, payload }`; delete `_execGeneric` and `_execModule`; invert the test to `testNoArbitraryCallSurfaceExists` asserting the attacker call reverts with `UnknownAdapter`.
- [ ] **Step 4: Run and observe** the inverted test pass and `scripts/ci/check_no_generic_call.sh` pass (source grep + runtime bytecode scan).
- [ ] **Step 5: Commit.**

### Task 5.2 — Multi-asset profit invariant (INV-27, INV-29)

- [ ] **Step 1: Write the failing tests** — `testMultiAssetInvariantHolds` (fuzz over asset count 1–4 and amounts), `testPartialRepaymentReverts`, `testUnaccountedResidueReverts`, `testDeclaredResiduePathAccountsExactly`.
- [ ] **Step 2: Run and observe the failures** — the current contract reverts with `InvalidLoanCount` on any multi-asset plan.
- [ ] **Step 3: Implement** `ProfitInvariant.assertMultiAsset(Debt[] memory)` and remove the single-loan restriction.
- [ ] **Step 4: Observe the passes.** **Step 5: Commit.**

### Task 5.3 — Commitment verification (INV-06, §25)

- [ ] **Step 1: Write the failing tests** — `testCommitmentMismatchReverts` (mutate one field of the plan after computing the commitment); Rust side `exec::commitment_is_stable_under_reencode` (property: encode → decode → re-encode yields the same hash).
- [ ] **Step 2–5:** as usual.

### Task 5.4 — Route validator and remaining invariants

- [ ] **Step 1: Write the failing tests** for INV-24, INV-25 (existing, migrated), INV-26, INV-28, INV-30, INV-31, INV-32.
- [ ] **Step 2–5:** as usual.

### Task 5.5 — Remove excluded features (C-03, C-04)

- [ ] **Step 1:** Delete JIT and bridge ops, libraries, step contracts, mocks and their tests.
- [ ] **Step 2:** Run `forge test` and `make check-contract-size`; confirm the runtime size gate passes with margin.
- [ ] **Step 3:** Resolve `BatchRouter.sol` from `UNKNOWN`: trace callers; if none, remove.
- [ ] **Step 4:** Commit.

### Task 5.7 — Retire the deploy tests' process-global env dependence (B-13)

Quarantined at Phase 0; due here because this phase rewrites `script/Deploy.s.sol` for the new contract set, so the tests are being touched anyway.

- [ ] **Step 1: Write the failing test** — `test/DeployHermetic.t.sol` asserting that a deploy's resolved configuration is a pure function of explicit inputs, with no read of `vm.envOr`/`vm.envAddress` on the path:

```solidity
function testResolvedConfigIsIndependentOfAmbientEnv() external {
    // Same explicit config must produce the same resolution regardless of what
    // the process environment says. Today this fails: .env supplies CHAIN=base,
    // ETH_UNIV3_ROUTER and BASE_EXECUTOR_OWNER, and prefixed keys outrank the
    // unprefixed ones a test can set.
    vm.setEnv("CHAIN", "optimism");
    DeployConfig memory a = deployScript.resolveConfig(explicitConfig());
    vm.setEnv("CHAIN", "eth");
    DeployConfig memory b = deployScript.resolveConfig(explicitConfig());
    assertEq(keccak256(abi.encode(a)), keccak256(abi.encode(b)));
}

function testDeployTestsDoNotWriteProcessEnv() external {
    // vm.setEnv is global and persists for the whole run; no test may rely on it.
    assertEq(vm.ffi(grepForSetEnvUnder("test/Deploy")).length, 0);
}
```

- [ ] **Step 2: Run and observe the failures** — both fail today; §3.4 B-13 and `docs/apex/BASELINE.md` carry the measured evidence and the list of approaches already tried and reverted, which must not be retried.
- [ ] **Step 3: Implement** a `resolveConfig(DeployConfig explicit)` seam on `Deploy` so the environment is read in exactly one place at the top and everything below is pure. Tests pass a struct; production passes the env-derived one.
- [ ] **Step 4: Run and observe** the full `forge test` deterministic and green over 10 consecutive runs — the acceptance bar Phase 0 could not meet.
- [ ] **Step 5: Commit**, and lift the B-13 quarantine from the Phase 0 gate wording.

### Task 5.6 — Invariant fuzzing and external review

- [ ] **Step 1:** Write `test/invariant/` suites for `ProfitInvariant` and `AdapterRegistry`.
- [ ] **Step 2:** Run `forge test --match-path 'test/invariant/*' --fuzz-runs 100000`.
- [ ] **Step 3:** Commission and complete an external review of `contracts/core/` and `contracts/chains/BaseArbExecutor.sol`. **No mainnet deployment before it clears.**

**Tests:** ~60 migrated + ~20 new `forge` tests; 2 invariant suites; Rust commitment property tests.
**Benchmarks:** executor runtime size under the EIP-170 limit with ≥ 15% margin; gas per 3-hop settlement measured and recorded.
**Acceptance criteria:**
1. `testNoArbitraryCallSurfaceExists` passes and `check_no_generic_call.sh` is green in CI.
2. Multi-asset invariant holds under 100k fuzz runs.
3. Commitment mismatch reverts on-chain and refuses to sign off-chain.
4. Every §8.4 invariant (INV-24 … INV-33) has a passing named test.
5. External security review complete with no unresolved high or critical findings.
6. JIT and bridge code paths are absent from source and bytecode.

**Failure criteria:** any reachable arbitrary call; any invariant without a test; any unresolved high/critical review finding.
**Exit gate:** **G-SOL-1** and **G-SEC-1**.

---

## PHASE 6 — Capture Assurance Controller and signer pool (SHADOW)

**Objective:** The full ticket lifecycle, resource reservation, multi-lane signing, last-mile revalidation and acknowledgement ladder — running end-to-end in shadow with a null dispatcher, **before** any live capital.

**Dependencies:** Phases 0–5.

**Files:**
- CREATE `crates/apex-capture/{Cargo.toml,src/lib.rs,src/ticket.rs,src/registry.rs,src/journal.rs,src/protocol.rs,src/reserve.rs,src/revalidate.rs,src/scheduler.rs,src/signer/{mod.rs,pool.rs,lane.rs},src/nonce.rs,src/dispatch/{mod.rs,router.rs,ack.rs},src/reconcile.rs,src/health.rs}`
- MOVE `src/health.rs` → `crates/apex-capture/src/health.rs`; extract `NonceManager` from `main.rs` → `crates/apex-capture/src/nonce.rs` (preserving its comments)
- CREATE `crates/apex-capture/tests/{ticket_lifecycle.rs,nonce_loom.rs,preemption.rs,crash_recovery.rs,revalidation.rs}`
- CREATE `crates/apex-risk/{Cargo.toml,src/lib.rs,src/policy.rs,src/breaker.rs,src/posture.rs,src/loss.rs,src/capital.rs}`; MOVE `src/risk_policy.rs`, `src/capital.rs`, and `main.rs`'s `CircuitBreaker` (with its tests)

### Task 6.1 — Ticket registry, journal, and INV-01

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-capture/tests/ticket_lifecycle.rs
#[test]
fn every_admitted_ticket_reaches_exactly_one_terminal_state() {
    proptest!(ProptestConfig::with_cases(100_000), |(script in lifecycle_script())| {
        let reg = TicketRegistry::in_memory();
        let id = reg.admit(fixture_ticket()).unwrap();
        script.apply(&reg, id);                       // random transitions, drops, panics, timeouts
        let outcome = reg.outcome(id);
        prop_assert!(outcome.is_some(), "INV-01: no ticket may lack a terminal outcome");
    });
}

#[test]
fn dropping_a_nonterminal_ticket_records_an_explicit_failure() {
    let reg = TicketRegistry::in_memory();
    let id = reg.admit(fixture_ticket()).unwrap();
    { let _guard = reg.checkout(id); }                // dropped without closing
    assert!(matches!(reg.outcome(id), Some(TicketOutcome::ExplicitFailure { .. })));
    assert_eq!(reg.metrics().ticket_drop_count, 0, "INV-02");
}
```

- [ ] **Step 2: Run and observe the failures.** **Step 3: Implement** the registry with an RAII checkout guard whose `Drop` closes non-terminal tickets explicitly, plus the append-only journal with `fsync` at/after `AUTHORIZED`. **Step 4: Observe the passes.** **Step 5: Commit.**

### Task 6.2 — Crash recovery (INV-39)

- [ ] **Step 1: Write the failing test** — `crash_recovery.rs` spawns the runtime, admits tickets, `SIGKILL`s mid-flight, restarts, and asserts (a) dispatch stays disabled until reconciliation completes and (b) every journaled ticket ends terminal.
- [ ] **Step 2–5:** as usual.

### Task 6.3 — Signer pool and nonce lanes (INV-04, §27.5, §27.6)

- [ ] **Step 1: Write the failing tests** — the seven §18.2 requirement tests plus a `loom` model asserting no cross-lane nonce reuse under every interleaving.
- [ ] **Step 2: Run and observe the failures.** **Step 3: Implement** `SignerPool` with per-lane `NonceManager` (ADAPTed, comments preserved), health scoring, per-lane breaker, and the healthiest-free-lane assignment rule. **Step 4: Observe the passes.** **Step 5: Commit.**

### Task 6.4 — Last-mile revalidation (INV-35)

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-capture/tests/revalidation.rs
#[test]
fn sign_requires_a_revalidation_token() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/sign_without_revalidation.rs");
}

#[test]
fn each_revalidation_check_can_reject() {
    for check in LastMile::CHECKS {                    // all 11 from §24.6
        let ctx = passing_but_failing(check);
        assert!(matches!(LastMile::run(&ctx), Err(e) if e.check == check),
                "check {check} did not gate");
    }
}
```

- [ ] **Step 2–5:** as usual.

### Task 6.5 — Priority scheduling and preemption (INV-09)

- [ ] **Step 1: Write the failing test** — under synthetic overload (10× candidate rate, 1 signer lane), assert no `Authorized` ticket is preempted and that shedding happens in the §57.1.2 order.
- [ ] **Step 2–5:** as usual.

### Task 6.6 — Acknowledgement ladder and null dispatcher (INV-34)

- [ ] **Step 1: Write the failing tests** — all seven stages observable with independent timeouts; `transport_accepted` never sets `included`.
- [ ] **Step 2–5:** as usual, with a `NullDispatcher` that records what *would* have been sent.

### Task 6.7 — Risk posture ladder and loss classification (INV-42, INV-43)

- [ ] **Step 1: Write the failing table tests** over every §28 trigger → posture, and every loss → class.
- [ ] **Step 2–5:** as usual, extracting the existing `CircuitBreaker` with its tests intact.

**Tests:** 5 dedicated test files, 2 `loom` models, 2 trybuild compile-fail fixtures, chaos tests from §29.4.
**Benchmarks:** `T_sign` p99 ≤ 3 ms including revalidation; ticket admit→reserve ≤ 500 µs; journal `fsync` ≤ 1 ms.
**Acceptance criteria:**
1. 100,000-case property run: **zero** tickets without a terminal outcome.
2. `loom` proves no cross-lane nonce reuse.
3. `SIGKILL` recovery blocks dispatch until every in-flight ticket is reconciled, over 20 consecutive kill cycles.
4. Under 10× overload, `Authorized` tickets are never preempted and `U_capture` stays ≥ 0.99.
5. All 11 last-mile checks independently gate; signing without a `Revalidated` token is a compile error.
6. 7-day shadow run with the null dispatcher: `ticket_drop_count == 0`, `unexplained_pre_dispatch_expiry == 0`, `nonce_reuse == 0`.

**Failure criteria:** any non-terminal ticket; any nonce reuse; any preempted authorized ticket; dispatch enabled before reconciliation.
**Exit gate:** **G-CAP-1**, **G-CAP-2**, **G-SIGN-1**, **G-RISK-1**.

---

## PHASE 7 — Base execution adapter

**Objective:** Flashblock eligibility scheduling, gas-limit minimization as capture, and real dispatch through the Capture Assurance Controller.

**Dependencies:** Phases 0–6.

**Files:**
- CREATE `crates/apex-chain/{Cargo.toml,src/lib.rs,src/adapter.rs,src/regime.rs,src/rpc/{mod.rs,failover.rs,ws.rs},src/base/{mod.rs,feed.rs,flashblock.rs,fee.rs,submit.rs,observe.rs,reconcile.rs}}`
- MOVE `src/rpc_failover.rs` → `crates/apex-chain/src/rpc/failover.rs`; `src/base_fast.rs` feed/dirty-set → `crates/apex-chain/src/base/feed.rs` + `crates/apex-state/src/fast/`; `src/chain.rs` → decomposed into `apex-config` (addresses) and `apex-chain` (behaviour)
- MOVE `src/util.rs` → split per §5.2

### Task 7.1 — `ChainExecutionAdapter` trait and the Base impl skeleton

- [ ] **Step 1: Write the failing test** — `BaseAdapter` implements all ten methods; a CI grep asserts no `match chain_name` / `chain == "base"` outside `apex-chain`.
- [ ] **Step 2–5:** as usual.

### Task 7.2 — Flashblock capacity model and eligibility (INV-37, INV-38)

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/apex-chain/tests/flashblock.rs
#[test]
fn eligibility_uses_the_measured_capacity_model_not_a_constant() {
    let q = MeasuredCapacityModel::from_observations(&recorded_flashblock_budgets());
    assert_eq!(earliest_eligible(1_000_000, &q), Some(0));
    assert_eq!(earliest_eligible(25_000_000, &q), Some(3));   // too big for early flashblocks
    // A hard-coded one-tenth rule would give a different answer; assert we do not use one.
    assert_ne!(earliest_eligible(25_000_000, &q), Some(0));
}

#[test]
fn no_retroactive_flashblock_entry() {
    proptest!(|(current in 0u32..10, gas in 21_000u64..30_000_000)| {
        let k = earliest_eligible_from(current, gas, &model());
        prop_assert!(k.map_or(true, |k| k >= current), "INV-38: ordering is locked");
    });
}

#[test]
fn no_safe_gas_limit_rejects_before_signing() {
    let ticket = ticket_needing_more_gas_than_any_eligible_window();
    let err = BaseAdapter::prepare(&ticket).unwrap_err();
    assert!(matches!(err, PrepareError::Terminal(TerminalFailure::…)));
    assert_eq!(signer_pool().signatures_issued(), 0, "must reject BEFORE signing");
}
```

- [ ] **Step 2: Run and observe the failures.** **Step 3: Implement** `flashblock.rs` with `MeasuredCapacityModel` learned from observed budgets. **Step 4: Observe the passes.** **Step 5: Commit.**

### Task 7.3 — Base submission lane and acknowledgement

- [ ] **Step 1: Write the failing tests** — dispatch goes to a Flashblocks-aware RPC endpoint (never the raw infrastructure stream); `base_transactionStatus = Known` maps to `node_known`, **not** `included`; redundant transport sends identical signed bytes (INV-10); a stale opportunity cancels the fallback before dispatch.
- [ ] **Step 2–5:** as usual.

### Task 7.3a — Record what each submission lane actually guarantees (B-14)

Base's lanes use **provider-level** MEV protection rather than builder relays, which is the right architecture for a sequencer chain. The gap is that `SubmissionPolicy` is currently an assertion nobody has verified, and §14.1 prices lanes by that policy — so a silently flipped provider toggle would degrade capture with no signal.

- [ ] **Step 1: Write the failing test:**

```rust
// crates/apex-chain/tests/lane_evidence.rs
#[test]
fn a_protected_policy_requires_recorded_evidence() {
    for lane in configured_lanes(ChainId(8453)) {
        if lane.policy() != SubmissionPolicy::Public {
            let ev = lane.privacy_evidence()
                .unwrap_or_else(|| panic!("lane {} claims {:?} with no evidence", lane.lane_id(), lane.policy()));
            assert!(ev.checked_at.elapsed() < ev.ttl, "evidence for {} is stale", lane.lane_id());
        }
    }
}

#[test]
fn evidence_distinguishes_provider_attestation_from_measurement() {
    // A vendor doc is weaker evidence than an observed mempool-absence probe.
    // Both are admissible; the EV model weights them differently.
    assert!(PrivacyEvidence::ProviderAttested { .. }.confidence()
          < PrivacyEvidence::Measured { .. }.confidence());
}
```

- [ ] **Step 2: Run and observe the failures** — no lane carries evidence today.
- [ ] **Step 3: Implement** `PrivacyEvidence { ProviderAttested { source, checked_at, ttl } | Measured { probe, checked_at, ttl } }`. Seed Base's BlockPI lane as `ProviderAttested` (MEV protection is a per-endpoint toggle, on by default) and schedule a periodic re-check, because the guarantee is a **dashboard setting** that can change without a deploy.
- [ ] **Step 4: Run and observe the passes.** Upgrade at least one lane to `Measured` once the Phase 9 competitor model can observe pre-inclusion visibility.
- [ ] **Step 5: Commit** (named paths only).

> **`public_mempool_jitter_bps: 35`.** §24.2 removes `apply_public_mempool_jitter` as an unjustified leakage-shaping heuristic. With protection on the lane it is most likely vestigial, so removal is low-risk — but confirm it is not load-bearing for a fallback path before deleting it.

### Task 7.4 — Regime discovery

- [ ] **Step 1: Write the failing test** — a chain whose regime cannot be discovered is not admitted to live trading.
- [ ] **Step 2–5:** as usual.

### Task 7.5 — Outcome observation and reconciliation

- [ ] **Step 1: Write the failing tests** — `observe_outcome` distinguishes preconfirmed / included / finalized; `reconcile_final_state` produces a `PnlAttribution` from the receipt and balance deltas that matches a recorded real trade within 1 wei.
- [ ] **Step 2–5:** as usual.

**Tests:** flashblock property tests, submission lane tests, regime discovery, fork-based reconciliation against recorded trades.
**Benchmarks:** `T_submit` p99 ≤ 20 ms to transport ack; end-to-end `T_signal→submit` p99 ≤ 135 ms.
**Acceptance criteria:**
1. `earliest_eligible_flashblock` predictions validated empirically against ≥ 500 observed transactions with ≥ 90% accuracy (Blueprint gate 21).
2. Gas-limit minimization measurably lowers `actual_flashblock_index` versus a fixed-limit control.
3. No retroactive-entry logic exists (property test).
4. Ack ladder distinguishes all seven stages on real Base traffic.
5. `T_signal→submit` p99 < 200 ms (one Flashblock).

**Failure criteria:** eligibility accuracy < 90%; any signing before eligibility rejection; latency budget breach.
**Exit gate:** **G-BASE-1**, **G-SUB-1**.

---

## PHASE 8 — Observability, missed-opportunity accounting, coverage auditing → FIRST PROFITABLE TRADE

**Objective:** Close the measurement loop, prove capture assurance on live traffic in shadow, then take the first live trade under canary limits.

**Dependencies:** Phases 0–7.

**Files:**
- CREATE `crates/apex-obs/{Cargo.toml,src/lib.rs,src/metrics.rs,src/miss.rs,src/coverage.rs,src/pnl.rs,src/attribution.rs}`
- MOVE `src/metrics.rs` → `crates/apex-obs/src/metrics.rs`; `src/accounting.rs` → `crates/apex-obs/src/accounting.rs`
- CREATE `crates/apex-runtime/{Cargo.toml,src/main.rs,src/plane.rs,src/bus.rs,src/workers.rs,src/supervise.rs,src/shutdown.rs}`
- CREATE `scripts/canary/{run_canary.sh,promote.sh}`
- MODIFY `ops/observability/` dashboards

### Task 8.1 — Missed-opportunity ledger (INV-40)
### Task 8.2 — Coverage auditor (INV-41)
### Task 8.3 — P&L attribution by optimization layer
### Task 8.4 — Control plane assembly (`apex-runtime`)
### Task 8.5 — Full-system shadow run
### Task 8.6 — Canary promotion

Each follows the same five-step TDD cycle. Task 8.1's failing test is the exhaustive `obs::every_rejection_path_records_a_miss`; Task 8.2's is `obs::auditor_detects_injected_miss`; Task 8.3's asserts that a trade touching two optimization layers attributes to both; Task 8.4's is an integration test driving a recorded event stream end-to-end and asserting a ticket reaches `Reconciled`; Task 8.5 is operational; Task 8.6 gates on §38.

**Tests:** exhaustive rejection-path test, injected-miss test, attribution test, full-system integration on recorded traffic, all §29.4 chaos scenarios.
**Benchmarks:** the complete §29.5 latency budget table, measured and recorded.
**Acceptance criteria — the first-profitable-trade gate:**
1. 14-day continuous shadow on Base with `SYSTEM_CAPTURE_ASSURANCE ≥ 0.99` and every hard-zero counter at 0.
2. `hot_path_recall ≥ 0.95` against the coverage auditor, or every gap explained and accepted.
3. Simulation fidelity `F_sim` inside band for every admitted strategy×venue.
4. Every §29.4 chaos scenario passes.
5. All 46 invariants have passing named tests; `invariant_coverage.sh` is green with no allowances.
6. Canary: 100 live trades at minimum size with positive aggregate realized net P&L after every material cost, zero unexplained losses, and revert rate below the class threshold.
7. Measured opportunity surface supports the planned scale (§38.1) — recorded in `docs/apex/reports/opportunity-surface-<date>.md`.

**Failure criteria:** capture assurance < 0.99; any hard-zero counter non-zero; negative canary P&L; any unexplained loss cluster.
**Exit gate:** **G-OBS-1**, **G-PROD-1** — **FIRST PROFITABLE TRADE.**

---

## PHASE 9 — Adversarial simulation and the competitor model

**Objective:** Tier 3 perturbation and an empirically calibrated `CompetitorModel`, replacing Phase 3's conservative fixed priors.

**Dependencies:** Phase 8 (needs realized outcomes to calibrate against).

**Files:** CREATE `crates/apex-sim/src/adversarial.rs`, `crates/apex-sim/src/competitor/{mod.rs,censoring.rs,capture_curve.rs}`, `crates/apex-sim/tests/adversarial.rs`; MODIFY `crates/apex-econ/src/ev/scenario.rs` to consume measured distributions.

**Tasks (TDD):**
- [ ] **9.1** Perturbation harness with the ten §36 perturbations. Failing test: a candidate profitable at base state becomes unprofitable under `+ one competing swap` and its `robustness_margin` is scaled accordingly, not binary-rejected.
- [ ] **9.2** Censored observations (§21.3). Failing test: `sim::censored_observation_does_not_impute_size` — a lost submission with no observed competitor produces `Observation::Censored` and the size histogram is unchanged. **`CompetitorSize` is `Option` with no default.**
- [ ] **9.3** Capture curve `P_capture = F(stateAge, latency, fee, gasLimit, strategy, venue, chain, competition)` fitted from realized outcomes, with the §21.2 latency buckets.
- [ ] **9.4** Replace fixed priors in `scenario.rs`; remove the `prior=unmeasured` flag for calibrated classes.

**Tests:** perturbation table, censoring property test, curve back-test against held-out outcomes.
**Benchmarks:** Tier 3 p99 ≤ 40 ms, and it must run **concurrently** with reservation so it never extends `T_signal→submit`.
**Acceptance criteria:** competitor model directionally calibrated (Blueprint gate 14) — predicted vs realized capture rate within 20% over a 7-day hold-out; zero imputed competitor sizes; fragile candidates measurably require higher margin.
**Failure criteria:** Tier 3 on the critical path; any fabricated competitor observation.
**Exit gate:** **G-ADV-1**.

---

## PHASE 10 — Parallel-pool allocation

**Objective:** Multi-pool splitting with tick-boundary segmentation and discrete refinement (§15).

**Dependencies:** Phases 3, 8.

**Files:** CREATE `crates/apex-econ/src/allocation/{mod.rs,kkt.rs,piecewise.rs,cluster.rs}`; ACTIVATE `crates/apex-econ/src/allocation/convex.rs` (moved dormant in Phase 3).

**Tasks (TDD):**
- [ ] **10.1** KKT warm start over genuinely concave segments (`f_i'(x_i) = λ`). Failing test: WETH/USDC across 100/500/3000 bps fee tiers splits rather than routing everything through one tier, and the split beats the best single tier by a measurable margin.
- [ ] **10.2** Tick-boundary segmentation: `continuous → tick-boundary discovery → piecewise candidates → discrete refinement → exact simulation`.
- [ ] **10.3** Shared-pool clusters (INV-22). Failing test: `econ::shared_pool_forces_joint_transition` — two routes over one pool must not be independently optimized and summed.
- [ ] **10.4** Gas-aware rejection (§16.3).

**Tests:** as above plus property tests that a split never underperforms the best single route after costs.
**Benchmarks:** allocation p99 ≤ 25 ms for ≤ 20 edges.
**Acceptance criteria (Blueprint gate 15):** **parallel splitting shows positive incremental realized P&L before activation**, measured against the frozen single-route baseline per §48 with normalized opportunity exposure.
**Failure criteria:** negative or unnormalized benchmark; any independently-summed shared pool.
**Exit gate:** **G-ALLOC-1**.

---

## PHASE 11 — Uniswap V4 programmable execution

**Objective:** Replace the 73-LOC fixed-price stub with a real §10 engine including hooks, dynamic fees and `PoolManager` flash accounting.

**Dependencies:** Phases 2, 5, 8.

**Files:** CREATE `crates/apex-venues/src/univ4/{mod.rs,pool_key.rs,hooks.rs,flash_accounting.rs,engine.rs,fingerprint.rs}`; CREATE `contracts/adapters/UniswapV4Adapter.sol`; REMOVE `src/quote_univ4.rs`; MODIFY `data/base/uniswap_v4/pools.json` to carry hook addresses and fingerprints.

**Tasks (TDD):**
- [ ] **11.1** `PoolKey` + hook permission decoding. Failing test: a pool with a `beforeSwap` hook is not priced by the static path.
- [ ] **11.2** Hook execution model (`beforeSwap → fee modification → core swap → afterSwap → custom accounting → settlement`).
- [ ] **11.3** `PoolManager` lock/unlock and delta settlement. Failing test: `univ4::intermediate_delta_is_not_a_settled_balance` — an intermediate delta must never be read as settled.
- [ ] **11.4** Hook fingerprinting and the shadow-only rule (§10.4). Failing test: an unmodelled hook forces `Exactness::Approximate`, which INV-17 then blocks from live dispatch.
- [ ] **11.5** Three-way differential against the V4 quoter and a REVM fork.

**Tests:** differential suite, hook fuzz (legal return values only), settlement property tests.
**Acceptance criteria (Blueprint gate 19):** V4 hook-dependent routes have exact or **explicitly bounded** execution models; every unmodelled hook is shadow-only; differential 0 bps on modelled pools.
**Failure criteria:** any unmodelled hook reaching live dispatch; intermediate deltas treated as settled.
**Exit gate:** **G-V4-1**.

---

## PHASE 12 — Joint allocation, shared-pool coupling, bounded packing

**Objective:** §16 certified joint allocation and §17 bounded cross-cycle packing.

**Dependencies:** Phase 10.

**Files:** CREATE `crates/apex-econ/src/allocation/{certificate.rs,improving_path.rs}`, `crates/apex-econ/src/packing/{mod.rs,conflict.rs,bounded.rs}`.

**Tasks (TDD):**
- [ ] **12.1** Validity predicate (§16.1) and `CertificateStatus` (INV-21). Failing tests: `econ::shared_pool_coupling_invalidates_certificate`, `econ::improving_move_denies_proven`.
- [ ] **12.2** Improving-path search.
- [ ] **12.3** Conflict graph with all nine §17.2 conflict classes **including signer/nonce and Flashblock-capacity conflicts**.
- [ ] **12.4** Packing rule (§17.3): risk-adjusted packed EV must exceed the best alternative **plus margin**, accounting for gas limit, delayed Base eligibility, L1 data fee, revert surface, state dependencies and flash requirements.
- [ ] **12.5** Search bound (§17.4): hard cap on enumerated subsets; a CI test asserts the cap.

**Acceptance criteria (Blueprint gates 16, 17):** joint allocation and packing each show **positive incremental realized P&L after inclusion effects** before activation.
**Failure criteria:** any unbounded enumeration; a heuristic labelled `Proven`.
**Exit gate:** **G-ALLOC-2**.

---

## PHASE 13 — Event-driven backruns

**Objective:** §12.5 / §18.3 / §22.4 — exact target simulation and post-event route generation with Base-specific timing.

**Dependencies:** Phases 4, 7, 9.

**Files:** CREATE `crates/apex-search/src/backrun/{mod.rs,target.rs,post_state.rs,timing.rs}`; REMOVE `src/backrun_state.rs`.

**Tasks (TDD):**
- [ ] **13.1** Target classification from decoded pending transactions (ADAPT `mempool.rs`).
- [ ] **13.2** **Exact** target simulation (Tier 2) producing a state branch — replacing the single-tick advance. Failing test: a multi-tick-crossing victim swap produces a post-state matching a REVM fork exactly.
- [ ] **13.3** Reprice the affected closure on the target branch; search successors.
- [ ] **13.4** Base timing (§22.4): target Flashblock index → residual capacity → next eligible slot → next-block transition → competition → decay.
- [ ] **13.5** The target transaction is never assumed final until the execution regime says it is. Failing test: a target that does not land invalidates its branch and closes dependent tickets with `TargetBackrunStateChanged`.

**Acceptance criteria:** backrun capture rate measurable and positive; zero tickets dispatched against a branch whose target did not land.
**Exit gate:** **G-BACKRUN-1**.

---

## PHASE 14 — Ethereum execution

**Objective:** Private-builder submission with an empirical bid-to-inclusion model.

**Dependencies:** Phase 8 (Base profitable), Phase 9 (competitor model).

**Files:** CREATE `crates/apex-chain/src/ethereum/{mod.rs,fee.rs,submit.rs,builders.rs,observe.rs}`, `contracts/chains/EthereumArbExecutor.sol`; ADAPT `main.rs`'s relay bundle plumbing into `submit.rs`.

**Tasks (TDD):** builder lane abstraction; empirical bid curve replacing any percentage-of-profit rule; multiplexing only on proven incremental inclusion probability (INV-10); bundle validity windows, targeting, replacement/cancellation; ack semantics (builder acknowledgement ≠ proposer selection).

**Acceptance criteria (Blueprint gate 20 + §40.11):** chain-specific fee model validated; Ethereum private submission lanes **measured independently**; positive realized net P&L on Ethereum before promotion beyond canary.
**Exit gate:** **G-ETH-1**.

---

## PHASE 15 — BSC, Arbitrum, OP and the adaptive chain allocator

**Objective:** §3 dynamic chain portfolio with runtime regime discovery.

**Dependencies:** Phases 8, 14.

**Files:** CREATE `crates/apex-chain/src/{bsc,arbitrum,optimism}/`, `crates/apex-econ/src/allocator/{mod.rs,chain_score.rs}`; MOVE `fees.rs`'s Arbitrum per-byte logic → `arbitrum/fee.rs`.

**Tasks (TDD):** per-chain regime discovery (INV: a chain whose regime cannot be discovered is not admitted); per-chain fee models; `ChainScore` with minimum observation and reliability requirements; the constrained online allocator with hard live-risk floors and shadow-preferred exploration; removal of every remaining `match chain_name` outside `apex-chain` (resolves C-12).

**Acceptance criteria (Blueprint gates 20, 24, 25):** each chain's fee model validated against recorded receipts; `ChainScore` demonstrably reallocates on measured evidence in a controlled replay; **no chain promoted on DEX volume alone**.
**Exit gate:** **G-CHAIN-1**.

---

## PHASE 16 — Liquidations and correlated dislocations

**Objective:** Strategies D and E with isolated resource budgets.

**Dependencies:** Phase 8.

**Files:** CREATE `crates/apex-strategy/src/{liquidation/,correlated/}`; MOVE `src/liquidations.rs` → `crates/apex-strategy/src/liquidation/monitor.rs`.

**Tasks (TDD):** full §18.4 protocol model (health factor, oracle state, eligibility, close factor, liquidation bonus, caps, isolation/collateral constraints, available debt/collateral liquidity, unwind path, flash cost, gas, competition); isolated resource budget; §18.5 correlated monitoring with **external venues as signals only** and explicit rejection of dislocations caused by stale feeds, oracle lag, low liquidity, untradeable token behaviour or state-version mismatch.

**Before starting this phase (R-21):** re-measure Aave's Chainlink SVR coverage on the target market. `ARCHITECTURE_PIVOT_HANDOFF.md §8` records that SVR recaptures **~73% of liquidation MEV to the protocol** and was extending to Base. If that holds, Strategy D is structurally unavailable here and the phase should be **dropped, not built** — §52's next-dollar rule makes building into a 73% protocol haircut the wrong call. Strategy E is unaffected and can proceed alone.

**Acceptance criteria (Blueprint gate 18):** liquidation has **positive standalone EV after competition**; no correlated trade admitted on an external feed alone.
**Exit gate:** **G-STRAT-1**.

---

## PHASE 17 — Legacy retirement

**Objective:** Remove `crates/arb-exec-legacy` once every path is migrated and every replacement has passed its gate.

**Dependencies:** all prior phases green for ≥ 14 days.

**Tasks:**
- [ ] **17.1** Confirm every legacy path is unreachable: a CI test asserts `arb-exec-legacy` has no callers from `apex-*`.
- [ ] **17.2** Remove `sandwich.rs`, `bridge.rs`, `hot_path.rs` (absorbed), `venue_adapter.rs` (already removed), `plan.rs` legacy encoder, the `Runner`/`RunnerConfig` god-object, `MevRole`, `Strategy`, `BroadcastEndpoint`.
- [ ] **17.3** Remove the blue paths whose red replacements have held for 14 days: legacy state reconstruction, legacy encoder, legacy dispatch. **Retain** the quoter parity oracle and the `eth_call` quorum verifier (documented exceptions, §11.4 / §15.6).
- [ ] **17.4** Retire the old deployed executor contract after its funds are swept and 14 days have elapsed since traffic migration.
- [ ] **17.5** Move superseded docs to `docs/legacy/`; regenerate `docs/apex/INVARIANTS.md`.
- [ ] **17.6** Enable `cargo clippy -D warnings` workspace-wide (now that every crate is migrated and clean).

**Acceptance criteria:** zero references to `arb-exec-legacy`; `cargo clippy --workspace -D warnings` green; all 46 invariants green; no behaviour change in a 7-day A/B against the pre-removal build.
**Exit gate:** **G-RETIRE-1**.

---

# 34. File-by-file change plan

Legend: **CREATE** / **MODIFY** / **REPLACE** (new file supersedes an old one; old removed at the stated phase) / **REMOVE** / **MOVE** (`git mv`, history preserved).

## 34.1 Root and build

| Action | Path | Phase | Note |
|---|---|---|---|
| MODIFY | `Cargo.toml` | 0 | → `[workspace]` virtual manifest + `[workspace.dependencies]` |
| MOVE | `src/` → `crates/arb-exec-legacy/src/` | 0 | package name stays `arb-exec`; declare all 5 binaries (fixes B-11) |
| MODIFY | `Makefile` | 0 | workspace-aware; `fmt` refuses tree-wide |
| MODIFY | `.gitignore` | 0 | vendored tarballs, `out/`, logs, `*.bak.*` |
| CREATE | `.github/workflows/ci.yml` | 0 | fixes B-3 |
| CREATE | `scripts/ci/{check_no_generic_call.sh,no_shared_mutable_state.sh,invariant_coverage.sh}` | 0 | |
| MODIFY | `foundry.toml` | 5 | `src = "contracts"` unchanged; add `fs_permissions` for fixture reads |
| REMOVE | `plan.md`, `HANDOFF.md`, `docs/ARCHITECTURE_PIVOT_HANDOFF.md`, `docs/PRODUCTION_AUDIT_FIX_PLAN.md` | 0 | superseded; commit the already-staged deletions |
| REMOVE | `grafana-10.4.2.linux-amd64.tar.gz`, `prometheus-2.52.0.linux-amd64.tar.gz`, `grafana-v10.4.2/`, `prometheus-2.52.0.linux-amd64/`, `out/`, `node/`, `eth-docker/`, `arbot-live.log`, `validation-*.log`, `parity_2026-09-14.csv`, `cl_pools.txt`, `cl_candidates.txt`, `python3 Convert.py` | 0 | `git rm --cached` + ignore |
| REMOVE | `base_venues_complete.yaml`, `generate_base_venues.py`, `base_all_pools_100k.{py,json}`, `base_multi_dex_pools.{py,json}`, `uniswap_v4_pools_99k.py` | 2 | fabricated addresses (B-7); superseded by the verified inventory pipeline |
| REMOVE | `ops/inputs.yaml.bak.*` (8), `config/*.bak*` (6), `.env.bak.*` (12), `data/**/*.bak*` (15+), `scripts/*.bak-*` (2) | 0 | |

## 34.2 `crates/apex-types` (CREATE, Phase 0)

`src/{lib,ids,state,candidate,ticket,commitment,cost,flash,sim,risk,miss,pnl}.rs`; `tests/{ticket_monotonic,miss_exhaustive,gas_types,compile_fail/*}.rs`.

## 34.3 `crates/apex-config` (Phase 0)

**Built fresh — nothing is moved here.** See Task 0.4's cycle note: the three legacy config modules import upward into `util`, `venues`, `chain` and `bridge`, so moving them would make `apex-config` depend back on `arb-exec-legacy`.

| Action | Path | From |
|---|---|---|
| CREATE | `src/{lib,schema,ops,registry,validate,secret}.rs` | written fresh against `ops/inputs.yaml` + `config/` |
| CREATE | `tests/{immutable,env_coverage,differential}.rs` | — |
| MODIFY | `crates/arb-exec-legacy/src/ops_inputs.rs` | add `to_apex_config()`; module stays put until Phase 17 |
| (unchanged) | `crates/arb-exec-legacy/src/{registry,config_validation}.rs` | retired in Phase 17, not moved |

## 34.4 `crates/apex-math` (Phase 2)

| Action | Path | From | Disposition |
|---|---|---|---|
| MOVE | `src/cl/fixed_point.rs` | `src/cl_math.rs` | KEEP verbatim |
| MOVE | `src/cl/swap.rs` | `src/cl_swap.rs` | KEEP |
| MOVE | `src/cl/ticks.rs` | `src/cl_ticks.rs` | KEEP |
| MOVE | `src/cl/sim.rs` | `src/cl_sim.rs` | ADAPT (env gate → config) |
| MOVE | `src/parity_gate.rs` | `src/cl_parity_gate.rs` | KEEP — **migrate the doc comment verbatim** |
| MOVE | `src/cpmm.rs` | `src/quote_univ2.rs` | KEEP |
| MOVE | `src/solidly.rs` | `src/quote_solidly.rs` | KEEP |
| MOVE | `src/curve.rs` | `src/quote_curve.rs` | ADAPT → six-method contract |
| MOVE | `src/balancer.rs` | `src/quote_balancer.rs` | ADAPT → six-method contract |
| MOVE | `src/ranking.rs` | `src/math.rs` | KEEP |
| MOVE | `src/prelude.rs` | `src/quote_common.rs` | ADAPT |
| CREATE | `src/engine.rs` | — | `ExactPricingEngine` trait |
| CREATE | `tests/{engine_contract,differential}.rs`, `fuzz/fuzz_targets/*.rs` | — | |
| MOVE | `src/bin/cl_parity.rs` → `crates/apex-tools/src/bin/cl_parity.rs` | | KEEP |

## 34.5 `crates/apex-state` (Phase 1)

| Action | Path | From | Disposition |
|---|---|---|---|
| MOVE | `src/live.rs` | `src/live_state.rs` | ADAPT |
| MOVE | `src/continuity.rs` | `src/continuity.rs` | ADAPT (`Ordinal` gains flashblock fields) |
| MOVE | `src/state_gate.rs` | `src/state_gate.rs` | KEEP |
| MOVE | `src/validation.rs` | `src/state_validation.rs` | KEEP |
| MOVE | `src/reconcile.rs` | `src/reconcile.rs` | KEEP |
| MOVE | `src/validation_select.rs` | `src/validation_select.rs` | KEEP |
| MOVE | `src/decode.rs` | `src/log_decode.rs` | KEEP |
| MOVE | `src/feed/ingestion.rs` | `src/ingestion.rs` | ADAPT |
| MOVE | `src/fast/` | `src/base_fast.rs` (dirty set, drain, `FlashFeed`) | ADAPT — split; Base-specific feed → `apex-chain` |
| MOVE | `src/pools.rs` | `src/pool_store.rs` | ADAPT |
| MOVE | `src/depth.rs` | `src/liquidity_cache.rs` | ADAPT (marked non-authoritative) |
| REPLACE | `src/tokens/{mod,classifier,fingerprint}.rs` | `src/token_refresh.rs` | REBUILD |
| CREATE | `src/{version,branch,fingerprint,patch,dep_index,versioned,differential}.rs`, `src/feed/{integrity,arbiter}.rs` | — | |
| CREATE | `tests/{versioned,ordinal,feed_integrity,branch,tokens,differential}.rs` | — | |

## 34.6 `crates/apex-venues` (Phase 2)

| Action | Path | From |
|---|---|---|
| MOVE | `src/cl/quoter.rs` | `src/quote_cl.rs` |
| MOVE | `src/univ3.rs` | `src/quote_univ3.rs` |
| MOVE | `src/slipstream.rs` | `src/quote_slipstream.rs` |
| MOVE | `src/discovery.rs` | `src/discovery.rs` |
| CREATE | `src/{lib,adapter,pancake,aerodrome,balancer,curve,registry,breaker}.rs` | — |
| REPLACE | `src/adapter.rs` | `src/venue_adapter.rs` (REMOVE — wrong trait, no implementors) |
| CREATE | `src/univ4/{mod,pool_key,hooks,flash_accounting,engine,fingerprint}.rs` (Phase 11) | REPLACES `src/quote_univ4.rs` (REMOVE) |
| CREATE | `tests/admission.rs`, `fuzz/` | — |

## 34.7 `crates/apex-search` (Phase 2, 13)

| Action | Path | From |
|---|---|---|
| MOVE | `src/graph.rs` | `src/graph.rs` (ADAPT: edges carry `StateVersion`) |
| MOVE | `src/frontier.rs` | `src/cycle_index.rs` + `src/hot_path.rs` (merged) |
| MOVE | `src/events/mod.rs` | `src/mempool.rs` (ADAPT → typed `StateEvent`) |
| CREATE | `src/{lib,finite_size,engine_a,engine_c,engine_d}.rs` | — |
| REPLACE | `src/backrun/{mod,target,post_state,timing}.rs` (Phase 13) | `src/backrun_state.rs` (REMOVE) |

## 34.8 `crates/apex-econ` (Phase 3, 10, 12)

| Action | Path | From |
|---|---|---|
| MOVE | `src/sizing/continuous.rs` | `src/sizing.rs` |
| MOVE | `src/allocation/convex.rs` | `src/convex.rs` |
| MOVE | `src/flash/mod.rs` | `src/flash_loan.rs` |
| MOVE | `src/compute/pool_priority.rs` | `src/hot_pools.rs` |
| REPLACE | `src/cost/{mod,l1_data,failure,calldata}.rs` | `src/fees.rs` (REMOVE at Phase 15, after Arbitrum logic moves out) |
| CREATE | `src/sizing/discrete.rs`, `src/ev/{mod,scenario}.rs`, `src/eligibility.rs`, `src/allocation/{kkt,piecewise,cluster,certificate,improving_path}.rs`, `src/packing/{mod,conflict,bounded}.rs`, `src/allocator/{mod,chain_score}.rs` | — |

## 34.9 `crates/apex-sim` (Phase 4, 9)

| Action | Path | From |
|---|---|---|
| MOVE | `src/backends/revm.rs` | `src/sim_revm.rs` (KEEP) |
| MOVE | `src/quorum.rs` | `src/sim_quorum.rs` (KEEP) |
| CREATE | `src/{lib,tier0,tier1,tier2,fidelity,adversarial}.rs`, `src/backends/{mod,eth_call,base_simulate_v1}.rs`, `src/competitor/{mod,censoring,capture_curve}.rs` | — |

## 34.10 `crates/apex-risk` / `apex-capture` / `apex-exec` / `apex-chain` / `apex-obs` / `apex-strategy` / `apex-runtime` / `apex-tools`

| Action | Path | From | Phase |
|---|---|---|---|
| MOVE | `apex-risk/src/policy.rs` | `src/risk_policy.rs` | 6 |
| MOVE | `apex-risk/src/breaker.rs` | `main.rs` `CircuitBreaker` + its tests | 6 |
| MOVE | `apex-risk/src/capital.rs` | `src/capital.rs` | 6 |
| CREATE | `apex-risk/src/{lib,posture,loss}.rs` | — | 6 |
| MOVE | `apex-capture/src/nonce.rs` | `main.rs` `NonceManager` (comments preserved) | 6 |
| MOVE | `apex-capture/src/health.rs` | `src/health.rs` | 6 |
| CREATE | `apex-capture/src/{lib,ticket,registry,journal,protocol,reserve,revalidate,scheduler,reconcile}.rs`, `src/signer/{mod,pool,lane}.rs`, `src/dispatch/{mod,router,ack}.rs` | — | 6 |
| MOVE | `apex-exec/src/abi.rs` | `src/abi.rs` | 5 |
| MOVE | `apex-exec/src/encode/legacy_plan.rs` | `src/plan.rs` (blue path; REMOVE at 17) | 5 |
| CREATE | `apex-exec/src/{lib,commitment}.rs`, `src/encode/{mod,steps}.rs` | — | 5 |
| MOVE | `apex-chain/src/rpc/failover.rs` | `src/rpc_failover.rs` (KEEP) | 7 |
| MOVE | `apex-chain/src/rpc/ws.rs` | `src/util.rs` WS helpers | 7 |
| REPLACE | `apex-chain/src/{adapter,regime}.rs` + `src/base/*` | `src/chain.rs` (REMOVE at 17) | 7 |
| CREATE | `apex-chain/src/{ethereum,bsc,arbitrum,optimism}/` | — | 14, 15 |
| MOVE | `apex-obs/src/metrics.rs` | `src/metrics.rs` | 8 |
| MOVE | `apex-obs/src/accounting.rs` | `src/accounting.rs` | 8 |
| CREATE | `apex-obs/src/{lib,miss,coverage,pnl,attribution}.rs` | — | 8 |
| MOVE | `apex-strategy/src/liquidation/monitor.rs` | `src/liquidations.rs` | 16 |
| CREATE | `apex-strategy/src/{lib,triangular,multihop,correlated,finite_size}.rs` | — | 8, 16 |
| REPLACE | `apex-runtime/src/{main,plane,bus,workers,supervise,shutdown}.rs` | `src/main.rs` (REMOVE at 17) | 8 |
| MOVE | `apex-tools/src/bin/{ingest,cycle_index_stats,ws_probe,cl_parity}.rs` | `src/bin/*` | 0 |
| CREATE | `apex-tools/src/bin/{coverage_audit,ticket_journal_inspect}.rs` | — | 8 |
| REMOVE | `src/{sandwich,bridge,venue_adapter,backrun_state,hot_path,token_refresh,integration_smoke}.rs` | | 2–17 |
| MOVE | `tests/integration_smoke.rs` | stays at workspace root `tests/` | 0 |

## 34.11 Solidity

| Action | Path | Phase | Note |
|---|---|---|---|
| CREATE | `contracts/core/{Types,ExecutionAuth,AdapterRegistry,RouteValidator,ProfitInvariant,FlashSourceRouter}.sol` | 5 | |
| CREATE | `contracts/adapters/{Aave,UniswapV3,Aerodrome,Slipstream,Pancake,Balancer}Adapter.sol` | 5 | |
| CREATE | `contracts/adapters/{Morpho,UniswapV4}Adapter.sol` | 11 | |
| CREATE | `contracts/chains/BaseArbExecutor.sol` | 5 | |
| CREATE | `contracts/chains/EthereumArbExecutor.sol` | 14 | |
| MOVE | `contracts/utils/{AccessController,ReentrancyGuard}.sol` → `contracts/core/` | 5 | |
| MODIFY | `contracts/executor/MultiVenueArbImplementation.sol` | 5 | remove `_execGeneric`, `_execModule`, JIT, bridge (kept as blue until 17) |
| REMOVE | `contracts/libraries/BridgeLib.sol`, `contracts/executor/steps/{JitExecutor,BridgeExecutor}.sol`, `contracts/mocks/MockBridge.sol` | 5 | |
| REMOVE | `contracts/artifacts/**` (60+ JSON) | 0 | |
| REMOVE | `contracts/executor/BatchRouter.sol` | 5 | if the UNKNOWN resolves to no callers |
| REMOVE | `contracts/executor/MultiVenueArbImplementation.sol`, `contracts/executor/steps/`, `contracts/interfaces/MultiVenueArbExecutor.sol` | 17 | |
| MODIFY | `test/MultiVenueArbExecutor.t.sol` | 5 | drop JIT/bridge; keep and extend the rest |
| CREATE | `test/{AdapterRegistry,RouteValidator,ProfitInvariant,NoArbitraryCall}.t.sol`, `test/invariant/*.sol` | 5 | |
| MODIFY | `script/Deploy.s.sol` | 5 | new contract set, per-chain executors |

## 34.12 Configuration, data, docs

| Action | Path | Phase | Note |
|---|---|---|---|
| MODIFY | `ops/inputs.yaml` | 0 | add `schema_version`; no semantic change |
| CREATE | `ops/observability/{prometheus.yml,grafana/*}` | 0 | MOVE from root + `docs/grafana/` |
| MODIFY | `config/registry.json` | 2 | every address bytecode-verified |
| CREATE | `data/manifest.json` | 2 | content hash + provenance per inventory file |
| CREATE | `scripts/data/verify_registry_bytecode.py` | 2 | |
| CREATE | `scripts/canary/{run_canary.sh,promote.sh}` | 8 | |
| CREATE | `docs/apex/{BASELINE,INVARIANTS,GATES,INFRA,RUNBOOK}.md`, `docs/apex/reports/` | 0 | |
| MOVE | `docs/whitepaper-v3.md`, `docs/deployments-v3.md`, `docs/executor-modularization.md`, `docs/bellman_ford_shadow_mode_analysis_2026-02-19.md` → `docs/legacy/` | 17 | |
| KEEP | `docs/superpowers/**`, `docs/arbot_docs_pack/**`, `docs/{operator_runbook,venue_onboarding,fork_dry_run,integration_testing,monitoring_guide}.md` | — | operational value |

---

# 35. Migration / rollback plan

## 35.1 The seven questions, answered

| Question | Answer |
|---|---|
| **What remains?** | Exact mathematics (`cl_math`, `cl_swap`, `cl_ticks`, `quote_univ2`, `quote_solidly`, `math`), both trust gates (`cl_parity_gate`, `state_gate`), validation (`state_validation`, `reconcile`, `validation_select`), decoding (`log_decode`), simulation (`sim_revm`, `sim_quorum`), RPC failover, the Base fast-path feed and dirty-set drain, the Prometheus surface, accounting, the Foundry suite and deployment tooling, `scripts/{ci,fork,shadow,data}`, `docs/superpowers`. |
| **What gets wrapped?** | `cl_sim`, `quote_curve`, `quote_balancer`, `quote_common` (→ six-method `ExactPricingEngine`); `quote_univ3`, `quote_slipstream`, `quote_cl` (→ `VenueAdapter`); `NonceManager` (→ one lane in a pool); `CircuitBreaker` (→ posture ladder); `risk_policy` (→ graduated response); `metrics`/`accounting` (→ `apex-obs`); `ops_inputs`/`registry`/`config_validation` (→ immutable `ApexConfig`). |
| **What gets refactored?** | `live_state`, `continuity`, `ingestion`, `base_fast`, `pool_store`, `liquidity_cache` (→ versioned state); `graph`, `cycle_index`, `hot_path`, `mempool` (→ frontier + typed events); `sizing`, `convex`, `flash_loan`, `hot_pools` (→ `apex-econ`); `liquidations` (→ full protocol model); `util` (dissolved by owner). |
| **What gets replaced?** | `main.rs` `Runner`/`RunnerConfig` → `apex-runtime` control plane; `fees.rs` → `TotalExecutionCost`; `chain.rs` → `ChainExecutionAdapter`; `plan.rs` → commitment encoder; `quote_univ4.rs` → full V4 engine; `venue_adapter.rs` → real `VenueAdapter`; `backrun_state.rs` → exact target simulation; `token_refresh.rs` → semantics classifier; `Published<T>` → `Versioned<T>`; 84 env vars → `ApexConfig`; `MultiVenueArbImplementation` → `BaseArbExecutor` + core + adapters. |
| **What gets deleted?** | `sandwich.rs`, `bridge.rs`, JIT and bridge contract paths, `_execGeneric`, `_execModule`, `contracts/artifacts/**`, `base_venues_complete.yaml` + its generator, all `.bak.<epoch>` files, vendored infrastructure tarballs and build output, `plan.md` and the three legacy handoff docs, `apply_public_mempool_jitter`. |
| **When does each legacy path become unreachable?** | State: Phase 1 traffic migration. Pricing: Phase 2. Cost/sizing: Phase 3. Simulation: Phase 4. Settlement: Phase 5 (contract deployed but not yet primary) → Phase 8 (canary flips authority). Dispatch/signing: Phase 6 (shadow) → Phase 7 (live). Orchestration: Phase 8, when the event-driven plane carries 100% of live tickets. Chain hard-coding: Phase 15. |
| **When may old code be removed?** | Only after its replacement has held its production gate for **14 consecutive days**, and only in Phase 17 — except repository hygiene (Phase 0) and features excluded by §42 (Phase 5/17), which have no replacement to prove. Two deliberate permanent exceptions: the on-chain quoter path (feeds `cl_parity_gate`) and the `eth_call` path (feeds `sim_quorum`) are **never** retired. |

## 35.2 Red/blue subsystems and their switches

Each is a field on the immutable `ApexConfig`, so flipping it requires a restart with a new config version — which is itself an audited, reversible event.

| Subsystem | Config field | Blue | Red | Differential artifact | Migration gate | Rollback |
|---|---|---|---|---|---|---|
| **Exact pricing** | `pricing.authority` | quoter RPC + `cl_parity_gate` | `apex-math::ExactPricingEngine` | `docs/apex/reports/pricing-diff-*.csv` | 0 bps on every trusted pool; every divergence explained | flip field; blue is always running as the parity oracle |
| **State reconstruction** | `state.authority` | `live_state` + `state_gate` | `apex-state` versioned | `state-diff-*.csv` | 72 h zero unexplained divergence | flip field; blue keeps running as the differential oracle |
| **Simulation** | `sim.authority` | `eth_call` + quorum | `eth_simulateV1` / REVM Tier 2 | `sim-diff-*.csv` | 7 d ≥ 99.9% agreement | flip field; blue remains the quorum verifier |
| **Execution encoding** | `exec.authority` | `plan.rs` legacy `Plan` | `apex-exec` `PlanV3` + commitment | `encode-diff-*.csv` | identical simulated economics on ≥ 99.9% | flip field |
| **Settlement contract** | `executor.address` | `MultiVenueArbImplementation` | `BaseArbExecutor` | fork A/B + canary trades | external review clear + 100 canary trades positive | point the field back at the old address (kept deployed and funded until Phase 17) |
| **Accounting** | `accounting.authority` | `accounting.rs` CSV | `apex-obs::pnl` attribution | `pnl-diff-*.csv` | penny-exact agreement on 500 trades | flip field |
| **Dispatch** | `dispatch.authority` | `main.rs` `dispatch_call` | `apex-capture` controller | shadow null-dispatch comparison | Phase 6 acceptance criteria | flip field |

## 35.3 Rollback design per major migration

| Migration | Rollback trigger | Rollback mechanism | Data compatibility | Config compatibility | Deployment compatibility |
|---|---|---|---|---|---|
| **Workspace split (Phase 0)** | Any behaviour change in the shadow-run fixture diff | `git revert` the split commit; it is a pure move | none touched | unchanged | same binary name and path |
| **State (Phase 1)** | Unexplained divergence, or fast-path p99 regression > 10% | `state.authority = blue` | `Versioned` snapshots are in-memory only; nothing persisted changes | `ApexConfig` is a superset; blue ignores the new fields | same process |
| **Pricing (Phase 2)** | Any pricing divergence on a trusted pool | `pricing.authority = blue` | none | superset | same process |
| **Cost/sizing (Phase 3)** | Realized cost outside the modelled band on > 1% of trades | `econ.cost_model = legacy_fee_estimate` (a retained shim) | none | superset | same process |
| **Simulation (Phase 4)** | Agreement < 99.9%, or Tier 2 p99 budget breach | `sim.authority = blue` | none | superset | same process |
| **Settlement (Phase 5→8)** | Any revert class not seen in fork testing; any invariant violation; any external-review finding reopened | `executor.address` → old address | **Plan encodings differ.** The encoder is selected by the same field, so the pair always matches. | superset | old contract stays deployed, funded and authorized until Phase 17 |
| **Capture/dispatch (Phase 6→7)** | `U_capture` < 0.95, or any hard-zero counter non-zero | `dispatch.authority = blue`; the ticket journal is replayed and all in-flight tickets reconciled **before** the switch | Journal format is versioned; blue ignores it | superset | same process |
| **Base adapter (Phase 7)** | Flashblock eligibility accuracy < 80%, or submission failure rate above band | `chain.base.scheduler = naive` (fixed gas limit, no eligibility model) | none | superset | same process |
| **Orchestration (Phase 8)** | Any regression in realized net P&L per hour versus the blue baseline over 24 h | run the `arb-exec` legacy binary; both binaries are built and shipped until Phase 17 | shares `ops/inputs.yaml`; the legacy binary reads env as before | the `ARBOT_*` env plane is **retained but unused** by the new binary until Phase 17, precisely so this rollback works | two binaries in the release artifact |
| **Contract retirement (Phase 17)** | — | **Not reversible.** Therefore it happens only after 14 days green and an explicit operator go/no-go, and funds are swept first. | — | — | — |

**Rule:** no migration proceeds without its rollback path tested at least once in staging. `tests/chaos_rollback.rs` exercises each config flip under load and asserts zero tickets are lost across the switch.

## 35.4 Git doctrine

- Every relocation uses `git mv` so `git log --follow` survives.
- No history rewrite, no force-push to `main`, no repository recreation.
- Large-file removal uses `git rm --cached` plus `.gitignore` — the blobs stay in history, which is correct: rewriting history to shrink the repository would destroy the provenance this migration depends on.
- One commit per task, message in the repository's existing style (lowercase, scoped, stating the *fact* the change establishes rather than the action taken).

---

# 36. First-profitable-trade path

The shortest safe route from today's tree to a live profitable trade, with nothing in it that creates rework.

```text
PHASE 0  workspace + types + config + CI            ← no behaviour change
   ↓
PHASE 1  state correctness                          ← nothing downstream is trustworthy without it
   ↓
PHASE 2  exact pricing + verified venue inventory   ← the 96% no_profitable_size finding is a pricing/fee truth
   ↓
PHASE 3  exact discrete sizing + total cost         ← the cheap-frontier hurdle is 10 bps; the cost model IS the trade
   ↓
PHASE 4  exact simulation (eth_simulateV1 on Base)
   ↓
PHASE 5  Solidity settlement correctness            ← the arbitrary-call hole must close before capital
   ↓
PHASE 6  capture assurance + signer pool (SHADOW)   ← the edge is event-triggered; capture is the remaining gap
   ↓
PHASE 7  Base Flashblock scheduling + submission
   ↓
PHASE 8  observability + miss + coverage → CANARY → FIRST PROFITABLE TRADE
```

## 36.1 Why this order and not the blueprint's §39 order

Blueprint §39 Phase 0 is a six-chain shadow. **This plan does Base only.** The repository has already run the multi-chain shadow in substance and produced the answer: the tradeable set is ~8 cross-venue cheap pairs on Base at 1.6–7 bps, and opportunity density was never the constraint. Spending the first months re-measuring six chains would violate §52 directly. Chains 2–6 arrive in Phases 14–15, gated on `ChainScore`, exactly as §3 requires — just not *first*.

## 36.2 Scope of the first trade

| Dimension | First-trade scope | Why |
|---|---|---|
| Chain | Base only | measured lowest-latency battlefield; everything else is Phase 14+ |
| Strategy | A (triangular) + B (short multi-hop, 2–3 hops) | the census showed 4-hop strictly worse |
| Venues | Aerodrome, Slipstream, Uniswap V3, PancakeSwap | exact engines exist and are 0 bps against the quoter (`cl_parity_gate`), but the **fast path does not currently use them** (§1.1.1). Phase 2 wires the ladder and re-measures. V4 is Phase 11. |
| Route surface | the cheap frontier **as re-measured under exact pricing in Phase 2**, not the provisional 8 pairs | §1.1.1: the measured set came from a single-tick model with a 50 bps haircut. If the re-measurement moves the frontier, this row and §36.2 move with it. |
| Trigger | event-driven fast path only | 88% of net-positive samples came from swap triggers |
| Sizing | single-path exact discrete | splitting is Phase 10, and §39 forbids packing here |
| Flash source | the cheapest reliable of Aave V3 / Balancer / UniV3 flash | existing capacity-bounded selection |
| Simulation | Tier 0 + Tier 1 + Tier 2 (`eth_simulateV1`) | Tier 3 is Phase 9 |
| Robustness | conservative fixed margin in place of the measured competitor model | honest, shippable, replaced in Phase 9 |
| Submission | private-first through a Flashblocks-aware RPC, gas-limit minimized | §24.4, §24.7 |
| Size | canary minimum, one signer lane active of four | §39 Phase 1 |

## 36.3 What is explicitly deferred past the first trade

Packing, joint allocation, parallel splitting, V4, backruns, liquidations, correlated dislocations, Hermes, multi-chain, adaptive allocation, out-of-process signing, and the **port of legacy `ethers` call sites** (new crates are `alloy` from Phase 0 — that is not deferred). **None of these unblocks the first dollar**, and §12 of the task mandate forbids letting exotic strategies delay the core executable profit loop.

## 36.4 The one thing that must not be deferred

**Capture assurance.** The repository's own measurements say the remaining gap is not discovery and not opportunity density — it is turning an event-triggered, thin-margin, exactly-priced opportunity into a dispatched transaction inside its window. That is Phase 6, and it ships *before* the first live trade, in shadow, with the full ticket lifecycle exercised on real traffic and a null dispatcher.

---

# 37. Production rollout

Mapped from Blueprint §39, reordered per §36.1 and the task mandate's profit-path priority.

| Rollout stage | Blueprint §39 | This plan | Enable | Gate |
|---|---|---|---|---|
| **R0 — Base shadow** | Phase 0 (6 chains) | Phases 1–6, Base only | state ingestion, exact reconstruction, triangles, short multi-hop, simulation, chain cost model, ticket lifecycle with null dispatch | Opportunity surface **measurably positive** after realistic fees and modeled capture; `SYSTEM_CAPTURE_ASSURANCE ≥ 0.99` in shadow |
| **R1 — Base core live** | Phase 1 | Phases 7–8 | triangles, short multi-hop, single-path exact sizing, exact simulation, Flashblocks-aware submission. **No packing.** | 100 canary trades positive net after every material cost; all 46 invariants green |
| **R2 — parallel allocation** | Phase 3 | Phase 10 | multi-pool split, KKT warm starts, piecewise tick handling, discrete refinement | positive incremental realized P&L vs the frozen single-route baseline |
| **R3 — programmable venues** | Phase 4 | Phase 11 | V4 exact hooks, custom accounting validation, hook fingerprints | modelled hooks differential-clean; unmodelled hooks shadow-only |
| **R4 — joint + packing** | Phase 5 | Phase 12 | shared-pool clusters, joint concave allocation, improving-path certificate, conflict graph, bounded packing | positive incremental realized P&L **after inclusion effects** |
| **R5 — backruns** | Phase 6 | Phase 13 | target-event simulation, post-event routes, chain-specific inclusion timing | positive backrun capture; zero dispatches against non-landing targets |
| **R6 — chain expansion** | Phase 2 | Phases 14–15 | Ethereum private submission, then BSC/Arbitrum/OP shadow → canary by `ChainScore` | per-chain fee model validated; **no promotion on DEX volume alone** |
| **R7 — liquidations + correlated** | Phase 7 | Phase 16 | Strategy D and E with isolated budgets | positive standalone EV after competition |
| **R8 — adaptive portfolio** | Phase 8 | Phase 15 (allocator) + ongoing | automatic compute/capital reallocation by measured realized EV | allocator decisions traceable to realized evidence |

**Chain expansion rule (§3.3, §39 Phase 2):** BSC and Ethereum enter through shadow/live-canary comparison on measured `ChainScore`. **Neither is promoted because its DEX volume is high.**

---

# 38. Go / no-go gates

## 38.1 Repository-level executable gates

Each gate is a script or CI job, not a judgement call. `docs/apex/GATES.md` is the operator-facing copy.

| Gate | Command | Passes when |
|---|---|---|
| **G-0** Baseline | `scripts/ci/phase0_gate.sh` | workspace check/test matches baseline; CI green; shadow-run fixture byte-identical; ≥200 MB removed; every env var typed or retired |
| **G-STATE-1** State correctness | `scripts/ci/state_gate.sh` | INV-08/11/12/13 green; 72 h differential zero unexplained; gap→`Unsafe`→verified-recovery observed |
| **G-STATE-2** Branch rollback | same | INV-14 green; `rollback_cost_ms` p99 ≤ 20 ms |
| **G-PRICE-1** Pricing correctness | `scripts/ci/pricing_gate.sh` | three-way differential 0 bps on every trusted pool; divergences explained or venue demoted; 24 h fuzz clean |
| **G-VENUE-1** Venue admission | `scripts/data/verify_registry_bytecode.py` | every production address bytecode-verified; every admitted pool carries the full §6.3 record |
| **G-ECON-1** Economic correctness | `scripts/ci/econ_gate.sh` | INV-18/19/20/23 green; L1 data fee within 1% on ≥50 receipts; USD perturbation leaves admission unchanged |
| **G-SIM-1** Simulation fidelity | `scripts/ci/sim_gate.sh` | 7 d red/blue ≥99.9%; `F_sim` in band; Tier 2 p99 ≤ 60 ms; quorum veto works |
| **G-SOL-1** Solidity invariants | `forge test && scripts/ci/check_no_generic_call.sh && make check-contract-size` | INV-24…INV-33 green; 100k invariant fuzz clean; no arbitrary-call surface in source or bytecode; size under limit with ≥15% margin |
| **G-SEC-1** Security | `scripts/secret_scan.sh && cargo audit && cargo deny check` + external review sign-off | no secrets; no known-vulnerable dependency; no unresolved high/critical review finding |
| **G-SIGN-1** Signer/nonce reliability | `scripts/ci/signer_gate.sh` | INV-04/05 green; `loom` model clean; 20 consecutive `SIGKILL` recovery cycles reconcile fully; lane quarantine does not stop the chain |
| **G-CAP-1** Capture assurance | `scripts/ci/capture_gate.sh` | INV-01/02/03/39 green; 100k-case property run with zero non-terminal tickets |
| **G-CAP-2** Capture scheduling | same | INV-09 green; under 10× overload `U_capture ≥ 0.99` and no authorized ticket preempted |
| **G-SUB-1** Submission reliability | `scripts/ci/submission_gate.sh` | INV-10/34/35/36 green; all seven ack stages observable on real traffic |
| **G-BASE-1** Base execution | `scripts/ci/base_gate.sh` | INV-37/38 green; eligibility prediction ≥90% accurate over ≥500 observed transactions; `T_signal→submit` p99 < 200 ms |
| **G-OBS-1** Observability | `scripts/ci/obs_gate.sh` | INV-40/41 green; every §32 metric family exported; auditor detects an injected miss |
| **G-RISK-1** Risk | `scripts/ci/risk_gate.sh` | INV-42/43/44 green; every §28 trigger maps to a posture; every loss classified |
| **G-PERF-1** Performance | `cargo bench --workspace` + budget check | every §29.5 stage budget met at p99 |
| **G-PROD-1** Realized P&L | `scripts/canary/promote.sh` | 100 canary trades positive aggregate net after every material cost; zero unexplained losses; revert rate below class threshold; opportunity surface supports planned scale |
| **G-ALLOC-1/2**, **G-V4-1**, **G-ADV-1**, **G-BACKRUN-1**, **G-ETH-1**, **G-CHAIN-1**, **G-STRAT-1**, **G-RETIRE-1** | per-phase scripts | as stated in each phase |

## 38.2 Blueprint §40's 36 conditions → gates

| §40 | Condition | Gate |
|---|---|---|
| 1 | State reconstruction matches chain state | G-STATE-1 |
| 2 | Exact AMM math passes differential testing | G-PRICE-1 |
| 3 | Exact route simulation matches fork execution | G-SIM-1 |
| 4 | Simulation-vs-realized divergence within bounds | G-SIM-1 |
| 5 | Venue adapter passes fuzz/property testing | G-PRICE-1, G-VENUE-1 |
| 6 | Flash-loan repayment invariants hold | G-SOL-1 (INV-32) |
| 7 | Contract access controls pass adversarial tests | G-SOL-1 (INV-24/25/26) |
| 8 | Revert rate below class threshold | G-PROD-1 |
| 9 | State staleness below class threshold | G-STATE-1 |
| 10 | Competitive capture positive | G-PROD-1 |
| 11 | Realized net P&L positive after every material cost | G-PROD-1 |
| 12 | Two materially different market regimes observed | G-PROD-1 |
| 13 | No unexplained loss cluster above threshold | G-RISK-1 |
| 14 | Competitor model directionally calibrated | G-ADV-1 |
| 15 | Parallel splitting positive incremental realized P&L | G-ALLOC-1 |
| 16 | Joint allocation positive incremental realized P&L | G-ALLOC-2 |
| 17 | Packing positive after inclusion effects | G-ALLOC-2 |
| 18 | Liquidation positive standalone EV after competition | G-STRAT-1 |
| 19 | V4 hook routes exact or explicitly bounded | G-V4-1 |
| 20 | Chain-specific fee models validated | G-ECON-1, G-CHAIN-1 |
| 21 | Base earliest-eligible-Flashblock empirically validated | G-BASE-1 |
| 22 | Signer/nonce lifecycle passes failure/replacement testing | G-SIGN-1 |
| 23 | Circuit breakers trigger safely in controlled tests | G-RISK-1 |
| 24 | Infrastructure expense justified by incremental captured profit | G-PROD-1 |
| 25 | Measured opportunity surface supports planned scale | G-PROD-1 |
| 26 | Capture-assurance invariant (zero silent expiry) | G-CAP-1 |
| 27 | Ticket-drop invariant | G-CAP-1 |
| 28 | Signer-lane health: no single mandatory signer | G-SIGN-1 |
| 29 | Last-mile revalidation | G-SUB-1 |
| 30 | Opportunity-coverage audit | G-OBS-1 |
| 31 | Capture-path utilization in band | G-CAP-2 |
| 32 | USD valuation isolation | G-ECON-1 |
| 33 | Feed continuity | G-STATE-1 |
| 34 | Submission-stage observability | G-SUB-1 |
| 35 | Capture path saturation headroom | G-CAP-2 |
| 36 | No silent expiry; every miss has a reason code | G-CAP-1, G-OBS-1 |

## 38.3 §57.2 architect sign-off checklist → owning phase

| Sign-off item | Phase |
|---|---|
| all 36 production gates pass | 8 |
| capture-assurance invariant has integration-test coverage | 6 |
| crash/restart recovery reconciles all live tickets | 6 |
| signer/nonce pool failover tested under load | 6 |
| feed-gap recovery blocks unsafe trading, resumes only after verification | 1 |
| Base Flashblocks simulation uses current pre-confirmed state correctly | 4 |
| Base gas-limit scheduling incorporated into earliest-inclusion selection | 7 |
| Ethereum private submission lanes measured independently | 14 |
| chain adapters discover and validate their active ordering regime | 7 (Base), 15 (others) |
| no live-path queue can silently expire an admitted ticket | 6 |
| capture-path saturation produces deterministic load shedding | 6 |
| every missed opportunity receives a machine-readable reason | 8 |
| realized P&L attributable to route, venue, strategy, chain, optimization layer | 8 |
| infrastructure changes promoted only on positive incremental captured NetUSD | 8 (process) |

---

# 39. Risk register

| ID | Risk | Likelihood | Impact | Mitigation | Owning phase | Trigger to escalate |
|---|---|---|---|---|---|---|
| **R-01** | The migration stalls mid-way, leaving two half-built architectures | Medium | High | Every phase has a green exit gate and leaves the system shippable; blue paths stay live until their red replacement passes | all | two consecutive phases exceeding their estimate by >50% |
| **R-02** | The measured cheap frontier is too small to reach $25k/month | **High** | High | **A prior session already concluded (`ARCHITECTURE_PIVOT_HANDOFF.md §8`): "No evidence was found that $25k/month is achievable for a new entrant in Base atomic arbitrage."** That is not a reason to stop — the same section argues the architecture fix is well-defined and reuses most of what exists — but it means the target is unproven from the outset, not merely unconfirmed. §38.1 requires proving density *before* scale is claimed. If the surface is insufficient the honest answers are venue expansion (Phase 11), chain expansion (Phase 14/15), or strategy expansion (Phase 13/16), chosen by measurement. Compounded by **R-18**. | 8 | canary positive but `profit_per_hour` extrapolating below target |
| **R-03** | The arbitrary-call hole is exploited before Phase 5 lands | Low | **Critical** | The current contract is only reachable by the `onlyExecutor` signer, and that key is under the operator's control. **Interim control: do not fund the existing executor beyond canary size until Phase 5 ships.** | 5 | any unexplained executor state change |
| **R-04** | Removing `_execGeneric` breaks a venue that silently depended on it, or the adapter set exceeds EIP-170 without the module split | Medium | Medium | Phase 5 Task 5.1 enumerates every production `Op.GENERIC` use from the candidate log before deletion; each becomes a typed adapter. `make check-contract-size` runs on every Phase 5 commit, and the ≥15% margin in G-SOL-1 is the early warning. | 5 | any route that cannot be expressed as a typed step, or size margin below 15% |
| **R-05** | `Versioned<T>` migration introduces a subtle state race | Medium | High | `loom` models on the drain and the branch tree; 72 h red/blue differential; fast-path p99 regression gate | 1 | any unexplained divergence |
| **R-06** | Flashblock capacity model `Q(k)` is wrong and systematically mis-schedules | Medium | High | Learned from observation with a confidence interval, never hard-coded; validated against ≥500 observed transactions; rollback to `scheduler = naive` | 7 | eligibility accuracy < 80% |
| **R-07** | Multi-lane signing introduces a nonce bug that costs real money | Medium | **Critical** | `loom` exhaustive model; 20 `SIGKILL` recovery cycles; per-lane quarantine; `nonce_reuse` is a hard-zero counter that halts the chain | 6 | any nonce reuse, ever |
| **R-08** | The ticket journal `fsync` becomes a latency bottleneck | Low | Medium | Only transitions at/after `AUTHORIZED` fsync; budget is 1 ms; if breached, move to an `io_uring` append or a batched group-commit with a bounded window | 6 | `T_sign` p99 > 3 ms attributable to journal |
| **R-09** | V4 hook modelling proves intractable for the pools that matter | Medium | Medium | §10.4's shadow-only rule means this degrades coverage, not correctness. V4 is Phase 11, after the first dollar. | 11 | >50% of V4 pools unmodellable |
| **R-10** | The coverage auditor shows a large unexplained hot-path miss rate | Medium | Medium | This is the designed trigger for template expansion / slow-path budget / strategy disable — and the gate that would justify building Hermes | 8 | `hot_path_recall` < 0.95 sustained |
| **R-11** | `ethers` 2.0.14 is deprecated; the tree runs two primitive type families | **High** (certain) | Medium | **Split** (§2.2 C-10): new crates are `alloy-primitives` from Phase 0 — already in `Cargo.lock` via revm, and required by Phase 4's `eth_simulateV1` backend, which `ethers` cannot type. Legacy call sites are **not** ported ahead of the first dollar. The residual risk is the conversion boundary in `apex-types`, which is covered by round-trip property tests (Task 0.3a). | 0 (new code), 17 (legacy) | a conversion bug reaching the dispatch path, or a security advisory against `ethers` |
| **R-12** | Pool inventories drift from on-chain reality | Medium | High | Phase 2 manifest with content hashes + on-chain re-verification at load; `verify_pool_venues.py` in the pipeline | 2 | any admitted pool failing verification |
| **R-13** | External review of the new contracts finds a critical issue late | Medium | High | Review is a Phase 5 **exit gate**, before any mainnet deployment; the old contract stays live and funded as the rollback | 5 | any high/critical finding |
| **R-14** | The event-triggered edge (the first net-positive samples) does not survive contact with real competition | **High** | High | This is precisely what the canary measures. §38.1 forbids claiming a path on anything but measured realized P&L. If capture is lost to competitors, the competitor model (Phase 9) and latency budget (§29.5) are the levers — and §30.1 governs whether spending on latency is justified. | 8, 9 | canary capture rate materially below the shadow estimate |
| **R-15** | Scope creep: a later phase's work leaks into the profit path | Medium | High | §36.3's deferral list is explicit; the phase exit gates are the enforcement; `PLAN.md` is the construction contract | all | any Phase 9+ file appearing in a Phase 0–8 commit |
| **R-19** | The EV model prices submission lanes by declared policy, with no recorded evidence of what each lane actually guarantees (B-14) | Medium | Medium | Not a leak — Base's lanes do carry provider-level MEV protection. The risk is that `SubmissionPolicy` is an *assertion* no one has checked, so a silently changed provider setting would degrade capture invisibly. Phase 7 Task 7.3a records a `PrivacyEvidence` per lane and re-checks it. **BlockPI's Base bundle support is an asset** the plan had not accounted for — see §24.5. | 7 | a lane's recorded evidence going stale or failing its periodic re-check |
| **R-20** | ~240 ms RPC RTT ceiling; no local Base node | Medium | Medium | Recorded from `ARCHITECTURE_PIVOT_HANDOFF.md §6.3`. A local node also "makes REVM viable" for Tier 2. Blueprint §31 permits it where economically justified and §30.1 gates the spend on `ΔCaptureEV > ΔInfraCost + ΔComplexityRisk`. Revisit with the Phase 8 latency budget in hand, not before. | 8 | `T_signal→submit` p99 breaching §29.5 with RTT as the dominant term |
| **R-21** | Liquidations (Phase 16) may be structurally unavailable on Base | Medium | Low (scope only) | `ARCHITECTURE_PIVOT_HANDOFF.md §8`: Aave's Chainlink SVR recaptures ~73% of liquidation MEV to the protocol and is extending to Base. Phase 16's gate already requires positive standalone EV after competition; this is prior evidence it will likely fail. **Re-measure SVR coverage before starting Phase 16**, and drop the phase rather than build it on hope. | 16 | SVR confirmed live on the target Aave market |
| **R-17** | This audit read `c5e4d44`, 5 commits behind `origin/main` | ~~High~~ **RESOLVED 2026-09-22** | — | Task 0.0 ran. The five commits are: three doc deletions, a one-line `agents.md` edit, and `7f059cb` — which is the **blueprint itself**, verified byte-identical to the file this plan was written against (`sha256 0f32b5a3…`, 4,053 lines / 113,992 bytes). **No source file was touched, so no §4 disposition is affected.** Residual: the local checkout is still behind and must be fast-forwarded (Task 0.0 Step 3). | 0 | closed |
| **R-18** | The cheap frontier was measured under single-tick CL pricing with a 50 bps haircut (§1.1.1) | **High** (certain) | **High** | G-PRICE-2 blocks Phase 2 exit until the frontier is re-measured under exact pricing and §36.2 is confirmed or revised. Compounds R-02: if the frontier moves, the $25k/month question moves with it. | 2 | re-measurement materially changing the tradeable set |
| **R-16** | Repository hygiene removal accidentally deletes a needed artifact | Low | Medium | `git rm --cached` only (blobs stay in history); the removal list is explicit and reviewed; Phase 0's zero-behaviour-change gate catches functional loss | 0 | shadow-run fixture diff |

---

# 40. Complete blueprint traceability matrix

Every material requirement in `APEX_MEV_v4_Final_Architect_Blueprint.md` receives an identifier and maps to a PLAN phase, a repository component, files, tests and an acceptance gate. Section 40.3 proves there are no unassigned material requirements.

**Legend:** *Files* names the primary owning path (the §34 file plan carries the full set). *Tests* names the principal test; per-phase task lists carry the rest.

## 40.1 Matrix

| BP | Requirement (blueprint §) | Phase | Component | Files | Tests | Gate |
|---|---|---|---|---|---|---|
| BP-001 | Dynamic chain admission replaces static priority (§0.1, §3) | 15 | `apex-econ::allocator` | `crates/apex-econ/src/allocator/chain_score.rs` | `econ::chain_score_reallocates_on_evidence` | G-CHAIN-1 |
| BP-002 | Flashblocks as a speculative branch/overlay engine, not a scalar pending state (§0.2, §5) | 1, 7 | `apex-state::branch` | `crates/apex-state/src/branch.rs` | `state::divergent_branch_rolls_back` | G-STATE-2 |
| BP-003 | Gas becomes a chain-specific execution-cost model (§0.3, §23) | 3 | `apex-econ::cost` | `crates/apex-econ/src/cost/` | `econ::l1_data_fee_matches_receipts` | G-ECON-1 |
| BP-004 | Base inclusion modelled as a scheduling problem (§0.4, §22) | 7 | `apex-chain::base::flashblock` | `crates/apex-chain/src/base/flashblock.rs` | `chain::eligibility_uses_measured_model` | G-BASE-1 |
| BP-005 | Uniswap V4 treated as programmable execution (§0.5, §10) | 11 | `apex-venues::univ4` | `crates/apex-venues/src/univ4/` | `univ4::differential` | G-V4-1 |
| BP-006 | Flash liquidity is a routed resource (§0.6, §19) | 3 | `apex-econ::flash` | `crates/apex-econ/src/flash/router.rs` | `econ::flash_selection_matches_optimum` | G-ECON-1 |
| BP-007 | EV is scenario-conditioned, not a product of independent probabilities (§0.7, §2) | 3, 9 | `apex-econ::ev` | `crates/apex-econ/src/ev/scenario.rs` | `econ::scenario_ev_is_not_a_product` | G-ECON-1 |
| BP-008 | Compute is economically allocated (§0.8, §29) | 8 | `apex-econ::compute` | `crates/apex-econ/src/compute/` | `econ::priority_is_dollars_per_ms` | G-CAP-2 |
| BP-009 | Realized net USD/hour is the principal metric; never route/tx/bps counts (§1.1) | 8 | `apex-obs::pnl` | `crates/apex-obs/src/pnl.rs` | `obs::profit_per_hour_exported` | G-OBS-1 |
| BP-010 | Flash loans are financing, not edge (§1.2) | 3 | `apex-econ::flash` | `crates/apex-econ/src/flash/` | `econ::flash_never_creates_edge` | G-ECON-1 |
| BP-011 | Approximations are candidate generators only; the 8-step survival list is mandatory (§1.3) | 2–5 | `apex-search` → `apex-sim` | `crates/apex-search/`, `crates/apex-sim/` | `econ::eligibility_requires_every_clause` | G-ECON-1 |
| BP-012 | No module ships without demonstrated incremental P&L over a benchmark (§1.4, §48) | 10+ | process + `apex-obs::attribution` | `crates/apex-obs/src/attribution.rs` | `obs::layer_attribution` | G-ALLOC-1 |
| BP-013 | `J(a\|I) = Σ_s P(s\|I,a)·Π(a,s) − C_irrecoverable(a)` (§2) | 3 | `apex-econ::ev` | `src/ev/scenario.rs` | `econ::scenario_ev` | G-ECON-1 |
| BP-014 | Scenario tree covers the 12 named futures (§2) | 9 | `apex-econ::ev` | `src/ev/scenario.rs` | `econ::scenario_set_complete` | G-ADV-1 |
| BP-015 | Robust gate: `J>0` ∧ `Pr(Π>0) ≥ p_min` ∧ acceptable downside; CVaR for high-risk classes (§2.1) | 3 | `apex-econ::eligibility` | `src/eligibility.rs` | `econ::robust_gate` | G-ECON-1 |
| BP-016 | Complete cost identity incl. opportunity cost of scarce resources (§2.2) | 3 | `apex-econ::cost` | `src/cost/mod.rs` | `econ::cost_identity_complete` | G-ECON-1 |
| BP-017 | Candidate eligibility requires all 9 clauses (§2.3) | 3 | `apex-econ::eligibility` | `src/eligibility.rs` | `econ::eligibility_requires_every_clause` | G-ECON-1 |
| BP-018 | **Capture-assurance boundary**: deterministic live dispatch before deadline or an explicit failure state (§2.4) | 6 | `apex-capture` | `crates/apex-capture/src/protocol.rs` | `capture::ticket_always_terminates` | G-CAP-1 |
| BP-019 | The 10 named internal failures are engineering defects, not market failures (§2.4) | 6 | `apex-capture` | `src/{reserve,scheduler,journal}.rs` | chaos suite §29.4 | G-CAP-1 |
| BP-020 | Opportunity Ticket with all 19 fields, created before any live submission work (§2.5) | 6 | `apex-types::ticket` | `crates/apex-types/src/ticket.rs` | `ticket_monotonic` | G-CAP-1 |
| BP-021 | Monotonic ticket status + explicit terminal loss states + hard TTL (§2.5) | 6 | `apex-capture::ticket` | `src/ticket.rs` | `capture::expiry_is_always_explained` | G-CAP-1 |
| BP-022 | Fast path and slow path are both mandatory; slow never delays fast (§2.6) | 2, 8 | `apex-search`, `apex-runtime` | `crates/apex-runtime/src/workers.rs` | `state::fast_path_never_blocks_on_slow_path` | G-CAP-2 |
| BP-023 | Opportunity-coverage auditor with the 7 named metrics and automatic response (§2.7) | 8 | `apex-obs::coverage` | `crates/apex-obs/src/coverage.rs` | `obs::auditor_detects_injected_miss` | G-OBS-1 |
| BP-024 | Resource reservation before signing; multiple signer lanes mandatory (§2.8) | 6 | `apex-capture::reserve`, `::signer` | `src/reserve.rs`, `src/signer/pool.rs` | `capture::reservation_precedes_signing` | G-CAP-1 |
| BP-025 | `SYSTEM_CAPTURE_ASSURANCE` and `MARKET_CAPTURE` reported separately; never conflated (§2.9) | 8 | `apex-obs::metrics` | `crates/apex-obs/src/metrics.rs` | `obs::two_capture_metrics_are_distinct` | G-OBS-1 |
| BP-026 | USD-mark isolation with a bounded `[V_low,V_high]` interval (§2.10) | 3 | `apex-econ`, `apex-types::pnl` | `src/ev/mod.rs` | `econ::usd_mark_cannot_flip_admission` | G-ECON-1 |
| BP-027 | Chain portfolio: Base primary; ETH/BSC first-class; ARB/OP specialized; Unichain research (§3.2) | 14, 15 | `apex-chain` | `crates/apex-chain/src/*/` | `chain::portfolio_membership` | G-CHAIN-1 |
| BP-028 | `ChainScore_c` formula with minimum observation/reliability requirements (§3.3) | 15 | `apex-econ::allocator` | `src/allocator/chain_score.rs` | `econ::chain_score_requires_observations` | G-CHAIN-1 |
| BP-029 | Chain portfolio controller allocates the 7 named resources; hard live-risk floors; shadow-preferred exploration (§3.4) | 15 | `apex-econ::allocator` | `src/allocator/mod.rs` | `econ::allocator_respects_risk_floor` | G-CHAIN-1 |
| BP-030 | `ChainExecutionAdapter` with all 10 methods (§4) | 7 | `apex-chain::adapter` | `crates/apex-chain/src/adapter.rs` | `chain::adapter_contract` | G-BASE-1 |
| BP-031 | Base: preconfirmation-aware sequencing market; documented RPC surface (§4.1) | 7 | `apex-chain::base` | `src/base/` | `chain::base_uses_flashblocks_aware_rpc` | G-BASE-1 |
| BP-032 | Ethereum: private-builder/relay with an empirical bid-to-inclusion model (§4.2, §24.2) | 14 | `apex-chain::ethereum` | `src/ethereum/submit.rs` | `chain::eth_bid_curve_is_empirical` | G-ETH-1 |
| BP-033 | Arbitrum: ordering regime discovered at runtime, never hard-coded (§4.3, §24.3) | 15 | `apex-chain::arbitrum` | `src/arbitrum/mod.rs` | `chain::arb_regime_is_discovered` | G-CHAIN-1 |
| BP-034 | OP: L2 exec + L1 data + priority + failure cost; calldata size is an economic variable (§4.4) | 15 | `apex-chain::optimism`, `apex-econ::cost` | `src/optimism/fee.rs` | `chain::op_fee_model` | G-CHAIN-1 |
| BP-035 | BSC: first-class candidate; adapter discovers 7 named properties; no assumed mempool semantics (§4.5) | 15 | `apex-chain::bsc` | `src/bsc/mod.rs` | `chain::bsc_discovers_regime` | G-CHAIN-1 |
| BP-036 | Unichain: shadow/specialized until density justifies (§4.6) | 15 | `apex-chain` | config | `chain::unichain_is_shadow_only` | G-CHAIN-1 |
| BP-037 | Canonical + speculative state layers (§5.1) | 1 | `apex-state::branch` | `src/branch.rs` | `state::layers` | G-STATE-2 |
| BP-038 | `StateFingerprint` with all 10 fields; prefer stable `diff` fields on Base (§5.2) | 1 | `apex-types::state` | `crates/apex-types/src/state.rs` | `state::fingerprint_complete` | G-STATE-1 |
| BP-039 | Patch engine: classify → identify → apply to immutable snapshot → update closure → publish (§5.3) | 1 | `apex-state::patch` | `src/patch.rs` | `state::patch_pipeline` | G-STATE-1 |
| BP-040 | **No global mutable state as shared truth**; workers get immutable snapshots (§5.3) | 1 | `apex-state::versioned` | `src/versioned.rs` | `state::snapshot_is_immutable` + CI grep | G-STATE-1 |
| BP-041 | Rollback/divergence handling with the 4 named measurements (§5.4) | 1 | `apex-state::branch` | `src/branch.rs` | `state::divergent_branch_rolls_back` | G-STATE-2 |
| BP-042 | Dependency indexes incl. state-version relationships (§5.5) | 1 | `apex-state::dep_index` | `src/dep_index.rs` | `state::dep_closure` | G-STATE-1 |
| BP-043 | Feed integrity: 10 tracked counters; gap → UNSAFE → stop → backfill → verify → resume (§5.6) | 1 | `apex-state::feed::integrity` | `src/feed/integrity.rs` | `state::sequence_gap_marks_unsafe` | G-STATE-1 |
| BP-044 | Two transports where available; disagreement never resolved by vote; only VERIFIED authorizes a ticket (§5.6) | 1 | `apex-state::feed::arbiter` | `src/feed/arbiter.rs` | `state::contradicting_feeds_never_vote`, `state::unsafe_state_is_unconstructible_as_verified` | G-STATE-1 |
| BP-045 | Graph is a candidate generator; `w = −ln(r_eff)`; negative cycles propose (§6.1) | 2 | `apex-search::graph` | `crates/apex-search/src/graph.rs` | existing BF tests, migrated | G-PRICE-1 |
| BP-046 | Finite-size path is authoritative; the 10 named reasons an infinitesimal cycle fails (§6.2) | 3 | `apex-econ::sizing` | `src/sizing/discrete.rs` | `econ::no_profitable_size_returns_none` | G-ECON-1 |
| BP-047 | Pool admissibility: all 11 fields verified; economic threshold, not a magic number (§6.3) | 2 | `apex-venues::registry` | `crates/apex-venues/src/registry.rs` | `venues::a_pool_missing_any_field_is_rejected` | G-VENUE-1 |
| BP-048 | Token admission on 7 measured thresholds; universe stays dynamic (§7.1) | 1 | `apex-state::tokens` | `src/tokens/mod.rs` | `tokens::admission_thresholds` | G-STATE-1 |
| BP-049 | ERC-20 semantics classifier (8 classes); non-standard output from **balance deltas** (§7.2) | 1 | `apex-state::tokens::classifier` | `src/tokens/classifier.rs` | `tokens::nonstandard_uses_balance_deltas` | G-STATE-1 |
| BP-050 | Token risk fingerprint; material change invalidates route assumptions (§7.3) | 1 | `apex-state::tokens::fingerprint` | `src/tokens/fingerprint.rs` | `tokens::code_change_invalidates` | G-STATE-1 |
| BP-051 | Initial Base venue universe (§8.1) | 2, 11 | `apex-venues` | `crates/apex-venues/src/` | `venues::universe` | G-VENUE-1 |
| BP-052 | Venue admission gate: 9 steps; independent circuit breaker per venue (§8.2) | 2 | `apex-venues::{registry,breaker}` | `src/registry.rs`, `src/breaker.rs` | `venues::breaker_is_independent` | G-VENUE-1 |
| BP-053 | `VenueAdapter` with all 6 methods; no hidden economic assumptions (§8.3) | 2 | `apex-venues::adapter` | `src/adapter.rs` | `venues::adapter_contract` | G-VENUE-1 |
| BP-054 | V3 exact engine: required state, 8 outputs, protocol rounding; **no router quote is authoritative** (§9) | 2 | `apex-math::cl` | `crates/apex-math/src/cl/` | `math::three_way_agreement` + CI grep | G-PRICE-1 |
| BP-055 | V4 required state: 12 fields incl. hook permissions and fingerprint (§10.1) | 11 | `apex-venues::univ4::pool_key` | `src/univ4/pool_key.rs` | `univ4::state_complete` | G-V4-1 |
| BP-056 | V4 hook execution model; hooks are part of the state machine (§10.2) | 11 | `apex-venues::univ4::hooks` | `src/univ4/hooks.rs` | `univ4::hook_lifecycle` | G-V4-1 |
| BP-057 | V4 flash accounting: lock/unlock, deltas; **intermediate ≠ settled** (§10.2, §10.4) | 11 | `apex-venues::univ4::flash_accounting` | `src/univ4/flash_accounting.rs` | `univ4::intermediate_delta_is_not_a_settled_balance` | G-V4-1 |
| BP-058 | V4 execution isolation: 6 carried fields; unmodelled hook ⇒ shadow-only (§10.3, §10.4) | 11 | `apex-venues::univ4::fingerprint` | `src/univ4/fingerprint.rs` | `univ4::unmodelled_hook_is_shadow_only` | G-V4-1 |
| BP-059 | Every pricing engine defines the 6-method contract; not proven exact ⇒ candidate-only (§11) | 2 | `apex-math::engine` | `crates/apex-math/src/engine.rs` | `math::every_engine_implements_the_full_contract`, `math::approximate_engine_cannot_authorize` | G-PRICE-1 |
| BP-060 | Engine A: incremental negative-cycle search, Top-K (§12.1) | 2 | `apex-search::engine_a` | `src/engine_a.rs` | migrated BF tests | G-PRICE-1 |
| BP-061 | Engine B (Hermes): accelerator, not authority; 5 tracked metrics; loses authority on repeated misses (§12.2) | gated | `apex-search` | — | gated on `hot_path_recall` | G-OBS-1 |
| BP-062 | Engine C: finite-size route search over the 8 named candidate families (§12.3) | 2 | `apex-search::engine_c` | `src/engine_c.rs`, `src/finite_size.rs` | `search::finds_finite_size_only_opportunity` | G-PRICE-1 |
| BP-063 | Engine D: 8 event template classes (§12.4) | 2 | `apex-search::engine_d` | `src/engine_d.rs`, `src/events/` | `search::event_templates` | G-PRICE-1 |
| BP-064 | Engine E: backrun prediction with **exact** target simulation; target never assumed final (§12.5) | 13 | `apex-search::backrun` | `src/backrun/` | `search::target_not_landing_invalidates_branch` | G-BACKRUN-1 |
| BP-065 | Route topology defaults; **hop count is not the complexity metric** — `ComplexityCost` is (§13) | 2 | `apex-types::candidate` | `crates/apex-types/src/candidate.rs` | `types::complexity_cost_governs` | G-ECON-1 |
| BP-066 | Sizing objective + 8 constraints (§14.1) | 3 | `apex-econ::sizing` | `src/sizing/` | `econ::sizing_respects_every_constraint` | G-ECON-1 |
| BP-067 | Continuous methods are warm starts only (Newton / Brent / bracketed / marginal) (§14.2) | 3 | `apex-econ::sizing::continuous` | `src/sizing/continuous.rs` | `econ::continuous_is_a_warm_start` | G-ECON-1 |
| BP-068 | **Final discrete refinement**: integer/wei truth; continuous is never an execution dependency (§14.3) | 3 | `apex-econ::sizing::discrete` | `src/sizing/discrete.rs` | `econ::continuous_result_cannot_become_a_candidate_input` | G-ECON-1 |
| BP-069 | Parallel-pool split objective; KKT on concave segments as warm start (§15.1) | 10 | `apex-econ::allocation::kkt` | `src/allocation/kkt.rs` | `econ::split_beats_best_single_tier` | G-ALLOC-1 |
| BP-070 | CL boundary segmentation: tick-boundary discovery → piecewise → discrete → simulate (§15.2) | 10 | `apex-econ::allocation::piecewise` | `src/allocation/piecewise.rs` | `econ::piecewise_segments_at_ticks` | G-ALLOC-1 |
| BP-071 | **Shared-pool coupling**: cluster, aggregate, one exact joint transition; never sum independent optima (§15.3) | 10 | `apex-econ::allocation::cluster` | `src/allocation/cluster.rs` | `econ::shared_pool_forces_joint_transition` | G-ALLOC-1 |
| BP-072 | Convex formulation valid only under the 5 named preconditions (§16.1) | 12 | `apex-econ::allocation::certificate` | `src/allocation/certificate.rs` | `econ::shared_pool_coupling_invalidates_certificate` | G-ALLOC-2 |
| BP-073 | Improving-path check; `PROVEN`/`HEURISTIC`/`INVALID_FOR_CERTIFICATION`; never silently promote (§16.2) | 12 | `apex-econ::allocation::improving_path` | `src/allocation/improving_path.rs` | `econ::improving_move_denies_proven` | G-ALLOC-2 |
| BP-074 | Gas-aware allocation rejection inequality (§16.3) | 10 | `apex-econ::allocation` | `src/allocation/mod.rs` | `econ::gas_aware_rejection` | G-ALLOC-1 |
| BP-075 | Candidate portfolio filtering on 5 criteria (§17.1) | 12 | `apex-econ::packing` | `src/packing/mod.rs` | `econ::portfolio_filter` | G-ALLOC-2 |
| BP-076 | Conflict graph with all 9 conflict classes incl. signer/nonce and Flashblock capacity (§17.2) | 12 | `apex-econ::packing::conflict` | `src/packing/conflict.rs` | `econ::conflict_classes_complete` | G-ALLOC-2 |
| BP-077 | Packing rule requires risk-adjusted EV > best alternative + margin, with 6 named effects (§17.3) | 12 | `apex-econ::packing::bounded` | `src/packing/bounded.rs` | `econ::packing_rule` | G-ALLOC-2 |
| BP-078 | Bounded search; **no unrestricted global integer/nonlinear program in the hot path** (§17.4) | 12 | `apex-econ::packing::bounded` | `src/packing/bounded.rs` | `econ::subset_enumeration_is_capped` | G-ALLOC-2 |
| BP-079 | Strategy A — triangular, multiple venue combinations (§18.1) | 8 | `apex-strategy::triangular` | `crates/apex-strategy/src/triangular.rs` | `strategy::triangular` | G-PROD-1 |
| BP-080 | Strategy B — short multi-hop on finite-size net profit (§18.2) | 8 | `apex-strategy::multihop` | `src/multihop.rs` | `strategy::multihop` | G-PROD-1 |
| BP-081 | Strategy C — event-driven backruns, full pipeline (§18.3) | 13 | `apex-search::backrun` | `src/backrun/` | `search::backrun_pipeline` | G-BACKRUN-1 |
| BP-082 | Strategy D — liquidation with 11 protocol details + isolated resource budget (§18.4) | 16 | `apex-strategy::liquidation` | `src/liquidation/` | `strategy::liquidation_eligibility` | G-STRAT-1 |
| BP-083 | Strategy E — correlated/stable; **external venues as signals only**; reject the 5 false-dislocation causes (§18.5) | 16 | `apex-strategy::correlated` | `src/correlated/` | `strategy::external_feed_is_never_truth` | G-STRAT-1 |
| BP-084 | Strategy F — finite-size inventory mismatch (§18.6) | 2 | `apex-search::finite_size` | `src/finite_size.rs` | `search::inventory_mismatch` | G-PRICE-1 |
| BP-085 | `FlashSourceQuote` with all 9 fields (§19.1) | 3 | `apex-types::flash` | `crates/apex-types/src/flash.rs` | `types::flash_quote_complete` | G-ECON-1 |
| BP-086 | Source candidates: Aave V3, Morpho, venue-native, approved future; **address book, not hand-maintained constants** (§19.2) | 3 | `apex-econ::flash`, `apex-config::registry` | `src/flash/mod.rs` | `econ::flash_addresses_are_verified` | G-VENUE-1 |
| BP-087 | Selection rule `argmin(fee + gas + failure risk + availability penalty)` (§19.3) | 3 | `apex-econ::flash::router` | `src/flash/router.rs` | `econ::flash_selection_matches_optimum` | G-ECON-1 |
| BP-088 | Multi-source fallback; contract supports **only approved and tested providers** (§19.4) | 3, 5 | `apex-econ::flash`, `FlashSourceRouter.sol` | `contracts/core/FlashSourceRouter.sol` | `forge testUnapprovedProviderReverts` | G-SOL-1 |
| BP-089 | Tier 0 analytic filter (§20) | 4 | `apex-sim::tier0` | `crates/apex-sim/src/tier0.rs` | `sim::tier0_rejects_without_rpc` | G-SIM-1 |
| BP-090 | Tier 1 exact local state simulation (§20) | 4 | `apex-sim::tier1` | `src/tier1.rs` | `sim::tier1_matches_exact_math` | G-SIM-1 |
| BP-091 | Tier 2 full EVM with the 8 named checks (§20) | 4 | `apex-sim::tier2` | `src/tier2.rs`, `src/backends/` | `sim::tier2_result_complete` | G-SIM-1 |
| BP-092 | Tier 3 adversarial inclusion simulation (§20) | 9 | `apex-sim::adversarial` | `src/adversarial.rs` | `sim::adversarial_scales_margin` | G-ADV-1 |
| BP-093 | Tier 4 controlled production validation; **never a latency technique, never a substitute** (§20) | 8 | `scripts/canary/` | `scripts/canary/run_canary.sh` | process gate | G-PROD-1 |
| BP-094 | `CompetitorModel` with all 10 fields (§21.1) | 9 | `apex-sim::competitor` | `src/competitor/mod.rs` | `sim::competitor_model_complete` | G-ADV-1 |
| BP-095 | Latency buckets are measurement buckets, not promises (§21.2) | 9 | `apex-sim::competitor` | `src/competitor/mod.rs` | `sim::latency_buckets` | G-ADV-1 |
| BP-096 | **Outcome censoring**: never fabricate competitor size from missing observations (§21.3) | 9 | `apex-sim::competitor::censoring` | `src/competitor/censoring.rs` | `sim::censored_observation_does_not_impute_size` | G-ADV-1 |
| BP-097 | Capture curve fitted from observed outcomes (§21.4) | 9 | `apex-sim::competitor::capture_curve` | `src/competitor/capture_curve.rs` | `sim::capture_curve_backtest` | G-ADV-1 |
| BP-098 | Flashblock scheduler state: 9 fields (§22.1) | 7 | `apex-chain::base::flashblock` | `src/base/flashblock.rs` | `chain::scheduler_state_complete` | G-BASE-1 |
| BP-099 | Eligibility from measured `Q(k)`; **no hard-coded one-tenth rule** (§22.2) | 7 | `apex-chain::base::flashblock` | `src/base/flashblock.rs` | `chain::eligibility_uses_measured_model` | G-BASE-1 |
| BP-100 | Ordering lock: no "pay more later, land earlier" (§22.3) | 7 | `apex-chain::base::flashblock` | `src/base/flashblock.rs` | `chain::no_retroactive_flashblock_entry` | G-BASE-1 |
| BP-101 | Backrun timing constrained by the remaining sequencer process (§22.4) | 13 | `apex-search::backrun::timing` | `src/backrun/timing.rs` | `search::backrun_timing` | G-BACKRUN-1 |
| BP-102 | `TotalExecutionCost` with all 12 fields (§23.1) | 3 | `apex-types::cost` | `crates/apex-types/src/cost.rs` | `types::cost_complete` | G-ECON-1 |
| BP-103 | OP Stack / rollup native data-fee modelling, not a generic EVM formula (§23.2) | 3, 15 | `apex-econ::cost::l1_data` | `src/cost/l1_data.rs` | `econ::l1_data_fee_matches_receipts` | G-ECON-1 |
| BP-104 | Calldata optimizer: encoding → size → compression → L1 fee → net EV (§23.3) | 3 | `apex-econ::cost::calldata` | `src/cost/calldata.rs` | `econ::calldata_optimizer` | G-ECON-1 |
| BP-105 | Gas limit (scheduling) and gas used (cost) kept separate (§23.4) | 3 | `apex-types::cost` | `crates/apex-types/src/cost.rs` | `econ::gas_limit_is_not_gas_used` | G-ECON-1 |
| BP-106 | Base submission optimises the 5 named variables; higher fee does not dominate when capacity-ineligible (§24.1) | 7 | `apex-chain::base::submit` | `src/base/submit.rs` | `chain::fee_does_not_beat_ineligibility` | G-BASE-1 |
| BP-107 | Ethereum `EV(b)` with empirical bid curves; multiplex only on incremental value (§24.2) | 14 | `apex-chain::ethereum::builders` | `src/ethereum/builders.rs` | `chain::multiplex_requires_incremental_ev` | G-ETH-1 |
| BP-108 | Arbitrum uses runtime-discovered ordering behaviour (§24.3) | 15 | `apex-chain::arbitrum` | `src/arbitrum/mod.rs` | `chain::arb_regime_is_discovered` | G-CHAIN-1 |
| BP-109 | **No public leakage by default; no probabilistic spam; no uncommitted probes** (§24.4) | 7 | `apex-capture::dispatch` | `src/dispatch/router.rs` | `capture::public_is_not_default` | G-SUB-1 |
| BP-110 | Last-mile dispatch protocol; **no nonessential work between AUTHORIZED and dispatch** (§24.5) | 6 | `apex-capture::protocol` | `src/protocol.rs` | `capture::no_config_read_on_dispatch_path` | G-SUB-1 |
| BP-111 | Per-chain dispatch specifics (Base / Ethereum / Arbitrum / OP / BSC) (§24.5) | 7, 14, 15 | `apex-chain::*::submit` | `src/*/submit.rs` | per-chain submission tests | G-BASE-1, G-ETH-1, G-CHAIN-1 |
| BP-112 | Last-mile revalidation covers all 11 checks; changed input ⇒ re-simulate or invalidate, never blind dispatch (§24.6) | 6 | `apex-capture::revalidate` | `src/revalidate.rs` | `capture::each_revalidation_check_can_reject` | G-SUB-1 |
| BP-113 | Base capture-critical simulation prefers `eth_simulateV1` with explicit state/block controls (§24.6) | 4 | `apex-sim::backends::base_simulate_v1` | `src/backends/base_simulate_v1.rs` | `sim::simulate_v1_sends_explicit_block_and_state_context` | G-SIM-1 |
| BP-114 | Gas-limit minimization as a capture control; no safe limit ⇒ reject before signing (§24.7) | 7 | `apex-chain::base::flashblock` | `src/base/flashblock.rs` | `chain::no_safe_gas_limit_rejects_before_signing` | G-BASE-1 |
| BP-115 | **Acknowledgement is not inclusion**: 7 distinct stages, each with a timeout (§24.8) | 6, 7 | `apex-capture::dispatch::ack` | `src/dispatch/ack.rs` | `capture::ack_does_not_imply_inclusion` | G-SUB-1 |
| BP-116 | Deterministic commitment hash over all 12 critical parameters; executor and signer agree (§25) | 5 | `apex-exec::commitment`, `RouteValidator.sol` | `crates/apex-exec/src/commitment.rs` | `forge testCommitmentMismatchReverts` | G-SOL-1 |
| BP-117 | Executor architecture: small, typed, per-chain, with shared core and adapters (§26) | 5 | `contracts/{core,adapters,chains}/` | as §34.11 | `forge test` suite | G-SOL-1 |
| BP-118 | **No unrestricted calls**: explicit target/selector/pool/token/provider allowlists (§26.1) | 5 | `AdapterRegistry.sol` | `contracts/core/AdapterRegistry.sol` | `forge testNoArbitraryCallSurfaceExists` | G-SOL-1 |
| BP-119 | Route validator: 15 checks before any external call (§26.2) | 5 | `RouteValidator.sol` | `contracts/core/RouteValidator.sol` | `forge` `RouteValidator.t.sol` | G-SOL-1 |
| BP-120 | Multi-asset profit invariant per debt asset + minimum profit (§26.3) | 5 | `ProfitInvariant.sol` | `contracts/core/ProfitInvariant.sol` | `forge testMultiAssetInvariantHolds` | G-SOL-1 |
| BP-121 | Residue policy: zero residue or a declared deterministic path; unaccounted residue is a failure (§26.4) | 5 | `ProfitInvariant.sol` | same | `forge testUnaccountedResidueReverts` | G-SOL-1 |
| BP-122 | Access model; emergency admin separate from routine execution (§26.5) | 5 | `ExecutionAuth.sol` | `contracts/core/ExecutionAuth.sol` | `forge testOnlyExecutorCanStart` | G-SOL-1 |
| BP-123 | Signer roles: execution / emergency / treasury / observer; no unrestricted wallet (§27.1) | 6 | `apex-capture::signer` | `src/signer/mod.rs` | `signer::roles_are_separate` | G-SIGN-1 |
| BP-124 | Nonce manager with the 5 tracked values per chain; prevents worker races (§27.2) | 6 | `apex-capture::nonce` | `src/nonce.rs` | `signer::no_cross_lane_nonce_reuse` | G-SIGN-1 |
| BP-125 | Transaction state machine + 6 failure states (§27.3) | 6 | `apex-capture::ticket` | `src/ticket.rs` | `ticket_monotonic` | G-CAP-1 |
| BP-126 | Replacement only while remaining EV exceeds incremental cost; **no blind gas escalation** (§27.4) | 6 | `apex-capture::dispatch` | `src/dispatch/router.rs` | `capture::no_blind_escalation` | G-SUB-1 |
| BP-127 | Multi-lane signer pool with all 7 hard requirements (§27.5) | 6 | `apex-capture::signer::pool` | `src/signer/pool.rs` | 7 named tests (§18.2) | G-SIGN-1 |
| BP-128 | Pool sizing `N ≥ Q_P99 + margin`; expansion before saturation; quarantine-then-return on failure (§27.6) | 6 | `apex-capture::signer::pool` | `src/signer/pool.rs` | `signer::pool_expands_before_saturation` | G-SIGN-1 |
| BP-129 | Risk is a hard gate; all 14 named triggers (§28) | 6 | `apex-risk::policy` | `crates/apex-risk/src/policy.rs` | `risk::every_trigger_maps_to_a_posture` | G-RISK-1 |
| BP-130 | Graduated response ladder (6 levels) (§28.1) | 6 | `apex-risk::posture` | `src/posture.rs` | `risk::posture_ladder` | G-RISK-1 |
| BP-131 | Loss classification into 9 classes; over-frequency tightens the gate (§28.2) | 6 | `apex-risk::loss` | `src/loss.rs` | `types::loss_class_is_exhaustive` (taxonomy, Phase 0 ✅); `risk::every_loss_is_classified` (runtime, Phase 6) | G-RISK-1 |
| BP-132 | Separate resource classes (9) (§29) | 8 | `apex-runtime::workers` | `crates/apex-runtime/src/workers.rs` | `runtime::resource_classes_isolated` | G-CAP-2 |
| BP-133 | Compute opportunity score `Priority(q)` subject to deadlines (§29.1) | 8 | `apex-econ::compute` | `src/compute/mod.rs` | `econ::priority_is_dollars_per_ms` | G-CAP-2 |
| BP-134 | Budgeting/shedding order when overloaded (§29.2) | 8 | `apex-capture::scheduler` | `src/scheduler.rs` | `capture::shedding_order` | G-CAP-2 |
| BP-135 | Admission control: no unbounded queue; 5 named bounds (§29.3) | 8 | `apex-capture::scheduler` | `src/scheduler.rs` | `capture::no_unbounded_queue` | G-CAP-2 |
| BP-136 | **Queues forbidden on the final execution path**; 5-level priority order; FIFO forbidden (§29.4) | 6 | `apex-capture::scheduler` | `src/scheduler.rs` | `capture::authorized_ticket_never_preempted` | G-CAP-2 |
| BP-137 | `U_capture` SLO with automatic response (§29.5) | 8 | `apex-obs::metrics`, `apex-capture` | `crates/apex-obs/src/metrics.rs` | `capture::u_capture_triggers_response` | G-CAP-2 |
| BP-138 | 7 zero-tolerance counters + 7 hard budgets; breach disables the path (§29.6) | 6 | `apex-capture::registry` | `src/registry.rs` | `capture::hard_zero_counters` | G-CAP-1 |
| BP-139 | Latency decomposed into 8 stages with p50/p90/p99 (§30) | 8 | `apex-obs::metrics` | `crates/apex-obs/src/metrics.rs` | `obs::stage_latency_exported` | G-PERF-1 |
| BP-140 | Latency work approved only on `ΔCaptureEV > ΔInfraCost + ΔComplexityRisk` (§30.1) | 8 | process | `docs/apex/GATES.md` | review gate | G-PERF-1 |
| BP-141 | Infrastructure baseline; Base uses a Flashblocks-aware feed path with external fallback; co-location only on measured benefit (§31) | 7 | `apex-chain::base::feed`, ops | `docs/apex/INFRA.md` | `chain::feed_has_fallback` | G-BASE-1 |
| BP-142 | Observability: state family (8 series) (§32) | 1, 8 | `apex-obs::metrics` | `src/metrics.rs` | `obs::state_family` | G-OBS-1 |
| BP-143 | Observability: search family (7 series) (§32) | 8 | same | same | `obs::search_family` | G-OBS-1 |
| BP-144 | Observability: optimization family (9 series) (§32) | 10 | same | same | `obs::optimization_family` | G-ALLOC-1 |
| BP-145 | Observability: execution family (9 series) (§32) | 7 | same | same | `obs::execution_family` | G-SUB-1 |
| BP-146 | Observability: capture-assurance family (12 series) (§32) | 6 | same | same | `obs::capture_family` | G-CAP-1 |
| BP-147 | Observability: competition family (7 series) (§32) | 9 | same | same | `obs::competition_family` | G-ADV-1 |
| BP-148 | Observability: economics family (11 series) (§32) | 8 | same | same | `obs::economics_family` | G-OBS-1 |
| BP-149 | Attribution: incremental P&L per optimization layer (10 layers) (§32) | 8 | `apex-obs::attribution` | `src/attribution.rs` | `obs::layer_attribution` | G-OBS-1 |
| BP-150 | Missed-opportunity accounting: 17 reason codes + 7 stored fields; counterfactual dataset (§33) | 8 | `apex-obs::miss` | `src/miss.rs` | `obs::every_rejection_path_records_a_miss` | G-OBS-1 |
| BP-151 | Simulation fidelity `F_sim` with the 3 responses; **no live trust from backtests alone** (§34) | 4 | `apex-sim::fidelity` | `src/fidelity.rs` | `sim::fidelity_breach_triggers_action` | G-SIM-1 |
| BP-152 | Venue differential tests: reference vs Rust exact vs forked EVM on 5 dimensions (§35.1) | 2 | `apex-math` tests | `crates/apex-math/tests/differential.rs` | `math::three_way_agreement` | G-PRICE-1 |
| BP-153 | Fuzz the 7 named input spaces (§35.2) | 2, 11 | `apex-math/fuzz`, `apex-venues/fuzz` | `fuzz/fuzz_targets/` | 24 h clean run | G-PRICE-1 |
| BP-154 | Executor invariants: 8 properties proven/tested; formal effort focuses on the Solidity core (§35.3) | 5 | `contracts/core/`, `test/invariant/` | as §34.11 | INV-24…INV-33 | G-SOL-1 |
| BP-155 | Adversarial execution testing: 10 perturbations; fragile candidates need higher margin (§36) | 9 | `apex-sim::adversarial` | `src/adversarial.rs` | `sim::adversarial_scales_margin` | G-ADV-1 |
| BP-156 | Dynamic opportunity surface over 9 dimensions setting 4 dynamic thresholds (§37) | 8 | `apex-econ::ev`, `apex-obs` | `src/ev/mod.rs` | `econ::thresholds_are_dynamic` | G-OBS-1 |
| BP-157 | Profit identity and the > $25k/month target as a measured capacity target (§38) | 8 | `apex-obs::pnl` | `src/pnl.rs` | `obs::monthly_net_identity` | G-PROD-1 |
| BP-158 | Credible-path evidence: 6 conditions; **no assumed trade count / average profit / hero trade** (§38.1) | 8 | process + `apex-obs` | `docs/apex/reports/opportunity-surface-*.md` | G-PROD-1 checklist | G-PROD-1 |
| BP-159 | Production rollout phases (§39) | all | — | §37 of this plan | per-phase gates | all |
| BP-160 | All 36 go/no-go conditions (§40) | 8+ | — | §38.2 of this plan | per-gate scripts | all |
| BP-161 | Engineering priority order (22 items), with submission economics and state fidelity ahead of advanced route mathematics (§41) | all | — | §1.3, §6.3 of this plan | phase ordering | all |
| BP-162 | Exclusions: JIT, bridge, cross-chain, spam, probing, default public leakage, unbounded search, unbounded NLP, unmodelled hooks, arbitrary calls (§42) | 5, 17 | removal | §34.1, §34.11 | `check_no_generic_call.sh`, absence tests | G-SOL-1 |
| BP-163 | Contract-level security: 8 mandatory controls (§43) | 5 | `contracts/core/` | as §34.11 | `forge` suite | G-SOL-1 |
| BP-164 | Adapter-level: 6 independent artifacts per adapter (§43) | 2, 5 | `apex-venues`, `contracts/adapters/` | per-adapter | `venues::breaker_is_independent` | G-VENUE-1 |
| BP-165 | Key management: never combine trading / admin / treasury signers (§43) | 6 | `apex-capture::signer` | `src/signer/mod.rs` | `signer::roles_are_separate` | G-SEC-1 |
| BP-166 | Operational security: credentials externalized; keys never in logs or telemetry (§43) | 0 | `apex-config::secret` | `crates/apex-config/src/secret.rs` | `sec::secret_never_serializes` | G-SEC-1 |
| BP-167 | Failure containment: 11 bounded external-boundary failures; fail closed for capital, open to healthy alternatives (§44) | 6 | `apex-risk`, `apex-chain::rpc` | `crates/apex-risk/src/`, `src/rpc/failover.rs` | chaos suite §29.4 | G-RISK-1 |
| BP-168 | Schema: Candidate (21 fields) (§45) | 0 | `apex-types::candidate` | `crates/apex-types/src/candidate.rs` | `types::candidate_complete` | G-0 |
| BP-169 | Schema: Opportunity outcome (11 fields) (§45) | 8 | `apex-types::pnl`, `apex-obs` | `crates/apex-types/src/pnl.rs` | `types::outcome_complete` | G-OBS-1 |
| BP-170 | Schema: State version (8 fields) (§45) | 1 | `apex-types::state` | `crates/apex-types/src/state.rs` | `state::fingerprint_complete` | G-STATE-1 |
| BP-171 | Schema: Pool state (10 fields) (§45) | 2 | `apex-state::pools` | `crates/apex-state/src/pools.rs` | `venues::a_pool_missing_any_field_is_rejected` | G-VENUE-1 |
| BP-172 | Capture Assurance Controller sits between the risk gate and transaction lifecycle (§46.1) | 6 | `apex-capture` | `crates/apex-capture/src/` | `capture::ticket_always_terminates` | G-CAP-1 |
| BP-173 | The 11-step mandatory capture protocol (§46.1) | 6 | `apex-capture::protocol` | `src/protocol.rs` | `capture::protocol_steps_cannot_be_skipped` | G-CAP-1 |
| BP-174 | Hard capture invariant: exactly one terminal outcome; no crash/timeout/queue may create an unclassified outcome; recovery reconciles from durable state (§46.1) | 6 | `apex-capture::{registry,journal,reconcile}` | `src/journal.rs` | `capture::boot_blocks_dispatch_until_reconciled` | G-CAP-1 |
| BP-175 | Parallel execution, not serial architecture; no artificial serialization of independent tasks (§46.2) | 8 | `apex-runtime::workers` | `crates/apex-runtime/src/workers.rs` | `runtime::independent_tasks_run_concurrently` | G-PERF-1 |
| BP-176 | Precomputed route frontier with 8 carried attributes; events revalue known routes first (§46.3) | 2 | `apex-search::frontier` | `crates/apex-search/src/frontier.rs` | `search::frontier_revalues_first` | G-PRICE-1 |
| BP-177 | Every control-plane transition is observable and idempotent (§46.3) | 6, 8 | `apex-capture`, `apex-obs` | `src/ticket.rs`, `src/metrics.rs` | `capture::transitions_are_idempotent` | G-OBS-1 |
| BP-178 | Adaptive opportunity allocation: `ROI_k` per strategy class, allocated under hard safety constraints (§47) | 15 | `apex-econ::allocator` | `src/allocator/mod.rs` | `econ::roi_allocation` | G-CHAIN-1 |
| BP-179 | Benchmark design: frozen baselines, `ΔRealizedNetUSD/hour` primary, invalid if exposure is not normalized (§48) | 10+ | process + `apex-obs` | `docs/apex/GATES.md` | `obs::benchmark_normalizes_exposure` | G-ALLOC-1 |
| BP-180 | The 14 mandatory interfaces of §51, each unit-testable and independently observable | 0–8 | all crates | §5.4 of this plan | trait conformance tests | all |
| BP-181 | Next-dollar rule governs task selection (§52) | all | process | `PLAN.md` Global Constraints, §1.3 | phase ordering review | all |
| BP-182 | v3→v4 coverage matrix: every preserved concept retained, every excluded one absent (§53) | all | — | §4 of this plan | migration matrix | all |
| BP-183 | Production doctrine DO/DO-NOT list (§54) | all | process | `docs/apex/GATES.md` | review rule | all |
| BP-184 | Success requires the 5-part measured evidence chain (§55) | 8 | `apex-obs` | `docs/apex/reports/` | G-PROD-1 checklist | G-PROD-1 |
| BP-185 | §57.1 mandatory capture protocol as the end-to-end flow | 6–8 | `apex-runtime` + `apex-capture` | §6.2 of this plan | end-to-end integration test | G-CAP-1 |
| BP-186 | §57.1.1 no-loss-of-opportunity invariant: 8 named hard failures | 6 | `apex-capture` | `src/{scheduler,reserve,journal,revalidate}.rs` | chaos suite §29.4 | G-CAP-1 |
| BP-187 | §57.1.2 preemption invariant: authorized live tickets are never preempted | 6 | `apex-capture::scheduler` | `src/scheduler.rs` | `capture::authorized_ticket_never_preempted` | G-CAP-2 |
| BP-188 | §57.1.3 capacity invariant: expand before measured saturation degrades capture assurance | 6 | `apex-capture::signer::pool` | `src/signer/pool.rs` | `signer::pool_expands_before_saturation` | G-CAP-2 |
| BP-189 | §57.1.4 submission invariant: ticket stays open until the next observable state; recovery reconciles after restart/reconnect/replacement/provider failure | 6 | `apex-capture::{dispatch,reconcile}` | `src/dispatch/ack.rs` | `capture::boot_blocks_dispatch_until_reconciled` | G-CAP-1 |
| BP-190 | §57.1.5 external-market boundary: no claim of deterministic 100% market capture | 8 | docs + dashboards | `docs/apex/GATES.md`, `ops/observability/` | `obs::two_capture_metrics_are_distinct` | G-OBS-1 |
| BP-191 | §57.2 architect sign-off checklist (14 items) | 8+ | — | §38.3 of this plan | per-item gate | all |
| BP-192 | §58.1 implement by transforming `arbot-main2`; blank rewrite prohibited | 0 | repository | `git mv` throughout §34 | `git log --follow` check in G-0 | G-0 |
| BP-193 | §58.2 authority hierarchy | all | process | §2.1 of this plan | review rule | all |
| BP-194 | §58.3 every material component classified exactly once (KEEP/ADAPT/REBUILD/REMOVE/UNKNOWN) | 0 | — | §4 of this plan | §40.3 completeness check | G-0 |
| BP-195 | §58.4 repository archaeology is the mandatory first phase | 0 | — | §3, §4 of this plan | `scripts/ci/invariant_coverage.sh` | G-0 |
| BP-196 | §58.5 preserve the 15 named proven asset classes | all | — | §4.8 of this plan | migration matrix | all |
| BP-197 | §58.6 legacy orchestration must not survive by inertia; 10 named categories classified | 0 | — | §4.9 of this plan | migration matrix | G-0 |
| BP-198 | §58.7 controlled migration with differential verification for the 6 high-consequence components | 1–8 | red/blue | §35.2 of this plan | per-subsystem migration gate | all |
| BP-199 | §58.8 Git and rollback doctrine; rollback path retained until the replacement is proven | all | — | §35.3, §35.4 of this plan | `tests/chaos_rollback.rs` | all |
| BP-200 | §58.9 blueprint→PLAN→component→file→test→gate traceability with nothing unassigned | 0 | — | this §40 | `scripts/ci/invariant_coverage.sh` | G-0 |
| BP-201 | §58.10 migration success condition: proven assets + v4 architecture, not "ARBot + features" and not a blank rewrite | 17 | — | §4.8, §4.9, §35.1 of this plan | 7-day A/B vs the pre-removal build | G-RETIRE-1 |
| BP-202 | §59 governing equation: `CapturedNetUSD = AvailableMarketEV × DiscoveryRecall × ExecutionReadiness × ExternalCaptureProbability − TotalExecutionCost`; the first three terms are engineering-controlled and each must be separately measured | 8 | `apex-obs` | `src/coverage.rs`, `src/metrics.rs`, `src/pnl.rs` | `obs::governing_equation_terms_exported` | G-OBS-1 |

## 40.2 Requirements deliberately deferred, with their trigger

Every deferral below is an explicit §52 next-dollar judgement, not an omission.

| BP | Requirement | Why deferred | Trigger to build |
|---|---|---|---|
| BP-061 | Engine B (Hermes) | §52 explicitly deprioritizes a graph algorithm that only raises candidate count | `hot_path_recall < 0.95` sustained and not closed by template expansion (Phase 8 auditor) |
| BP-005, BP-055…058 | Uniswap V4 exact engine | The measured tradeable set is on venues already exactly priced; V4 expands surface but does not unblock the first dollar | Phase 11, after G-PROD-1 |
| BP-092, BP-094…097, BP-155 | Tier 3 adversarial + competitor model | Raises margin quality, not capture ability; the first trade uses a conservative fixed margin instead | Phase 9, after G-PROD-1 |
| BP-069…078 | Parallel split, joint allocation, packing | §39 forbids packing in the first live phase; each requires its own positive-incremental-P&L proof | Phases 10, 12 |
| BP-064, BP-081, BP-101 | Backruns | Needs Tier 2 target simulation and the competitor model | Phase 13 |
| BP-032, BP-033…036, BP-107, BP-108 | Ethereum / BSC / Arbitrum / OP / Unichain | §3 requires measured `ChainScore`, and Base must be profitable first | Phases 14, 15 |
| BP-082, BP-083 | Liquidations, correlated | §39 Phase 7: "only after the core engine is stable and profitable" | Phase 16 |
| C-10 (`ethers`→`alloy`) | Not a blueprint requirement | Zero incremental dollars; §52 forbids prioritizing it | Security advisory against `ethers`, or a blocked chain feature |

## 40.3 Completeness proof

**Method.** Every numbered blueprint section §0–§59 was read in full and decomposed into its normative statements — every "must", "mandatory", "required", "never", "do not", every enumerated list that constrains implementation, and every named structure, formula or metric set. Each produced one or more BP identifiers above.

**Coverage by blueprint section:**

| §§ | Topic | BP range | Unassigned |
|---|---|---|---|
| 0 | v3 audit — the 8 upgrades | BP-001…008 | none |
| 1 | Non-negotiable principles | BP-009…012 | none |
| 2 | Objective function, capture boundary, ticket, fast/slow, coverage, reservation, USD isolation | BP-013…026 | none |
| 3 | Chain portfolio and admission | BP-027…029 | none |
| 4 | Chain execution regimes | BP-030…036 | none |
| 5 | State acquisition, branches, patching, rollback, dep indexes, feed integrity | BP-037…044 | none |
| 6 | Market graph, finite-size warning, pool admissibility | BP-045…047 | none |
| 7 | Token admission, ERC-20 semantics, risk fingerprint | BP-048…050 | none |
| 8 | Venue universe, admission gate, adapter contract | BP-051…053 | none |
| 9 | Uniswap V3 exact engine | BP-054 | none |
| 10 | Uniswap V4 programmable execution | BP-055…058 | none |
| 11 | Pricing engine contract | BP-059 | none |
| 12 | Candidate generation engines A–E | BP-060…064 | none |
| 13 | Route topology and complexity cost | BP-065 | none |
| 14 | Exact route sizing | BP-066…068 | none |
| 15 | Parallel-pool splitting and shared-pool coupling | BP-069…071 | none |
| 16 | Joint allocation and certification | BP-072…074 | none |
| 17 | Cross-cycle portfolio and packing | BP-075…078 | none |
| 18 | Strategy stack A–F | BP-079…084 | none |
| 19 | Flash-liquidity router | BP-085…088 | none |
| 20 | Simulation tiers 0–4 | BP-089…093 | none |
| 21 | Competitor model, buckets, censoring, capture curve | BP-094…097 | none |
| 22 | Base Flashblocks execution model | BP-098…101 | none |
| 23 | Gas and total cost engine | BP-102…105 | none |
| 24 | Submission optimization, last-mile, revalidation, gas-limit, ack ladder | BP-106…115 | none |
| 25 | Transaction commitment and duplication control | BP-116 | none |
| 26 | Solidity executor architecture | BP-117…122 | none |
| 27 | Signer, nonce, lifecycle, signer pool, sizing/rotation | BP-123…128 | none |
| 28 | Risk engine, graduated response, loss containment | BP-129…131 | none |
| 29 | Compute economics, scheduling, utilization, control limits | BP-132…138 | none |
| 30 | Latency architecture and approval rule | BP-139…140 | none |
| 31 | Node and infrastructure architecture | BP-141 | none |
| 32 | Observability families and attribution | BP-142…149 | none |
| 33 | Missed-opportunity accounting | BP-150 | none |
| 34 | Simulation fidelity and calibration | BP-151 | none |
| 35 | Differential testing, fuzzing, executor invariants, formal verification | BP-152…154 | none |
| 36 | Adversarial execution testing | BP-155 | none |
| 37 | Dynamic opportunity surface | BP-156 | none |
| 38 | Profit target framework and evidence rules | BP-157…158 | none |
| 39 | Production rollout | BP-159 | none |
| 40 | Go/no-go gates (36 conditions) | BP-160 (+ §38.2 row-by-row) | none |
| 41 | Engineering priority order | BP-161 | none |
| 42 | Explicit production exclusions | BP-162 | none |
| 43 | Security architecture | BP-163…166 | none |
| 44 | Failure containment | BP-167 | none |
| 45 | Minimum production data schemas | BP-168…171 | none |
| 46 | Search and execution control plane | BP-172…177 | none |
| 47 | Adaptive opportunity allocation | BP-178 | none |
| 48 | Benchmark design | BP-179 | none |
| 49 | Final architecture diagram | — | Non-normative (a picture of BP-001…178); realized by §5, §6 of this plan |
| 50 | Final strategic thesis | — | Non-normative statement of why the above interact |
| 51 | Architect implementation mandate — 14 interfaces | BP-180 | none |
| 52 | Final "next dollar" rule | BP-181 | none |
| 53 | v3 coverage matrix | BP-182 | none |
| 54 | Final production doctrine | BP-183 | none |
| 55 | Final statement / evidence chain | BP-184 | none |
| 56 | Sources and research basis | — | Non-normative bibliography; the substantive claims it supports are carried by BP-031, BP-034, BP-057, BP-086, BP-099, BP-108, BP-113, BP-115 |
| 57 | Final capture-assurance protocol and sign-off | BP-185…191 | none |
| 58 | Implementation baseline and ARBOT migration doctrine | BP-192…201 | none |
| 59 | Final architectural disposition and governing equation | BP-202 | none |

**Result: 202 material requirements identified; 202 assigned to a PLAN phase, repository component, file set, test and acceptance gate. Three sections (§49, §50, §56) are non-normative and carry no independent requirement — their substance is realized by requirements assigned elsewhere, as noted.**

**Converse check (§58.9, second half):** every repository component now has a defensible v4 role or an explicit disposition. §4's matrix assigns exactly one classification to each of the 67 Rust modules, 23 Solidity files, and every configuration, data, script and documentation asset. Components with no v4 role are `REMOVE` with a named phase; components with insufficient evidence are `UNKNOWN` with a named resolving phase (§4.7) and are prohibited from the production path until resolved.

---

# Appendix A — Self-audit

Performed after re-reading the complete blueprint and the complete plan, per the task mandate §23.

| Question | Answer | Where |
|---|---|---|
| Did the plan account for the entire blueprint? | **Yes.** All 59 sections decomposed; 202 requirements assigned; 3 non-normative sections identified as such with their substance traced elsewhere. | §40.3 |
| Did it inspect the actual repository? | **Yes.** 67 Rust modules read at the implementation level (doc headers, public surface, key function bodies), 23 Solidity files, the Foundry suite, configuration, data, scripts, docs, git state, and a measured `cargo check` run. Twelve findings (B-1…B-12) and twelve blueprint/repository conflicts (C-01…C-12) are proved from evidence with file and line references. | §3, §2.2 |
| Did it identify reusable ARBOT assets? | **Yes**, explicitly and as a binding register: any task reimplementing one without a documented v4 incompatibility is a plan violation. | §3.2, §4.8 |
| Did it identify obsolete architecture? | **Yes**, with disposition, unreachability date and deletion date for each of eleven categories. | §3.5, §4.9 |
| Did it preserve proven mathematics? | **Yes.** `cl_math`, `cl_swap`, `cl_ticks`, `quote_univ2`, `quote_solidly`, `math`, `cl_parity_gate` are `KEEP` and moved with `git mv`; the parity gate's doc comment migrates verbatim because it encodes the `0xc211…b3f3` production discovery. | §4.1, §34.4 |
| Did it define the migration? | **Yes.** All seven mandated questions answered; seven red/blue subsystems with named config switches and differential artifacts. | §35.1, §35.2 |
| Did it define exact dependencies? | **Yes.** A cycle-free build graph plus the runtime capture path, with each deviation from blueprint section order justified. | §6 |
| Did it define the fast path? | **Yes.** Fast/slow separation with separate runtimes, the route frontier as the primary structure, and a failure-injection test that the slow path cannot delay the fast one. | §12.1, §12.3 |
| Did it define capture assurance? | **Yes**, as a core dependency built in Phase 6 in shadow before any live capital: ticket, TTL, dispatch deadline, signer/nonce/simulation/submission reservation, priority scheduling, preemption, last-mile revalidation, duplicate suppression, dispatch acknowledgement, recovery, terminal reconciliation. | §16, §17, Phase 6 |
| Did it define the signer/nonce lifecycle? | **Yes.** Roles, multi-lane pool with seven tested hard requirements, per-lane nonce manager adapted from the existing battle-tested one, `Q_P99`-driven sizing, quarantine-and-return rotation, replacement policy with no blind escalation. | §18 |
| Did it define Solidity? | **Yes.** Target contract set, the removal of the arbitrary-call surface (with a test that first *proves* the hole), multi-asset invariant, residue policy, commitment verification, ten named invariants with named `forge` tests, invariant fuzzing, and an external review as a phase exit gate. | §19, §8.4, Phase 5 |
| Did it define chain-specific execution? | **Yes.** A ten-method adapter, Base in depth (Flashblock capacity model, eligibility, ordering lock, gas-limit minimization, ack ladder, `eth_simulateV1`), and Ethereum/BSC/Arbitrum/OP/Unichain with runtime regime discovery. | §20–§23 |
| Did it define testing? | **Yes.** Eleven test classes with locations and cadence, the three-way differential method, nine mandatory failure-injection scenarios, and a measured per-stage latency budget — all defined before implementation. | §29 |
| Did it define rollback? | **Yes.** Trigger, mechanism, and data/config/deployment compatibility for all ten major migrations, plus a chaos test that exercises every config flip under load. | §35.3 |
| Did it define every file-level change? | **Yes.** CREATE/MODIFY/REPLACE/REMOVE/MOVE for every crate, contract, config, data, script and doc asset, using real repository paths throughout. | §34 |
| Did it define measurable acceptance criteria? | **Yes.** Every phase has numeric acceptance and failure criteria and a named exit gate; every gate is a command, not a judgement call; all 36 blueprint conditions map to a gate. | Phases, §38 |
| Did it preserve NEXT DOLLAR > NEXT FEATURE? | **Yes.** It is a Global Constraint, it drives the phase order, it is the stated reason Hermes, V4, Tier 3, packing, backruns and multi-chain are deferred, and it is why the `ethers`→`alloy` migration is explicitly refused with a documented escalation trigger. | Global Constraints, §1.3, §6.3, §36.3, §40.2 |
| Can another engineer implement this without architectural guessing? | **Yes.** Exact type definitions, exact trait signatures, exact file paths, exact test names, exact numeric thresholds, and exact gate commands. Where evidence is insufficient, the component is `UNKNOWN` with a named resolving phase rather than a guess. | §7, §8, §29, §34, §38, §4.7 |

## A.1 Known gaps in this plan, stated rather than hidden

1. **Phase 8's task list is compressed.** Tasks 8.1–8.6 name their failing tests and acceptance criteria but do not expand every TDD step, because their shape is determined by Phases 1–7's concrete interfaces. Expand them at the start of Phase 8, not before.
2. **Phases 9–17 are specified at task granularity, not step granularity.** This is deliberate: they sit behind the first-profitable-trade gate, and their detail depends on what Phases 0–8 measure. Each must be expanded to step granularity before it begins — that expansion is itself the first task of each phase.
3. **Initial performance budgets (§29.5) are derived from the 200 ms Flashblock cadence, not yet measured end-to-end on the v4 path.** They are re-derived after Phase 8, and any change is recorded with its justification.
4. **The signer-pool initial size (4 lanes) is an estimate** from the existing candidate log's concurrency, not a measured `Q_P99` on v4 traffic. It is re-derived weekly from Phase 6 onward.
5. **R-02 is unresolved and is the plan's central commercial risk.** Whether the measured cheap frontier can support $25k/month is not knowable before Phase 8's canary. The plan's response is structural: §38.1 forbids claiming the path on anything but measured realized P&L, and Phases 11/13/14/15/16 are the measured expansion levers if the answer is no.
