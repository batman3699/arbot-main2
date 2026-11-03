# Armed lane v4.1: every route, any move (Task 8.5 R40.1)

2026-10-30, revised 2026-10-31 after an independent review
(`~/Documents/APEX_Armed_Lane_Deep_Research_Profit_Capture_2026-10-31.md`).
The operator asked for the original after the "why do other bots make money
and I don't" audit: "yes, write the design for item 1". **Approved
2026-11-02: all six decisions at the end, as written.** Every call I made is
marked **(my call)**. What changed after the review, and what didn't, is in
"Review response" at the end. The plan is
`docs/superpowers/plans/2026-11-02-r40-1-any-size-armed-lane.md`.

## Why

**The market is real, and it trades on our own routes.** The host's daily
census race files (`var/census/race-<day>.json.gz`) record every arbitrage
landed on Base:

- **The week of 10-23..10-29:** $183.8k of trades of $1 or more, about
  $26k a day. Two outliers are excluded: a $43k exploit and a $51k
  one-off. The total is rising (10-29: $51k).
- **Timing:** over 90% lands in the trigger's own flashblock. Only a
  pre-armed validity transaction, or spam, reaches it.
- **On today's 50 target routes:** about $0.9k to $21k a day.

**What happened in our first 6.2 live hours** (10-29 17:49–24:00 UTC):

