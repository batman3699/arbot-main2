# R27.2: a size search that costs what its answer is worth

Task 8.5 R27.2. Status: direction approved by the operator on 2026-10-14;
**revised the same day after the operator's design review**
(`APEX_R27_2_SIZE_SEARCH_DESIGN_REVIEW_2026-10-14.md`), which found the first
draft's search unsafe. "What the review changed" lists each finding and its
answer. The operator's instruction was to consider the review and proceed.

## Goal

Price a route's best size in a fraction of the quotes, never quote a size far
above the answer, and **be able to prove how far the answer can be from the
best size**: something today's search cannot do.

What the search promises, each claim separate:

1. **The net it reports is exact for the size it reports.** Unchanged.
2. **No size nets more than the reported net plus a stated tolerance**, when
   the route's gross is concave and its cost never falls as the size grows.
   Both hold for every venue here. The tolerance includes the route's integer
   rounding, which is stated, not ignored. Today's search makes no such promise
   once costs step.
3. **The reactive lane's executed size** is still
   `apex_econ::sizing::discrete::refine`'s, which is never worse than the size
   it starts from. That is refine's contract and all this document claims for
   it; it is not a proof of the integer optimum.

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
- The prototype (walk up in steps of 4 on the gross from a millionth of a
  WETH, then a golden-section search on the net, to 100 ppm of the size) is
  4.1 to 9.2 times faster than today's search under the Morpho cap, and its
  largest quoted size is 67 to 268 WETH.
- **The prototype is evidence of the cost, not the design.** The review showed
  its search is unsafe, and it had a defect of its own: when the best size is
  the cap, it stopped 100 ppm short and gave up 2 × 10^-5 of the net. The
  design below is a different search; its quote counts are measured in the
  plan before any gate is fixed.
- One route is not the validation. The rollout runs every route of the live
  universe (see Testing).

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

## What the review changed

| Finding | Answer |
|---|---|
| **P0: the gross bracket can leave out the best net size.** A cost step below the gross's peak can put the best net below the bracket. | Correct. The search no longer confines itself to a bracket. Every unpriced size sits in a region with a proven upper bound on its net, and the search ends only when no region can beat the best found (§1). |
| **P0: golden section on the net is unjustified**, the net not being unimodal. | Correct. Nothing in the new search assumes the net has one peak. Concavity is used for the gross alone. |
| **P1: `refine` does not guarantee the integer optimum.** | Correct; the claim is withdrawn (Goal, claim 3). A test holds refine's net from the new search's size at least equal to its net from the old search's. |
| **P1: the old search is not an oracle.** | Correct. The oracle is exhaustive search on bounded integer domains; the old search is kept as a comparator only (Testing). |
| **P1: 24 quotes against the prototype's 26.** | The 26 was the prototype, a different search. Quote counts are now defined, measured first, then fixed as gates (Testing). |
| **P1: ties and flat nets.** | Specified (§1, "Ties"). |
| **P1: one route is not enough.** | The harness runs every cycle of the live universe at a pinned block, with moved pools (Testing). |
| **P2: the Morpho gas figure confounds the experiment.** | The comparison runs both searches in one binary under one cost model. The gas figure has its own tests and its own on-chain proof (27c839c) and ships with this revision; the status line shows it. |
| **P2: `SearchBudget` gains fields.** | Every literal in the workspace is updated (one, in `live_pricing.rs`); no crate outside the workspace uses it. |

## Approaches considered

1. **Search Balancer's range first, Morpho's only if the best is at Balancer's
   edge.** Restores R26.3's cost for small sizes and leaves 128 quotes a
   search. It fixes the regression and nothing else.
2. **Keep the ternary search and stop at a tolerance.** About 70 to 80 quotes,
   the first twenty still at giant sizes, and still no guarantee once costs
   step.
3. **Walk on the gross, then golden section on the net** (the first draft).
   Fast, and unsafe: see the review's two P0 findings.
4. **A search that bounds what it has not priced (chosen).** Described next.

## Design

### 1. The search (`apex_math::finite_size::best_size`)

