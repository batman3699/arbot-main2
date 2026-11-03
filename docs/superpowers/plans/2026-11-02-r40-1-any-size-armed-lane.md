# R40.1 Any-size armed lane Implementation Plan

> **For agentic workers:** executed inline (superpowers:executing-plans), then
> a self-review of the whole branch (no subagents: the operator's standing
> rule). Steps are test first and use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Realised net goes up: standing arms on the trigger legs worth holding,
each traded at its best size, computed on chain at execution. Revised
2026-11-03 after an independent review: build now, launch on the evidence
already held, and treat only correctness failures as stops.

**Architecture:**
- **`LeanExecutor3`** takes a plan with no size and no hop minimums. It reads
  both pools at execution, sizes the trade with a two-pool tick walk, quotes
  that size exactly, and either trades or exits with `NoTrade` before
  borrowing. The floor is priced at execution from gas used and `tx.gasprice`.
- **`apex_math::walk`** mirrors the walk in Rust, integer for integer. The
  engine predicts with it, the evaluator judges with it, and shared fixtures
  hold it to the contract.
- **`SizedEngine`** sits beside today's `Engine`. It prices each slot through the
  mirror, values each candidate by its expected net, and fills senders and budget
  greedily by marginal net, replaying each trigger group's trades on the book.
  Both run in one canary: R39 live, R40.1 in shadow, on the same feed.
- **First live is at 16 senders**, with today's worker reads. Only then does the
  worker learn local nonces and balances for 32 and 64.

**Tech Stack:**
- Solidity 0.8.24, `via_ir`, Foundry: `contracts/lean`, `test/lean`,
  `test/fork`.
- Rust: `apex-math`, `apex-exec`, `apex-runtime`.
- Python 3.12: `scripts/data/arb_census`.