| | Rival $ on our target routes |
|---|---|
| All rival trades (1,242; 92% in the trigger's flashblock) | **$2,533** |
| …on routes we never armed: only **13 of 50** routes were ever armed | ~$1,366 |
| …on armed routes, with no fire of ours within 2 blocks (944 of 1,020 trades) | ~$1,141 |
| …where we did fire, and were outbid or refused for sender balance | the rest |
| **What we took** | **about $0.30** |

These are rivals' gross figures, not money available to us. Rivals pay
about 20% of the value as priority fee, and what we could net depends on our
size, gas and bid. The route set is today's target file, because the 10-29
file isn't kept.

**Two design choices cause both large leaks:**

1. **Fixed size.** An arm is a pre-signed plan with a fixed size and fixed
   hop minimums. It pays only if the move lands inside a narrow band around
   one predicted step. So the engine spreads six rungs over each route
   (`RUNG_OFFSETS = [0, 1, 2, 4, 8, 16]`).
2. **Sender-limited.** Each pending arm holds one sender, and there are 16.
   Ranking by expected value packs the far rungs onto a few routes. In the
   live window, rungs 5–6 held most arms, and they almost never fire.

Rivals do neither. They watch every pool, and their trade is sized when it
executes. The 20:47:49 burst shows it: one bot swept about 20 pools in one
transaction for $3,180, at a 2.5 gwei bid.

## Prerequisite: Denim, 2026-11-05 18:00 UTC

**Verified 2026-10-30:** Base's stable node release `base/base` v1.5.1
(2026-10-22) says "Operators must upgrade to this release by November 5 at
18:00:00 UTC for the Denim hardfork" on mainnet (timestamp 1793901600).
docs.base.org still says only "November 2026".

Denim ends flashblocks, `pendingLogs` and the `pending` tag, and makes
blocks 200 ms. R36/R37 already built the switch as configuration
(`feed.pending_logs: false`, no `flashblocks:` section,
`armed.block_time_ms: 200`; `ops/host/ARMED.md`, "The Denim day"), and the
cadence guard stops sends while the configuration and the chain disagree.
What has never been done is to run it against a 200 ms chain, and three
gaps are known: the evaluator places fires by flashblock (`eval.rs`
`LAST_INDEX`, `next_flashblock`); the live worker reads the owner's stops on
every head, which becomes 5 reads a second; and the switch needs someone at
18:00 UTC (07:00 NZDT).

**So Denim readiness is its own small project (R41), done first.** R40.1 is
built after the cutover, on a lane that has run a few days on Denim, and its
fitted numbers (tier thresholds, bids, bands) come from post-Denim days. One
change at a time: Denim is not combined with `LeanExecutor3` or a bid change.

## Goal

1. **Every trigger leg worth holding holds an arm**, by marginal expected net,
   within the sender and budget limits of section 5.
2. **Any move past the arm's threshold is traded at its best size,**
   computed on chain from the pools' state at execution.
3. **Realised net goes up.** This is the measure of success. A capture
   needs four things, and each is measured separately:
   1. **armed:** an arm on the route was pending when the move came;
   2. **eligible:** its predicates held;
   3. **first:** it was included before a rival closed the gap;
   4. **net positive:** it paid more than its gas, L1 fee and bid.

**Fire coverage** (the share of rival value on target routes at moments
when one of our arms was eligible) is the diagnostic for 1 and 2. Today it
is about 7% of rival trades on armed routes, and 13 of 50 routes are armed.

## Non-goals

- **The refused and missing pools** (the audit's item 2): the v4
  protocol-fee refusal, and the 12–16 target pools missing from the book.
  Breadth here covers loaded pools only. They may be worth more than extra
  senders, so they are measured separately and come next.
- **The new Slipstream CLFactory `0x9cb22058…a76`** (item 3).
- **A new bid model** (item 4). Tiers use R38's `best_bid` / `best_share`
  machinery as it stands, refit on post-Denim days.
- **Paths of 3 or more hops, and multi-pool sweeps.** The sizer is two-pool.
- **One arm choosing among several routes** from one trigger. It would save
  senders, but costs sizing gas per candidate. Later, if grouping (section
  4) shows many routes per trigger.
- **The reactive lane.** It stays stopped.
- **Raising the size caps or the daily budget.** The caps move on chain
  unchanged (5 WETH, 12,000 USDC), and the budget stays 0.01 ETH.

## Anti-goals (a failure even if it works)

- An included loss past the day's budget.
- BlockPI run out of request units by per-send balance or nonce reads.
- A trade whose on-chain size disagrees with the Rust mirror on a recorded
  book, beyond rounding.
- A secret printed, logged or held off the host.
- Arms on a book that disagrees with chain storage.
- A trade the executor lets through whose own gas, L1 fee and bid exceed
  its gross.
- More transactions without more net. Breadth that adds landings and no net
  is a failure.

## Constraints

Carried over unchanged:

- The owner broadcasts every deploy, authorization and funding transaction.
  The `.env` PRIVATE_KEY is the owner's and never trades.
- No new environment variables; config-file fields are fine.
- Merge, push and deploy each need the operator's explicit yes.
- All five CI gates pass before a push.

New:

- **The executor is not upgradeable**, so this is a new contract,
  `LeanExecutor3`. `LeanExecutor2` stays deployed for rollback.
- **One pending transaction per sender** (Cobalt replaces by sender and
  nonce). Breadth costs senders, and each sender costs funding.
- **Denim.** Nothing may depend on `flashblock_index` predicates or
  flashblock-level timing. Block bounds are counted in blocks, from
  `armed.block_time_ms` (10 s is 50 blocks after Denim).

## Facts verified, and what still needs checking

Verified, in this repo or on chain:

- **Predicates are AND'ed storage comparisons.** Today's arms carry a
  trigger threshold, the other pool's price band, a liquidity floor and a
  block bound (`armed/predicate.rs`), and 26 landings honoured them.
- **The builder evaluates predicates immediately before execution.** A
  transaction whose predicates fail is not included and pays nothing.
- **`LeanExecutor2` today** (`contracts/lean/LeanExecutor2.sol`): it
  borrows `amount` from Balancer, Morpho or PoolManager, then runs the hops.
  Each hop has an exact `minOut` (Aerodrome v2 is asked for exactly it). A
  profit floor applies, and the profit goes to `profitRecipient`.
- **Our 26 armed landings used 227–275k gas**, against a 614,600 gas limit.
  Rivals use 190–740k.
- **The L1 fee is negligible, and our budget's bound for it is far too
  high.** Three landings' receipts (10-30): 2.7e9, 6.3e9 and 7.6e10 wei, at
  L1 blob base fees of 8.7M to 257M wei. The configured
  `l1_fee_bound_wei` is 5e13, 660 to 18,000 times what was paid. For
  comparison, landing 0xf89bdb2d grossed 14,616 USDC units ($0.0146) and
  paid 232,636 gas × 10.3 Mwei = 2.4e12 wei ($0.0055) on L2.
- **Base fee is about 0.0003 gwei** (effective price 10,300,001 wei on a
  10,000,000 wei bid). A bid is almost all of the gas price.
- **The live ledger already computes net from receipts** (`net_wei` in
  `var/armed/armed-live.jsonl`), not from the `Profit` event.
- **What a sized trade costs, all in** (forked Base at block 53,155,787, each
  call its own transaction, 2026-11-03): a `NoTrade` 82k to 113k by venue
  pair; a trade with no boundaries 323k to 468k; 469k at three boundaries and
  701k to 715k at seven, on PancakeSwap's spacing-1 pool. A square root seeded
  from x / 2 had cost up to 479k of that at seven; seeded from its bit length,
  it costs nothing worth measuring.
- **What the sizer's reads cost** (forked Base, 2026-11-02, `via_ir`, inside
  one transaction). The first figure is cold, the second warm.

  | Read | Uniswap v3, Pancake | Slipstream |
  |---|---|---|
  | `liquidity` | 2.9k | 3.3k |
  | `tickBitmap` | 3.1k | 3.9k |
  | `ticks` | 9.9k, 1.9k | 14.5k, 2.5k |
  | `fee` | 0.7k | 20–67k, 7–24k |

  `slot0` is about 9k on Slipstream (a proxy). Slipstream's `fee()` varies
  with its TWAP search, so it is the one large read. Its cold cost is paid by
  the swap anyway, so a trade adds only the warm read; an early exit pays it
  cold.
- **The venues' views differ past their first words.** A typed Slipstream
  `slot0` reverts on Uniswap v3 and Pancake, because its last field is a
  `bool` where theirs is a fee. `slot0` and `ticks` must be read raw: the
  first two words of each are the same on all three.
- **The L1 oracle today:** `getL1FeeUpperBound(600)` is 1.48e9 wei.
- **Sequencer limits** (`base/base` v1.5.1 and `main`, 2026-11-03): no send-rate
  limit in the node. Each validity transaction needs a block expiry inside the
  wall-clock window, and a predicate count within the batch limit, which R39's
  arms already meet. `main` holds validity transactions in their own pool, at
  reth's pending limits by default. Any rate limit in front of the node isn't
  visible from source.

Still to read from the host, for launch values and not holding up the build:
R39's peak accepted send rate, its revert share, every landing's L1 fee, and how
many (trigger pool, direction) groups the target routes have.

## Design

### 1. The idea

An arm stops being a fixed plan, and becomes a standing order:

- **When:** the trigger pool's price passes threshold X, with the other
  pool inside band B.
- **Then:** trade the gap at whatever size is best at that moment, and only
  if it pays at the transaction's actual gas price.

The predicates stay as they are. The calldata no longer names a size; the
executor computes it. One arm on each trigger leg of each route then covers
every move past its threshold.

Larger moves can carry more bid through a second tier (section 3).
Breadth, meaning one arm per (trigger pool, direction) group before any
extra, becomes a ranking rule (section 4).

### 2. `LeanExecutor3`: sized at execution

**Entry point.** `run(bytes plan)` with a new codec, `LeanPlan3Codec`.

- **Header:** lender, start token, **size cap** (in the start token),
  deadline, and the cost terms below.
- **Each hop:** the venue kind, the pool (for v4, its key), and tokenOut.
- **Gone:** `amount` and the hop minimums. The executor computes the size,
  and the profit floor replaces the minimums.

**Kept checks:** trader-only access, the owner's circuit, the deadline, the
plan's shape, and the profit floor. **New:** a batch
`setExecutors(address[], bool)`, so one owner transaction authorizes all
senders.

**The profit floor is priced at execution** (changed after the review). An
arm can wait a whole window, and its gas depends on the size chosen, so an
arm-time estimate is not a floor. The contract measures:

```
cost_wei = (used_in_run + used_in_callback + gasOverhead) × tx.gasprice + l1Bound
floor:     out ≥ borrowed + lenderFee + toStart(cost_wei) + minNet
```

- Gas is measured a frame at a time: `run`'s work before it borrows, and the
  lender's callback's from its own entry. One `gasleft()` against the start
  would also count what the 63/64 rule holds back in each calling frame,
  which is reserved, not spent (corrected while building: about 30k gas at a
  1M limit). `tx.gasprice` is the effective price (base fee plus our bid),
  read at execution.
- `gasOverhead` (in the plan) covers what the measurement can't see: the
  intrinsic gas and calldata of this exact transaction, the lender's own
  work around the callback, and the work after the floor (event, transfer). The engine sets it at signing from
  fork-measured constants, rounded up. Refunds are ignored, which only
  overstates the cost.
- **The contract's cost is a guard, not the accounts.** Realised net comes from
  the receipt, and `NoTrade`s and reverts are charged at their real cost.
- `l1Bound` (in the plan) bounds the L1 fee. **(my call)** Twice the
  `GasPriceOracle.getL1FeeUpperBound(tx size)` that the worker reads once a
  minute. The receipts above show it is about 0.3% of the L2 cost.
- `toStart` converts wei to a non-WETH start token at a rate in the plan:
  the arm-time rate plus **(my call)** 5%, rounded up. For WETH it is 1.
- `minNet` is 1: the smallest value that stops rounding from recording a zero.
  The ranking, not a fixed floor, refuses inferior arms.

So a trade the executor lets through never nets below zero after its own
gas, bid and L1 fee, except by the L1 bound being exceeded, which the ledger
sees in the receipt.

**What the executor reports.** `Profit(gross)` becomes `Traded(start,
amountIn, out, lenderFee, costWei, steps, flags)`, and the early exit emits
`NoTrade(reason, steps)`. The flags mark a size held at the cap or at
`maxSteps`. The ledger joins the receipt for gas used, effective price and
L1 fee, as it does today.

**The sizer** is a two-pool walk over initialized ticks, the same
arithmetic as `apex_math`'s CL swap, step for step:

1. **Read each pool's state at execution.**

   | Venue | How it is read |
   |---|---|
   | v3, Pancake, Slipstream | through the pool's own views: `slot0`, `liquidity`, `fee`, `tickBitmap`, `ticks`. Each venue's ABI differs (Slipstream's `slot0` and `ticks` return other fields). **(my call)**: views cost about 1k gas more a read than raw slots, but carry no layout risk. Slipstream's fee is dynamic, so it is read at execution |
   | Uniswap v4 | through `extsload` on PoolManager: slot0 and liquidity at the slots R37 verified, and the tick-bitmap and tick-info slots, verified on chain the same way before use. Hooks are allowed only from an allowlist of code hashes, checked at inventory admission, with no swap-altering permission (no `beforeSwap` delta, no dynamic fee) |
   | Aerodrome v2 | reserves and the factory fee; volatile pools only, and a stable pool is refused |

   The reads warm the slots the swaps then touch, so part of their gas
   comes back as cheaper swaps.
2. **Find the best input for the current segment pair.** Inside one
   initialized-tick segment, each pool is a constant product on virtual
   reserves. The two-hop output is then `a·Δ / (b + c·Δ)`, and the best
   input has the closed form `Δ* = (√(a·b) − b) / c`, profitable only when
   `a > b`. This is local to the segment pair; it is not a global formula.
3. **Cross a boundary if needed.** If Δ* takes either pool past its next
   initialized tick (one bitmap word, cached), advance that pool to the
   boundary. Apply its `liquidityNet`, carry the consumed input and output
   forward, recompute the next segment pair, and repeat. Each crossing's gas
   (a per-venue constant) is charged in the objective, so a crossing that
   earns less than its gas is not taken.
4. **Stop** at the optimum, at the size cap, or after `maxSteps` crossings
   (a constant; **(my call)** 8, set by fork-measured gas). At the step
   bound, the size is the last boundary reached: smaller, still profitable,
   and flagged as truncated, not called best.
5. **Exit early when it doesn't pay.** If the predicted profit is below the
   predicted floor (gas so far plus the swaps' predicted gas, at
   `tx.gasprice`), return before borrowing with `NoTrade`. That costs the
   reads (measured: about 20k on a v3 or Pancake route, and up to about 170k
   on Slipstream to Slipstream), not two swaps. It is a loss, and section 3
   keeps it rare.
6. **Otherwise check the lender holds Δ** (`NoTrade` if not), **and trade:**
   borrow exactly Δ, run both hops as exact-input
   swaps with no price limit, and check the floor on real balances.

**Rounding is always against us:** outputs round down, costs and repayments
up. So a predicted profit never exceeds the real one, and a trade that
passes the early exit cannot fail the final floor on rounding. Overflow,
unknown tick data or an unsupported pool fails closed.

**Why the walk, not a port of `best_size`.** `best_size` (R27.2) is a
search over a quote function: 12 quotes, each a two-pool tick walk. On
chain that is 12 walks. The closed-form walk needs one. What must be exact
is the swap arithmetic, and that is what the mirror proves step for step.
`best_size` stays the reference for optimality: the walk's net must reach
`best_size`'s net to within rounding. A sizer that is off loses some value,
never money: the floor is checked on real balances.

**Why not a price limit** **(my call)**. The cheaper alternative is to
compute a target price from in-range liquidity alone, and swap hop 1 to that
price limit. But the pool's own crossings then decide how much is bought,
and hop 2 can overshoot into thinner liquidity and fall below the floor.
That reverts, and a revert pays gas.

**The Rust mirror.** It lives in `apex_math` and is used three ways:

- the engine predicts with it;
- the evaluator judges fires with it, including the value a capped trade
  left beyond the cap;
- the differential tests compare it against the contract.

Fixtures go in `crates/apex-exec/tests/fixtures/lean3_plans.json` and the
contract's forge tests, as `lean2_plans.json` does today. The test matrix:
single and multi-tick walks, both directions, positive and negative
`liquidityNet`, a tick exactly at a boundary, zero and out-of-range
liquidity, fee rounding, USDC's 6 decimals, v4 with and without an allowed
hook, Aerodrome volatile, and the `maxSteps` and cap clamps. Contract and
mirror agree to the unit; fork tests land each venue pair at the mirror's
size and output.

### 3. Arms: one per group, a bid tier on top

**The threshold.** A route's arm on trigger leg L sits at the smallest
move of L's pool that pays the arm's cost at its bid, with the other pool
taken at its band's adverse edge. This is `threshold::rungs`' search for
rung 1, with the band's edge in place of the pool's current price. So
whenever the predicates hold, the trade pays: an early exit happens only
on state the predicates don't see (the tick ladder beyond the active tick,
a base-fee jump). The evaluator counts these, and each one is treated as an
anomaly, not a cost of doing business.

**The band** can be wider than today's 5 bps:
- The executor sizes at execution, so the band no longer has to keep a
  fixed size profitable. It only has to keep the threshold honest.
- Wider means fewer re-arms, but a higher threshold, which misses moves
  just past it. Wider is not more profit.
- **(my call)** The band cap rises to 20 bps or ticks (`band_cap_bps`), as
  an upper limit. The engine chooses each route's band by expected value
  (re-arms against missed fires), from post-Denim shadow data.

**Tiers.** Tier 2 is the same route-leg with a higher threshold and a
higher bid, priced by R38's model at that move's surplus.

- **(my call)** Two tiers at most. Tier 2's threshold is fitted from the
  census: the surplus quantile at which R38's best bid at least doubles,
  fitted on three post-Denim days and checked on a fourth. 10× tier 1's
  surplus is only the placeholder until that fit exists.
- Tier 2 is limited by the budget, not senders (section 5).

**Tiers of one route-leg don't contest each other.** On a large move both
are eligible; the higher bid lands first, sizes the whole gap, and pulls the
trigger pool back below both thresholds. The other is then not included and
pays nothing.

**Arms on different routes sharing a trigger pool can both be eligible**
(corrected after the review: the first draft overclaimed here). They don't
cost each other money:
- each bid is a fixed priority fee, paid only on inclusion, so one arm
  doesn't raise another's price;
- the second to land sizes only what is left, and only trades if that pays
  at the execution-time floor; otherwise its predicates usually fail
  first.

What they can cost is **value** (the worse route landing first) and
**senders** (two arms on one correlated move). Section 4 handles both, and
phase 3's tests prove the remainder behaviour with 2–20 eligible arms on
overlapping routes, with and without a rival in between.

**The fingerprint** that forces a re-arm drops size and mid. It keeps the
threshold, the band edges and the bid. Re-arms should fall sharply, which
the sequencer's rate limit may require.

### 4. Ranking: by marginal expected net

`Engine::tick` replaces "the top `max_arms` rungs by `rank_value`" with a
greedy fill under two limits: senders, and the budget's pending reservation
(section 5).

1. **Value each candidate by its expected net:**
   `P(trigger) × P(wins) × expected trade net − P(no trade) × its cost −
   P(revert) × its cost`. Trade net is the mirror's sized output less principal,
   lender fee, gas, L1 fee and bid, integrated over the move distribution.
2. **Take the best** per sender and unit of reserved budget, and stop at no
   positive value.
3. **Re-value its group,** the candidates sharing its (trigger pool, direction):
   replay the chosen trades on the book, in order, and value the rest only on
   what remains. A tier 2 counts only what it adds to its tier 1.

A candidate whose tier 1 can't pay at any move up to `MAX_STEP` holds no
sender. A sample's zero chance of winning isn't evidence of zero, so it is
floored: such an arm ranks last, and is not excluded. A greedy fill is enough to
start; a knapsack only if replay shows the greedy leaving value.

R39's cap filter goes away. The cap is now a clamp inside the executor:
over-cap moves are traded up to the cap instead of skipped, and the
evaluator reports what the cap left.

### 5. Senders, funding and budget

**The budget rule doesn't change** (changed after the review). Every
pending arm's worst case counts against the day's 0.01 ETH, as today:
`gas_limit × (2·baseFee + bid) + l1_fee_bound_wei`. The first draft's
"largest K = 8" was wrong: 8 tier-2 worst cases at 1M gas and 2 gwei are
0.016 ETH, more than the whole budget.

**What makes breadth fit is the L1 bound.** At 5e13 wei it is 660–18,000
times the L1 fees paid, and it is most of a tier-1 arm's worst case. With
`l1_fee_bound_wei` set to **(my call)** 1e12 (13 times the highest seen) and
tied to the oracle's upper bound:

| | Gas limit, bid | Worst case each | 64 tier-1 arms |
|---|---|---|---|
| Tier 1 | ~1.0M, 0.01 gwei (0.005–0.05) | ~1.2e13 wei (0.6e13–5.2e13) | ~0.0007 ETH |
| Tier 2 | ~1.0M, 2 gwei | ~2.0e15 wei | n/a |

So 64 tier-1 arms reserve under a tenth of the budget, and the rest holds
about 4 tier-2 arms at 2 gwei. (Measured while building, on a fork: eight
steps on PancakeSwap's spacing-1 pool use 683k gas, so the 1.0M limit holds
with a quarter for refunds.) The ranking decides whether a tier-2 arm is
worth its reservation. Rivals' 51 gwei bids on big gaps are out of reach
under this rule; that is deliberate.

**A later option, not in R40.1: an on-chain fuse.** A counter in the
executor that every failed inclusion increments (an early exit, or a revert
caught around the trade), and a predicate in every arm requiring it below N.
The sequencer would then stop including any arm after N failures, a hard
bound that would let the reservation be N worst cases instead of all. Only
if tier 2 proves budget-bound and worth it.

**How many senders.** **(my call)** Up to 64, ramped 16 → 32 → 64
(section 7). More only while the last step's realised net was positive. Senders are a config
count (`max_arms`); the keys file holds that many keys.

**What each sender needs.** Tier-1 senders 0.001 ETH each, tier-2 senders
0.005 ETH. For 64 senders with about 4 for tier 2: about 0.08 ETH, funded
by the owner in steps.

### 6. The live worker

- **`readable()` reads the new plan.** The "size" skip is gone, because the
  executor clamps to the cap.
- **Chain reads don't grow with the sender count or the block rate.**
  BlockPI's request units have run out three times already. Today, at 16
  senders, the worker reads:
  - a sender's nonce whenever a pending transaction's bound passes, about
    1.2 a second;
  - its balance, cached for 60 s (`BALANCE_TTL_MS`);
  - the owner's stops, in one multicall per head (5 a second after Denim;
    R41 puts it on a timer).

  At 64 senders and a 10 s window, the nonce reads alone would reach about
  6 a second.
  - **The rule:** a sender's balance and nonce are read at boot, on a
    refusal, and when one of its transactions is seen landing. Otherwise
    they are tracked locally: a sender's balance changes only when its own
    transaction lands, or when the owner funds it.
  - Landings are detected from the blocks the node already streams, or from
    one batched read per interval. That choice is the builder's.
  - **The bar:** the lane's reads stay within the post-Denim R39 level,
    measured by R28.1's per-method HTTP counters.