Same function, same result type, same meaning of every `NoSize`. The body is
replaced.

**What it assumes** of a route, on the sizes the route prices:

- **A1. The gross, `output(x) − x`, is concave, from zero at zero size.** A
  hop's output is a concave, rising function of its input (constant product
  and concentrated liquidity both), and a composition of such functions is
  one.
  - **Up to rounding.** A hop truncates its output, so the quoted gross is a
    sawtooth under the concave curve, at most `rounding()` below it:
    `SizedRoute::rounding`, a new method whose default is zero. A live cycle's
    is two units of its middle token, valued in the input token at the second
    pool's price. That is about 10^-9 WETH through a dollar token and 10^-6
    WETH through an 8-decimal bitcoin token.
- **A2. The cost never falls as the size grows.** Gas is a fixed part a hop,
  plus an amount for every tick crossed and bitmap word stepped, and a larger
  size crosses no fewer. The Morpho lender adds its gas above Balancer's
  holding.
- **A3. A refused size bounds the range:** every larger size is refused too.
  As today.
- **A4. No size costs less than `fixed_cost()`,** the settlement that crosses
  nothing.

**The bounds.** Let the priced sizes be `x_0 < x_1 < … < x_n`, with gross
`g_i` and cost `c_i`. Concavity (A1) puts the gross below the line through any
two priced sizes, outside those two. So for a size `x` between `x_i` and
`x_{i+1}`:

- `g(x) ≤` the line through `x_{i−1}, x_i`, carried right;
- `g(x) ≤` the line through `x_{i+1}, x_{i+2}`, carried left;
- and below `x_0`, or above `x_n`, the one line there is.

The line below `x_0`'s left neighbour is the one through zero, where the
gross is zero. Each line is drawn through its nearer point raised by
`rounding()`, which is the highest the concave curve can be there.

And `c(x) ≥ c_i` by A2, or `fixed_cost()` below `x_0` by A4. A region's bound
is the lower of its gross bounds, less that cost. It is computed in integers
and rounded up. **A cost step inside a region does not weaken the bound:** the
bound charges the whole region the cost of its left end.

**The loop.**

1. Price `start` (clamped into the range).
2. Find the region with the highest bound. If that bound is at most
   `max(best, 0) + tolerance`, stop.
3. Otherwise price one size inside that region, and go to 2:
   - **above the largest priced size:** 4 times it, capped at the range's top
     (or the midpoint, once a refusal has bounded the range);
   - **below the smallest:** a quarter of it, floored at `min_input`;
   - **between two priced sizes of equal cost:** where the two gross lines
     cross, kept a sixteenth of the region away from its ends;
   - **between two of different cost:** the midpoint, which closes in on the
     step.
4. The answer is the best net among the sizes priced: `Ok` for a gain,
   `NoProfitableSize { best }` otherwise, `Unpriceable` if nothing priced, as
   today.

`tolerance = (max(best, 0) + fixed_cost) × tolerance_ppm / 10^6 + 4 ×
rounding()`:

- one part in a million of the net and the fixed cost together, so it is
  defined when the net is zero or negative;
- and four times the route's rounding. The sawtooth is real: a size that
  lands just under a truncation nets up to a unit of the middle token more
  than its neighbour, and no search short of pricing every size can promise
  to find it. The tolerance says so instead of pretending otherwise.
- With `tolerance_ppm = 0` on a route with no rounding, the search runs until
  every region is empty or cannot beat the best: exact, and what the oracle
  tests use.

**What follows, and why.**

- **The guarantee (Goal, claim 2).** When the loop stops by its rule, every
  unpriced size is in a region whose bound is at most `max(best, 0) +
  tolerance`. So no size nets more than that. If it returns
  `NoProfitableSize`, no size nets more than the tolerance.
- **The review's counter-example** (gross peaking at 5, a cost step at 2):
  the region holding the step keeps the low cost of its left end in its bound,
  so it stays above the best found and is halved until the size just under the
  step is priced.
