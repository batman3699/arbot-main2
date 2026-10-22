# Armed lane v2: validity transactions where the arbitrage is (Task 8.5 R36)

Draft, 2026-10-23. Asked for by the operator ("check Denim and draft the
design") after R33–R35. Nothing here is approved; the decisions that are the
operator's are listed at the end, and every call I made is marked **(my call)**.

## Why

Three measurements in two days changed what the reactive lane is worth:

- **R33:** on our 52 pools, 80% of the gaps the canary priced die in the
  flashblock right after their trigger's; through Base's sequencer we land one
  or two later. Even landing in the next flashblock every time, the lane was
  worth ~0.01 ETH a day.
- **R34:** Uniswap v4's arbitrage is taken just as fast: 86% of its value in
  the trigger's own flashblock (ours: 79%). No venue changes the race.
- **R35:** that position is reached by public means. Of $33.5k taken in the
  trigger's own flashblock over 10-20/21 (our venues and v4 with them,
  arbitrages of $1 or more):
  - **53.5% by Cobalt validity transactions:** about eight bots, silent until
    they fire, signing from 4–194 addresses, bidding a median **22% of the
    arbitrage's value** as priority fee (p75 47%);
  - most of the rest by low-priority spam already sitting in the pool;
  - 0.5% by self-backruns. No private flow is needed.
  - Only **$331** of it was wholly inside the pools our tickets used;
    $10.1k touched them and $23.2k was elsewhere. The value is spiky: $4.3k
    on 10-20, $29.2k on 10-21.

R26's armed lane already uses validity transactions, and its first fire was
exact to the wei. It lost because it is aimed too narrowly: rungs 4–16 ticks
out on 52 pools fire only in spikes (8 fires in the canary period, none won
in the pessimistic landing), and its one real contest was lost on priority
per gas. The bots that win fire on ordinary moves, across thousands of pools,
and bid a fifth of the value.

**Denim** (checked 2026-10-23, by probes that sent nothing): live on Base
Sepolia since 2026-10-21; mainnet "targeting November 2026". After Denim,
Sepolia's sequencer still accepts `base_sendRawTransactionValidity`, and both
networks' nodes accept the same five predicates (`balance`, `nonce`,
`storage`, `block_number`, `flashblock_index`). The docs say nothing about
`flashblock_index` once flashblocks end, so this design uses `block_number`
only. Denim does end `pendingLogs`, `newFlashblocks`, the `pending` tag and
pending `eth_simulateV1`: the feed has to move to `logs` and `newHeads` before
November regardless of this design.

## Goal

Win a share of the arbitrage taken in its trigger's own block, with
validity transactions armed where that arbitrage actually happens, priced to
fire on ordinary moves, and bid against what the winners measurably pay.

**What success looks like:**

- **Pilot:** seven days live with realized net positive after every fee,
  included-revert fees under 20% of gross, and no tripwire events (R26's
  gate B).
- **The target:** $25k a month is ~$830 a day: roughly 5–10% of the
  trigger-block value our venues and v4 carry on a typical mix of calm and
  spike days. That is a mid-tier bot's share (the leading validity bots take
  2.5–11% each). Phase 1 alone (our venues, WETH start) cannot reach it; it
  exists to prove the lane wins contests at a profit.

## Non-goals

- **Spam.** Keeping transactions in the pool every block costs money every
  block whether or not anything fires, and needs on-chain search in the
  contract. Out.
- **The reactive lane.** R33/R34 closed it as a profit source. The canary can
  stay as a sensor or be stopped; nothing here depends on it.
- **v4, non-WETH starts and 3-hop routes in phase 1.** They need executor
  changes (an owner deploy). They are phase 2, after the pilot shows the lane
  wins at a profit.
- **Other chains.**

## Anti-goals (a failure even if it works)

- An included trade that loses money past the loss cap.
- Running BlockPI out of request units again (it has happened twice: 10-12,
  10-17), or saturating the host so the arming engine falls behind the chain.
- A key or secret printed, logged or held anywhere but the host's key file;
  the owner key ever on the host.
- Arms built on a book that disagrees with chain storage (R26's
  `verify_layouts` check stays mandatory).

## Constraints

- Owner transactions are prepared and dry-run by me and broadcast by the
  operator; merges, pushes and deploys each need the operator's approval.
- Secrets never printed; keys passed by file or environment, never argv.
- Analysis runs on public endpoints from the host; BlockPI carries the live
  feed, reads and simulation.