- **The L1 bound** is read from `GasPriceOracle.getL1FeeUpperBound` once a
  minute, one call.
- **The send window** stays 10 s in this release. A longer one holds senders
  and goes stale on the USDC rate, for value nobody has shown.
- **Local tracking and the rate limiter wait for 32 senders:** at 16, R39's reads
  stand.
- **A send-rate limiter** (a config field) holds sends to whatever phase 1
  finds the sequencer allows.
- **Unchanged:** the ledger's write-ahead, replacement bumps, expiry,
  landing checks and halts. `executor2` becomes `executor3` (address and
  code hash), and live refuses any other.

### 7. Evaluation and gates

**The evaluator** judges a fire at the size the executor would compute at
the fire's state, using the Rust mirror. Today it judges the fixed plan.
Its landing model is block-based after Denim (R41).

**Every rival trade on a target route gets one class:** unarmed, armed but
not eligible, eligible but taken first, or ours. **Every inclusion of ours
gets one:** traded, capped, truncated, early exit, or reverted. These split
the four parts of the goal, so a fix can be aimed at the one that leaks.

**The daily report** is a new script under `scripts/data/arb_census/`. It
runs after the census and joins the race file to our arm, verdict and ledger
logs. It reports:
- rival value on target routes, by class;
- groups and routes armed, and for how long;
- fire coverage;
- our inclusions by class, and realised net from receipts;
- the value the cap left.