- **A route with no gap is settled from a handful of quotes at sizes of a
  sixteenth of a token and up.** The line through two of them, carried down,
  bounds every smaller size. The dust sizes, where rounding swamps a thin gap,
  are not quoted to reach the verdict.
- **Sizes far above the answer are not quoted.** The walk up stops one step
  past the gross's peak: under 16 times it.
- **The walk up and the walk down are the same rule,** applied to the open
  region at either end.

**Ties.** The best is the highest net, and of equal nets the smallest size: it
costs no more and risks less. A flat gross (output equal to input at every
size) is settled by the bounds like any other: the lines are flat, no region
can beat the best, and the search stops.

**The budget** counts every `priced` call and ends the search where it stands
(§29). The search then returns its best size without the guarantee, as today's
always does. **A quote** in this document is one `SizedRoute::priced` call
made by `best_size`; the reactive lane's near-miss ladder (seven more a route)
is outside it.

**`SearchBudget` gains two fields:**

| Field | Default | Meaning |
|---|---|---|
| `start` | 10^18 | the first size priced: one at which the route's integer arithmetic is exact to well under a basis point |
| `tolerance_ppm` | 1 | the stopping tolerance, in parts per million of the best net plus the fixed cost |

`max_evaluations` (128) and `min_input` (10^12) keep their values and
meanings.

**Why `start` is a whole token.** Every hop rounds its output down by up to a
unit a tick crossed. For an 8-decimal bitcoin token that unit is about
4 × 10^-7 WETH. At a millionth of a WETH the rounding is larger than a thin
gap's gross. At 1 WETH the rounding of two hops is about 0.01 bps of the size.

### 2. The lanes

Neither lane's code changes; both call `best_size` with
`SearchBudget::default()`.

- **Reactive.** Engine C's size is a hint (`size_hint`). The executed size is
  `discrete::refine`'s, which starts at the hint and moves only to better
  sizes.
- **Armed.** `threshold::best_at` takes the search's size as the rung's size.
  The rung's plan, band and surplus are quoted exactly at that size, as today.
- **`NoProfitableSize { best }`** is still the best size priced. With no gap
  it is no longer driven down to the smallest size, since nothing in the
  runtime reads it; the miss ledger and the near-miss figure come from
  elsewhere.

### 3. What ships with it

- **The Morpho gas figure with the WETH dust** (83,000, read at boot), merged
  at 27c839c and not deployed.
- **The harness**, kept as an ignored test: both searches over the live
  universe, in one binary, under one cost model.

## Testing

- **The oracle is exhaustive search.** On integer domains of a few thousand
  sizes, every size is priced and the best net taken. With `tolerance_ppm = 0`
  and a budget that cannot bind, the new search must return **exactly** the
  oracle's net. With the default tolerance, at least the oracle's net less the
  tolerance.
- **The cases, each against the oracle:**
  - the review's counter-example;
  - a cost step below, inside and above the gross's peak; several steps; a
    best size one below a step;
  - the best at the cap; a cap below `start`; a refusal below, at and above
    the best size;
  - a flat gross; equal nets at two sizes; no gap at all; a gap that pays less
    than the fixed cost;
  - a few hundred generated routes: concave piecewise-linear gross with kinks,
    a cost that steps at generated sizes, generated caps and refusals. The
    generator is a fixed sequence, not random at run time.
- **The assumptions are tested where they are made:**
  - `SettlementGas::estimate` never falls when a hop's crossings or words
    rise;
  - on the harness, every route's cost is checked nondecreasing and its gross
    concave across a sweep of sizes, and violations are counted and printed.
- **Outside the assumptions the contract still holds:** on a gross that is not
  concave the search never reports a gain the route does not pay at that size,
  and never exceeds its budget.
- **Rounding:** a route whose output is truncated to a coarse unit, as an
  8-decimal token truncates it, declaring that unit as its `rounding()`: the
  search returns within its tolerance of the oracle, inside its quote budget.
  The same route declaring no rounding shows what the declaration is for.
- **`LiveCycle`'s rounding** is two units of the middle token at the second
  pool's price, for a dollar token and for a constant-product pool.
