# ARBOT — Master Execution Plan (Authoritative)

**Status:** LIVE production system  
**Objective:** reach and sustain **$25,000 net monthly profit** (net of gas + builder/relay fees + failures) by maximizing **realized** EV, not theoretical EV.

This document is the **single source of truth** for all agents. If a change is not in this plan, it does not ship.

---

## 0) Non‑negotiable invariants (profit safety rails)

**Fail closed.** If any required signal is stale/unknown, the engine must **not** broadcast.  
**No negative‑EV execution.** Every candidate must pass deterministic guards:

- `minProfitWei` (after *all* costs, including builder fee)  
- `maxGasWei` (cap) + profit‑aware bidding  
- slippage floors per hop + per route  
- strict token/decimal sanity checks  
- circuit breaker on abnormal revert rate / inclusion drop / RPC lag

**Determinism > cleverness.** Any new logic must be reproducible via fork tests and deterministic simulation.

---

## 1) Target architecture (what “done” looks like)

### Execution pipeline (must be private)
**Scan → Simulate → Size → Bundle → Submit (private) → Settle**

- **Rust engine** builds/updates directed swap graph, finds negative cycles, sizes trade.
- **Simulation layer** validates profitability on forked state (and optionally mempool‑diff state).
- **Bundle layer** submits **only** to private relays/builders (no public mempool).
- **Contracts** execute swaps + flash loan callback, repay, withdraw profit.

### Performance targets (hard)
- **Scan latency:** < **5 ms** p95 per chain (after warm cache)
- **Bundle inclusion:** > **70%** for positive EV bundles (rolling 1h)
- **Revert rate:** < **3%** (rolling 1h)
- **Net EV:** `netWei/day` trending to the $25k/mo target

---

## 2) Workstreams (agents + authoritative prompts)

### Agent A — Rust Core (scanner + graph + caches)
**Mission:** remove crash paths, fix cycle detection correctness, and cut scan latency to <5ms.

**Authoritative prompt (execute exactly):**
1) **Panic purge (mandatory):**
   - Remove `unwrap()` / `expect()` / indexing panics from *all hot paths* (`src/` and runtime `lib/` usage).
   - Replace with `Result` propagation or fast `Option` short‑circuit.
   - Add a “never panic in runtime” CI check: fail build if new unwrap/expect added outside tests.

2) **Cycle detection correctness:**
   - In `src/graph.rs`, implement negative‑cycle detection using `relax_count[v] > n` (or equivalent) **and** maintain `pred[]` for cycle reconstruction.
   - Add fork tests that create a known negative cycle and verify:
     - detection is consistent
     - path reconstruction matches expected token sequence
     - sizing returns a non‑zero profitable size

3) **Incremental graph updates (latency win):**
   - Replace full graph rebuilds with edge‑level updates driven by pool state diffs.
   - Store adjacency lists in a structure optimized for reads (scanner is read‑heavy).
   - Add metrics: `graph_update_ms`, `scan_ms_p50/p95`, `edges_updated`.

4) **Liquidity cache bounded memory:**
   - Implement LRU/TTL eviction (default TTL 30s; configurable).
   - Add metrics: cache hit rate, eviction count.

**Acceptance criteria:**
- `cargo test` + fork tests pass
- p95 scan < 5ms on target chains with realistic universe
- no runtime panics under induced missing pool / stale registry cases

---

### Agent B — Solidity (execution contracts)
**Mission:** ensure flash loan execution is correct, MEV‑safe, and cannot be griefed.

**Authoritative prompt:**
1) **Flash loan correctness audit:**
   - Verify initiation, callback, repayment, and profit extraction for each provider integration.
   - Ensure *only* trusted callback entrypoints can call settle.
   - Confirm profit is measured in a canonical asset and transferred to treasury safely.

2) **Hard security controls:**
   - Add `nonReentrant` to external entrypoints that touch state / transfers.
   - Strict access control:
     - executor role
     - config/registry admin role
   - Enforce slippage/minOut per hop (do not rely on offchain slippage).

3) **Gas & calldata minimization:**
   - Reduce storage reads, pack structs, avoid redundant approvals (use permit2 / max approvals where safe).
   - Emit minimal events (only those required for monitoring and accounting).

**Acceptance criteria:**
- Foundry tests cover: happy path, revert path, insufficient repayment, malicious callback
- No external call allows reentrancy to steal funds
- Slippage protection is enforced onchain

---

### Agent C — Private Execution + MEV (bundle sender + gas strategy)
**Mission:** stop leakage, raise inclusion, and convert theoretical EV into realized EV.

**Authoritative prompt:**
1) **Ban public mempool broadcasting:**
   - Remove/disable any `eth_sendRawTransaction` execution path in production mode.
   - Only allow private bundle submission.