**Spec:** `docs/superpowers/specs/2026-10-30-any-size-armed-lane-design.md`
(approved 2026-11-02, all six decisions; its "Review response (2026-11-03)" holds
each of this revision's verdicts). **Branch:** `r40-1-any-size`, from `main` at
`661b0cd` (R41, merged locally, unpushed).

## When this runs

- **Phases 2 to 4 start now.** They are maths, a contract and an engine, on a
  branch. Nothing reaches the host until R41's gate D has passed, and R39 stays
  live and unchanged until the go decision.
- **R41 deploys first** (on "yes, push and deploy <rev>"). If it needs a fix on
  `main`, rebase this branch onto it.
- **Launch values come from what is already measured** (the table below). Keep
  pre-Denim and post-Denim observations labelled apart, and change a value only
  where Denim changed its meaning.

| Launch value | |
|---|---|
| Start tokens, hops | WETH and USDC; 2 |
| Size caps, budget | 5 WETH, 12,000 USDC; 0.01 ETH a day, pending worst cases reserved |
| Window, band cap | 10 s; 20 bps, chosen online among 5, 10 and 20 steps |
| Tier 1's bid reference, tier 2's multiple | 4 steps past the no-tip threshold; 10×, and 2× tier 1's bid |
| Bid model | The production R38 rule, unchanged |
| `maxSteps`, `minNet` | 8; 1 |
| USDC rate cushion | 5% |
| Senders | 16, then 32, then 64 at most |
| `send_rate_per_s` | R39's peak accepted rate (Task 1), raised with each sender step |

## Global Constraints

- **Secrets:** never printed, echoed or logged (`BLOCKPI_KEY`, `PRIVATE_KEY`,
  `TRADER_PRIVATE_KEY`, `APEX_SECRET_*`). Keys go by file or environment, never
  argv.
- **The owner broadcasts** every deploy, authorization and funding
  transaction. The `.env` PRIVATE_KEY is the owner's and never trades.
- **The repo's habits:**
  - never `cargo fmt`;
  - cargo with `-j 3`;
  - stage named paths only;
  - no new environment variables (config fields are fine);
  - no `unwrap`/`expect` in `--bins --lib` (`no_runtime_panics.sh`);
  - `cast to-check-sum-address` on every `.sol` address literal.
- **Before any push**, all five gates (the `scripts/ci/*.sh` sweep, CI-wide
  clippy `--all-targets -- -D warnings`, `cargo test --workspace
  --all-targets`, `cargo test --workspace --doc`, `forge test`), plus
  `python3 -m pytest scripts/data/arb_census`.
  `check_placeholder_endpoints.sh` and `cl_parity_sweep.sh` are known red.
- **After each structural commit**, run `git diff --stat` before committing,
  and build the commit in a throwaway worktree.
- **The operator approves** each merge, push and deploy. No new venues, hops or
  bid rule in this release.
- **A transaction is not sent** when: the mirror and the contract disagree beyond
  rounding; a pool's state can't be reconciled; a v4 pool is unsupported; the
  lender is short; a gas bound can't cover its transaction; the pending worst
  cases plus realised losses could pass the budget; a sender's nonce, balance or
  receipt is uncertain; the feed is stale; or the executor's code hash or
  authorization differs from what was approved.
- **Everything else is telemetry:** coverage, capped and truncated value, early
  exits, and bids improve the strategy, and hold nothing up.
- **Mutation:** each new guard gets a mutant. A surviving one usually means
  delete the guard, not add a test. Touch files after restoring a mutant, and
  use `--no-fail-fast`.

## Rulings this plan makes (beyond the spec)

1. **Size, then quote.** The walk chooses the size from virtual-reserve closed
   forms, and its rounding only affects how good the size is. A second pass
   then quotes that size step for step with the pools' own arithmetic: word
   edges and zero-net ticks included, as
   `quote_exact_input_multi_tick` does. Only the quote must match a swap to the
   unit. This is what makes "a trade that passes the early exit cannot fail
   the floor on rounding" true: the predicted output equals the real one.
2. **Plans are fixed-length:** two hops, 162 bytes. The spec's sizer is
   two-pool, so a list is no use.
3. **Slipstream gets its own kind (4).** Its fee is read with `fee()` at
   execution. v3 and Pancake fees are immutable and travel in the plan, saving
   a read. v4's come from the slot0 word the sizer reads anyway, which also
   exposes `protocolFee`: a non-zero one is `NoTrade(Unsupported)`, not a
   mispriced trade. Aerodrome v2's is read from its factory.
4. **Views are read raw.** `slot0` and `ticks` are low-level static calls
   keeping the first two words. A typed interface reverts across venues
   (measured: Slipstream's `slot0` type on Uniswap v3).
5. **"Real balances" means what the swaps returned.** A pool that returned less
   than it reported fails the repayment or the profit transfer, so
   `balanceOf` calls would add gas and no safety.
6. **The lender is checked before borrowing.** `cap = min(max_size, lender's
   holding)` at arm time, and `balanceOf(lender)` once a trade is decided: short
   is `NoTrade(LENDER_SHORT)`, not a revert. About 2k gas a trade, since the loan
   then finds that slot warm.
7. **For 32 senders and up, landings come from the executor's own logs.**
   `Traded` and `NoTrade`
   carry an indexed `sender`, and the feed also subscribes to the executor.
   A pending transaction whose bound passes with no log costs no read: the
   sender may send again on that nonce, holding the same reservation. A revert
   emits nothing, so a sweep runs `landing_check` on one sender every 2 s,
   round robin, whatever the sender count: 0.5 reads a second against today's
   1.2. At 64 senders a revert's cost is on the ledger within about 2 minutes,
   and its reservation stays held until then, so the budget never understates
   it.
8. **The reservation's L1 term** is `max(l1_fee_bound_wei, 2 × oracle upper
   bound)`. It can never understate the bound, and a spike doesn't idle the
   lane.
9. **A new engine, not a mode.** R39 must keep running beside it, so
   `armed/sized.rs` is a new file. Deleting `Engine`, `RUNG_OFFSETS` and
   `Lean2` arming is a later change, after gate L.
10. **The band is chosen online** from `MoveStats`, not from a fit: of 5, 10
    and 20 steps (capped by `band_cap_bps`), the width with the best
    `p(move past threshold(B)) × (1 − p(other pool leaves B in a window))`. The
    threshold is taken as `threshold(5) + (B − 5)`, so only the chosen width
    pays for a search.
11. **The tier 1 bid has a reference.** A threshold's own surplus is near zero,
    which would bid nothing. `arm_bid` is priced at the net
    `TIER1_BID_STEPS = 4` past the no-tip threshold (where ordinary moves land,
    as in R36), and the threshold is then searched again with that tip as its
    margin.
12. **A candidate's value is its expected net.** It integrates the mirror's net
    over `MoveStats`' move distribution, at R36's six offsets, times the contest's
    chance, less the expected cost of `NoTrade`s and reverts. A point estimate at
    one step would undervalue exactly the moves that pay most.
13. **`fill` is greedy on marginal net, not group-first.** A second route is worth
    what is left after replaying the first's trade on the book, so the census's
    `marginal_usd` isn't needed, and neither is an unconditional base pass:
    breadth comes from second arms in a group being worth less.
    One addition to the review: `p_win` is floored at `P_WIN_FLOOR`. A sample's
    zero isn't evidence of zero, and an arm costs nothing unless it pays, so an
    unmeasured route ranks last rather than being excluded. Without this, the
    stop rule would leave senders idle on exactly the routes rivals are taking.
14. **`maxSteps` travels in the plan,** up to a contract limit of 16. The executor
    isn't upgradeable, and this lets evidence raise it without a redeploy.
15. **v4 is enforced at the executor by what can change at run time.** The
    hook's permission bits (no delta-returning hook), the dynamic-fee flag and the
    protocol fee are checked in `load`. The code hash stays at inventory admission:
    since EIP-6780, code at an address can't change, so an on-chain registry would
    add an owner transaction per hook and catch nothing. The engine signs only
    allowlisted hooks.
16. **Tier 2 launches at 10× with no fit,** so `tiers.py` and the census task are
    gone. The R38 refit on post-Denim days (decision 6) stands, separately.

## Review Focus

The five failures most likely to bite that the spec implies and no gate names.
Each has its test in the task named.

1. **A simulation at gas price zero** collapses the floor, so a trade that
   could never pay "passes" `eth_simulateV1`, and gate C's agreement means
   nothing. Expected: simulations carry the arm's real fee. (Task 14)
2. **The gap is gone when the predicates hold** (an equal move on the other
   pool, inside its band). Expected: `NoTrade(NoGap)`, success, no borrow, no
   revert. (Task 7)
3. **A v4 pool's fee changes after admission** (protocol fee switched on).
   Expected: `NoTrade(Unsupported)`, and the route drops out at the next
   inventory. (Task 5)
4. **A restart with transactions pending**, under local nonce and balance
   tracking. Expected: the ledger and one read per sender rebuild the truth,
   and no nonce is reused or skipped. (Task 19)
5. **The owner funds a sender** that local tracking believes is empty.
   Expected: the "balance" skip re-reads at most once a minute per sender, so
   funding takes effect without per-send reads. (Task 19)

## Files

| File | Responsibility |
|---|---|
| `contracts/lean/LeanPlan3Codec.sol` (new) | Plan 3's bytes to a `Plan3` struct |
| `contracts/lean/LeanSwapMath3.sol` (new) | `computeSwapStep` and its parts, exact input only |
| `contracts/lean/LeanPools3.sol` (new) | Per-venue reads into a `Leg`, the bitmap walk |
| `contracts/lean/LeanSizer3.sol` (new) | The walk and the exact quote |
| `contracts/lean/LeanExecutor3.sol` (new) | Entry, lenders, hops, floor, events, admin |
| `script/DeployLeanExecutor3.s.sol` (new) | The owner's deploy |
| `test/lean/Lean3Mocks.sol` (new) | Pools with real swap arithmetic over settable ticks |
| `crates/apex-math/src/walk.rs` (new) | The mirror |
| `crates/apex-exec/src/lean3.rs` (new) | Plan 3's encoder, commitment and sealed plan |
| `crates/apex-exec/tests/fixtures/lean3_plans.json` (new) | Rust ↔ codec |
| `crates/apex-exec/tests/fixtures/sizer3_cases.json` (new) | Rust ↔ sizer |
| `crates/apex-runtime/src/armed/sized.rs` (new) | `SizedEngine`, grouping, the fill |
| `crates/apex-runtime/src/armed/tracked.rs` (new) | Local nonce and balance state |
| `scripts/data/arb_census/coverage.py` (new, last) | The daily report |

Modified: `armed/{threshold,plan,live,live_chain,live_ledger,run,lines,eval,engine,mod}.rs`,
`live/{calls,gas,book}.rs`, `canary/chain.rs`, `shadow/{config,mod}.rs`,
`apex-exec/src/{lib,call}.rs`, `ops/*.yaml`, `ops/host/ARMED.md`,
`script/AuthorizeArmedSenders.s.sol`.

---

## Phase 1: facts

### Task 1: The facts

**Files:** Modify: the spec's "Facts verified" section.

**Interfaces:** Produces `armed.sized.send_rate_per_s`, `p_revert`, and the
groups Task 12's tests size against.

Done on 2026-11-03, from `base/base` at v1.5.1 and its `main`: no send-rate limit
in the node; validity transactions get their own pool on `main`, at reth's
pending limits by default, with `--rollup.validity-pool-*` overrides; arms use
one account slot each, and no more predicates than R39's.

- [x] **Step 1: The host's numbers** (2026-11-04 NZ): 1,038 of 179,921 sends
  refused with http 429, in bursts of a median 13 sends a second, so
  `send_rate_per_s` is 10 when Task 19 builds the limiter (before 32 senders);
  the 15 s validity cap, which R41's 10 s window is inside; and 73 (pool,
  direction) groups from 50 routes.
- [x] **Step 2: The L1 fees and reverts** (2026-11-03, from the chain): 324
  landings and 1 revert in 325 sends; L1 fees at most 8.17e11; the oracle's
  512-byte bound 2.08e12 at that spike.
- [x] **Step 3: Record them** in the spec. `p_revert_ppm: 3100` is in all four
  configs. 1e12 covers every fee paid, so decision 3 stands for Task 16.

## Phase 2: the mirror

### Task 2: `apex_math::walk`

**Files:**
- Create: `crates/apex-math/src/walk.rs`
- Modify: `crates/apex-math/src/lib.rs`
- Test: in `walk.rs`, and `crates/apex-runtime/tests/walk_vs_best_size.rs`

**Interfaces:**
- Consumes: `cl_math::{compute_swap_step, get_next_sqrt_price_from_input,
  get_amount0_delta, get_amount1_delta, mul_div_checked,
  mul_div_rounding_up, get_sqrt_ratio_at_tick}`, `cl_swap::{TickLadder,
  LadderStep, word_edge, quote_exact_input_multi_tick}`.
- Produces:

```rust
/// The launch value of the plan's `maxSteps`; the contract holds it to 16.
pub const MAX_STEPS: u32 = 8;

pub enum WalkLeg<'a> {
    Cl { state: &'a ClPoolState, ladder: &'a TickLadder, zero_for_one: bool, cross_gas: u64 },
    Reserves { reserve_in: U256, reserve_out: U256, fee_ppm: u32 },
}

/// What one gas costs in start-token units: wei per gas, then the plan's rate.
#[derive(Clone, Copy)]
pub struct WalkPrice { pub wei_per_gas: U256, pub rate: Option<U256> }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walked {
    pub size: U256,
    /// Boundaries reached, word edges included, on both pools.
    pub steps: u32,
    pub capped: bool,
    pub truncated: bool,
    /// The ladder ended before the walk did: the mirror is short of the chain.
    pub ladder_end: bool,
    /// Crossing gas of the steps taken.
    pub cross_gas: u64,
}

/// None when there is no gap: the pair's margin is at most one.
pub fn walk(legs: [WalkLeg; 2], cap: U256, price: WalkPrice, max_steps: u32) -> Option<Walked>;
/// The exact outputs at `size`: [hop 1's, hop 2's].
pub fn quote(legs: [WalkLeg; 2], size: U256) -> Option<[U256; 2]>;
/// A wei amount in start units, rounded up: `ceil(wei × rate / 1e18)`, 1:1
/// with no rate. The contract's `toStart`.
pub fn to_start(wei: U256, rate: Option<U256>) -> Option<U256>;
```

**The algorithm** (the contract follows it integer for integer):

Let `P = 1_000_000`, `Q = 2^96`, and `g_i = P − fee_i`. Each leg has virtual
reserves at its current position:
- CL, zero-for-one: `vIn = (L << 96) / sqrtP`, `vOut = mulDiv(L, sqrtP, Q)`;
- CL, one-for-zero: those two swapped;
- reserves: `rIn`, `rOut`.

Each leg's price root, in Q96, is `s = sqrtP`, `Q² / sqrtP` when one-for-zero,
or `isqrt(mulDiv(rOut, Q², rIn))` for reserves.

```text
margin(a, b) = mulDiv(mulDiv(a.s, b.s, Q), isqrt(g_a × g_b), P)
D(a, b)      = mulDiv(a.vIn, b.vIn × P², g_a × (b.vIn × P + g_b × a.vOut))
step(a, b)   = mulDiv(D, margin − Q, Q)                     // the segment pair's best input
gain(a, b)   = mulDiv(mulDiv(D, margin − Q, Q), margin − Q, Q)   // its profit

total = 0
loop:
  if margin ≤ Q: stop
  dx = step(a, b); clamped = total + dx ≥ cap; if clamped: dx = cap − total
  (in_a, out_a) = input and output taking a to its boundary        // ∞ for reserves
  in_b          = input taking b to its boundary                   // ∞ for reserves
  mid           = a's constant-product output for dx
  hit_a = dx ≥ in_a
  hit_b = (hit_a ? out_a : mid) ≥ in_b
  if neither: total += dx; capped = clamped; stop
  if hit_a and (not hit_b or out_a ≤ in_b):
       a to its boundary; b forward by out_a inside its segment; total += in_a
  else:
       dx_b = a's input returning in_b, rounded up
       a forward by dx_b inside its segment; b to its boundary; total += dx_b
  steps += 1
  if steps == max_steps: truncated; total −= 1; stop
  apply the crossed leg's liquidityNet if its boundary is initialized
  if gain(a, b) ≤ to_start(cross_gas × wei_per_gas): total −= 1; stop
  find the crossed leg's next boundary
size = min(total, cap)
```

- A boundary is `nextInitializedTickWithinOneWord`'s answer, a word edge
  included. `word_edge` gives it in Rust. An edge costs no crossing gas.
- Stopping at a boundary takes one unit off, so the real swap doesn't pay to
  cross a tick it gains nothing from.
- `gain` ignores the next boundary, so it is an upper bound: at worst one
  crossing's gas is spent on a segment that didn't quite repay it.
- Liquidity reaching zero stops the walk at that boundary.
- Any `mulDiv` overflow returns `None`; the contract reverts there.

- [ ] **Step 1: Write the failing tests** in `walk.rs`.

```rust
#[test] fn no_gap_is_none();                 // equal prices: None
#[test] fn fees_alone_close_a_small_gap();   // margin above 1 before fees, not after: None
#[test] fn single_segment_matches_the_closed_form();
    // two wide-ladder pools: size within 1 unit of (√(ab) − b)/c computed in f64 on 1e6-scaled values
#[test] fn the_cap_holds();                  // capped, size == cap, profit still positive
#[test] fn a_crossing_is_taken_when_it_pays();
#[test] fn a_crossing_is_refused_when_it_does_not(); // same pools, wei_per_gas raised: size is the boundary's less 1
#[test] fn negative_net_narrows_and_positive_net_deepens();
#[test] fn word_edges_are_steps_and_cost_no_crossing_gas();
#[test] fn several_bitmap_words_and_negative_ticks();
#[test] fn an_input_exactly_at_a_boundary();
#[test] fn max_steps_truncates_below_the_boundary();
#[test] fn both_directions_mirror();         // swapping token order gives the same size
#[test] fn reserves_leg_has_no_boundary();
#[test] fn usdc_six_decimals();              // rate applied, costs rounded up
#[test] fn ladder_end_is_flagged();
#[test] fn quote_equals_the_multi_tick_quote(); // to the unit, against quote_exact_input_multi_tick
```

- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-math walk`. Expected: FAIL,
  no module `walk`.
- [ ] **Step 3: Implement `walk.rs`** and `pub mod walk;`.
- [ ] **Step 4: Run them.** Same command. Expected: PASS.
- [ ] **Step 5: Hold it to `best_size`** in
  `crates/apex-runtime/tests/walk_vs_best_size.rs`: 500 seeded random pool
  pairs (a local xorshift, as the repo's other generators do), each with 1 to
  6 initialized ticks in range, both directions, three gas prices. For each,
  `walk` at the gas's start-token cost against `best_size` on the same
  `CostedCycle`. Assert the walk's net is at least `best_size`'s net less the
  rounding allowance `2 × (steps + 1)` start units, plus one crossing's gas
  when `gain`'s upper bound took a crossing `best_size` wouldn't. Print the
  worst shortfall. No tick ladders are stored anywhere, so real books are
  compared in shadow: every fire logs both nets (Task 14).

  Run: `cargo test -j 3 -p apex-runtime --test walk_vs_best_size -- --nocapture`.
  Expected: PASS.
- [ ] **Step 6: Commit.**

```bash
git add crates/apex-math/src/walk.rs crates/apex-math/src/lib.rs crates/apex-runtime/tests/walk_vs_best_size.rs
git commit -m "feat(math): the two-pool tick walk (Task 8.5 R40.1)"
```

## Phase 3: the executor

### Task 3: Plan 3's codec, Rust and Solidity, held by a fixture

**Files:**
- Create: `contracts/lean/LeanPlan3Codec.sol`, `crates/apex-exec/src/lean3.rs`,
  `crates/apex-exec/tests/lean3_plan.rs`,
  `crates/apex-exec/tests/fixtures/lean3_plans.json`,
  `test/lean/Lean3PlanFixture.t.sol`
- Modify: `crates/apex-exec/src/lib.rs`, `crates/apex-exec/src/call.rs`

**Interfaces:** Produces, in Rust (`Lean2Lender` and `StartToken` reused from
`lean2`):

```rust
pub const LEAN3_PLAN_VERSION: u32 = 5;
pub const LEAN3_PLAN_LEN: usize = 162;
pub enum Hop3Kind { UniswapV3 = 0, PancakeV3 = 1, AerodromeV2 = 2, UniswapV4 = 3, Slipstream = 4 }
pub struct Lean3Hop { pub kind: Hop3Kind, pub pool: Address, pub token_out: Address,
                      pub fee: u32, pub tick_spacing: i32, pub cross_gas: u16 }
pub struct Lean3Plan { pub lender: Lean2Lender, pub start: StartToken, pub cap: u128, pub deadline: u64,
                       pub rate: u128, pub gas_overhead: u32, pub swap_gas: u32,
                       pub l1_bound: u64, pub min_net: u64, pub max_steps: u8, pub hops: [Lean3Hop; 2] }
pub fn encode_plan3(plan: &Lean3Plan) -> Result<Vec<u8>, Lean3Error>;
pub fn run3_calldata(plan: &Lean3Plan) -> Result<Vec<u8>, Lean3Error>;
pub fn lean3_commitment(plan: &Lean3Plan, chain_id: u64, executor: Address) -> Result<B256, Lean3Error>;
pub struct SignedLean3Plan { /* sealed, as SignedLean2Plan */ }
// call.rs: CallPlan::Lean3(Lean3Plan), ExecutorCall::from_lean3(SignedLean3Plan)
```

In Solidity:

```solidity
struct Hop3 { uint8 kind; address pool; address tokenOut; uint24 fee; int24 tickSpacing; uint16 crossGas; }
struct Plan3 { uint8 lender; uint8 start; uint256 cap; uint256 deadline; uint256 rate;
               uint256 gasOverhead; uint256 swapGas; uint256 l1Bound; uint256 minNet; uint256 maxSteps; Hop3[2] hops; }
