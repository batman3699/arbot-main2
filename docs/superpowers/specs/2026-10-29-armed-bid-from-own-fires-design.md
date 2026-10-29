# Armed bids measured at our own fires (Task 8.5 R38, draft 2)

Status: **approved by the operator, 2026-10-29 21:02 UTC** (draft 2), with
two decisions:
- no second shadow-only engine;
- re-judge 10-29 into the fit.

Implementation started on branch `r38-bid-from-fires`.
- Draft 1 had its direction approved conditionally by the R38 profit-capture
  review (`~/Documents/R38_profit_capture_review_2026-10-30.md`).
- This draft takes in the review's four mandatory corrections and its P1/P2
  items. The answers to each are in **Review disposition** at the end.

## Why

Armed-live landed its first two trades on 2026-10-29. The same hour showed
that the bid model measures the wrong contest.

- Each arm bids `best_bid`: the bid `b` maximising `F(b) × (surplus − b·gas)`.
  `F(b)` is the share of the census's measured winners on the route that bid
  below `b` (`targets.json` `bids_wei_per_gas`).
- Those winners are every offset-0 arbitrage on the route, at any gap size.
  The contest an arm actually enters is narrower: the moment its trigger
  fires, against whoever else backruns that trigger.
- The arms bid 0.005 gwei a gas and predict `p_win` 0.28. On the held-out half
  days they won 2–4% of their fires.
- The census's `calibration` checks only that the bid CDF is stable from day
  to day. It never compares a predicted `p_win` with wins.

R37's evaluator fix (a5e11d4, deployed 2026-10-29 19:55 UTC) judges each
fire's contest correctly:
- a trigger that closes its flashblock is contested in the next one, by fee;
- our own landings are recognised;
- every verdict judges the arm's own bid.

That gives the sample this design measures `F` on.

The rule stays measurement-based and per arm: maximise `F × what the win
keeps`. The operator's guidance (2026-10-29) was to retain this approach, not
to fall back to a static percentage tip, and to validate it on held-out
calibration and on the live ledger.

## What it cannot do

Per fire, bidding perfectly is bounded by outbidding each observed rival by
1 wei whenever that still pays.
- Over 2026-10-29 04:33–18:56 UTC that bound is $151.36, about **$250 a day**
  at that pace.
- The R38 rule took $31.04 of it, about $52 a day.
- The $25k/month target is $833 a day: 3.3× the bound, and 16× R38.

Bidding can at best take a larger share of the fires our arms already catch.
Reaching the target needs more fires: more routes, venues and pools in the
targets, and the size cap's skipped arms. That is outside R38.

## The rule

### The sample

Each judged fire yields at most one row. The sample comes from the
evaluator's verdicts (R37 fields) and the arm record written **when the arm
was signed**. Nothing is re-priced from state after the fire.

