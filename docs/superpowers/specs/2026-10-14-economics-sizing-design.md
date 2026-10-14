# R27.3: the economics stage sizes a candidate by the certified search

Task 8.5 R27.3. Status: the operator asked for this fix on 2026-10-14 ("fix
economic stage sizing"), after R27.2 found it. Written and built in one
session; this document records the design for review after the fact.

## Goal

A reactive candidate is sized, on the capture path, by the search R27.2
proved, in about 17 quotes instead of 211, and the size it executes is the
one that search priced, exactly.

## Evidence

**The reactive lane sizes every candidate twice, by two different searches.**

- Engine C calls `apex_math::finite_size::best_size` to decide whether a route
  is proposed. R27.2 replaced that search with one that bounds what it has not
  priced.
- The economics stage then sizes the candidate again
  (`LiveEconomics::evaluate`), and its size is the one executed:
  1. `apex_econ::sizing::continuous::optimize`: a golden-section search on the
     **net**, 60 iterations, in `f64`, from the top of the range;
  2. `apex_econ::sizing::discrete::refine_detailed`: a hill climb from the
     nearest whole number, in steps from one token down to one wei, up to 256
     quotes.
- `LiveEconomics` runs the pair **four times a candidate**, once for each
  method of the economics port.

**`optimize` has both faults the operator's review found in R27.2's first
draft.** A net that steps up with gas is not unimodal, so a golden section on
it proves nothing; and a cost step below the gross's peak can leave the best
size outside wherever it converges. The climb that follows is local: it
finds the best size near the start, not the best size.

**Measured** (R27.2's harness, the 1,049 paying cases of the live universe,
release build, the laptop): one pass of the pair takes 211 quotes and 3.1 ms
at the median, 16.7 ms at the 90th percentile and 50 ms at most. Four passes:
about 12 ms at the median and 67 ms at the 90th percentile, and up to 200 ms
on the largest candidates, after a candidate is proposed and before it is
simulated.

**As built, the same cases:** one pass is 17 quotes and 0.24 ms at the median,
1.5 ms at the 90th percentile and 3.9 ms at most. The new pair nets strictly
more on 16 cases and is never under the old by more than the tolerance.

## Design

### 1. `optimize` asks the certified search

`continuous::optimize` keeps its signature and its contract: a warm start for
`refine`, `None` when the route prices nowhere, and nothing about whether the
route pays. Its body becomes `best_size`, with the budget's `min_input` and at
most `iterations + 2` quotes (what the golden section could spend).

- A route that prices but does not pay still has a warm start: the best size
  the search priced.
- `ContinuousOptimum::value` is that size as an `f64`, as before, for
  whoever reports it.

### 2. The start reaches `refine` exactly

An `f64` cannot hold a wei amount: at 18.69 WETH it is quantised to 2,048 wei.
The golden section's answer was approximate anyway, and the climb's sixty
halvings were what recovered a whole number from it. The certified search's
size is already a whole number it priced, and can sit exactly one wei under a
cost step, where 2,048 wei of rounding would cross it.

So `ContinuousOptimum` carries that size beside the float, readable only
inside `apex_econ::sizing` (`pub(super)`), and `refine_detailed` starts there.

- Outside the module the type is what it was: a float and a count. It still
  has no path to a `DiscreteSize`; `refine` is still the only function that
  mints one (INV-18).
- A warm start with no exact size (none is made today) falls back to the
  nearest whole number, as before.

### 3. `LiveEconomics` prices the start and mints it

The live refinement budget becomes one quote: `refine` prices the start and
mints it.

- **A climb from a certified start can gain no more than the search's
  tolerance:** one part in a million of the net and the fixed cost, plus four
  times the route's rounding. R27.2 proved that no size nets more.
- Against that, the climb cost about 120 to 250 quotes, four times over, on
  the capture path.
- `RefineBudget::default()` is unchanged for every other caller.

One pass is then the search (16 quotes at the median on a paying case) and
one more. The four passes stay: each stage must run on its own (§46.2), and
together they are now about 70 quotes.

### What changes for a trade

**The reactive lane's executed size changes.** It was the best whole number
near wherever the golden section stopped. It is now the certified search's
size on the book as the economics stage sees it.

- Where the old pair found the best size, the new size nets at most the
  tolerance less.
- Where a cost step misled the old pair, the new size nets more.
- The expected output the plan is built on is the exact quote at that size, as
  before.

## Testing

- **Against the oracle**, on the table routes R27.2's tests use: the pair
  (`optimize`, then `refine_detailed` at one quote) returns exactly the best
  net of every size, across cost steps, caps and refusals; and no size when
  none pays.
- **The exact start:** a route in wei whose best size, just under a cost
  step, is one an `f64` cannot hold. The refined size is that size to the wei,
  in one quote. This fails if the start passes through the float.
- **The route's rounding reaches the search:** a route whose first quote
  truncates to nothing is still sized.
- **A start outside the range is clamped into it,** never priced outside it.
- **One pass of the stage is counted** through `LiveEconomics` itself: 15
  quotes on a fixture that pays, 5 on one that does not.
- **The warm start is bounded:** at most `iterations + 2` quotes.
- **The old pair is kept in the tests as a comparator** (`tests/support`), and
  on the stepped tables it is shown to fall short where the new pair does
  not.
- **Existing tests pass unchanged**, among them
  `discrete_refinement_is_never_worse_than_the_continuous_optimum`,
  `a_bad_warm_start_does_not_produce_a_bad_trade` and
  `a_continuous_optimum_is_not_a_size`.
- **The harness** times both pairs on the live universe and holds the new
  one's net to the old one's, less the tolerance.

## Rollout and success

1. `apex-econ`, then `LiveEconomics`; every gate.
2. The harness. **Stop if the new pair nets under the old by more than the
   tolerance on any case.**
3. A commit verified in a clean worktree; merge; the shadow redeploy (a new
   14-day clock).
4. **Success on the host:** the reactive lane's picked-to-handled time for
   candidates that reach simulation, against R27.2's run; and no rise in
   Tier 2 refusals for a quote that did not hold (`MinOutNotMet`).

## Risks

- **The executed size moves**, by design, for the first time since the shadow
  began. The quote at it is exact, the simulation still checks it, and
  nothing is sent.
- **The search's assumptions** (a concave gross, a cost that never falls) now
  carry the executed size as well as the proposal. R27.2's harness checks
  both on every route of the universe; a new venue must pass it.
- **Through an 8-decimal token the tolerance is about 3 × 10^-6 WETH,** under a
  cent: a candidate netting less may be sized a little off its best.

## Not in R27.3

- **One pass instead of four.** The four port methods each size the candidate.
  They are cheap now; sharing a result between them is shared state on the
  capture path (INV-11) and its own decision.
- **The failure cost charged twice** in `LiveEconomics::scenarios`, and the
  unmeasured scenario priors.