**When a rival trade counts as covered.** The race file names each rival
trade's trigger (block, tx index). The trade is covered when one of our arms
on its route **fired** at or after that trigger and at or before the rival's
own transaction. Fired means the evaluator found the arm's predicates first
holding. Coverage is the covered share of rival value.

**What shadow can and can't show.** Shadow proves mechanics and coverage.
It can't prove who lands first: the evaluator's landing model is the weak
part (R38: "won by position" was 93% artifact; the pessimistic estimate
tracked 9 of 11 landings). So realised net is measured live, in small steps.

**Gates:**
- **S (sizing):**
  - The contract equals the Rust mirror on every fixture, to the unit.
  - The mirror's net is within rounding of `best_size`'s on recorded books.
  - Fork tests land each venue pair at the mirror's size and output, and
    the gas at `maxSteps` is measured.
  - The overlapping-routes tests of section 3 pass.
- **C (shadow, on one post-Denim day).** The canary runs the new engine in
  shadow next to the live R39 engine, on the same feed: no live day is given up.
  If two engines in one process cost too much, the fallback is one day with live
  off (about $1.70 forgone). Passing means no correctness failure:
  - book and chain storage agree for every armable pool;
  - the mirror matches the real-contract simulation on every simulated fire;
  - the reservations and budget hold, with no stale pool armed;
  - overlapping arms behave as gate S's tests say.

  Early exits, capped and truncated value, and the paired replay against R39 are
  reported, not gated.