- The lean executor (R29): two hops, a Morpho WETH loan, 350–560k gas a
  trade. Under Cobalt, eligible transactions are ordered by priority fee *per
  gas*, so every unit of gas both shrinks the surplus and divides the bid
  (R26's first fire lost exactly this way). Winners measured at 190–740k.
- The host: one AWS instance (4 cores, 7.7 GB) also running the canary.

## Design

### 1. Targeting: arm where the arbitrage happens (a daily census)

A daily job on the host, built from R34/R35's `arb_census.race` and
`offset0`, writes `var/apex/targets-YYYY-MM-DD.json`. Over the trailing 7
days it lists the arbitrage taken in its trigger's own block, by **route**:
the pool pair, the direction, value taken, number of fires, the winners'
priority share of value, and their gas.

The arming universe is the routes our executor can run: two hops, WETH start,
both pools on our five venues. They are ranked by trailing value; the top
**K = 200 routes** are armed **(my call; K tunes with the sender count)**. Their
pools join the book; pools outside them leave it, except the current 52,
which stay.

**Gap to fill before planning:** how much of the R35 value has that shape,
and how many routes carry it. The script is written (`/root/r35/shape.py`);
it needs an AWS sign-in to run. If the WETH-start two-hop share is small,
phase 2's executor work moves ahead of the pilot.

### 2. The book and feed at that scale, Denim-first

- **Pools:** from 52 to a few hundred: the pools of the targeted routes,
  whatever their depth, each checked against its factory and bytecode before
  it is armed (base_venues_complete.yaml once invented routers and quoters).
  R28.1's positions-in-place keeps each pool exact.
- **Feed:** a `logs` subscription filtered to the armed pools' addresses, and
  `newHeads`, replacing `pendingLogs` and the flashblock stream. This is the
  Denim migration, done once here rather than twice. Before Denim, the same
  subscription delivers each block's logs every 2 s; R26's arms watch
  storage, not the feed, so the arming engine needs the feed only to
  re-price. The canary keeps its own `pendingLogs` feed until it is retired
  or Denim ends it **(my call)**.
- **Budget:** the report counts request units per method (R28.1). Moving the
  feed to `logs` over a few hundred pools must come in under today's ~124M RU
  a day; if it does not, the choice of a second provider or an own node
  (~$1.1k a month) is the operator's.

### 3. Arming: thresholds close in, re-priced every block

R26's machinery stays: slots (a route and its trigger pool), the threshold
search, break-even floors, storage predicates (trigger past threshold, the
other pool in a band, its liquidity floor), the expiry bound, the armed
ledger. What changes:

- **Rungs start where profit starts.** The first rung is the threshold
  `m₁`; then `m₁ + 1, + 2, + 4, + 8, + 16` ticks, or bps on a reserve pool
  **(my call)**: fine steps where ordinary moves land, instead of R26's
  `+5 … +200`.
- **Re-priced every block** for slots whose pools changed, with R27.2's
  certified size search (12 quotes, not 128). The pass is budgeted; slots it
  cannot reach in a block keep their arms until their bands fail.
- **Ranking for the senders we have:** expected net per arm, the probability
  of a move past the rung within W, times net at the rung after the expected
  bid, with the probability learned per pool and direction as in R26.
- **W = 10 s**, as a `block_number` bound from `block_time_ms` (5 blocks now,
  50 after Denim). No `flashblock_index` predicates.

### 4. Bidding against what the winners pay

A losing validity transaction is never included and pays nothing: once the
winner closes the gap, our trigger or band predicate is false. So, unlike
R31's tip, a bid is paid only on a win, and for a rung with expected surplus
π the bid share `f` that maximizes `F(f) × (1 − f) × π` is the one to use.
`F(f)` is the share of measured winners on the same route class whose
priority share was below `f`, from the daily census **(my call: per route
class, falling back to all routes)**. The cap `max_bid_fraction` bounds it.

Included-but-losing (a revert that still lands, or a fill that loses
money) is the exception the loss cap and the accounting watch, not the
model's base case.

### 5. Senders

- **N senders**, one pending arm each (a pending transaction at nonce n
  blocks every later nonce from that address). Start with **N = 16**,
  R26's figure, and scale with gate B **(my call)**. N bounds coverage: with
  200 routes, two slots and six rungs there are ~2,400 candidate arms, and
  16 senders hold the best 16 of them at a time. The winners sign from 50–194
  addresses. Each sender costs the operator one `setExecutor` broadcast and
  a funding transfer, which is why N grows only after gate B.
- Keys generated on the host (`apex keys new --count N` into
  `/etc/apex/armed.keys`, mode 0400, only addresses printed), separate from
  the reactive trader `0xCB43…`.
- **Authorization:** the lean executor's `setExecutor(account, true)`, one
  owner transaction per sender. I prepare and dry-run them and their
  revocations; the operator broadcasts.
- **Funding:** ~0.002 ETH each, sent by the operator; a sender below 0.0005
  ETH is left out and reported. Senders pay the gas of every included
  transaction while profit goes to the executor's recipient, so they drain;
  the report shows each sender's balance and days left, and top-ups are the
  operator's until an automated sweep is designed.

### 6. Submission and the API probe

`base_sendRawTransactionValidity` to `https://mainnet-sequencer.base.org`,
predicates as `{"type", "params"}` (the wire format R26 already sends). A
re-arm of a live slot replaces its pending transaction at the same nonce; an
"underpriced replacement" bumps both fees by 12.5%, at most twice.

R26's probe runs first, on one sender, for about $0.02: a false predicate
is not included and costs nothing; a true one is included next block;
expiry then nonce reuse; the largest predicate set accepted. Added: **the
re-arm rate the sequencer accepts per sender and in total**, which sets how
often the engine can re-price.

### 7. Shadow briefly, then a capped pilot

Because a loser costs nothing, a long shadow buys little; its job is
exactness. **(my call)**

- **Gate A′ (shadow, ≥ 1 day):** quote and simulation agree on ≥ 95% of
  fires (R26's measure); every armed pool's storage words match the book;
  the API probe passed; RU under budget.
- **The pilot** under R26's caps (all enforced before signing): a daily loss
  cap, `max_priority_gwei`, `max_bid_fraction`, `max_size_weth`, `max_arms
  = N`, the kill file, and the nonce tripwire per sender.
- **Gate B (7 days):** realized net positive after every fee; included-revert
  fees under 20% of gross; zero tripwires. Then more senders, a larger K,
  and phase 2.

### 8. Phase 2 (after gate B; outline only)

A new executor revision with v4 hops (unlock/settle) and token-generic
settlement (USDC start), deployed by the owner; 3-hop routes. The daily
census already measures their value, so the case for it will be in hand.

## Accounting

Per arm: armed, fired (became eligible), included, won (our transaction took
the gap), realized net. Per route and day against the census: our share of
the trigger-block value, our bids against the winners'. The report's `armed`
section carries the totals.

## Risks

- **Competition compresses margins.** Winners already pay a fifth of the
  value; more bidders push it up. The daily census tracks the winners' share,
  and gate B fails a lane that cannot clear it.
- **Our gas.** 350–560k a trade against winners' 190–740k; per-gas ordering
  penalizes it. A leaner path is a phase-2 lever.
- **Spikes carry the money.** Calm days were $4.3k across everything we
  measured; a week without a spike may show little.
- **Targeting lags.** A trailing 7-day ranking misses new pools for up to a
  day.
- **Undocumented limits:** re-arm rate, pending count, predicate count. The
  probe measures them before any armed trade.
- **Denim's date.** If mainnet activates before the feed moves, the feed goes
  blind; building §2 first removes that.

## What is left to the builder

Module layout; the census job's schedule and file format; the ranking's
probability model; the slot pass's scheduling and budget; how routes enter
and leave the book; test fixtures. R26's verification list (move-to-price
exactness, threshold monotonicity, golden predicate JSON, storage-word
checks, evaluator cases, mutations) carries over and extends to the new rungs
and the bid rule.

## Decisions that are the operator's

1. **Pilot risk:** the daily loss cap (R26 set $25) and the ETH spread across
   senders (16 × 0.002 = 0.032 ETH).
2. **Senders and keys:** N to start, and keys generated on the host as R26
   planned.
3. **Infrastructure:** stay on BlockPI for a few-hundred-pool `logs` feed, or
   budget for a second provider or an own node if the RU count says so.
4. **Shadow length:** a one-day exactness gate (proposed), or R26's three
   days.
5. **Phase order:** pilot on our venues first (proposed), or executor work for
   v4 and USDC first if the shape numbers show phase 1 is too small to prove
   anything.