2) **Bundle submission engine:**
   - Implement multi‑relay sending (parallel) with per‑relay health scoring.
   - Implement bundle simulation before submission (local fork), reject on revert.
   - Maintain bundle idempotency (calldata hash) to avoid duplicate losses.

3) **Profit‑aware bidding:**
   - Implement a gas strategy where:
     - `max_total_cost <= profit * cost_share` (default 35%)
     - adaptive based on recent inclusion / competition
   - Record `bid`, `included`, `profit_realized`.

**Acceptance criteria:**
- 0 public broadcasts in logs (hard assertion)
- Inclusion rate improves vs baseline
- Net profit increases (not just gross profit)

- https://www.alchemy.com/docs/chains
- https://www.alchemy.com/docs/reference/mev-protection
- https://www.alchemy.com/docs/node
- https://www.alchemy.com/docs/get-started
  
---

### Agent D — Mempool Advantage (10× profit multipliers)
**Mission:** add the three MEV search primitives that typically multiply arb profits.

**Authoritative prompt:**
1) **State‑diff simulation (“pre‑state arb”):**
   - Subscribe to mempool, apply candidate tx state diffs to a local fork (revm/anvil).
   - Re‑run scan on the *post‑diff* state to find opportunities that do not exist yet.

2) **Backrun engine:**
   - Detect large swaps (dynamic threshold based on pool liquidity; start at ~$100k).
   - Simulate post‑swap state; if profitable, build bundle: `[target_tx, arbot_tx]`.
   - Ensure target tx hash binding in bundle to prevent drift.

3) **Multi‑block simulation (2–3 blocks):**
   - Implement optional multi‑block lookahead for delayed imbalance cases.
   - Gate behind strict EV thresholds (avoid expensive simulation when low EV).

**Acceptance criteria:**
- Each primitive can be toggled independently
- Each has fork tests + replay tests from captured mainnet traces
- Measurable lift in realized profit on at least one primary chain

---

### Agent E — Config Integrity + Observability (the “profit dashboard”)
**Mission:** prevent silent misconfigs and make profit measurable in real time.

**Authoritative prompt:**
1) **Config validator (startup hard‑fail):**
   - Validate token addresses, decimals, fee tiers, factory/router addresses, flash loan provider addresses.
   - Validate RPC URLs present; validate chain id; validate block time assumptions.

2) **Profit accounting:**
   - Persist `gross_profit`, `gas_cost`, `builder_fee`, `reverts`, `net_profit`.
   - Export metrics to Prometheus; build Grafana dashboard:
     - netWei/day by chain
     - inclusion rate
     - revert rate
     - scan latency p95
     - top routes by net profit

3) **Safety automation:**
   - Circuit breaker triggers when:
     - inclusion drops below threshold
     - revert rate spikes
     - RPC lag exceeds threshold
   - When tripped: reduce universe / stop submitting until healthy.

**Acceptance criteria:**
- system refuses to start on invalid config
- dashboards show net profit and health within 60s of runtime
- circuit breaker prevents bleed during bad conditions

---

## 3) Rollout sequence (zero‑downtime shipping)

**Phase 1 — Stop bleeding (immediate)**
1) Agent C: disable public broadcasts + enable private bundles only  
2) Agent A: remove runtime panics in hot path  
3) Agent E: enable circuit breaker + net profit accounting

**Phase 2 — Win rate & latency**
4) Agent A: incremental graph updates + cache eviction  
5) Agent C: profit‑aware bidding + relay health scoring  
6) Agent B: slippage enforcement + reentrancy hardening

**Phase 3 — 10× opportunity capture**
7) Agent D: backrun engine  
8) Agent D: state‑diff simulation  
9) Agent D: multi‑block simulation (gated)

At the end of each phase, compare **netWei/day** to baseline. If net drops, rollback.

---

## 4) Chain focus (maximize ROI now)

**Primary:** Ethereum, Arbitrum, Base  
**Secondary:** Optimism, Linea, Scroll, Mantle  
Disable low‑density chains until primary chains hit profitability targets.

---

## 5) Definition of Done (profit, not code)

Ship only when these are true (rolling 24h):

- net profit trending to **≥ $25k/month**
- inclusion rate **≥ 70%**
- revert rate **≤ 3%**
- scan latency p95 **≤ 5ms**
- 0 public mempool broadcasts

---

## Appendix — Commands (standard)

Rust:
- `cargo fmt && cargo clippy -- -D warnings && cargo test`
- fork tests via your existing harness (add if missing)

Solidity:
- `forge fmt && forge test -vvv`

Ops:
- production run must enable:
  - private bundles only
  - strict guards
  - metrics export

---

## Appendix — Profit gates (recommended defaults)

- `minProfitWei`: set per chain (start conservative; auto‑raise on competition)
- `cost_share`: 0.35 (profit share to gas+builder)
- `max_hops`: 4–6 (cap for latency; expand only if profitable)
- `pool_liquidity_floor_usd`: 100k (increase if latency high)