- **L (live ramp).** 16 senders (the same as R39, so the step measures the
  executor and ranking alone), then 32, then 64. Each step runs one UTC day. A
  step goes up when realised net from receipts was positive and no lower than
  the last step's, positive-EV candidates are waiting for the new senders, no
  invariant failed, and BlockPI was not exhausted. Stop if a sequencer limit
  appears or the added senders earn nothing. A positive lower bound on added net
  is required only for raising a risk limit (budget, caps).

## Phasing

0. **R41, Denim readiness** (its own spec and plan): cut over by
   2026-11-05 18:00 UTC with R39 unchanged.
1. **Facts, on Denim:** the sequencer's rate limit, Cobalt's limits, the
   groups, the L1 oracle bound. No code.
2. **The Rust mirror** and its tests against `best_size`.
3. **`LeanExecutor3` and `LeanPlan3Codec`,** with unit, differential, fork
   and overlapping-route tests. Gate S.
4. **Engine and evaluator:** expected-net ranking, tiers, the wider band, the
   new plan, the L1 bound, and the sized judgement.
5. **Shadow on the host,** next to live R39. Gate C. Then the owner deploys
   `LeanExecutor3`, and first live at 16 senders.
6. **Scale:** landings from logs, cached chain reads, the rate limiter, then 32
   and 64 through gate L. The daily report comes only if a fix needs aiming.