library LeanPlan3Codec { uint256 internal constant LENGTH = 162; function decode(bytes calldata p) internal pure returns (Plan3 memory); }
```

**The bytes** (big-endian, packed):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | lender: 0 Balancer V2, 1 Morpho Blue, 2 PoolManager |
| 1 | 1 | start: 0 WETH, 1 USDC |
| 2 | 16 | cap, in the start token |
| 18 | 5 | deadline, unix seconds |
| 23 | 16 | rate: start units per 1e18 wei, rounded up, the arm-time rate plus 5%; 0 for WETH |
| 39 | 4 | gasOverhead |
| 43 | 4 | swapGas: both hops and the loan, as the gas model expects them |
| 47 | 8 | l1Bound, wei |
| 55 | 8 | minNet, in the start token |
| 63 | 1 | maxSteps, at most 16 |
| 64 + 49i | 1 | kind |
| +1 | 20 | pool; for kind 3 the key's hooks |
| +21 | 20 | tokenOut (tokenIn: the start for hop 0, else the previous tokenOut) |
| +41 | 3 | fee: ppm for kinds 0, 1 and the key's for 3; 0 for 2 and 4 |
| +44 | 3 | tickSpacing, two's complement; 0 for kind 2 |
| +47 | 2 | crossGas: one tick crossed on this hop |

`Lean3Error` refuses, in Rust and by `BadPlan` in Solidity: a length other than
162, a `maxSteps` of 0 or over 16, the last hop not ending in the start token, a v4 hop under another lender,
a kind above 4, a deadline past 40 bits, and a key field past its bytes.

- [ ] **Step 1: Write the failing tests.** `tests/lean3_plan.rs` encodes each
  fixture and compares bytes, calldata and commitment, as `lean2_plan.rs` does,
  and asserts each `Lean3Error`. The fixture covers WETH under Balancer, USDC
  under Morpho, each kind in each hop position, and v4 under PoolManager.
  `Lean3PlanFixture.t.sol` decodes the same hex, field for field, and asserts
  `BadPlan` on 161 and 163 bytes.
- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-exec --test lean3_plan` and
  `forge test --match-path test/lean/Lean3PlanFixture.t.sol`. Expected: FAIL.
- [ ] **Step 3: Implement** the Rust module, `CallPlan::Lean3`, and the codec.
- [ ] **Step 4: Run them.** Same commands. Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add contracts/lean/LeanPlan3Codec.sol crates/apex-exec/src/lean3.rs crates/apex-exec/src/lib.rs crates/apex-exec/src/call.rs crates/apex-exec/tests/lean3_plan.rs crates/apex-exec/tests/fixtures/lean3_plans.json test/lean/Lean3PlanFixture.t.sol
git commit -m "feat(lean3): plan 3, Rust and Solidity (Task 8.5 R40.1)"
```

### Task 4: `LeanSwapMath3`

**Files:**
- Create: `contracts/lean/LeanSwapMath3.sol`, `test/lean/LeanSwapMath3.t.sol`,
  `crates/apex-exec/tests/fixtures/swap_steps3.json`
- Modify: `crates/apex-math/src/cl_math.rs` (test only: the fixture's writer)

**Interfaces:** Consumes `contracts/libraries/{FullMath,TickMath}.sol`.
Produces:

```solidity
library LeanSwapMath3 {
    function computeSwapStep(uint160 current, uint160 target, uint128 liquidity, uint256 remaining, uint24 feePips)
        internal pure returns (uint160 next, uint256 amountIn, uint256 amountOut, uint256 feeAmount);
    function nextSqrtFromInput(uint160 sqrtP, uint128 liquidity, uint256 amountIn, bool zeroForOne) internal pure returns (uint160);
    function amount0Delta(uint160 a, uint160 b, uint128 liquidity, bool roundUp) internal pure returns (uint256);
    function amount1Delta(uint160 a, uint160 b, uint128 liquidity, bool roundUp) internal pure returns (uint256);
    function mulDivUp(uint256 a, uint256 b, uint256 d) internal pure returns (uint256);
    function sqrt(uint256 x) internal pure returns (uint256);
}
```

Exact input only: the exact-output branches are gone.

- [ ] **Step 1: Write the fixture and the failing test.** An `#[ignore]`d Rust
  test writes 200 seeded `compute_swap_step` cases to `swap_steps3.json`: both
  directions, fees of 100, 500, 3000 and 10000 ppm, reaching and not reaching
  the target, a remainder of one unit. `LeanSwapMath3.t.sol` reads the file with
  `vm.readFile` and asserts all four outputs equal, plus `sqrt` against known
  values and `2^256 − 1`.
- [ ] **Step 2: Run it.** `forge test --match-path test/lean/LeanSwapMath3.t.sol`.
  Expected: FAIL.
- [ ] **Step 3: Implement it.** `isqrt` is the same Newton iteration in both
  languages, so the mirror holds.
- [ ] **Step 4: Run it.** Expected: PASS, 200 cases equal.
- [ ] **Step 5: Commit.**

```bash
git add contracts/lean/LeanSwapMath3.sol test/lean/LeanSwapMath3.t.sol crates/apex-exec/tests/fixtures/swap_steps3.json crates/apex-math/src/cl_math.rs
git commit -m "feat(lean3): the swap step, exact input (Task 8.5 R40.1)"
```

### Task 5: `LeanPools3`: reading a pool at execution

**Files:**
- Create: `contracts/lean/LeanPools3.sol`, `test/lean/Lean3Mocks.sol`,
  `test/lean/LeanPools3.t.sol`

**Interfaces:** Produces:

