# Arbot — Architecture Pivot Handoff

Written at the end of a long debugging session. Everything below is **measured**,
not assumed; where something is inference it says so. Target: $25k net/month.

---

## 0. Arbot is multi-chain. Base is a config file, not the product.

Every Base-specific number in this document is a **measurement of one venue**,
not a limit of the system. `ops/inputs.yaml` already scaffolds ethereum,
arbitrum, optimism, linea, abstract and ink; a new venue is a config entry plus
a pool inventory, not a rewrite.

This matters because the two hardest constraints found this session are both
**properties of Base**, not of Arbot:

- The ~99.5 bps floor was measured on Base pools against Base competition.
- The "741 broad-scanner bots exited" selection event was caused by **Base
  flashblocks**. Chains without 200ms preconfirmations have not run that filter.

So there are **two independent levers**, and they multiply:

1. **Architecture** — probe → wait (§2). Fixes *how well* we compete.
2. **Venue** — which chain/DEX set we compete in (§2b). Fixes *who we compete
   against*.

Optimising one while holding the other fixed is what consumed this session.

### 0a. Chain selection should be measured, not chosen

The instrumentation built this session is **chain-agnostic** and is exactly the
apparatus for picking a venue on evidence:

| instrument | question it answers |
|---|---|
| shortfall distribution (`cycle output <= input`, bps) | how far from profitable are candidates *here*? |
| rejection taxonomy (7 distinct reasons) | are we blocked by plumbing or by economics? |
| `found_total` cycle-supply census | do opportunities exist at all, and when? |
| `rpc_calls` + `elapsed_ms` per simulation | what does a verdict cost on this provider? |
| head-age (`block head observed`, ms) | how late are we, in this chain's block cadence? |

**Method:** point the harness at a candidate chain for a multi-hour window,
collect the shortfall distribution, and compare. A chain where the best
candidates cluster at −20 bps is a fundamentally different proposition from one
stuck at −99.5 bps. That comparison costs a config file and RPC budget — it is
by far the cheapest high-information experiment available.

Do this **before** committing to the architecture rewrite on any one venue: the
rewrite is worth more on a chain where opportunities exist.

---

## 1. The finding that reframes the project

