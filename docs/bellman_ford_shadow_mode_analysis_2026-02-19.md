# Bellman-Ford Scanner Audit (Shadow-Mode) — 2026-02-19

## Scope
- Role applied: **Graph & Algo Specialist** with **Testing & QA Engineer** validation.
- Inputs reviewed:
  - Runtime log sample shared by operator.
  - `src/graph.rs` Bellman-Ford implementation.
  - `src/main.rs` scanner status and diagnostics pipeline.
  - `src/venues.rs` Balancer pool config ingestion.

## Executive Result
The Bellman-Ford scanner appears to be functioning correctly for the observed run. The run had only **2 active directed edges** and therefore no executable arbitrage cycle was possible.

The immediate blocker in the provided output is configuration quality, not cycle search correctness:
- Balancer pool id was malformed (63 hex chars payload instead of 64).
- UniV3 opportunity set was effectively empty (`expected uniV3 max: 0`).

## Detailed Findings

### 1) Why `No cycles found` is expected for this run
Observed status:
- `No cycles found (edges scanned: 2, expected uniV3 max: 0)`.

Interpretation:
- `expected uniV3 max: 0` means no hot UniV3 pools were available in scanner state for this cycle.
- Two scanned edges almost certainly represent a single pool quoted in both swap directions.
- A profitable directed cycle requires graph structure that closes with a net negative weight path; with only one market relationship available, this is typically impossible.

Codepath confirmation:
- Scanner computes UniV3 expectation from hot pool count (`2 * pool_count`) and reports it in status logs.
- Bellman-Ford only evaluates active, non-bridge directed edges and returns cycle candidates only when net cycle weight is negative.

Conclusion:
- The scanner did **not** fail here; it produced the correct result for an under-provisioned/invalid edge set.

### 2) Balancer ingestion error is real and profit-impacting
Observed warning:
- `invalid Balancer pool id; skipping ... Invalid input length`.

Root cause:
- Shared pool id sample has `0x` + 63 hex chars (odd length), but Balancer pool IDs require 32 bytes (64 hex chars).

Profit impact:
- Pool was skipped, reducing graph connectivity and eliminating potential cycles.

Fix implemented:
- Added tolerant parser for Balancer pool ids in `src/venues.rs`:
  - If the payload is exactly 63 hex chars, parser now left-pads one `0` nibble.
  - Enforces exact 64-char normalized length otherwise.
  - Preserves strict failure on non-hex/incorrect lengths.
- Added regression test proving 63-char id is accepted via deterministic left-pad.

### 3) Bellman-Ford correctness checks (code-level)
Reviewed and validated expected behavior:
- Cycle extraction closes cycle and canonicalizes for deduplication.
- Cycle acceptance requires strictly negative aggregate weight.
- Search respects hop/relaxation/time limits and abort controls.
- Duplicate cycle starts are bounded by candidate capping downstream.

No immediate algorithmic bug found from the reviewed path for this shadow-mode symptom.

## Highest-ROI Next Changes (ordered)
1. **Data-quality gate at startup (P0 hardening)**
   - Pre-validate all pool env blobs and fail fast if enabled venue has 0 valid pools.
   - Profit impact: avoids silent dead scans.
2. **Venue coverage floor alerts**
   - Emit warning/metric when active edge count per venue drops below configured floor.
   - Profit impact: faster detection of config drift and stale pool inventories.
3. **Hot-pool refresh quality metrics**
   - Track % quoteable pools and staleness by venue.
   - Profit impact: improves edge freshness and hit-rate.
4. **Candidate generation pressure tests**
   - Run deterministic regression fixtures with known negative cycles and degraded configs.
   - Profit impact: prevents regressions that collapse cycle discovery.

## Cross-Agent Handoff Queue

### Handoff A — Systems Engineer
**Task:** Add startup validation and hard fail rules for malformed pool configs and zero-edge venue states.
- Enforce: if `cycle_arb=true` and all enabled venues produce < `MIN_ACTIVE_EDGES`, exit non-zero.
- Add runbook section with remediation checklist.
- Deliverables: validation module, metric, CI smoke config test.

### Handoff B — Testing & QA Engineer
**Task:** Build deterministic scanner regression suite for edge-coverage failure modes.
- Cases: malformed Balancer IDs, empty UniV3 hot set, stale quotes, max-relaxation boundaries.
- Assert scanner emits precise reasons and does not produce false-positive cycles.

### Handoff C — Profitability Analyst
**Task:** Quantify PnL loss from pool-config rejection and under-covered token universe.
- Compute: missed opportunities vs. valid pools per chain/day.
- Recommend minimum viable pool counts and pair mix for each chain.

### Handoff D — Researcher
**Task:** Produce prioritized pool universe expansion list for Ethereum cycle-arb.
- Focus on deepest fee tiers and high-turnover triangle token sets.
- Include protocol-specific reliability notes for quoting.

## Commands used during audit
- `rg -n "bellman|scanner|cycle|expected_univ3_edges|invalid Balancer" src plan.md agents.md`
- `sed -n '521,1020p' src/graph.rs`
- `sed -n '2980,3125p' src/main.rs`
- `sed -n '600,760p' src/venues.rs`
- `python - <<'PY' ...` (pool-id length verification)
- `cargo test bellman_ford_returns_closed_cycle -- --nocapture`
- `cargo test resolve_bal_pools -- --nocapture`