## Risks

- **The sequencer limits sends or accounts.** Then breadth is capped by
  sends, not senders. Fallback: one arm per trigger group choosing among its
  routes on chain. Fewer senders, more sizing gas.
- **The sizer's gas** (about 20k for two v3 or Pancake pools, more with
  Slipstream's fee read, plus about 5–8k per crossing step) lowers our bid
  per gas a little. Fork tests measure it, and `maxSteps` bounds it.
- **Dynamic fees** (Slipstream) are read at execution, so the sizer sees
  the fee the swap pays.
- **Big moves past the cap** are traded at the cap, not fully. The report
  shows what that leaves; raising the cap is a separate decision.
- **Budget-bound tier 2.** Under the unchanged rule only a few high bids fit.
  If the evaluator shows tier 2 is where the money is, the fuse is the way
  to more.
- **Funding sits in 64 hot keys.** Each key holds at most 0.005 ETH, and
  the owner can revoke any key and trip the circuit.
- **Post-Denim competition is unknown.** The bid contest changes with the
  block cadence. Nothing is fitted on pre-Denim days.
- **Signed predicates.** `base/base` has an opt-in `--validity-signature-mode`
  (default `off`), with a stated rollout to `required`. At `required`, unsigned
  arms are refused, R39's too. Arms would then need an EIP-712 sidecar signature.
  The refusal's text would show it; nothing is built for it yet.
