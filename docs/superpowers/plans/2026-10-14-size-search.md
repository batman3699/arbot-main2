# R27.2 Size Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `apex_math::finite_size::best_size`'s ternary search with a search that keeps a proven upper bound on every region it has not priced, so it stops early with a stated guarantee.

**Architecture:** One loop in `apex-math`: price a size, bound each unpriced region from concavity of the gross and a cost that never falls, price inside the region with the highest bound, stop when none can beat the best found by more than the tolerance. A new defaulted `SizedRoute::rounding` carries each route's integer truncation into the bounds and the tolerance. `apex-runtime` supplies it for live cycles. Neither lane's code changes.

**Tech Stack:** Rust, `ethers_core::types::{U256, I256}`; tests in `crates/apex-math/tests`, `crates/apex-runtime/tests`, `crates/apex-econ/tests`.

**Spec:** `docs/superpowers/specs/2026-10-14-size-search-design.md` (fac16b7).

## Global Constraints

- `best_size`'s signature, `SizedOpportunity`, `NoSize` and `Priced` do not change.
- `SearchBudget` defaults: `max_evaluations` 128, `min_input` 10^12, `start` 10^18, `tolerance_ppm` 1.
- Tolerance: `(max(best, 0) + fixed_cost) × tolerance_ppm / 10^6 + 4 × rounding()`.
- A quote is one `SizedRoute::priced` call made by `best_size`. The search no longer calls `output`.
- Of equal nets, the smaller size is the best.
- Any refusal sets the range's top to one below the refused size, as today.
- Targets, fixed after the first harness run, never loosened to fit: at most 64 quotes on every case, a median of at most 32, a median of at most 8 where no size pays; the median search at least 3 times faster than the old one under the same cost model, the 99th percentile no slower.
- Never `cargo fmt`. `-j 3`. Five gates before a commit; the commit verified in a clean worktree with its own target directory. Stage named paths.
- No new non-secret environment variable.

## File Structure

| File | Change |
|---|---|
| `crates/apex-math/src/finite_size.rs` | `SizedRoute::rounding`; `SearchBudget.start`, `.tolerance_ppm`; the new `best_size` |
| `crates/apex-math/tests/support/ternary.rs` | new: the old search, verbatim, as `ternary_best_size` (comparator; included by path) |
| `crates/apex-math/tests/support/synth.rs` | new: table routes, the exhaustive oracle, a counting wrapper, the generator |
| `crates/apex-math/tests/size_search_oracle.rs` | new: the oracle tests |
| `crates/apex-math/tests/finite_size.rs` | quote-count and comparator assertions on the existing fixtures |
| `crates/apex-runtime/src/live/pricing.rs` | `LiveCycle::mid_unit`, `CostedCycle::rounding` |
| `crates/apex-runtime/tests/live_pricing.rs` | the `SearchBudget` literal; rounding tests; gas monotonicity |
| `crates/apex-econ/tests/discrete.rs` | refine from either search's size |
| `crates/apex-runtime/tests/size_search_harness.rs` | new, `#[ignore]`: both searches over the live universe |
| `PLAN.md`, `ops/host/README.md` | the record |

---

### Task 1: The oracle, the comparator and the failing tests

**Files:**
- Create: `crates/apex-math/tests/support/ternary.rs`, `crates/apex-math/tests/support/synth.rs`, `crates/apex-math/tests/size_search_oracle.rs`

**Interfaces:**
- Produces, in `support/synth.rs`:
  - `pub struct Table { pub gross: Vec<i64>, pub cost: Vec<u64>, pub cap: u64, pub limit: Option<u64>, pub unit: u64, pub rounding: u64 }`: sizes `1..gross.len()`, `gross[0] = 0`; `output(x) = x + gross[x]`, truncated down to a multiple of `unit`; refused above `limit`; `fixed_cost = cost[1]`.
  - `pub fn oracle(t: &Table, lo: u64) -> Option<(u64, i128)>`: every size from `lo` to `min(cap, limit)` priced; the highest net, the smallest size among equals.
  - `pub struct Counting<'a, R>`: wraps a route; `quotes()`, `largest()`.
  - `pub fn generated(seed: u64) -> Table`: concave piecewise-linear gross with up to 8 kinks, slopes from +60 down to −1; up to 12 cost steps; cap and refusal from the same fixed sequence.
  - `pub fn net_of(r: Result<SizedOpportunity, NoSize>) -> Option<(u64, i128)>`.
- Produces, in `support/ternary.rs`: `pub fn ternary_best_size<R: SizedRoute + ?Sized>(route: &R, max_evaluations: u32, min_input: U256) -> Result<SizedOpportunity, NoSize>`, the body of `best_size` as of 27c839c.