- **The old search stays in the tests as a comparator:** on every case the new
  net is at least the old net less the tolerance. Agreement with it proves
  nothing by itself.
- **`refine`:** on every case, refine's net from the new search's size is at
  least its net from the old search's, less the tolerance.
- **Existing tests pass unchanged.** `best_size_returns_ok_only_for_a_gain`,
  `a_refused_size_bounds_the_search_rather_than_ending_it`,
  `the_search_respects_its_compute_budget` and
  `engine_c_reports_a_size_not_just_a_verdict` state the contract.
- **Mutation:** each bound, each probe rule, the stop rule and the tie rule.
- **The harness, on the live universe** at a pinned block: every cycle, either
  pool the trigger, the trigger pool moved by 0, 5, 20, 50, 100 and 200 ticks
  or bps. For each search of each case: quotes, time, size, net, cost, the
  largest size quoted, and whether the budget ended it. It prints the median,
  90th and 99th percentile of quotes and of time, by venue pair.
- **Quote counts and time are separate gates,** fixed after the first harness
  run and asserted from then on. The targets the run is held to:
  - quotes: at most 64 on every case (half of today's 128), a median of at
    most 32, and a median of at most 8 where no size pays;
  - time: the median search at least 3 times faster than today's under the
    same cost model, and the 99th percentile no slower;
  - the largest size quoted: under 16 times the size where the gross peaks, or
    `4 × start` when that is below `start`.
  - **If the run misses a target, the work stops and the miss is reported.**
    The target is not loosened to fit.

## Rollout and success

1. The search and its oracle tests in `apex-math`; every literal updated;
   every gate.
2. The harness on the live universe. **Stop if any net is under the old
   search's by more than the tolerance, if an assumption is violated beyond
   rounding, or if a target above is missed.**
3. A commit verified in a clean worktree; merge; the shadow redeploy (a new
   14-day clock).
4. **Success on the host,** measured on the AWS host against R27.1's figures,
   with sample counts:
   - reactive, received to proposed: a median under 15 ms (40 to 62 ms now);
   - armed pass: 99% of slots priced at the book as it stands, and under one
     waiting a pass on average (93 to 95% and 19 to 29 now);
   - the evaluator's simulations of fires still agree with their quotes.
5. **A faster search is not profit.** Nothing here changes a gate, a bid or a
   risk limit, and nothing is sent.

## Risks

- **`rounding()` is an estimate of the sawtooth, not a proof of it.** The
  steps a quote has fully crossed truncate the same way at every nearby size,
  so only the last step's truncation moves with the size: one unit, and two is
  taken. If a route's sawtooth were higher, a bound could be low by the
  difference. The harness measures it: on every route it counts the sizes
  whose gross sits above the line through their neighbours by more than
  `rounding()`.
- **Through an 8-decimal token the tolerance is about 3 × 10^-6 WETH,** under a
  cent. A size that would net less than that may be reported as paying
  nothing. Today's search would find it. Nothing that small survives the risk
  gate's expected-value test.
- **A venue whose gross is not concave, or whose cost can fall.** None here.
  The harness checks both on every route; the contract tests show the search
  stays honest (claim 1) if one ever is.
- **The budget ends a search** on a route with many cost steps near its best
  size. It then returns its best size without the guarantee, as today's search
  always does. The harness counts these.
- **A new clock.** The 14-day run restarts at the deploy.

## Not in R27.2

- **The near-miss ladder** (R14): seven more quotes a route on the reactive
  path. After this change it is most of a no-gap route's pricing. Left as it
  is, and raised separately.
- **`discrete::refine`'s climb** (up to 256 quotes): once per candidate,
  beside a simulation that takes tens of milliseconds.
- **How the armed lane shares its sixteen senders between small and spike
  rungs.** Since R26.3, rungs 3 to 6 hold 71 to 84% of the arms and have not
  fired. A ranking question, raised separately.
- **The profit audit's other findings** (`APEX_PROFIT_AUDIT_2026-10-14.md`):
  the failure cost charged in every scenario, the unmeasured priors, the L1
  fee estimate, a terminal reason on every armed verdict. Each is its own
  change.