```solidity
struct Bound { int24 tick; bool initialized; uint160 sqrtPrice; int128 net; bool netRead; }
struct Leg {
    uint8 kind; address pool; bool zeroForOne; bytes32 poolId;   // poolId: v4 only
    uint160 sqrtP; uint128 liquidity; int24 tick; int24 spacing; uint24 feePpm;
    uint256 reserveIn; uint256 reserveOut;                        // kind 2 only
    uint256 word; int16 wordPos; bool wordRead;                   // the one cached bitmap word
    Bound[18] bounds; uint8 known; uint16 crossGas;               // the limit of 16, plus 2
}
library LeanPools3 {
    /// False: a pool the sizer must not trade (`NoTrade(Unsupported)`).
    function load(Hop3 memory hop, address tokenIn, Leg memory leg) internal view returns (bool);
    /// The boundary after `bounds[known − 1]`, appended: bitmap first, then TickMath.
    function nextBound(Leg memory leg) internal view;
    /// The liquidityNet at `bounds[i]`, read once.
    function netOf(Leg memory leg, uint256 i) internal view returns (int128);
}
```

Per-venue reads, each from the table in the spec's section 2:

| Kind | Read |
|---|---|
| 0, 1 | raw `slot0()` first two words; `liquidity()`; fee and spacing from the plan |
| 4 | the same; `fee()`; spacing from the plan |
| 3 | `extsload` of `keccak256(abi.encode(poolId, 6))` and that slot + 3. The word holds sqrtP (0–159), tick (160–183), protocolFee (184–207), lpFee (208–231) |
| 2 | `getReserves()`, `factory().getFee(pool, false)` × 100 as ppm |

v4's bitmap word is `keccak256(abi.encode(int16 word, state + 5))`, and its tick
info is `keccak256(abi.encode(int24 tick, state + 4))`, with liquidityNet in the
high 128 bits of the first word. Verify both against PoolManager on a fork
before relying on them, as R37 verified slot0 and liquidity.

`load` returns false when: v4's `protocolFee` is non-zero; its `lpFee` has the
dynamic flag (`0x800000`) set; its hook's address has `beforeSwapReturnDelta` or
`afterSwapReturnDelta` set (bits 3 and 2); or liquidity is zero and the pool is
CL.

`Lean3Mocks.sol` holds `MockCl3Pool` (settable slot0, liquidity, bitmap and
ticks, a Slipstream-shaped and a v3-shaped `slot0`, a real `swap` over
`LeanSwapMath3`, and a settable `fee()`), `MockAero3Pool` and
`MockPoolManager3` (`extsload`, `unlock`, and a `swap` over the same arithmetic).

- [ ] **Step 1: Write the failing tests** in `LeanPools3.t.sol`:

```solidity
function testV3ShapedSlot0ReadsSqrtAndTick() public;
function testSlipstreamShapedSlot0ReadsTheSame() public;
function testSlipstreamFeeIsReadAtExecution() public;     // setFee between two loads: the second sees it
function testPancakeFeeComesFromThePlan() public;
function testAeroFeeBpsBecomesPpm() public;
function testV4WordsDecode() public;                      // negative tick, lpFee, protocolFee
function testV4ProtocolFeeRefused() public;               // load returns false
function testV4DynamicFeeRefused() public;
function testV4DeltaReturningHookRefused() public;     // each of the two bits
function testV4NoDeltaHookLoads() public;
function testZeroLiquidityRefused() public;
function testBoundsMatchNextInitializedTickWithinOneWord() public; // both directions, negative ticks, spacing 1, 60, 200
function testBitmapWordIsReadOnce() public;               // vm.expectCall count 1 across two bounds in one word
function testMinAndMaxTickClamp() public;
```

- [ ] **Step 2: Run them.** `forge test --match-path test/lean/LeanPools3.t.sol`.
  Expected: FAIL.
- [ ] **Step 3: Implement** `LeanPools3`, using assembly only for the raw
  two-word static call.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add contracts/lean/LeanPools3.sol test/lean/Lean3Mocks.sol test/lean/LeanPools3.t.sol
git commit -m "feat(lean3): reading each venue at execution (Task 8.5 R40.1)"
```

### Task 6: `LeanSizer3`, held to the mirror

**Files:**
- Create: `contracts/lean/LeanSizer3.sol`, `test/lean/LeanSizer3.t.sol`,
  `crates/apex-exec/tests/fixtures/sizer3_cases.json`,
  `crates/apex-exec/tests/sizer3_cases.rs`

**Interfaces:** Consumes `Leg`, `LeanPools3`, `LeanSwapMath3`. Produces:

```solidity
uint8 constant FLAG_CAPPED = 1;
uint8 constant FLAG_TRUNCATED = 2;
struct Sized { uint256 size; uint256 out; uint256 crossGas; uint8 steps; uint8 flags; }
library LeanSizer3 {
    uint256 internal constant MAX_STEPS_LIMIT = 16;
    /// size 0: no gap. `costPerGas` is what one gas costs in start units, scaled by 1e18.
    function size(Leg memory a, Leg memory b, uint256 cap, uint256 costPerGas, uint256 maxSteps) internal view returns (Sized memory);
    /// The exact output of `amountIn`, step for step, extending `bounds` as needed.
    function quote(Leg memory leg, uint256 amountIn) internal view returns (uint256 out);
}
```

`size` follows Task 2's algorithm. `Sized.out` is `quote(b, quote(a, size))`.
The two walks start from each leg's position as loaded: `size` works on copies
of `sqrtP`, `liquidity` and the reserves, and leaves `Leg` intact for `quote`.

- [ ] **Step 1: Write the cases.** An `#[ignore]`d test,
  `regenerate_sizer3_cases`, in `crates/apex-exec/tests/sizer3_cases.rs` writes
  `sizer3_cases.json`, and the normal test there checks that `walk` still
  produces every expected value. Each case holds both pools, cap and price, and
  expects `size`, `out`, `steps` and `flags`. They are the spec's matrix:

```text
single_tick                 multi_tick_walk           zero_for_one / one_for_zero (each)
positive_net / negative_net tick_exactly_at_boundary  out_of_range_liquidity_zero
fee_rounding_1ppm_gap       usdc_six_decimals         usdc_start_rate
v4_no_hook                  v4_allowed_hook           aero_volatile_first / second
max_steps_clamp             cap_clamp                 crossing_refused
word_edge_step              zero_net_tick             several_bitmap_words
negative_ticks              exact_boundary_amount
```

- [ ] **Step 2: Write the failing test.** `LeanSizer3.t.sol` deploys the mocks
  from each case and asserts `size`, `out`, `steps` and `flags` equal the JSON,
  to the unit. It also runs each mock's real `swap` at `size` and asserts
  `out` equals what the swaps returned.
- [ ] **Step 3: Run it.** `forge test --match-path test/lean/LeanSizer3.t.sol`.
  Expected: FAIL.
- [ ] **Step 4: Implement `LeanSizer3`.**
- [ ] **Step 5: Run it.** Expected: PASS, every case equal. A disagreement is
  fixed in whichever side is wrong against Task 2's algorithm, and the fixture
  is regenerated only after that.
- [ ] **Step 6: Commit.**

```bash
git add contracts/lean/LeanSizer3.sol test/lean/LeanSizer3.t.sol crates/apex-exec/tests/sizer3_cases.rs crates/apex-exec/tests/fixtures/sizer3_cases.json
git commit -m "feat(lean3): the sizer, held to the mirror (Task 8.5 R40.1)"
```

### Task 7: `LeanExecutor3`

**Files:**
- Create: `contracts/lean/LeanExecutor3.sol`,
  `test/lean/LeanExecutor3Run.t.sol`, `test/lean/LeanExecutor3Admin.t.sol`

**Interfaces:** Consumes `LeanPlan3Codec`, `LeanPools3`, `LeanSizer3`. Produces:

```solidity
constructor(address owner_, address trader_, address profitRecipient_);
function run(bytes calldata plan) external;
function setExecutors(address[] calldata accounts, bool allowed) external; // onlyOwner
// As LeanExecutor2: setExecutor, setProfitRecipient, tripCircuit, resetCircuit,
// isCircuitOpen, transferOwnership, sweep, receiveFlashLoan, onMorphoFlashLoan,
// unlockCallback, uniswapV3SwapCallback, pancakeV3SwapCallback.

event Traded(address indexed sender, uint8 start, uint256 amountIn, uint256 out,
             uint256 lenderFee, uint256 costWei, uint8 steps, uint8 flags);
event NoTrade(address indexed sender, uint8 reason, uint8 steps);
// reasons
uint8 constant NO_GAP = 0; uint8 constant BELOW_FLOOR = 1; uint8 constant UNSUPPORTED = 2; uint8 constant LENDER_SHORT = 3;
```

**`run`, in order:**
1. `gasStart = gasleft()`.
2. Trader, circuit, deadline, then `decode`, each failing as `LeanExecutor2`'s
   does.
3. `load` both legs; false emits `NoTrade(UNSUPPORTED, 0)` and returns.
4. `costPerGas` from `tx.gasprice` and `rate`.
5. `size`. A size of 0 emits `NoTrade(NO_GAP, steps)` and returns.
6. Predicted floor:
   `size + toStart(((gasStart − gasleft()) + swapGas + gasOverhead + crossGas) × tx.gasprice + l1Bound) + minNet`.
   `out` below it emits `NoTrade(BELOW_FLOOR, steps)` and returns.
7. `balanceOf(lender)` in the start token. Below the size, emit
   `NoTrade(LENDER_SHORT, steps)` and return.
8. Store the size and `gasStart` in transient slots `T_SIZE` and `T_GAS_START`,
   set `T_LOAN`, and borrow from the plan's lender.

**In the trade:** hop 1 is exact input at the size, and hop 2 at what hop 1
returned. An Aerodrome hop computes its own output from the loaded reserves and
fee, with the pool's formula, and asks for exactly it. No price limits; no hop
minimums.