Peer-reviewed study of cyclic arbitrage **on Base**, June 2025–Feb 2026
([arXiv:2606.00720](https://arxiv.org/html/2606.00720v1)) classifies searchers by
architecture:

| architecture | mechanism | success rate | cost per success |
|---|---|---|---|
| **"Wait"** — off-chain discovery | find offline, simulate, submit pre-committed tx | **42.51%** | 0.000095 ETH |
| On-chain evaluation | confined reads on one intended route | 18.49% | 0.000045 ETH |
| **"Probe"** — on-chain discovery | broad scan, routes found at execution | **4.58%** | **0.000253 ETH** |

**Arbot is the third row.** It scans 584 pools, quotes broadly, discovers routes
at execution time. That design was structurally selected against twice:

- **Flashblocks (Jul 2025):** on-chain discovery fell 63.1% → 25.1% of active
  bots. **741 broad-scanner bots exited.**
- **Fee-floor escalation (Dec 2025–Feb 2026):** further 22.0% → 8.4%.

This independently confirms what was measured locally (§3). We are not narrowly
losing races; we are in a category that has already been cleared out.

---

## 2. The pivot: "probe" → "wait"

**Keep** (all verified sound this session): pool inventory, executor + plan
builder, REVM simulation path, risk policy, flash-loan wiring, rotation, fee
accounting.

**Replace** only the discovery layer:

1. Maintain pool state **off-chain in memory**, updated from the flashblock
   stream (200ms preconfirmations, live on Base since Jul 2025).
2. Compute candidate cycles against **local state — no RPC in the hot loop**.
3. Simulate locally, submit a **pre-committed** transaction.

Prerequisite: **flashblock ingestion**. `newHeads` fires on 2s full blocks and
cannot get below ~300ms (measured floor 294ms, p50 328ms, 0/84 under 200ms).

### 2b. Venue characteristics worth measuring per chain

Not a recommendation — a checklist for the comparison in §0a. Each materially
changes who you compete against:

- **Preconfirmation cadence.** Base has 200ms flashblocks; most chains do not.
  Chains without them have *not* run the filter that removed 741 scanners, so a
  probe-style bot may still be viable there — and a wait-style bot is simply
  ahead.
- **Public mempool.** Base has none (single Coinbase sequencer), so ordering is
  first-to-sequencer. Chains with a public mempool + bundle auctions (Ethereum)
  let you compete on **bid** rather than purely on latency — a different, more
  accessible game for a well-built bot without co-location.
- **Gas cost vs opportunity size.** Sets the minimum profitable trade. Cheap L2s
  admit small edges; Ethereum needs large ones.
- **Venue depth beyond the dominant DEX.** On Base, regenerating three
  inventories found 146 and 162 pools of which **~85% were sub-threshold dust** —
  UniV3 carried the whole graph. A chain with several genuinely deep venues has
  more cross-venue dislocation to capture.
- **Whether MEV recapture is deployed.** Aave's Chainlink SVR recaptures ~73% of
  liquidation MEV and is extending across chains; check per venue before
  assuming a category is open.

### 2c. The optimal formulation: joint convex optimization, not detect-then-size

**This is the target architecture.** Detect-then-size is a decomposition that
throws away optimality at every stage boundary. The academically-optimal
formulation does not separate the stages at all.

Maximum-profit arbitrage across a set of CFMMs is a **single convex program**
(Angeris/Chitra/Evans/Boyd, [arXiv:2204.05238](https://arxiv.org/abs/2204.05238),
ACM EC'22) that solves **routing, sizing and splitting simultaneously and
globally**. Crucially it *subsumes detection*: the paper states the optimal
routing problem "includes as a special case the problem of identifying an
arbitrage present in a network of CFMMs, **or certifying that none exists**."

That means no candidate generation, no false-positive filtering, no
ranking heuristics — it optimises realised profit directly. Everything Arbot
currently does in `graph.rs` + `sizing.rs` + `hot_pools.rs` ranking collapses
into one solve.

#### Three caveats that decide the implementation

**1. Gas makes it mixed-integer convex, NOT convex.**
This is the single most important practical point and it is explicit in the
source: *"When fixed costs are included, the optimal routing problem is a
mixed-integer convex problem, which can be solved using (sometimes slow) global
optimization methods, or approximately solved using various heuristics based on
convex optimization."* Each pool touched costs gas — a fixed charge — which
introduces indicator/cardinality constraints and breaks convexity.
**Practical route:** solve the convex relaxation (fast, globally optimal
ignoring gas), then apply a pool-selection heuristic / cardinality prune, then
re-solve on the chosen support. Do not expect to solve the exact MICP in-block.

**2. Concentrated liquidity must be multi-tick, or the program is wrong.**
UniV3/Slipstream trading functions are piecewise — liquidity is constant within
a tick range and jumps at boundaries. Aggregate concavity survives, so the
convex formulation is still valid, **but only if the model crosses ticks**.
`cl_sim::quote_exact_input_single_tick` holds liquidity CONSTANT (`tick`,
`tick_spacing` are carried but `#[allow(dead_code)]`). Feeding a single-tick
model into the optimiser produces confidently wrong optima.
**This is the same gap that made `min_out` overstate by up to 1070 bps.** A
correct multi-tick CL simulator is therefore the highest-leverage single piece
of math in the codebase: it is a prerequisite for BOTH correct min_out AND the
convex program.

**3. Speed is less of a problem than the literature implies — if scoped.**
The "milliseconds-plus" figure applies to general-purpose solvers over hundreds
of pools. On a *small active subgraph* (a cycle plus its parallel pools, ~5-20
edges) the problem is tiny and highly structured; a hand-rolled Newton /
projected-gradient converges in microseconds and needs no CVXPY-class solver.
Decomposition methods report "significant performance improvements … versus an
off-the-shelf commercial solver" ([arXiv:2302.04938](https://arxiv.org/abs/2302.04938));
reference implementation: [CFMMRouter.jl](https://github.com/bcc-research/CFMMRouter.jl).

#### The production shape (hybrid)

1. **Cheap detection** isolates the *active subgraph* — the handful of pools
   whose state actually moved this flashblock (§2a's per-pool cycle index does
   exactly this).
2. **Convex program** runs only on that subgraph → exact optimal sizing AND
   splitting across parallel pools, handling cross-pool interaction natively.

This strictly dominates the current 1-D ternary search in `optimize_trade_size`,
which optimises a single scalar over one-pool-per-hop and cannot express a split.

#### Splitting and cross-cycle allocation

- **Splitting:** one-pool-per-hop leaves money on the table. Optimal execution
  splits each hop across parallel pools to minimise aggregate price impact. On
  Base, WETH/USDC exists at the 100/500/3000/10000 tiers *simultaneously* — the
  current planner picks one and eats the full impact.
- **Cross-cycle:** when several profitable cycles fire in the same block sharing
  pools or capital, they are **not independent** — executing cycle A moves the
  price cycle B was priced against. That is a portfolio optimisation over a
  shared budget, not N independent maxima. The convex formulation extends to it
  naturally (one program over the union of pools, capital as a constraint).
  **Execution prerequisite:** the executor currently supports a SINGLE flash loan
  and a linear step list (`PlanV2`, `InvalidLoanCount` if `loans.length != 1`).
  Joint multi-cycle execution needs multi-loan support or intra-plan sequencing.

#### Honest scoping

This improves **capture of edge that exists**; it does not create edge. The
measured Base floor was ~99.5 bps underwater — optimal splitting does not close
99.5 bps. So this multiplies with the venue lever (§0), it does not substitute
for it. Build it for a venue where the shortfall distribution shows real
opportunities; it is what converts a thin edge into a captured one.

### 2a. Bellman-Ford is the wrong algorithm for this shape

Current: `bellman_ford` re-runs per scan, O(V·E) per relaxation, rediscovering
graph **structure** every time.

But structure (which tokens connect to which) is nearly static; **state**
(reserves, sqrtPrice, liquidity) changes every 200ms. The "wait" architecture
separates them:

- **Offline / rarely:** enumerate the candidate cycle set from the token graph
  once. Persist it. Refresh only when the pool universe changes.
- **Per flashblock:** re-price the precomputed cycles against updated local
  state. O(cycles touched) rather than O(V·E).
- **Index cycles by pool** so a state update only re-prices the cycles through
  the pools that actually changed — typically a handful per flashblock.

This also removes the failure mode where BF returned `found_total=1` in quiet
markets: cycle *supply* becomes a property of the precomputed set, not of
whether a search happened to converge that scan.

Keep `bellman_ford` behind a flag for offline cycle enumeration; it is a
reasonable generator, just not a per-scan hot-path tool.

---

## 3. What was measured locally (all reproducible)

- **~99.5 bps floor**, immovable across five independent experiments (+55% edges,
  3-hop unlocked, latency 500→328ms, fee-aware ranking, inventory regen).
- **Hand-built 2-hop round trip** through Base's *deepest* WETH/USDC pool
  (0.001 WETH, 5bps tier) still ends below start — traced in the EVM. The final
  `WETH.transfer` repay reverts. **Not a bug: the trade genuinely loses.**
- **Head latency:** min 294ms, p50 328ms, **0 of 84 under 200ms**.
- **Simulation:** p50 179ms / 2 RPC calls after fixes (was 2802ms / 17). Rises to
  p50 670ms / max 924ms post-allowlist, when it executes the full plan.
- **RPC RTT ~240ms** (BlockPI). This is the binding infra constraint; a local
  Base node takes it to ~1ms and makes REVM forking the right architecture again.

---

## 4. Bugs found and fixed this session (all verified, tests green)

| fix | evidence |
|---|---|
| Hot-pool ranking dropped 53% of pools silently | 1511→711; now 584/584 |
| Slipstream `exactInput` selector `0xc04b8d70` (nonexistent) | → `0xc04b8d59`, verified in deployed bytecode |
| Quorum tx had **no gas limit** → every verifier failed `intrinsic gas too low` | `apply_gas_parameters` never applied `gas_limit` |
| Gas estimation retried deterministic reverts | 48 wasted round trips/run → 0 |
| `newHeads` monitor gated behind the **backrun** flag | decoupled; 0→84 head observations |
| REVM fork got `endpoint_label()` (redacted, comma-joined) as its RPC URL | REVM had **never** worked on any provider |
| Unfundable-anchor filter tested `cycle.first()` only | a cycle is a loop — now tests any node |
| `min_out` from linear secant on CL venues | up to **1070 bps** overstatement; now curve-priced |
| Failover retried reverts | 2543 of 3378 rotations wasted |

Net: simulation 15× faster, funnel runs end-to-end, attestation + allowlist green.

---

## 5. ⚠️ Config drift — read this before trusting any config

`ops/inputs.yaml`, `config/registry.json` **and `.env`** revert themselves
between runs. **Seven** settings verified live then found reverted: BlockPI
endpoints, templated credentials, the publicnode URL,
`must_simulate_before_send`, `max_slippage_bps`, the simulation budget, and
`ARBOT_SIM_REVM`.

`ARBOT_SIM_REVM` is worth calling out because the cost is silent and large:
reverted to `1`, simulation goes back to REVM forking over remote RPC — one
`eth_getStorageAt` per storage slot at ~240ms RTT. Measured impact of that one
line: **sim p50 670ms with it on vs 170ms with it off (~4x)**, and at its worst
it produced >8000ms and 100% timeouts. Nothing errors; it just gets slow.

Note the split failure mode: the *code* fix (removing the launcher's
`export ARBOT_SIM_REVM="${ARBOT_SIM_REVM:-1}"`) survived, while the *.env value*
reverted. Code changes have held; config values have not. Verify config, not
just code.

**`must_simulate_before_send` was found flipped to `false`** — that is the last
gate before broadcast, and it disables dispatch safety on a funded run.

Also found: **live BlockPI API key in cleartext** in both tracked files (never
committed; now `${BLOCKPI_KEY}`).

**Action:** commit a known-good config and diff against it before every run.

---

## 6. Open items

1. **Flashblock ingestion** — prerequisite for the pivot.
2. **Cycle-set precompute + per-pool index** — replaces per-scan BF (§2a).
3. **Local Base node** — removes the 240ms RTT ceiling; makes REVM viable.
4. **P0-3: private key** flagged compromised in `docs/VALIDATION_RUN.md`.
   Rotate before funding. Never reuse the old key.
5. `path_tokens` / `fee_tiers` / `block_number` are declared in the candidate log
   schema but never populated at the post-sim call site.
6. `private_relays` for Base are public RPCs (one with a dead `${ALCHEMY_KEY}`) —
   `mode: private` currently buys no MEV protection.

---

## 7. Suggested ordering for the next session

0. **Multi-tick CL simulator** (§2c caveat 2) — unblocks correct `min_out` AND
   the convex optimiser. Highest-leverage single piece of math in the repo; pure
   computation, unit-testable offline against the on-chain quoter, no RPC or
   deploy needed. Start here — it is the only item with no prerequisites.
1. **Chain comparison run** (§0a) — point the existing harness at 2-3 candidate
   venues, collect shortfall distributions. Needs no code changes beyond a config
   file; run it in parallel with everything else.
2. **Flashblock / preconfirmation ingestion** — prerequisite for "wait" on any
   chain that has it. Removes the 240ms RTT ceiling from the critical path.
3. **Cycle-set precompute + per-pool index** (§2a) — replaces per-scan BF and
   produces the active-subgraph isolation the optimiser needs (§2c step 1).
4. **Convex optimiser on the active subgraph** (§2c) — replaces the 1-D ternary
   search in `optimize_trade_size`. Start with the convex relaxation + a
   cardinality prune for gas; do not attempt the exact MICP in-block.
5. **Local node** on whichever chain (1) selects — RTT ~240ms → ~1ms, which also
   makes REVM forking the correct simulation architecture again.
6. **Multi-loan / intra-plan sequencing** in the executor — prerequisite for
   cross-cycle portfolio execution (§2c). Contract change, so batch it with any
   other executor work.
7. Only then: re-tune ranking, budgets, thresholds. Every parameter tuned this
   session was tuned against the wrong architecture on a possibly-wrong venue —
   and most of that tuning surface disappears under §2c anyway.

## 8. Honest position on the target

No evidence was found that **$25k/month is achievable for a new entrant in Base
atomic arbitrage**. The research gives architecture/cost data, not
revenue-per-searcher. The one category with hard revenue numbers — CEX-DEX,
$233.8M across 19 searchers — is ~75% captured by three players and depends on
builder integration.

What *is* supported: the current architecture is measurably the weakest of the
three, the fix is well-defined, and it reuses most of what exists. It moves us
from the category that shed 741 participants into the one that absorbed them.

Liquidations are **not** the answer on these venues: Aave's Chainlink SVR
recaptures ~73% of liquidation MEV to the protocol and is extending to Base.