- [ ] **Step 1:** Write `support/ternary.rs` by copying the current `best_size` body.
- [ ] **Step 2:** Write `support/synth.rs`.
- [ ] **Step 3:** Write `size_search_oracle.rs` with these tests, each asserting against `oracle`:
  - `the_reviews_counter_example`: gross `0.5x − 0.05x²` scaled to integers over 1..=1000 (peak at 500), cost 0 below 200 and 2,000 units from 200; `tolerance_ppm = 0`, budget 100,000: the net equals the oracle's, at size 199.
  - `a_cost_step_below_inside_and_above_the_peak`, `several_steps`, `a_best_size_one_below_a_step`.
  - `the_best_at_the_cap`, `a_cap_below_start`, `a_refusal_below_at_and_above_the_best`.
  - `a_flat_gross`, `equal_nets_take_the_smaller_size`, `no_gap`, `a_gap_that_pays_less_than_the_fixed_cost`.
  - `generated_routes_match_the_oracle_exactly`: seeds 0..300, five starts each, `tolerance_ppm = 0`.
  - `generated_routes_are_within_the_tolerance`: the default tolerance; `net ≥ oracle − tolerance`.
  - `a_truncated_output_is_within_its_declared_rounding`: `unit = 1000`, `rounding = 2000`.
  - `a_gross_that_is_not_concave_is_still_priced_honestly`: two humps; an `Ok` is the route's own net at that size; quotes ≤ the budget.
  - `the_new_search_is_never_under_the_old_by_more_than_the_tolerance`: every case above.
  - `quotes_and_sizes_stay_small`: quotes ≤ 64 at the default budget; the largest size quoted under 16 times the gross's peak or `4 × start`.
- [ ] **Step 4:** Run `cargo test -j 3 -p apex-math --test size_search_oracle`. Expected: it does not compile (`SearchBudget` has no `start`).
- [ ] **Step 5:** Add the two `SearchBudget` fields and `SizedRoute::rounding` (default zero) with the old body untouched; run again. Expected: the counter-example, the step cases, the quote-count test and the generated exact test FAIL on the old search. Record which.

### Task 2: The search

**Files:**
- Modify: `crates/apex-math/src/finite_size.rs`

**Interfaces:**
- Consumes: Task 1's tests.
- Produces: `best_size` with the new body.

The body, in outline (all arithmetic in `I256`, every overflow read as "no bound"):

```rust
struct Probe { x: U256, output: U256, cost: U256, gross: I256, net: I256 }

// The line through (ax, ag) and (bx, bg), ax < bx, at x: rounded up.
fn line_at(ax: U256, ag: I256, bx: U256, bg: I256, x: U256) -> Option<I256>;

// Regions, low to high, each with its sizes, its bound (None: unbounded) and
// the size to price next:
//   Low  [lo, x0)        gross ≤ max(g0 + r, line((x0, g0 + r), (x1, g1)) at lo)       cost ≥ fixed    next max(x0 / 4, lo)
//   Mid  (xi, xi+1)      gross ≤ min(max(gi + r, line(left, (xi, gi + r)) at xi+1),
//                                    max(gi+1 + r, line((xi+1, gi+1 + r), xi+2) at xi)) cost ≥ ci       next: the midpoint
//   High (xn, top]       gross ≤ max(gn + r, line(left, (xn, gn + r)) at top)           cost ≥ cn       next min(4 xn, top), or the midpoint after a refusal
// `left` of the first probe is the origin (0, 0), exact.
```

- [ ] **Step 1:** Implement `Probe`, `line_at`, the three bounds and the loop:
  1. price `clamp(start, lo, hi)`; while refused, a quarter of it, down to `lo`;
  2. `bar = max(best, 0) + tolerance`; the regions above `bar`: `High` first if it is one, else the highest bound, the higher region among equals;
  3. price its next size; a refusal sets `top = x − 1` and drops the probes above;
  4. stop when no region is above `bar`, or the budget is spent.
- [ ] **Step 2:** Run `cargo test -j 3 -p apex-math`. Expected: every test passes, the existing ones unchanged.
- [ ] **Step 3:** Mutate, one at a time, and confirm a test fails: each bound's `+ r`; `min` → `max` in `Mid`; the cost floor `ci` → `ci+1`; `fixed` → `c0` in `Low`; the stop rule's `>` → `>=`; `High` first removed; `4 ×` → `2 ×`; the tie rule; the refusal rule. Restore, `touch`, rerun.
- [ ] **Step 4:** Commit: `feat(math): a size search that bounds what it has not priced (Task 8.5 R27.2)`.