**Then the floor:**

```text
cost     = (used in run before the loan + used in the callback + gasOverhead) × tx.gasprice + l1Bound
required = size + lenderFee + toStart(cost) + minNet
out < required: revert Floor(out, required)
```

Each frame measures its own gas: one `gasleft()` against `run`'s start would
also count what the 63/64 rule holds back in the calling frames (found by
these tests: 33M gas under forge's limit).

The gross `out − size − lenderFee` goes to `profitRecipient`, and `Traded` is
emitted. A mocked hop 2 returning less than quoted must hit `Floor`.

- [ ] **Step 1: Write the failing tests.**

`LeanExecutor3Run.t.sol`:

```solidity
function testTradesAtTheSizersSize() public;          // Traded carries size, out, flags; gross to the recipient
function testNoDustLeft() public;                     // both tokens zero after a trade
function testFloorCountsGasAtTheTxGasPrice() public;  // vm.txGasPrice high enough: NoTrade(BELOW_FLOOR), low: Traded
function testNetNeverNegativeAtAnyGasPrice() public;  // fuzz gas price: gross ≥ gasUsed × price + l1Bound whenever Traded
function testUsdcCostConvertsAtTheRateRoundedUp() public;
function testGapClosedByThePredicatesTimeIsNoTrade() public; // equal prices: NoTrade(NO_GAP), status ok, no borrow (vm.expectCall count 0)
function testBelowFloorBorrowsNothing() public;
function testLenderShortIsNoTrade() public;           // each lender: no borrow, no revert
function testShortHopRevertsFloor() public;           // mock pays less than the quote
function testCapClamps() public;                      // FLAG_CAPPED
function testMaxStepsFlagsTruncated() public;
function testV4ProtocolFeeIsNoTrade() public;
function testExpiredReverts() public; function testCircuitReverts() public; function testNotTraderReverts() public;
function testBadPlanReverts() public;
function testCallbacksOnlyInsideALoan() public;       // each callback outside run: UnexpectedCallback
function testAPoolCannotBeCalledBackTwice() public;
function testLenderFeeCounts() public;                // Balancer mock with a fee
function testEachLender() public;                     // Balancer, Morpho, PoolManager
```

`LeanExecutor3Admin.t.sol` mirrors `LeanExecutor2Admin.t.sol`, plus
`testSetExecutorsBatch`, `testSetExecutorsOnlyOwner` and
`testSetExecutorsRefusesZero`.

- [ ] **Step 2: Run them.** `forge test --match-path 'test/lean/LeanExecutor3*'`.
  Expected: FAIL.
- [ ] **Step 3: Implement it,** from `LeanExecutor2` where the two agree.
- [ ] **Step 4: Run them,** and the size gate.
  `forge test --match-path 'test/lean/LeanExecutor3*'` and
  `scripts/ci/check_executor_size.sh`. Expected: PASS, and runtime under 24,576
  bytes.
- [ ] **Step 5: Mutate.** Remove in turn: the deadline check; `T_LOAN`'s clear;
  `T_POOL`'s check; `gasOverhead` from the floor; `l1Bound` from the floor;
  `lenderFee` from `required`; the lender check. Each must fail a test. Touch the file after each
  restore.
- [ ] **Step 6: Commit.**

```bash
git add contracts/lean/LeanExecutor3.sol test/lean/LeanExecutor3Run.t.sol test/lean/LeanExecutor3Admin.t.sol
git commit -m "feat(lean3): LeanExecutor3, sized and floored at execution (Task 8.5 R40.1)"
```

### Task 8: Fork tests, gas, and the constants

**Files:**
- Create: `test/fork/LeanExecutor3Fork.t.sol`
- Modify: `crates/apex-runtime/src/live/gas.rs`

**Interfaces:** Produces `gas::SIZED: SizedGas` for Task 11:

```rust
pub struct SizedGas {
    /// The executor's own work beyond `LEAN`'s hops: the reads and the walk, by venue.
    pub read: PerVenue,
    /// Each boundary the walk reaches, beyond the crossing itself.
    pub step: u64,
    /// `gasOverhead`: intrinsic, calldata, and the work after the floor.
    pub overhead: u64,
    /// The limit every sized arm carries.
    pub limit: u64,
}
```

- [ ] **Step 1: Write the fork tests.** On a fresh fork block through drpc, with
  a trigger swap opening each gap as `LeanExecutor2Fork.t.sol` does: WETH/USDC
  across v3, Pancake, Slipstream and Aerodrome v2, every pair of them, and
  AXIS/USDC for v4 with v3, each in both hop orders:

```solidity
function testPair_<a>_<b>() public; // size and out equal Sized's; out equals the swaps'; Traded emitted; gas logged
```

  plus `testV4SlotsMatchThePool` (bitmap and tick slots against `StateLibrary`'s
  answers), `testGasAtMaxSteps` (a move big enough to reach 8 steps, then 16,
  logging gas: 8 must stay under `SIZED.limit`, and 16 shows what a higher value
  would cost), `testNoTradeGasByVenuePair`, and
  `testSlipstreamFeeAtTheLimit` (the fee read at the sized gas limit equals the
  fee with unlimited gas: `getSwapFee` calls its module with a 200,000 gas cap
  and falls back if that fails).
- [ ] **Step 2: Run them.**

```bash
FOUNDRY_PROFILE=fork FOUNDRY_COMPUTE_UNITS_PER_SECOND=25 FOUNDRY_FORK_RETRIES=12 FOUNDRY_FORK_RETRY_BACKOFF=4000 forge test --match-path test/fork/LeanExecutor3Fork.t.sol -vv --threads 1
```

  Expected: PASS, with gas logged per pair. A size or output mismatch stops the
  plan: go back to Task 6 with the failing state as a new case.
- [ ] **Step 3: Set the constants** from those logs, rounded up to the next 100.
  The plan's `maxSteps` stays 8 unless gas at it exceeds `limit`. Add
  `sized_limit_covers_max_steps`, asserting `limit` covers the logged worst case
  plus 25% for refunds (`gas-limits-must-cover-refunds`).
- [ ] **Step 4: Commit.**

```bash
git add test/fork/LeanExecutor3Fork.t.sol crates/apex-runtime/src/live/gas.rs
git commit -m "test(lean3): every venue pair on a fork, and the gas constants (Task 8.5 R40.1)"
```

### Task 9: Overlapping routes, and the deploy

**Files:**
- Create: `test/lean/LeanExecutor3Overlap.t.sol`,
  `script/DeployLeanExecutor3.s.sol`, `test/lean/DeployLeanExecutor3.t.sol`
- Modify: `script/AuthorizeArmedSenders.s.sol`

**Interfaces:** Produces the deploy script, logging `armed.sized.executor3`'s
address and code hash, and an `--sig "runBatch(address)"` path through
`setExecutors`.

- [ ] **Step 1: Write the failing overlap tests,** on mocks sharing a trigger
  pool, each arm a separate `run` in one block:

```solidity
function testSecondArmTradesOnlyTheRemainder() public; // two routes: the second's size is the walk on the state the first left,
                                                       // and portfolio net, each tx's gas counted, beats the first alone
function testFirstClosesTheGapSecondIsNoTrade() public;// the second's input is 0: NoTrade(NO_GAP), no borrow (expectCall count 0)
function testRemainderBelowFloorIsNoTrade() public;    // a remainder that doesn't pay: NoTrade, no borrow, no revert
function testTwentyArmsPortfolioNet() public;          // 20 overlapping arms: portfolio net after every included tx's gas,
                                                       // NoTrades' included, is logged and ≥ the best single arm's less those NoTrades' gas
function testRivalBetweenOurArms() public;             // a mock rival swap between two runs: the second re-sizes on the new state
function testHigherBidTierSweepsTheLowerTier() public; // tiers of one route: the second is NoTrade(NO_GAP) once the first traded
```

- [ ] **Step 2: Run them.** `forge test --match-path test/lean/LeanExecutor3Overlap.t.sol`.
  Expected: PASS at once if Task 7 is right. A failure is a Task 7 bug.
- [ ] **Step 3: Write the deploy test and script,** from `DeployLeanExecutor2`:
  same checks (owner, trader, recipient, Morpho's approvals), the code hash
  logged, and the forge-clean deploy order in its header comment.
- [ ] **Step 4: Run them.** `forge test --match-path 'test/lean/DeployLeanExecutor3*'`.
  Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add test/lean/LeanExecutor3Overlap.t.sol script/DeployLeanExecutor3.s.sol test/lean/DeployLeanExecutor3.t.sol script/AuthorizeArmedSenders.s.sol
git commit -m "test(lean3): overlapping arms, and the deploy script (Task 8.5 R40.1)"
```

**Gate S (sizing).** All must hold before Phase 4 merges anywhere:
- the contract equals the mirror on every fixture, to the unit;
- the mirror's net is within rounding of `best_size`'s (Task 2's test);
- every venue pair lands at the mirror's size and output, and gas at 8
  and 16 steps is measured;
- the overlapping-routes tests pass.

## Phase 4: engine and evaluator (to first live)

### Task 10: Config, and the sized call

**Files:**
- Modify: `crates/apex-runtime/src/shadow/config.rs`, `armed/plan.rs`,
  `live/calls.rs`, `armed/lines.rs`
- Test: in those modules

**Interfaces:** Produces:

```rust
// shadow/config.rs: ArmedConfig.sized: Option<SizedConfig>
pub struct SizedConfig {
    pub mode: ArmedMode,                 // shadow | live
    pub ledger: PathBuf, pub verdicts: PathBuf,
    pub executor3: Executor2Config,      // same shape: address, code_hash, runtime_code
    pub max_senders: usize,              // 16, then 32, then 64
    pub band_cap_bps: u32,               // 20
    pub window_s: u64,                   // 10
    pub tier2_multiple: u32,             // 10
    pub min_net_wei: u64,                // 1: rounding alone can't record a zero
    pub max_steps: u8,                   // 8
    pub p_no_trade: f64,                 // 0.02, until live data replaces it
    pub p_revert: f64,                   // R39's measured share, from Task 1
    pub send_rate_per_s: u32,            // used from Task 19
}

// live/calls.rs
pub fn lean3_hop(leg: &Leg, cross_gas: u16) -> Result<Lean3Hop, String>;

// armed/plan.rs
pub struct SizedTerms { pub cap: U256, pub rate: Option<(U256, U256)>, pub overhead: u32,
                        pub swap_gas: u32, pub l1_bound: u64, pub min_net: u64, pub max_steps: u8 }
pub fn sized_call(cycle: &Cycle, terms: &SizedTerms, executor3: Address, chain_id: u64,
                  deadline: u64, lender: Lean2Lender) -> Result<ExecutorCall, String>;
/// What the executor would do on `book`: the mirror's walk and quote.
pub fn sized_outcome(cycle: &Cycle, book: &BTreeMap<Address, Arc<PoolSnapshot>>, terms: &SizedTerms,
                     wei_per_gas: U256) -> SizedOutcome;
pub enum SizedOutcome { Trade { size: U256, gross: U256, walked: Walked, beyond_cap: U256, beyond_steps: U256 }, NoTrade(u8) }

// armed/lines.rs, on Arm, all #[serde(default)]
pub sized: Option<SizedLine>,   // cap, rate, overhead, swap_gas, l1_bound, min_net, max_steps, tier
```

`beyond_cap` is what a walk with no cap would add at the same gas price, and
`beyond_steps` what a walk of 64 steps would: the value each limit left, in the
start token. They are logged on every outcome, and decide `maxSteps`.

A load-time check refuses a config with both `armed.mode: live` and
`armed.sized.mode: live`, a `band_cap_bps` over 20, and an `executor3` equal to
`executor2`.

- [ ] **Step 1: Write the failing tests:** `sized_call_round_trips` (decode the
  calldata's plan); `kind_follows_the_venue` (both Slipstream factories give
  kind 4; v3, 0; Pancake, 1); `a_v4_route_borrows_from_pool_manager`;
  `rate_is_arm_time_plus_five_percent_rounded_up`;
  `sized_outcome_matches_walk`; `beyond_cap_is_zero_under_the_cap`;
  `beyond_steps_is_zero_when_the_walk_finished`;
  `both_lanes_live_refuses_to_load`; and `arm_lines_without_sized_still_read`.
  Stable Aerodrome pools need no test here: `inventory.rs` refuses them at
  admission, unchanged.
- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime sized`.
  Expected: FAIL.
- [ ] **Step 3: Implement them.** No `ops/*.yaml` carries `armed.sized` until
  Task 15, so this task changes no running behaviour.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/apex-runtime/src/shadow/config.rs crates/apex-runtime/src/armed/plan.rs crates/apex-runtime/src/armed/lines.rs crates/apex-runtime/src/live/calls.rs
git commit -m "feat(armed): the sized call and its config (Task 8.5 R40.1)"
```

### Task 11: Pricing a slot through the mirror: threshold, tiers, expected net

**Files:** Modify: `armed/threshold.rs`. Test: in it.

**Interfaces:** Consumes `walk`, `quote`, `sized_outcome`, `move_of`,
`trigger_up`, `arm_bid`. Produces:

```rust
pub const TIER1_BID_STEPS: u32 = 4;
pub const BAND_WIDTHS: [u32; 3] = [5, 10, 20];
/// Past the threshold, where expected net is measured: R36's rung offsets.
pub const NET_GRID: [u32; 6] = RUNG_OFFSETS;

/// The trade with the trigger `step` from where it is and the other pool as
/// given, as the executor would make it, net of gas, L1 and `tip_wei`, in wei.
pub fn net_at(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, other: &PoolSnapshot, step: u32,
              px: &Pricing, terms: &SizedTerms, tip_wei: U256) -> Option<U256>;

/// The other pool at its adverse edge: `band` steps against the cycle's swap there.
pub fn adverse_edge(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, band: u32) -> Option<PoolSnapshot>;

pub struct SizedArmPoint { pub tier: u8, pub step: u32, pub trigger: PoolSnapshot, pub edges: [PoolSnapshot; 2],
                           pub bid: ArmBid, pub gas: u64,
                           /// (step, net in wei) across NET_GRID.
                           pub grid: [(u32, f64); 6] }

/// Tier 1 and, when it can pay, tier 2. Empty when no move up to MAX_STEP pays.
pub fn sized_points(cycle: &Cycle, pair: &[Arc<PoolSnapshot>; 2], leg: usize, px: &Pricing, terms: &SizedTerms,
                    band: u32, tier2_multiple: u32, inputs: &BidInputs, cap_wei_per_gas: u128) -> Vec<SizedArmPoint>;

/// Σ over the grid of P(the move ends in that bucket) × net there.
pub fn expected_net(grid: &[(u32, f64)], p_at_least: impl Fn(u32) -> f64) -> f64;
```

`sized_points`, in order:
1. Replace the other pool with `adverse_edge`, and double then bisect to the
   smallest step with `net_at` above zero at no tip.
2. `net_at` at `step + TIER1_BID_STEPS`, on the real pair, is the reference.
   `arm_bid` on it gives tier 1's bid.
3. Search again with that bid's tip: tier 1's step.
4. Fill `grid` on the real pair, tip included.
5. Tier 2: from that step, double then bisect to the first step whose net is at
   least `tier2_multiple ×` the reference; `arm_bid` there. It exists only if its
   bid is at least twice tier 1's.

Everything here is the mirror, so a slot is priced by what the chain will do, cap
and crossing refusals included. `best_size` stays the reference in tests only,
and a slot costs a handful of walks where `rungs` costs dozens of quotes.

`gas` in every tip is the sized figure: `LEAN`'s hops plus `SIZED.read` and one
step.

- [ ] **Step 1: Write the failing tests:** `the_threshold_pays_at_the_adverse_edge`
  (a trade at the threshold with the other pool at the edge nets at least the
  tip); `a_wider_band_raises_the_threshold`; `the_adverse_edge_follows_the_cycle`
  (both legs, both directions); `no_paying_move_is_empty`;
  `tier_two_needs_twice_the_bid`; `tier_two_sits_at_the_multiple`;
  `a_usdc_cycle_prices_in_usdc`; `expected_net_weights_each_bucket`
  (hand-worked: p of 0.5, 0.2, 0.1 with nets 1, 5, 10);
  `the_cap_limits_the_grid`; and `net_at_is_within_rounding_of_best_at`.
- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime threshold`. Expected:
  FAIL.
- [ ] **Step 3: Implement them.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/apex-runtime/src/armed/threshold.rs
git commit -m "feat(armed): pricing a slot through the mirror (Task 8.5 R40.1)"
```

### Task 12: `SizedEngine`

**Files:**
- Create: `crates/apex-runtime/src/armed/sized.rs`
- Modify: `crates/apex-runtime/src/armed/mod.rs`, `armed/engine.rs`,
  `live/book.rs`
- Test: in `sized.rs`

**Interfaces:** Consumes `sized_points`, `expected_net`, `sized_call`,
`sized_outcome`, `MoveStats`, `arm_bid`, `predicate`. Produces:

```rust
pub const P_WIN_FLOOR: f64 = 0.001;

pub struct SizedSettings { pub max_senders: usize, pub window_s: u64, pub block_time_ms: u64,
                           pub band_cap_bps: u32, pub liquidity_floor_bps: u32, pub tier2_multiple: u32,
                           pub executor3: Address, pub chain_id: u64, pub max_size_wei: u128, pub max_size_usdc: u128,
                           pub max_priority_wei_per_gas: u128, pub min_net_wei: u64, pub max_steps: u8,
                           pub p_no_trade: f64, pub p_revert: f64, pub reprice_for: Duration,
                           pub workers: usize, pub l1_fee_bound_wei: u128 }

pub struct Candidate { pub key: SizedKey, pub group: (Address, bool), pub tier: u8, pub ev: f64, pub worst_wei: u128 }

/// The greedy fill: indices of the chosen candidates, in the order chosen.
/// `marginal(candidate, chosen in its group)` is its EV after those.
pub fn fill(cands: &[Candidate], senders: usize, budget_wei: u128,
            marginal: impl FnMut(&Candidate, &[usize]) -> f64) -> Vec<usize>;

impl SizedEngine {
    pub fn new(cycles: BTreeMap<RouteId, Cycle>, settings: SizedSettings, unarmable: BTreeSet<Address>) -> Self;
    pub fn with_bids(self, bids: RouteBids) -> Self;
    pub fn with_contest(self, contest: Option<Arc<ShareSample>>, model: BidModel) -> Self;
    pub fn stats(&self) -> EngineStats;
    /// As `Engine::tick`, the fill bounded by `budget_left_wei`, and `l1_upper_wei`
    /// the oracle's bound.
    pub fn tick(&self, snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>, head: u64, base_fee_wei: u128,
                px: &Pricing, now_ns: u64, budget_left_wei: u128, l1_upper_wei: u128) -> Vec<ArmedLine>;
}
// live/book.rs
impl PoolSnapshot { /// This pool after an exact-input swap, by the book's own arithmetic.
    pub fn swapped(&self, amount_in: U256, zero_for_one: bool) -> Option<PoolSnapshot>; }
```

Repricing, the stale ordering, the worker pool, senders' busy-until blocks, and
`Renew` all follow `Engine`. What differs:

- **A slot** prices `sized_points` at the width Ruling 10 chooses. A slot whose
  tier 1 can't pay holds no sender.
- **A candidate's EV** is Ruling 12's:

```text
EV = max(p_win, P_WIN_FLOOR) × expected_net(grid, p)
     − p(threshold) × (p_no_trade × no_trade_cost + p_revert × revert_cost)
```

  `p` is the spread's `p_at_least`. `no_trade_cost` is the route's reads at base
  fee plus bid, plus the L1 bound. `revert_cost` is its expected gas at the same
  price.
- **`fill`** is Ruling 13's. It repeatedly takes the candidate with the highest
  `ev / (1 / senders + worst_wei / budget_wei)`, and re-evaluates only the group
  it just touched:
  - A second route in a group gets `marginal`: replay, at the first's reference
    move, each chosen arm's trade with `swapped`, in the order chosen, and scale
    the candidate's EV by its net there over its net alone, clamped to 0..1.
  - A tier 2 gets only what it adds to its tier 1: over the buckets past its
    threshold, `p_win₂ × net₂ − p_win₁ × net₁`.
  - It stops at no positive EV, or at senders or budget. Every arm holds a sender
    for one window, so sender-time needs no term of its own.
- **`worst_wei`** is `limit × (2·baseFee + bid) + max(l1_fee_bound_wei, 2 × l1_upper_wei)`.
- **The fingerprint** is threshold price, band edges and bid. Size and mid are
  gone.
- **The cap** clamps in the plan, so nothing is filtered for size. `cap = min(max_size(start), lender holding)`.
- **The predicates** are `predicate::trigger` at the tier's threshold,
  `predicate::band` at its edges, and `expiry`.
- **The line:** `rung` is the tier, and `step` its threshold. `size_wei`, `mid`
  and `output_wei` are `sized_outcome` at the threshold: logged, never enforced.
- **`gasOverhead`, `swapGas`, `crossGas`** come from `SIZED` and `LEAN`.
  `l1Bound` is `2 × l1_upper_wei`, rounded up.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn the_best_ev_goes_first_whatever_its_group();
#[test] fn a_low_value_group_does_not_outrank_a_better_second_arm();
#[test] fn a_second_route_counts_only_what_the_first_leaves();  // real `swapped` replay, not a ratio of census gross
#[test] fn a_second_route_on_a_closed_gap_takes_no_sender();
#[test] fn a_tier_two_counts_only_what_it_adds();
#[test] fn budget_stops_the_fill();                     // tier 2 worst cases: only what fits
#[test] fn sixty_four_tier_one_arms_fit_a_tenth_of_the_budget(); // the spec's table: 64 × 1.2e13 < 1e15
#[test] fn a_zero_p_win_ranks_last_but_is_not_excluded();
#[test] fn it_stops_at_no_positive_ev();
#[test] fn a_candidate_that_cannot_pay_holds_no_sender();
#[test] fn no_trade_and_revert_costs_lower_the_ev();
#[test] fn an_unchanged_arm_renews_and_does_not_rearm();
#[test] fn a_size_change_alone_does_not_rearm();        // a pool's liquidity moves, thresholds don't
#[test] fn a_threshold_change_rearms_when_its_sender_frees();
#[test] fn the_cap_is_the_lenders_when_it_holds_less();
#[test] fn the_reservation_uses_the_larger_l1_bound();
#[test] fn the_band_is_the_best_of_three_within_the_cap();
#[test] fn it_writes_lines_eval_can_read();             // Arm round trip with `sized` set
```

- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime sized::`. Expected:
  FAIL.
- [ ] **Step 3: Implement `sized.rs`.** Share `Engine`'s private helpers
  (`position`, `words_of`) by making them `pub(super)`.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Time a pass.** On the test book at 64 senders, log a pass's
  duration: it must stay under `reprice_for` with `workers` threads, or `sized`
  needs the same stale budget as `Engine`.
- [ ] **Step 6: Commit.**

```bash
git add crates/apex-runtime/src/armed/sized.rs crates/apex-runtime/src/armed/mod.rs crates/apex-runtime/src/armed/engine.rs crates/apex-runtime/src/live/book.rs
git commit -m "feat(armed): SizedEngine, chosen by marginal expected net (Task 8.5 R40.1)"
```

### Task 13: Receipts and the ledger's classes

**Files:** Modify: `canary/chain.rs`, `armed/live.rs`, `armed/live_ledger.rs`.
Test: in those modules.

**Interfaces:** Produces:

```rust
// canary/chain.rs: Read gains
pub sized: Option<SizedEvent>,
pub enum SizedEvent { Traded { gross: u128, amount_in: u128, out: u128, cost_wei: u128, steps: u8, flags: u8 },
                      NoTrade { reason: u8, steps: u8 } }

// live_ledger.rs: Landed gains, #[serde(default)]
pub class: Option<LandedClass>,
pub enum LandedClass { Traded, Capped, Truncated, EarlyExit, Reverted }
```

`gross` is `out − amount_in − lender_fee`. A `NoTrade` lands with status 1, pays
nothing, and its cost counts against the day's loss as any loss does. `read_receipt`
keeps reading `Profit` for `LeanExecutor2`, so the two run side by side.

`readable()` makes `size` optional: `arm.sized.is_some()` skips the size check,
because the plan clamps. A sized arm's `size_wei` is the engine's expectation at
the threshold, and is only logged.

- [ ] **Step 1: Write the failing tests:** `traded_reads_gross_and_flags`,
  `no_trade_is_a_landing_that_paid_nothing`, `a_revert_is_classed_reverted`,
  `a_capped_trade_is_classed_capped`, `profit_still_reads_for_executor2`,
  `a_no_trade_counts_against_the_budget`, `a_sized_arm_passes_the_size_check`,
  `an_unsized_arm_over_the_cap_is_still_skipped`, and
  `ledger_lines_without_class_still_read`.
- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime live_ledger canary::chain armed::live`.
  Expected: FAIL.
- [ ] **Step 3: Implement them.** The `Traded` and `NoTrade` topics are computed
  by a test from the event signatures, and pasted as constants like `PROFIT`.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/apex-runtime/src/canary/chain.rs crates/apex-runtime/src/armed/live.rs crates/apex-runtime/src/armed/live_ledger.rs
git commit -m "feat(armed): Traded and NoTrade in receipts and the ledger (Task 8.5 R40.1)"
```

### Task 14: The evaluator judges sized fires

**Files:** Modify: `armed/run.rs` (around `plan::armed_gross` and
`simulate_request`), `armed/eval.rs`. Test: in them.

**Interfaces:** Produces, in each verdict line of a sized arm:

```rust
pub struct SizedVerdict { pub outcome: String, // traded | capped | truncated | early_exit
                          pub size: String, pub gross: String, pub beyond_cap: String, pub beyond_steps: String,
                          pub best_size_net: String, pub walk_net: String, pub ladder_end: bool }
```

`simulated_gross` reads `Traded` as well as `Profit`. `simulate_request` sets the
call's `gasPrice` to the arm's base fee plus bid; with a zero, the floor means
nothing. The landing model stays R41's block mode.

- [ ] **Step 1: Write the failing tests:** `a_sized_fire_is_judged_at_the_walks_size`;
  `a_fire_past_the_cap_reports_what_it_left`;
  `a_truncated_fire_reports_what_the_steps_left`; `a_gap_gone_is_early_exit`;
  `the_simulation_carries_the_real_gas_price` (Review Focus 1: the request's
  price is non-zero and equals base fee plus bid);
  `simulated_gross_reads_traded`; `walk_and_best_size_are_both_logged`; and
  `unsized_arms_are_judged_as_before`.
- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime armed::run armed::eval`.
  Expected: FAIL.
- [ ] **Step 3: Implement them.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/apex-runtime/src/armed/run.rs crates/apex-runtime/src/armed/eval.rs
git commit -m "feat(armed): the evaluator judges sized fires (Task 8.5 R40.1)"
```

### Task 15: Two engines on one feed

**Files:** Modify: `armed/run.rs`, `ops/{canary,shadow}.yaml`,
`ops/{canary,shadow}.denim.yaml`, `ops/host/ARMED.md`. Test: `armed/run.rs`.

**Interfaces:** Consumes `SizedEngine`. Produces `LiveChain::l1_fee_upper_bound(
&self, tx_size: u64) -> Result<u128, String>`, read once a minute on R41's
`ReadClock` and passed to `SizedEngine::tick`, and, in `run`: with `armed.sized`
set and `mode: shadow`, a second pass on its own thread over the same snapshot,
writing `armed.sized.ledger`, and `armed-eval` judging that ledger into
`armed.sized.verdicts`. It holds no keys, sends nothing, and reads no chain state
of its own beyond the executor's code hash at boot.

- [ ] **Step 1: Write the failing tests:** `the_shadow_engine_never_reaches_the_worker`
  (a sized `Arm` from a shadow engine doesn't reach `ArmedLive`);
  `the_two_ledgers_stay_apart`; `the_sized_pass_cannot_delay_the_live_one`
  (a stalled sized pass leaves the live pass's timing unchanged);
  `a_wrong_code_hash_arms_nothing`; `the_l1_read_is_once_a_minute`;
  `an_l1_spike_raises_the_reservation`; and `the_sized_section_absent_changes_nothing`.
- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime armed::run`.
  Expected: FAIL.
- [ ] **Step 3: Implement them.** The section goes in all four files, so R41's
  drift test still sees seven differing fields, with `mode: shadow` and the
  pre-deploy `runtime_code` override R37 used. `ARMED.md` gains R40.1's section:
  how to read the report, how to flip, and how to roll back to R39.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Run all five gates** and `python3 -m pytest
  scripts/data/arb_census`. Expected: as R41's, with the two known-red scripts
  unchanged.
- [ ] **Step 6: Commit.**

```bash
git add crates/apex-runtime/src/armed/run.rs ops/canary.yaml ops/shadow.yaml ops/canary.denim.yaml ops/shadow.denim.yaml ops/host/ARMED.md
git commit -m "feat(armed): SizedEngine in shadow beside the live lane (Task 8.5 R40.1)"
```

- [ ] **Step 7: Self-review the whole branch,** against the spec's sections 2 to 7.

## Phase 5: shadow, then first live (operator steps, each approved at the time)

### Task 16: Shadow, then the go decision

Needs R41's gate D, the five gates, and the operator's "yes, push and deploy
<rev>". `LeanExecutor3` is not yet on chain, so the shadow simulates with the
`runtime_code` override.

- [ ] **Step 1: The L1 bound, on its own.** After gate D, as its own config
  change, set `l1_fee_bound_wei: 1000000000000` (decision 3), but only if Task 1's
  receipts and the oracle show 1e12 covers the signed transaction. If not, use
  the smallest round figure that does. R39 gets it too, so the first live step
  compares like with like.
- [ ] **Step 2: Deploy.** `ops/host/deploy.sh`, then confirm the canary boots
  with R39's live numbers unchanged, and `armed-sized.jsonl` growing. If two
  engines cost too much CPU (a live pass's p95 up by more than 20%), take the
  spec's fallback, one day with live off, and ask first.
- [ ] **Step 3: One post-Denim UTC day.** Pass means no correctness failure:
  - book and chain storage agree for every armable pool;
  - the mirror matches the real-contract simulation on every simulated fire;
  - the reservations and the budget hold, with no stale pool armed;
  - overlapping arms behave as Task 9 says.
- [ ] **Step 4: Set `maxSteps`.** From the verdicts' `beyond_steps`: raise it, up to
  what fork gas allows, if truncation left real money. Otherwise it stays 8.
- [ ] **Step 5: Report to the operator,** with early exits, capped and truncated
  value, and the paired replay against R39. These are telemetry, not gates.

### Task 17: First live, at 16 senders

Each step is the operator's explicit yes. The owner broadcasts.

- [ ] **Step 1: Deploy `LeanExecutor3`.** Forge clean, `forge build
  script/DeployLeanExecutor3.s.sol`, a dry run, then the owner broadcasts. Check
  the printed code hash against `armed.sized.executor3.code_hash`.
- [ ] **Step 2: Authorize** the 16 senders in one batch with `setExecutors`.
- [ ] **Step 3: Flip:** `armed.sized.mode: live`, `armed.mode: shadow`. The same
  sender count as R39, so the step measures the executor and ranking alone.
  Rollback: flip them back; `LeanExecutor2` stays deployed.
- [ ] **Step 4: Run one UTC day,** then report realised net against R39's.

## Phase 6: scale (after first live)

Not needed at 16 senders: R39's reads stand there, so this waits until the
executor has earned the extra senders.

### Task 18: Landings from the executor's logs

**Files:** Modify: `armed/run.rs`, `shadow/mod.rs` (the feed's log filters),
`armed/live.rs`. Test: in those modules.

**Interfaces:** Produces `ArmedLive::on_executor_log(sender: Address, tx: B256, now_ms: u64)`,
which fetches that receipt and calls `land`, and a feed option adding
`armed.sized.executor3.address` to the canonical log filter.

- [ ] **Step 1: Write the failing tests:** `a_traded_log_lands_that_sender`;
  `a_no_trade_log_lands_that_sender`; `a_log_for_an_unknown_tx_is_ignored`; and
  `a_duplicate_log_lands_once`, since a replay after a reconnect repeats logs.
- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime on_executor_log`.
  Expected: FAIL.
- [ ] **Step 3: Implement them.** The filter adds one address, not a subscription,
  so the WS byte count barely moves. Confirm with R41's by-subscription counter.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/apex-runtime/src/armed/run.rs crates/apex-runtime/src/armed/live.rs crates/apex-runtime/src/shadow/mod.rs
git commit -m "feat(armed): landings from the executor's logs (Task 8.5 R40.1)"
```

### Task 19: Local nonces and balances, and the rate limiter

**Files:**
- Create: `crates/apex-runtime/src/armed/tracked.rs`
- Modify: `armed/live.rs`, `armed/run.rs`

**Interfaces:** Produces:

```rust
pub const SWEEP_EVERY_MS: u64 = 2_000;

pub struct Tracked { /* per sender: nonce, balance, last_read_ms; the sweep's position */ }
impl Tracked {
    pub fn seeded(reads: impl IntoIterator<Item = (Address, u64, u128)>, now_ms: u64) -> Self;
    /// The sender the sweep checks now, if SWEEP_EVERY_MS has passed: round robin.
    pub fn sweep(&mut self, now_ms: u64) -> Option<Address>;
    pub fn nonce(&self, a: Address) -> Option<u64>;
    pub fn balance(&self, a: Address) -> Option<u128>;
    pub fn landed(&mut self, a: Address, cost_wei: u128);
    /// A send refused for nonce or balance: whether to re-read now.
    pub fn should_reread(&mut self, a: Address, now_ms: u64) -> bool;  // at most once a minute a sender
    pub fn reread(&mut self, a: Address, nonce: u64, balance: u128, now_ms: u64);
}
pub struct SendLimiter { /* token bucket */ }
impl SendLimiter { pub fn new(per_s: u32) -> Self; pub fn admits(&mut self, now_ms: u64) -> bool; }

// LivePolicy gains
pub tracked: bool,          // false under R39: its reads stay as they are
pub send_rate_per_s: u32,   // 0: no limit
```

With `tracked`, a bound passing with no landing log sends again on the same
nonce with no read, and `balance()` answers from `Tracked`. The sweep calls the
existing `landing_check`, so a revert is costed and its pending slot cleared as
today. A slot's reservation is held until a log or the sweep settles it; a send
on that nonce shares it, as `admits` already does.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn a_landing_moves_nonce_and_balance();
#[test] fn a_bound_passing_reads_nothing();
#[test] fn the_sweep_is_round_robin_at_a_fixed_rate();
#[test] fn the_sweep_finds_a_revert_and_costs_it();     // no log, nonce moved: the day's loss includes it
#[test] fn a_reservation_holds_until_the_nonce_is_known();
#[test] fn a_refusal_rereads_once_a_minute();
#[test] fn funding_shows_after_the_next_reread();       // Review Focus 5
#[test] fn a_restart_rebuilds_from_ledger_and_one_read(); // Review Focus 4: pending txs keep their nonces
#[test] fn reads_do_not_grow_with_senders();            // a counting fake chain, 16 against 64 senders, a thousand heads: equal
#[test] fn the_limiter_holds_the_rate();
#[test] fn the_limiter_skips_as_rate_not_budget();
#[test] fn untracked_keeps_r39s_reads();                // policy.tracked false: reads as before
```

- [ ] **Step 2: Run them.** `cargo test -j 3 -p apex-runtime tracked armed::live`.
  Expected: FAIL.
- [ ] **Step 3: Implement them.**
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add crates/apex-runtime/src/armed/tracked.rs crates/apex-runtime/src/armed/live.rs crates/apex-runtime/src/armed/run.rs crates/apex-runtime/src/armed/mod.rs
git commit -m "feat(armed): reads that do not grow with senders (Task 8.5 R40.1)"
```

### Task 20: 32, then 64

Each step is the operator's yes.

- [ ] **Step 1: Generate the keys** on the host (`keys::generate`), authorize in a
  batch, and fund in steps: tier-1 senders 0.001 ETH each, tier-2 senders 0.005
  ETH, about 0.08 ETH at 64. Set `send_rate_per_s` to cover the new count.
- [ ] **Step 2: Run one UTC day** at each step. Go up when:
  - realised net from receipts was positive and no lower than the last step's;
  - the fill has positive-EV candidates waiting for the new senders;
  - no safety or accounting invariant failed, and BlockPI was not exhausted.

  Stop if a sequencer limit appears or the added senders earn nothing. A sender
  step risks only its funding and what the unchanged budget allows.
- [ ] **Step 3: Close out.** With 64 senders stable, propose a separate change
  deleting `Engine`, the rungs and `Lean2` arming.

### Task 21: The daily report (only when needed to aim a fix)

**Files:**
- Create: `scripts/data/arb_census/coverage.py`,
  `scripts/data/arb_census/test_coverage.py`,
  `ops/host/apex-coverage.service`, `ops/host/apex-coverage.timer`

**Interfaces:** Consumes `var/census/race-<day>.json.gz`, the sized and R39 arm
ledgers, their verdicts, and `var/armed/armed-live.jsonl`. Produces
`var/census/coverage-<day>.json`:

```text
{day, rival_usd: {unarmed, armed_not_eligible, eligible_taken_first, ours},
 groups: {total, armed_90pct_of_time}, routes_armed,
 coverage,                      // covered share of rival value
 ours: {traded, capped, truncated, early_exit, reverted},
 net_wei, beyond_cap_usd, reread_counts, rearms_per_hour,
 pairs: {r39_net, sized_net, ci_low, ci_high}}   // pessimistic landing, bootstrapped by market move
```

**Covered** is the spec's rule: a rival trade is covered when one of our arms on
its route fired at or after its trigger `(block, tx index)`, and at or before
the rival's own transaction. Every rival trade on a target route gets exactly
one class, and every inclusion of ours gets exactly one.

- [ ] **Step 1: Write the failing tests:** `test_each_rival_trade_has_one_class`;
  `test_a_fire_after_the_rival_does_not_cover`; `test_a_fire_at_the_trigger_covers`;
  `test_an_unarmed_route_is_unarmed`; `test_our_own_landing_is_not_a_rival`;
  `test_groups_armed_counts_time_not_arms`; and `test_paired_ci_resamples_by_move`
  (two arms on one move resample together).
- [ ] **Step 2: Run them.** `python3 -m pytest scripts/data/arb_census/test_coverage.py -q`.
  Expected: FAIL.
- [ ] **Step 3: Implement it.** It runs after the census, using no RPC. The
  timer ships disabled.
- [ ] **Step 4: Run them.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add scripts/data/arb_census/coverage.py scripts/data/arb_census/test_coverage.py ops/host/apex-coverage.service ops/host/apex-coverage.timer
git commit -m "feat(census): the daily coverage report (Task 8.5 R40.1)"
```

