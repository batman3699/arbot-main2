# R27.2: a size search that costs what its answer is worth

Task 8.5 R27.2. Status: direction approved by the operator on 2026-10-14
("go, write the R27.2 spec"); this document is for review before the
implementation plan.

## Goal

Price a route's best size in about a quarter of the quotes, and never quote a
size far above the answer. Both lanes pay for the size search on every route
they price: the reactive lane before it can propose, the armed lane before it
can arm.

The answer does not change in any way that reaches a trade:

- the size found is within 0.01% of the one found today;
- the net it reports is still exact for that size;
- the reactive lane's executed size is still the integer optimum, because
  `apex_econ::sizing::discrete::refine` climbs from the search's size to it
  (INV-18), as it does today.

## Evidence

**The search spends its whole budget, every time.**
`apex_math::finite_size::best_size` is a ternary search over wei from
`min_input` (10^12) to the route's `max_input`. It stops when the range is 2
wei wide, which takes about 110 iterations from 19 WETH, and its budget is 128
quotes, or 64 iterations. So it never converges: **every search costs 128
quotes**, and resolves the size to about 10^-10 WETH.

**R27 moved the top of the range.** `max_input` is the smaller of the lender
cap and the paying pool's holding. With Balancer it was 18.69 WETH. With
Morpho it is the pool's holding: 1,294 WETH on the route measured below. The
first twenty iterations now quote sizes of hundreds of WETH, which cross the
whole tick ladder.

**Measured on real pools** (2026-10-14, release build, the laptop): ticket
51's route (Uniswap WETH/USDC 0.3% then 0.05%) at Base block 52,526,060, with
the first pool moved by a number of ticks to make gaps of different sizes. One
search, in microseconds:

| Gap (ticks moved) | Best size (WETH) | Today, Balancer cap | Today, Morpho cap | Prototype | Quotes, prototype | Net against today's |
|---|---|---|---|---|---|---|
| none pays (−20) | none | 790 | 1,460 | 160 | 26 | same verdict |
| as found (0) | 17.76 | 1,139 | 1,638 | 308 | 39 | −1.0 × 10^-8 |
| +20 | 42.4 | capped at 18.69 | 2,386 | 520 | 41 | −4.8 × 10^-9 |
| +40 | 68.0 | capped | 2,610 | 630 | 41 | +1.5 × 10^-10 |
| +60 | 94.8 | capped | 3,397 | 760 | 39 | −1.3 × 10^-9 |
| +90 | 135.2 | capped | 4,162 | 900 | 39 | −6.4 × 10^-10 |

- Today's search is 128 quotes in every row. Its largest quoted size under
  the Morpho cap is 862.85 WETH, whatever the answer.
- **R27 alone made a search 1.4 to 3.8 times slower** (Balancer cap against
  Morpho cap).
- The prototype (walk up in steps of 4 on the gross, then a golden-section
  search on the net, to 100 ppm of the size) is 4.1 to 9.2 times faster than
  today's search under the Morpho cap, and its largest quoted size is 67 to
  268 WETH.
- **One defect in the prototype, fixed in the design:** when the best size is
  the cap itself, it stopped 100 ppm short of it and gave up 2 × 10^-5 of the
  net. The design quotes the bracket's ends.

**What the cost does on the host** (2026-10-13):

- **Reactive lane.** Received to proposed, the median: 40.4 ms over R26.3's
  167 candidates, 62.3 ms over R27's 12, 52.2 ms over the next build's 3. An
  event prices every template through its pools, about 128 quotes each.
- **Armed lane.** A slot (a cycle with one pool as the trigger) takes 12 to 15
  searches: a doubling and a bisection for the threshold, then five rungs.
  - One thread priced 83 slots a second in R26.3 and 58 in R27, pegged in
    both.
  - R27.1 put the pass on three threads: 95% of the 316 slots are priced at
    the book as it stands, 19 to 29 wait a pass, and bursts back up to 60 and
    more.

## Approaches considered