### Task 3: The lanes' side

**Files:**
- Modify: `crates/apex-runtime/src/live/pricing.rs`, `crates/apex-runtime/tests/live_pricing.rs`, `crates/apex-math/tests/finite_size.rs`, `crates/apex-econ/tests/discrete.rs`

**Interfaces:**
- Produces: `LiveCycle::mid_unit(&self) -> U256` (one unit of the middle token in the input token at the second pool's price, rounded up); `impl SizedRoute for CostedCycle { fn rounding(&self) -> U256 { 2 × mid_unit + 2 } }`.

- [ ] **Step 1:** Failing tests in `live_pricing.rs`: `a_cycles_rounding_is_two_units_of_its_middle_token` (a dollar token through a concentrated pool; a constant-product second pool); `gas_never_falls_as_crossings_rise` (`SettlementGas::estimate` over every venue, direction, 0..40 crossings and words).
- [ ] **Step 2:** Implement; update `budget()` in `live_pricing.rs` to `SearchBudget { max_evaluations: 128, min_input: …, ..SearchBudget::default() }`.
- [ ] **Step 3:** ~~`refine` from either search's size.~~ Dropped while executing: `LiveEconomics::evaluate` does not start refine from `best_size` but from `continuous::optimize`, so nothing in `apex-econ` depends on this change. The harness times that pair instead (Task 4).
- [ ] **Step 4:** In `apex-math/tests/finite_size.rs`: the comparator and quote counts on `cycle(…)` fixtures.
- [ ] **Step 5:** All five gates. Commit: `feat(pricing): a live cycle states its rounding (Task 8.5 R27.2)`.

### Task 4: The harness on the live universe

**Files:**
- Create: `crates/apex-runtime/tests/size_search_harness.rs` (`#[ignore]`, network)

- [ ] **Step 1:** Load the shipped universe's book at a pinned block; build every cycle; for either pool as the trigger and moves of 0, 5, 20, 50, 100, 200 ticks or bps, build the `CostedCycle` under the Morpho cap and the boot cost model.
- [ ] **Step 2:** For each case run both searches (200 repetitions for time): quotes, microseconds, size, net, cost, the largest size quoted, whether the budget ended it. Sweep 64 sizes a route for the assumptions: cost nondecreasing; gross above the line through its neighbours by more than `rounding()`.
- [ ] **Step 3:** Print medians and 90th/99th percentiles, overall, where no size pays, and by venue pair; assert the targets and `new ≥ old − tolerance` on every case.
- [ ] **Step 4:** Run it in release (thin LTO). **Stop and report if a target or the parity is missed.**
- [ ] **Step 5:** Commit: `test(pricing): both size searches over the live universe (Task 8.5 R27.2)`.

### Task 5: Record, verify, deploy, measure

- [ ] **Step 1:** `PLAN.md` R27.2 entry with the harness's figures; `ops/host/README.md` if a status line changes (none expected).
- [ ] **Step 2:** Five gates; commit; clean-worktree verification; merge to `main`; push.
- [ ] **Step 3:** Deploy to the host (a new clock). The Morpho gas figure goes with it: the boot log shows `extra_gas=83000`.
- [ ] **Step 4:** After 30 minutes and again after a candidate sample: received-to-proposed with its count; the armed pass's share and backlog; arms by rung; evaluator agreement. Report against the spec's success criteria.

## Self-Review

- Spec coverage: §1 search → Tasks 1–2; `rounding` → Tasks 1–3; lanes and `refine` → Task 3; harness and targets → Task 4; rollout → Task 5.
- The oracle tests are written before the search and seen to fail on the old one (Task 1, step 5).
- No placeholder: every test is named with what it asserts; the search's bounds and rules are written out.

## Changes found while executing

1. **No floating point.** The first cut priced, between two sizes of equal cost, where the two bounding lines cross. Measured on the eighteen constant-product fixtures it saved nothing over the midpoint (217 quotes either way), so it and its `f64` helpers were deleted.
2. **Both bounding lines stay.** With either alone the same fixtures take over 300 quotes against 198; a test pins the total.
3. **The line is not rounded up.** A gross is a whole number, so the line's whole part bounds it.
4. **A direct test of the bounds** (`no_size_in_a_region_nets_more_than_its_bound`, in the library) was added: the oracle tests alone let several unsound mutants live, because a wrong bound often prunes a region that did not hold the best size anyway.
5. **`rounding` is needed at every line.** Three of the four were shown by routes the search got wrong without it (kept as a test); the fourth only by the direct test.
6. **Dead under A3, deleted:** dropping the priced sizes above a refusal.
7. **`refine` does not start from this search** (see Task 3, step 3).