- **Unknown value.** The prize on target routes ranges from about $0.9k to
  $21k a day of rival gross, and what we can net from it is not known until
  gate L measures it.

## What is left to the builder

- The sizer's fixed-point details (`mulDiv`, the square root), within the
  rounding rule above.
- Per-venue reads and tick-bitmap traversal, from the layouts
  `predicate.rs` already verified.
- The exact `maxSteps`, `gasOverhead` constants and per-crossing gas, from
  fork gas.
- The band per route, from the shadow.
- How landings are detected for the local balance and nonce tracking.
- Two engines in one process, or the fallback day.
- The daily report's format.
- Key generation, and the keys file's growth from 16 to 64.

## Review response (2026-10-31)

An independent review recommended approving the direction and revising
before implementation. Each point was checked against the code, the chain
or the release notes.

| Review point | Response |
|---|---|
| Denim may activate 5 Nov | **Accepted, verified:** `base/base` v1.5.1 pins 2026-11-05 18:00 UTC. Denim goes first, as R41 |
| Arm-time `minProfit` is not a floor | **Accepted.** The floor is computed at execution from `tx.gasprice` and gas used (section 2) |
| K = 8 exceeds the 0.01 ETH budget | **Accepted:** 8 × 0.002 = 0.016 ETH. K is withdrawn; every pending worst case is reserved, as today. The fit comes from the L1 bound, measured at 660–18,000× the fees paid |
| Early exits cost gas | **Accepted.** Priced in the ranking, counted as anomalies, gated in C |
| Routes sharing a trigger pool can both fire | **Accepted as a correction.** They can't raise each other's fees, but can waste value and senders: grouping, marginal ranking, and tests |
| The 10× tier boundary is arbitrary | **Accepted.** Fitted on post-Denim days; 10× is a placeholder |
| 150 senders is not a target | **Accepted.** Up to 64, ramped 16 → 32 → 64 |
| Coverage is not profit | **Accepted.** Realised net is the goal; coverage is a diagnostic |
| Port `best_size` exactly | **Modified.** Porting it means 12 walks on chain. The walk is exact, rounds against us, and must match `best_size`'s net. A sizer error can lose value, not money |
| v4 hooks need a code-hash allowlist | **Accepted** |
| Report truncation and the value beyond the cap | **Accepted** |
| `Profit(gross)` is not net | **Already so:** the ledger's `net_wei` comes from receipts. The event is made richer anyway |
| Arrival-aware bids | **Partly.** An armed transaction is pending before the trigger by construction; "late" means not armed, which coverage measures. The four classes split what remains |
| Every ramp step needs a positive lower confidence bound | **Modified.** At today's volumes that stalls the ramp indefinitely. Required for raising risk limits; reported for sender steps |
| Paired replay R39 against R40 | **Accepted** as the C report, with both engines on one feed; realised net decides |
| Verify the live bid model | **Known:** `best_bid` (contest empty, `fit_ok` false at 10-30 02:07). R38's gates on 11-02 would fit flashblock-era contests that Denim ends three days later (decision 6) |