1. **Search Balancer's range first, Morpho's only if the best is at Balancer's
   edge.** Restores R26.3's cost for sizes within Balancer's holding and
   leaves 128 quotes a search. A size past it pays for both searches. It fixes
   the regression and nothing else.
2. **Keep the ternary search and stop at a tolerance.** About 70 to 80 quotes
   instead of 128, and the first twenty still at giant sizes.
3. **Walk to the answer, then narrow (recommended).** Start at a size where
   the route's arithmetic is exact, walk up in steps of 4 while the gross
   still rises, and search the net inside the bracket that leaves. About 18 to
   30 quotes, none far above the answer. It is the prototype measured above.

## Design

### 1. The search (`apex_math::finite_size::best_size`)

Same signature, same result type, same meaning of every `NoSize`. The body
changes.

**Why the walk is on the gross.** A route's output is a composition of
concave pool curves, so its gross, `output(x) − x`, is concave and has one
peak. Its net is the gross less a cost that steps up with the ticks a size
crosses, so the net can dip and rise again across a step; a walk on the net
could stop at a dip. The cost never falls as the size grows, so the net's best
size is at or below the gross's peak. The walk finds a bracket that holds the
gross's peak, and the net is searched below its top.

**Why the walk starts at a whole token.** Every hop rounds its output down by
up to a unit a tick crossed. For an 8-decimal bitcoin token that unit is about
4 × 10^-7 WETH. At a millionth of a WETH the rounding is larger than a thin
gap's gross, and the gross would look as if it fell. At 1 WETH the rounding of
two hops is about 0.01 bps of the size, below any gap that pays.

**Steps:**

1. **Bounds.** `lo = max(min_input, 1)`, `hi = max_input()`. `RangeEmpty` if
   `hi < lo`, as today.
2. **Walk up on the gross.** From `s = clamp(budget.start, lo, hi)`, quote
   `output` at `s`, `4s`, `16s`, … up to `hi`. Stop at the first size whose
   gross is lower than the one before, or that the route refuses, or at `hi`.
   - A refusal bounds everything above it, as today.
   - The bracket is from the size before the best one to the size that
     stopped the walk. It holds the gross's peak, and its top is under 16
     times that peak.
3. **Walk down on the net**, only when the walk up did not get past `s`: the
   gross at `4s` is lower, or `4s` is refused or past `hi`, so the gross's
   peak is below `4s`. From `s`, price `s/4`, `s/16`, … down to `lo`, while
   the net improves. The bracket is the two sizes either side of the best,
   and `lo` when the best is the last.
   - If the route refuses `s` itself, the walk down starts below it.
   - With no gap at all the net improves all the way down, as it does today:
     ten steps from 1 WETH to `min_input`.
4. **Golden-section search on the net** inside the bracket, until the bracket
   is narrower than the larger of `tolerance_ppm` of its top and `min_input`.
   One new quote an iteration.
   - A refused size bounds the bracket from above.
   - **Both ends of the bracket are priced.** A best size that is the cap is
     then the cap, not a size 100 ppm short of it.
5. **The answer** is the best net among every size priced, as today: `Ok` for
   a gain, `NoProfitableSize { best }` otherwise, `Unpriceable` if nothing
   priced.

**The budget** counts every quote, gross or net, and still ends the search
where it stands (§29). At 128 it is a ceiling the search no longer reaches.

**`SearchBudget` gains two fields:**

| Field | Default | Meaning |
|---|---|---|
| `start` | 10^18 | where the walk begins: a size at which the route's integer arithmetic is exact to well under a basis point |
| `tolerance_ppm` | 100 | the search stops when its bracket is this narrow, as parts per million of the bracket's top |

`max_evaluations` (128) and `min_input` (10^12) keep their values and
meanings. No size is left out of the search: sizes below `start` are reached
by the walk down.

**What 100 ppm costs.** Near its best size the net is flat to first order, so
a size off by a fraction `e` gives up about `e²` of the net: 10^-8 at 100 ppm.
The table's measured figures are 10^-8 to 10^-10.

### 2. The lanes

Neither lane's code changes; both call `best_size` with
`SearchBudget::default()`.

