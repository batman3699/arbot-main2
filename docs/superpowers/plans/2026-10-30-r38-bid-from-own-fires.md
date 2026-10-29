# R38 bid from our own fires: implementation plan

> For agentic workers: executed inline (superpowers:executing-plans). Steps
> are test first.

**Goal:** arms carry a second bid, `best_share`, fitted by the census on our
own judged fires. It is judged paired against `best_bid` on every fire, and
it becomes the sending rule only by `armed.bid_model: best_share` with a fit
that passed the gates.

**Spec:** `docs/superpowers/specs/2026-10-29-armed-bid-from-own-fires-design.md`
(approved, draft 2).

**Architecture:**
- The census (Python, daily) turns verdicts, arms, flashblock ledgers and the
  live ledger into a weighted share sample. It runs the held-out gates and
  writes `contest` into `targets.json`.
- The engine (Rust) computes both bids per rung.
- The evaluator judges `own`, `alt` and a 1-wei baseline on every fire.

## Global constraints

- Integer arithmetic for every bid and share: Rust `U256` checked; Python
  `int`.
- The share scale is 10⁹ (parts per billion), rows rounded up.
- `F(b)` = weight of rows with `x × surplus < b × gas × 10⁹`, over the total
  weight. Strictly below; a tie keeps the lower bid.
- Thresholds:
  - `MIN_EFFECTIVE` = 300 clusters;
  - calibration buckets of at least 30 rows;
  - 2× calibration;
  - bootstrap 95%, 2,000 resamples, seed fixed;
  - revert sample 200 clusters a day.
- Fail closed to `best_bid`, recording `bid_model: "best_bid (fallback:
  <reason>)"`.
- No funding, budget or cap change. Never `cargo fmt`. Run the five gates
  plus pytest before pushing.

## Tasks

### Task 1: `best_share` and `arm_bid` (armed/bid.rs)

- **`ShareSample { rows: Vec<(u128, u64)>, total: u128, effective: u64,
  fit_ok: bool, model_version: String }`**: rows sorted by `x`.
- **`best_share(sample, surplus_wei: U256, gas: u64, cap: u128) ->
  Result<Bid, &'static str>`**:
  - candidates: for each distinct `x`, `b = x·surplus / (gas·10⁹) + 1`, plus
    `b = 0`;
  - `F` at the exact `b`;
  - drop `b > cap` or `b·gas ≥ surplus`;
  - maximise `below × (surplus − b·gas)`.
  - It returns `Err` on zero gas, zero surplus or an overflow.
- **`BidModel { BestBid, BestShare }`**.
- **`arm_bid(winners, sample: Option<&ShareSample>, model, surplus_wei, gas,
  cap) -> ArmBid { sent: Bid, alt: Option<Bid>, model: String }`**:
  - the alternative exists when the sample has at least `MIN_EFFECTIVE` rows;
  - sending `best_share` also needs `fit_ok`, else the fallback with its
    reason.
- **Tests (`tests/armed_bid.rs`):**
  - maximiser and tie;
  - `p_win` at the integer bid after rounding;
  - the cap and strict surplus;
  - zero gas, zero surplus and overflow;
  - the `arm_bid` matrix;
  - the golden vectors in `tests/data/best_share_vectors.json`, which the
    Python mirror also reads.

### Task 2: `contest` in targets (armed/targets.rs)

- `Targets.contest: Option<ContestSection>`, with fields `model_version`,
  `scale`, `rows`, `effective`, `fit_ok`, `reasons`; the report is ignored.
- Malformed (scale ≠ 10⁹, unsorted, zero weight, empty): the section loads as
  unusable, with a reason, and never fails the targets.
- `Targets::share_sample() -> Result<ShareSample, String>`.
- Tests in `tests/armed_targets.rs`.

### Task 3: engine, config, arm fields

- `armed.bid_model: best_bid | best_share` (default `best_bid`), in
  `shadow/config.rs`.
- `Engine::with_contest(sample, model)`. `bid_and_band` takes `BidInputs {
  winners, sample, model }`, and the rung carries `ArmBid`; ranking uses
  `sent`.
- New `Arm` fields, all `serde` default: `bid_model: Option<String>`,
  `alt_bid_wei_per_gas: Option<String>`, `alt_p_win: Option<f64>`,
  `model_version: Option<String>`.
- Wiring in `shadow/mod.rs`, with a boot log of the model and its fallback
  reason.
- Tests: `tests/armed_bid.rs` (`bid_and_band` with each input) and the
  config's default and parse.

### Task 4: the evaluator judges `alt` and the 1-wei baseline

- `Verdict.alt` and `Verdict.base`, both `Option<OwnBid>`, judged like `own`.
- `Summary.alt` and `Summary.base` by day.
- Tests in `tests/armed_run.rs`.

### Task 5: census `contest.py`

- `classify` → exact / uncontested / censored / excluded(reason), on the
  corrected flashblock rule.
- `arm_surplus_wei`:
  - with `start_per_eth`, exact and rounded down;
  - otherwise the verdict's WETH price, flagged `rate_from_verdict`;
  - WETH as is.
- Rows: ceil share in ppb; censored at `x0` (lower bound) or 0 (upper).
- Clusters `(block, trigger tx)`, with weights `10⁶ // k`.
- `best_share` mirror, checked against the golden vectors.
- Held-out evaluation:
  - both bids per fire (`best_bid` as signed, `best_share` from the fit);
  - outcome per class;
  - net = gross − gas·(base fee + bid) − other;
  - calibration buckets, both bounds;
  - paired net with a cluster bootstrap stratified by route.
- Gates 1–3, then `fit_ok`.
- `contest_section(var, live_ledger, fit_days, held_out_day)`.
- Tests in `test_contest.py`.

### Task 6: wire into targets.py; documentation

- `build`/`main` add `contest` to `targets.json`.
- The fit days are the three complete UTC days before the held-out day, which
  is the last complete day; 10-29 counts.
- `ARMED.md`: the contest sample, `bid_model`, the switch, and the fallback.

### Task 7: dry run on the host's 10-29 data

- Run `contest.py` read-only on the host for 10-29 (fit) and the hours after
  (held-out). Check that the classes reconcile and the numbers agree with the
  analysis in the spec.

### Final

- Five gates plus pytest, then a self-review of the branch (no subagents).
- Ask before merge, push and deploy. The deploy restarts the live canary.