## Review response (2026-11-03)

A second independent review, "build now", with six amendments. Each was checked
against the code, the chain or Base's source.

| Review point | Response |
|---|---|
| Build now; correctness checks, not research gates | **Accepted.** Phases 2 to 4 start now; gates C and L are cut to correctness and realised net |
| Value candidates by expected net, with a state-aware marginal | **Accepted.** It replaces the census's `marginal_usd`, so a task goes away |
| No unconditional base pass in `fill` | **Accepted, with an addition:** `p_win` is floored, or the stop rule would idle senders on unmeasured routes |
| Compare the walk on historical pool snapshots | **Modified.** None hold tick ladders, so real books are compared in shadow on every fire, as the plan had it |
| Record what the cap and the step limit left | **Accepted.** `maxSteps` also moves into the plan, so evidence can raise it without a redeploy |
| Positive `minNet`, receipt accounting | **Accepted:** `minNet` is 1. The accounting was already so |
| Set the L1 bound to 1e12 only if data shows it covers | **Accepted.** It is computed from every landing's receipt before the change |
| Keep the 10 s window | **Accepted** |
| Lender preflight into `NoTrade` | **Accepted.** It costs about 2k gas a trade, and reverses my earlier ruling |
| v4 restrictions at the executor | **Modified.** Delta-returning hook bits, dynamic fee and protocol fee are checked there. The code hash stays at admission: since EIP-6780 code at an address can't change, so an on-chain registry adds an owner transaction per hook and catches nothing |
| Overlap tests on portfolio net | **Accepted** |
| Launch values from existing evidence | **Accepted.** No tier fit; 10× launches |
| Add senders on the model alone, with no dwell | **Modified.** One UTC day a step, and realised net must have been positive. The model has been wrong before (R38's 93% artifact), and realised net is the objective. A sender step's risk is small, so three days wasn't needed either |
| Stop on a feed gap, reorg or block-hash mismatch | **Already so in effect.** R41's head-lag guard stops stale sends, and the predicates and the floor mean a stale book loses a fire, not money |

Scope also moved: local tracking and the limiter now follow first live, since
R39's reads stand at 16 senders, and the daily report waits until a fix needs
aiming.

## Decisions that are the operator's

1. **Approve the direction:** a new executor that sizes on chain and
   prices its own floor at execution, plus group-first arming. This replaces
   the fixed-size rung ladder.
2. **Denim first:** R41 is designed, built and cut over before any R40.1
   code. R40.1's fitted numbers come from post-Denim days.
3. **The L1 bound:** `l1_fee_bound_wei` from 5e13 to 1e12, tied to the
   oracle's upper bound. The budget rule and the 0.01 ETH budget are
   unchanged.
4. **Sender count and funding:** up to 64 senders, about 0.08 ETH in all,
   funded by the owner in steps through the ramp.
5. **The deploy:** `LeanExecutor3` by the owner, after gates S and C. Then
   the ramp of section 7.
6. **R38's bid switch:** hold `best_bid` through Denim, and refit R38 on
   post-Denim days (three fit days and a held-out one, so 11-10 at the
   earliest), instead of switching on 11-02's pre-Denim gates.