- **Reactive.** Engine C's size is a hint (`size_hint`). The executed size is
  `discrete::refine`'s, which climbs from the hint to the integer optimum, in
  steps that start at a whole token whatever the hint. A test holds the
  refined size equal from either search's hint.
- **Armed.** `threshold::best_at` takes the search's size as the rung's size.
  A rung's size moves by at most 100 ppm, and its plan, band and surplus are
  quoted exactly at that size, as today.

### 3. What ships with it

- **The Morpho gas figure with the WETH dust** (83,000, read at boot), merged
  at 27c839c and not deployed: it goes out with this revision.
- **The timing harness**, kept as an ignored test beside the Morpho proof: the
  table above, reproducible, with the old search as its reference.

## Testing

- **The old search stays, in the tests, as the reference.** For each route in
  a family the new search's net is at least the reference's less one part in a
  million, and it never reports a gain the route does not pay at that size.
- **The families** (deterministic, no randomness at run time):
  - two-venue constant-product cycles across spreads, fees and fixed costs;
  - costs that step with the size (the existing `SteppedCost`), with steps
    larger than a thin gap's gross at small sizes;
  - routes that refuse past a limit, below and above `start`;
  - caps below `start`, at the best size, and far above it;
  - a route whose output is truncated to a coarse unit, as an 8-decimal token
    truncates it, with a 0.1 bps gap: found.
- **Quote counts are asserted**, so a slower search fails a test: at most 48
  on each family, and at most 24 when no size pays.
- **The largest size quoted is asserted:** under 16 times the size where the
  gross peaks, or `4 × start` when that is below `start`.
- **The reactive lane's size is unchanged:** on each family,
  `discrete::refine` from the new search's size and from the reference's give
  the same integer size.
- **Existing tests pass unchanged.** `best_size_returns_ok_only_for_a_gain`,
  `a_refused_size_bounds_the_search_rather_than_ending_it`,
  `the_search_respects_its_compute_budget` and
  `engine_c_reports_a_size_not_just_a_verdict` state the contract; none pins a
  wei-exact size.
- **Mutation:** every stop condition and both walks.
- **On real pools:** the harness's table, with the net never more than one
  part in a million under the reference's.

## Rollout and success

1. The search and its tests in `apex-math`; then every gate.
2. The harness on real pools. **Stop if any net is short by more than one part
   in a million, or the search is not at least 3 times faster.**
3. A commit verified in a clean worktree; merge; the shadow redeploy (a new
   14-day clock).
4. **Success on the host,** against R27.1's figures:
   - reactive, received to proposed: a median under 15 ms (40 to 62 ms now);
   - armed pass: 99% of slots priced at the book as it stands, and under one
     waiting a pass on average (95% and 19 to 29 now);
   - arms' sizes for the same slot and book within 0.01% of the old search's;
   - the evaluator's simulations of fires still agree with their quotes.

## Risks

- **A route whose gross is not concave.** Every venue here is a constant
  product or a concentrated-liquidity curve, and both are concave. A venue
  that is not would need its own search; the reference tests would show it.
- **Rounding at small sizes.** The walk starts at 1 WETH for that reason. A
  best size below it is searched by the walk down and the golden section,
  whose decisions rounding can blur only among sizes whose nets differ by less
  than the rounding itself, as today.
- **The net's cost steps** can still leave the search beside the best size
  rather than on it, as the current search's own comment says. The reported
  net is exact for the size reported.
- **A new clock.** The 14-day run restarts at the deploy.

## Not in R27.2

- **The near-miss ladder** (R14): seven more quotes a route on the reactive
  path, for the status's "closest to paying". After this change it is about a
  quarter of a no-gap route's pricing. Left as it is.
- **`discrete::refine`'s climb** (up to 256 quotes): it runs once per
  candidate, beside a simulation that takes tens of milliseconds.
- **How the armed lane shares its sixteen senders between small and spike
  rungs.** Since R26.3, rungs 3 to 6 hold 71 to 84% of the arms and have not
  fired. That is a ranking question, raised separately.