| Class | Condition | Row |
|---|---|---|
| exact | a rival lands in our earliest flashblock (the trigger's own, or the next one when the trigger closed it) | `c` = the rival's priority fee per gas |
| uncontested | no rival, or the rival lands after our earliest flashblock | `c = 0` |
| censored | the rival is our own send (`ours`), sent at bid `b0` | `c < b0`, value unknown |
| excluded | shadowed; not priced; fire or rival not placed in a flashblock; the rival's transaction unreadable; own-send attribution ambiguous | none; counted by reason |

- **Scale.** A row is stored as the share of the arm's surplus that a bid
  must exceed: `x = c × gas_expected / surplus_wei`. It is a fixed-point
  integer, parts per 10⁹, rounded up. A censored row stores its bound
  `x0 = b0 × gas_expected / surplus_wei` instead.
  - `surplus_wei` is the arm's threshold surplus, already net of base-fee gas
    and the L1 and other costs (`threshold.rs`: `output − size − cost`).
  - A USDC arm's surplus is converted at its recorded `start_per_eth`, exact
    rational, rounded down.
  - An arm without a positive surplus, gas, or rate is excluded.
- **Why share, not per gas.** Across 4,115 contested fires the rival's bid per
  gas rises with our surplus (Spearman 0.65). A pooled per-gas `F` underbids
  big gaps and overbids small ones.
- **At most 1,000 rows.** Past that, neighbouring shares merge into weight
  quantiles, each at its largest share: rivals rounded the stronger way.
  This keeps the engine's pass cheap, since `best_share` runs on every rung.
  It was added in implementation.
- **One fire, one row.** Arms of different routes or rungs fired by the same
  trigger transaction in the same flashblock are one market move. Rows are
  clustered by `(block, trigger tx)` for inference, and the fit counts a
  cluster's rows with weight 1 between them.

### The fit

`F(x)` is the weighted share of rows whose competing share is strictly below
`x`.
- Exact and uncontested rows place their weight at their value.
- **A censored row** is known only to lie below `x0`. The fit computes two
  bounds:
  - lower: the row counts below `x` only when `x > x0`;
  - upper: the row counts at 0.
  - **The fit arms on the lower bound** and reports both. The lower bound is
    the pessimistic one for `p_win`.
  - The report flags any bucket where the two bounds differ by more than 2×
    in `p_win`.
  - Today that is 2 rows of about 4,100. As armed-live lands more, it grows,
    and excluding these rows would bias `F` against exactly the fires our
    bids win.
- **Fit window:** at least three complete UTC days, with a fourth complete
  day held out, untouched. All four must postdate a5e11d4's verdicts, or be
  re-judged by the corrected rule.
- **Pooled first.** Per-route fits come later, only where a route has the
  samples to beat pooled on held-out data.

### The bid (`best_share`)

`best_share(rows, surplus_wei, gas, cap) -> Bid`, in `armed/bid.rs` beside
`best_bid`. All arithmetic is integer: `U256` and checked multiplication.

1. **Candidates.** For each distinct sample value `x`, the smallest integer
   bid per gas whose share exceeds `x`:
   - `b = floor(x × surplus / (gas × 10⁹)) + 1`;
   - raised by 1 while `b × gas × 10⁹ ≤ x × surplus`;
   - plus `b = 0`.
2. **Probability at the exact bid.** For each candidate, `p_win = F(share(b))`
   with `share(b) = b × gas × 10⁹ / surplus`. It is computed at the integer
   bid that would be signed, never at the sample value it came from.
   `rank_value` uses this `p_win` and this `b`.
3. **Keep only bids that pay.** A candidate is dropped unless
   `b ≤ max_priority_wei_per_gas` and `b × gas < surplus`.
4. **Choose** the candidate maximising `F(share(b)) × (surplus − b × gas)`.
   A tie keeps the lower bid.
5. **Fail closed.** These all fall back to `best_bid` and count in the report
   as `bid_model_fallback` by reason:
   - fewer than 300 effective rows (clusters);
   - targets missing, stale or unfit;
   - a malformed or non-finite row;
   - zero gas, zero surplus or an arithmetic overflow.

   The fallback is labelled in every arm (`bid_model: "best_bid (fallback:
   …)"`) as the known-miscalibrated rule, never as a healthy model.

Every arm records `bid_model` and the targets' `model_version`.

## Evidence so far (per fire, paired)

Both rules were scored on the same held-out fires, 2026-10-29, under the
corrected contests. Each was fitted on the other half day.

| Rule | Afternoon (898 fires), fit on morning | Morning (3,254 fires), fit on afternoon |
|---|---|---|
| Current: census winners' `F` | 39 wins, $1.15; predicted p 0.28, realised 0.04 | 73 wins, $5.15; 0.28 / 0.02 |
| `F` from our fires, per gas, pooled | 90, $1.92; 0.09 / 0.10 | 223, $19.36; 0.41 / 0.07 |
| `F` from our fires, per gas, per route | 84, $2.63 | 225, $19.92 |
| **`F` from our fires, as surplus share** | **107, $3.02**; 0.08 / 0.12; 5 losing wins | **259, $28.02**; 0.13 / 0.08; 13 losing wins |
| (reference) static 500‰ | 96, $2.98 | 234, $26.88 |
| (reference) static 900‰ | 213, $6.51; 35 losing wins | 429, $47.16; 98 losing wins |
| (bound) outbid each rival by 1 wei when it pays | 507, $22.38 | 873, $128.98 |

This is preliminary:
- It covers one day, split in two, a quiet afternoon and a busy morning.
- It has no censored rows to speak of, no clustering, and no confidence
  interval.
- It scores per fire, not per portfolio.
- The 37 fires that are in the 4,152 but not in the 4,115 contested rows are
  the uncontested and unwinnable classes. The implementation's report
  reconciles every class.

Valuing a win at a constant multiple of the surplus was tested and rejected.
The fire's gross ran a median 6.5× the surplus in the morning and 2.2× in the
afternoon. A multiple fitted on one half lost $7.05 on the other, with 453 of
529 wins at a loss.

## Validation

### Paired, per fire: both rules judged on every fire

Each arm carries both bids:
- the one sent, from `bid_model`;
- the other rule's, as `alt_bid_wei_per_gas` with its `p_win`.

armed-eval judges both on the same fire, as it now judges `own`. The verdict
gains `alt` beside `own`, and a zero-tip baseline (bid 1 wei) beside the
fractions.

The daily report gives, for each rule:
- wins, net after every cost, and losing wins;
- included reverts;
- predicted against realised `p_win` by bucket, with each bucket's count.
  Buckets under 30 rows are reported but not judged.

It also gives the paired net difference, with a 95% interval from a bootstrap
that resamples whole `(block, trigger tx)` clusters, stratified by route.

### Portfolio: not replayed (operator, 2026-10-29)

`p_win` feeds `rank_value`, so the rule changes which arms occupy the 16
slots. Per-fire pairing judges only the arms the sending rule chose. There is
no engine replay (`armed-replay` re-quotes recorded fires at other sizes),
and the operator declined a second shadow-only engine.

The portfolio effect is therefore seen only in the realised live net after
the switch, against the paired per-fire comparison. That comparison continues
there, with `best_bid` as the alternative on every arm.

### Gates

1. **Data integrity.**
   - Every class in the sample table is counted and reconciles to the
     judged fires.
   - Every row uses the arm's own record.
   - Censoring and clustering have unit tests.
2. **Prediction.**
   - Three fit days, one untouched held-out day.
   - Per bucket with at least 30 rows, realised `p_win` within 2× of
     predicted, for both censoring bounds.
   - Reported by route and by time segment (00–08, 08–16, 16–24 UTC) where
     counts permit.
3. **Economic, per fire.** On the held-out day, `best_share` beats `best_bid`
   on paired net after costs, and the bootstrap's 95% interval excludes zero.
   Losing wins are reported.
4. **Alternative day.** One full UTC day with `best_share` computed as the
   alternative on every live arm and judged paired:
   - the gate 3 result holds on that day;
   - no accounting, state or BlockPI-unit violations;
   - risk caps unchanged.

   Switching the sending rule is then the operator's call.
5. **Live.** `best_share` becomes the sending rule, and `best_bid` becomes
   the alternative on every arm, so the comparison continues.
   - Revert to `best_bid` when two consecutive UTC days, each with at least
     200 judged fire clusters, show a negative paired net or calibration
     outside 2×.
   - A day under 200 clusters decides nothing.
   - Accounting, state or safety failures revert at once, whatever the
     sample.
6. **Scale.** R38 justifies no scaling. Raising funding, the budget or caps
   needs sustained realised net, and evidence that the sender-limited
   portfolio supports the run rate.

## Risks and bounds

- **Rivals adapt.** The fit is at least a day old, and the held-out check and
  the paired live comparison catch drift.
- **Losing wins.** A win whose fire grosses less than its bid's gas. Held-out,
  5 of 107 and 13 of 259. Each counts against the 0.01 ETH daily loss budget,
  which halts the lane when spent. Wins refund nothing (decision 4).
- **Sender balances.** Bids stay below `surplus / gas`, around 1.3e7 wei a gas
  on today's arms, far under the 20 gwei cap. Worst cases rise by at most
  `gas_limit × surplus / gas`, inside the 0.0018 ETH a sender can spend. No
  funding or budget change is part of R38.
- **The band follows the sent bid** (found in implementation).
  - An arm's band pays the tip it sends. A rule bidding higher would have a
    narrower band, and fire on fewer moves.
  - Judging the other rule's bid on the sending rule's fires therefore
    flatters the dearer rule: `best_share` while `best_bid` sends, and the
    reverse after a switch.
  - The census's held-out comparison shares the bias.
  - Only the realised live net after a switch settles it, so the revert rule
    reads that too.
- **Time of day.** The morning's contests were harder than the afternoon's.
  The time-segment calibration shows whether a segmented fit is needed.
- **Denim (~November 2026)** ends flashblocks. The contest classes are
  defined on flashblocks, so on the Denim day:
  - the fit and the evaluator's earliest-position rule must be re-derived on
    200 ms blocks;
  - the targets fall back to `best_bid` until three Denim days are measured.

## Schedule

- 10-29 is re-judged into the fit (the operator, 2026-10-29). The census
  derives every fire's class from:
  - its recorded positions;
  - the flashblock ledger;
  - the live ledger's sent transactions.

  10-29's R37 arms run from 04:33 UTC.
- The fit days are 10-29, 10-30 and 10-31, with 11-01 held out. Gates 1–3
  come at the 11-02 00:20 UTC census.
- The alternative bid runs from the deploy. Gate 4's alternative day is
  11-02, so the switch is the operator's call from 11-03.

## Decided

- Draft 2 approved, with no second engine, and 10-29 re-judged into the fit
  (2026-10-29 21:02 UTC).
- The thresholds as drafted:
  - 300 effective rows;
  - 30 rows a bucket;
  - 200 clusters a day;
  - 2× calibration.

## Review disposition (R38 profit-capture review, 2026-10-30)

- **P0, own landings as censored rows: adopted.** They are kept as `c < b0`.
  The fit arms on the pessimistic bound and reports both, and buckets where
  the bounds diverge are flagged.
- **P0, exact economics at arm time and `p_win` at the integer bid: adopted.**
  Two parts were already true:
  - the arm's surplus is net of base gas and L1 (`threshold.rs`);
  - `best_bid`'s `p_win` is at the exact bid, `b = v + 1`, against "strictly
    below".

  `best_share` keeps the second property through the share-to-wei conversion
  (steps 1–2), and its invariants and fallbacks are listed.
- **P0, paired comparison: adopted.**
  - The draft-1 evidence was already paired on identical fires.
  - Its live rollback, "beat the day before", was not. It is replaced by
    both bids on every arm, judged on the same fire, with a
    cluster-bootstrap interval, a minimum sample, and an immediate safety
    revert.
- **P1, portfolio-level validation: not adopted (the operator declined the
  second engine).** No engine replay exists, so the portfolio effect is seen
  only in the realised live net after the switch.
- **P1, calibration and provenance: adopted.**
  - Three fit days plus one held-out day.
  - Bucket counts, with small buckets suppressed.
  - Route and time segments.
  - Deduplication by `(block, trigger tx)` cluster.
  - Every exclusion counted. The 4,152 against 4,115 gap is the uncontested
    and unwinnable classes.
- **P2, loss controls unchanged, the fallback labelled, no funding change:
  adopted.**
- **The target gap: agreed, and wider than the review's figure.** Perfect
  bidding on the fires our arms already catch is bounded near $250 a day,
  3.3× short of $833. The target needs more fires, not only better bids.
