# Armed lane v3: Uniswap v4 pools and USDC routes (Task 8.5 R37)

2026-10-28. Asked for by the operator: "whichever lever captures the most,
scope that first", then "looks right, write the design doc". Status: design
only, nothing built. Every call I made is marked **(my call)**. The
operator's decisions are listed at the end.

## Why

**Lever 1 is refuted (R36, `802d520`).** `apex armed-replay` quoted all 24
recorded armed fires on their own books, at sizes from 1/64× to 4× the armed
size. Smaller single-tick sizes flip none of the 7 fires lost on bid.
Crossings are only 12.8% of lean gas; the rest is each hop's base cost. On our
pools, ranking, sizing and bid tweaks are worth about $8.50 a day at best.

**Who took our 24 fires** (each rival's receipt, 2026-10-27):

| What the winner did | Fires | Their tips |
|---|---|---|
| A 6–8-hop route across the BTC tokens during sharp moves | 2 | $906 (98%) |
| A 2-hop route like ours (3 of them through v4) | 13 | $17 |
| A 3-hop route on our venues | 1 | $0.06 |
| One visible swap, no visible cycle | 8 | $1 |

**Where the market's trigger-block arbitrage is.** The daily census race
files on the host cover 10-18 to 10-26. Of $251k detected, $156k (62%) landed
in its trigger's own flashblock, the segment validity transactions win.
That segment by what it would take us to capture:

| Venues, hops, profit token | 9 days | Per day | Routes holding 80% | Biggest day's share |
|---|---|---|---|---|
| **v4 + our venues, 2-hop, USDC** | **$37.9k** | **$4.2k** | **7** | 31% |
| v4 + our venues, 3-hop, WETH | $26.5k | $2.9k | 3 | 69% (one $18.3k trade) |
| Other venues, 2-hop, USDC | $14.1k | $1.6k | 21 | 26% |
| **Our venues only, 2-hop, USDC** | **$9.1k** | **$1.0k** | 13 | 33% |
| Our venues only, 2-hop, WETH (today's armed lane) | $7.7k | $0.9k | 19 | 29% |
| Our venues only, 3+ hops | $15.1k | $1.7k | 2–41 | 22–59% |

Two cells this design can reach together come to **about $5.2k a day**: v4
with our venues (2-hop USDC) and our venues alone (2-hop USDC).

**This week the v4 cell is mostly one token.** AXIS (`0x5be75e3b…d755ea`,
"Axis Token") launched about 10-21. It has 145 pools: 136 on v4, against
USDC, plus 7 Uniswap v3 and 2 PancakeSwap v3. It carried $59k of
trigger-block value in 6 days, 38% of the whole market, at $1.4k–26.5k a
day.
- One v4 pool, `0xc19d91d9…a627fa` (fee 100 ppm, tick spacing 1), carries
  $31.7k of the lever's $43k of v4 value.
- That pool has a hook: `V4AlphaHook`, `0xb0b24b89…592880`. Its permission
  bits are before-initialize, before-add-liquidity and before-swap, and none
  return a delta. Its verified `_beforeSwap` is a view that reverts until the
  pool has "started", then returns no delta and no fee. The swap math is
  plain concentrated liquidity.
- The next two pools have no hook: 8,200 ppm at spacing 82, and 3,800 ppm at
  spacing 38.
- AXIS's implementation (`0x24eb5084…08788`) is a plain OpenZeppelin ERC-20:
  no transfer tax and no blocking. It sits behind a UUPS proxy the owner (a
  timelock) can upgrade.

Tokens like AXIS come and go. What is worth building is the ability to arm
whichever v4 and USDC routes the daily census finds.

**The competition is real.** I sampled ten of the winning transactions on
AXIS routes:
- they came from eight different contracts, two of them R35's validity bots
  (`0x8d48…`, `0x2328…`);
- they used 206k–759k gas;
- they paid 0 to 11 gwei a gas in priority, which is 0% to 97% of each
  arbitrage's value, and half of them paid more than 38%.

## Goal

Win a share of the trigger-block arbitrage on v4 pools and our venues, with
USDC as well as WETH as start and profit token. Use the R36 armed lane:
validity transactions, the daily census's targets, per-gas bids.

**What success looks like:**
- **Shadow:** on the census's v4/USDC targets, quotes agree with simulation,
  and we measure how many fires we would have won against the actual
  winners, at what net.
- **Pilot:** live under the R36 caps, net positive after every fee.
- **The target:** $25k a month is about $833 a day, roughly 16% of the
  ~$5.2k a day the two cells carry before tips. At a 50% tip that means
  winning about a third of their value. R36 measured the leading validity
  bots taking 2.5–11% each. **This is a top-bot share, not a given.** The
  shadow's would-have-won figure is what decides whether the lane gets there.

## Non-goals

- **Native-ETH v4 pools** (currency 0 is the zero address). R23 put them at
  about $6k of v4's $33k a week. They need value settlement; this version
  leaves them out. **(my call)**
- **Dynamic-fee pools**, and **hooks that return a delta.** Either one makes
  the swap math the hook's, not the pool's.
- **3+ hops.** The 3-hop v4 cell is mostly one $18.3k trade, and the
  BTC-cluster routes are spike-only. They are a later lever.
- **Other venues.** One unidentified factory, `0x9cb22058…a76`, touched $36.9k
  of 9-day value. Worth identifying next, not here.
- **The reactive lane.**

## Anti-goals (a failure even if it works)

- R36's, unchanged:
  - an included loss past the cap;
  - BlockPI run out of request units;
  - a secret printed, logged or held off the host;
  - arms on a book that disagrees with chain storage.
- **Subscribing to all of PoolManager's logs.** That is every v4 swap on
  Base. v4 events are filtered by pool ID, or not subscribed to at all.
- **A pool admitted whose hook can change swap math or the fee,** or whose
  tokens move a different amount than asked.
- **The executor holding any token but its start token after a trade.**
- **An executor that can make an arbitrary call** (`check_no_generic_call.sh`
  stays green).

## Constraints

- Owner transactions are prepared and dry-run by me and broadcast by the
  operator. Merges, pushes and deploys each need the operator's approval.
- Secrets are never printed; keys go by file or environment, never argv.
- Analysis runs on public endpoints. BlockPI carries the live feed, reads
  and simulation. Archive logs come from an archive source (BlockPI or an
  explorer API), never as a burst on BlockPI while the canary is live.
- **Ordering is by priority per gas.** Gas both shrinks the surplus and
  divides the bid. The lean executor uses 242–463k a trade, and the AXIS
  winners 206–759k.
- **Senders hold 0.002 ETH each.** At the current gas limits that caps a bid
  near 2.3 gwei a gas; AXIS winners paid up to 11. See the decisions.
- **Denim** (~November 2026) ends flashblocks and `pendingLogs` and makes
  blocks 200 ms. Nothing here may depend on them; validity transactions and
  block-number bounds carry over (ARMED.md's Denim day).
- The host: one AWS instance (4 cores, 7.7 GB), also running the canary.

## Facts verified on chain (2026-10-28)

- **PoolManager:** `0x498581fF718922c3f8e6A244956aF099B2652b2b`, which has
  code.
- **A pool's state starts at slot `keccak256(abi.encode(poolId, 6))`.** For
  `0xc19d91d9…` slot0 there decodes as follows:
  - sqrtPriceX96 is the low 160 bits;
  - the tick is the next 24;
  - the protocol fee is the next 24;
  - the LP fee is the next 24, and read 100, matching the pool's
    `Initialize` event.
- **Liquidity** is at slot0 + 3. `extsload(bytes32)` reads both.
- **The tick and bitmap mappings** are at slot0 + 4 and + 5, per v4-core's
  `StateLibrary`. Not yet verified on chain; the builder proves them.
- **Event topics:**
  - `Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)` is
    `0x40e9cecb…7112f`, the census's own;
  - `ModifyLiquidity(bytes32,address,int24,int24,int256,bytes32)` is
    `0xf208f491…d5ec`;
  - `Initialize(…)` is `0xdd466e67…6438`.
- **Hook permissions** are the low 14 bits of the hook's address
  (v4-core `Hooks.sol`).

## Design

### 1. One new executor, deployed once

A revision of the lean executor (R29), deployed by the owner, with the 16
senders authorized on it. The current executor stays as it is, and arms name
the executor they were built for.

- **The plan names its start token,** WETH or USDC. Both are constants in the
  contract; any other token refuses. The loan, the profit floor, the profit
  transfer and the `Profit` event are all in that token. **(my call:
  allowlisted, not arbitrary)**
- **Hop kind 3 is a Uniswap v4 swap.**
  - The hop carries its pool key: currency0, currency1, fee, tick spacing
    and hooks. Hook data is empty.
  - It is exact-input, with the same minimum-out check as the other kinds.
- **Lender kind 2 is PoolManager's own flash accounting.**
  - The whole trade runs inside one `unlock`.
  - The start amount is owed to PoolManager and settled at the end; v4 hops
    add to the same deltas.
  - A hop on another venue is paid from tokens taken out of PoolManager just
    before it.
  - This drops Morpho's flash loan (about 83k gas) and an unlock per v4 hop.
  - Plans with no v4 hop keep kinds 0 and 1 (Balancer, Morpho). Morpho must
    also lend USDC for USDC plans on our venues.
  - **(my call: the gas matters more than the extra code path. If PoolManager
    lending fails a fork test, Morpho plus one unlock per v4 hop is the
    fallback.)**
- **Existing hop kinds are unchanged:** v3-style, PancakeSwap, Aerodrome v2
  (exact minimum).
- **The plan codec's version goes up,** with new fixtures held between the
  Rust encoder and the Solidity decoder, as today.
- **Fork tests at a recorded block** run AXIS/USDC between its v3 pool and
  the hooked v4 pool `0xc19d91d9…`, in both directions and both start
  tokens. They cover the profit floor, every revert path, and the gas used
  (which feeds the gas model).
- **The deploy:** a script, a dry run, then the operator broadcasts. The R36
  authorize script, pointed at the new address, authorizes the senders.
- **After the deploy, every arm uses the new executor,** WETH routes
  included: one set of authorized senders, one plan version. The current
  executor keeps only the halted reactive lane. **(my call)**
- **Shadow runs before the deploy.**
  - Until then, `eth_simulateV1` places the new executor's runtime code at a
    fixed placeholder address by state override.
  - The trader is authorized there by a storage override of `executors`.
  - Gate A″ is measured against exactly the code the owner will deploy.
  - **(my call)**

### 2. v4 pools in the book

- **Identity:** a v4 pool has no address. The book keys it by the first 20
  bytes of its pool ID, and its spec carries the full pool key and ID.
  Admission refuses a key that collides with a real pool's address. **(my
  call: it keeps every `Address`-keyed map, and the risk is negligible)**
- **Inventory:** `data/base/uniswap_v4/`, one record per pool: pool ID,
  currencies, fee, tick spacing, hooks, init block.
  - It is built from PoolManager's `Initialize` events.
  - It is extended daily from the day's blocks, so new pools (AXIS's
    kind) arrive within a day.
- **Where those logs come from:**
  - Public endpoints serve a day's recent blocks. PublicNode refuses older
    ranges ("archive requests require a personal token").
  - So the daily extension scans the day's blocks there, filtered to
    PoolManager and the `Initialize` topic.
  - The one-time backfill resolves only the pool IDs the census has seen,
    one `Initialize` lookup per ID. I ran it once from the operator's machine
    through an explorer API on 2026-10-28, and that's the procedure. Its
    output goes into the inventory, outside git like the rest of `data/`.
  - No archive sweep runs on BlockPI. **(my call)**
- **Admission:** a pool needs all of these:
  - both currencies are ERC-20s;
  - a static fee (`fee & 0x800000 == 0`);
  - no return-delta hook bit;
  - its hook is either zero or on a reviewed allowlist
    (`ops/v4-hooks.base.json`: address, the reason, and who reviewed it).
    The first entry is `V4AlphaHook`.
  - **(my call: a list kept by hand, not a rule)** A before-swap hook can
    still revert or charge gas, and only its source says which.
- **Token admission:** intermediate tokens must move exactly what is asked.
  At admission, a simulated round trip checks the balances (`transfer` in and
  out). A fee-on-transfer or rebasing token refuses.
- **Reads:** `PoolManager.extsload(bytes32[])` batches slot0, liquidity, and
  the ladder's ticks and bitmap words, at the slots above. `verify_layouts`
  extends to v4.
- **Pricing:**
  - the existing concentrated-liquidity ladder (`apex-math`) applies
    unchanged;
  - the swap fee is the LP fee composed with the protocol fee for the
    swap's direction, ported exactly from v4-core's `ProtocolFeeLibrary`;
  - exactness is proven step for step against `eth_simulateV1`, as for
    every venue.
- **Gas:** a `uniswap_v4` hop base in `gas::LEAN`, from the fork tests. A
  hooked pool adds its hook call.

### 3. The feed

- **Subscription:** Swap and ModifyLiquidity from PoolManager, with topic 1
  filtered to the admitted pool IDs. Today's filter takes topic 0 only, so it
  grows topic lists. A v4 subscription without a pool-ID filter refuses.
- **Decoding:**
  - Swap carries the swap's actual fee, as well as price, liquidity and
    tick;
  - ModifyLiquidity becomes a position write (R28.1). Its `int256` delta
    must fit `i128`, or the pool is reloaded.
- **Request units:** a pool's v4 events cost about what a v3 pool's do. The
  report counts them per method, as today.

### 4. USDC as start and profit token

- The frontier builds 2-hop cycles from USDC as well as WETH.
- **Economics run in the start token.** Costs are in ETH: gas, the L1 fee
  and the tip. They convert at the WETH/USDC mid price of the book's deepest
  WETH/USDC pool at the arm's block. **(my call)** The bid stays in wei a
  gas.
- **The lender:** PoolManager for routes with a v4 hop; Morpho Blue's USDC
  otherwise. The builder confirms Morpho's USDC balance on Base.
- **The daily loss budget stays in ETH,** as everything a trade can lose is
  gas. Net is reported in both tokens.
- **Size caps are per start token:** `armed.max_size_wei` for WETH, and a
  new config field `armed.max_size_usdc`, 12,000 USDC, about R36's 5 WETH.
  It is a field in `ops/*.base.yaml`, not an environment variable. **(my
  call)**

### 5. Arming v4 pools

- **Predicates are storage predicates on PoolManager:**
  - price: slot0's slot, with today's price mask (sqrtPriceX96 is slot0's
    low 160 bits on v4 too);
  - liquidity: slot0 + 3, masked to 128 bits.
- **Slots become per pool.** `armed::predicate::layout` fixes slots per venue
  today; for v4 it gives an address and a 256-bit slot per pool. The
  evaluator's `Write::Set` slot widens from `u64` to 256 bits.
- Moves, spreads, rungs and the ranking are unchanged: a v4 position is its
  tick.
- **Bids** are per gas, from the surplus converted to ETH, by R36's rule.

### 6. Targets

- **The census's targets accept:**
  - 2 hops;
  - venues within `race.OURS` plus `uniswap_v4`;
  - one gain token, WETH or USDC, which is also the start token;
  - offset 0;
  - every pool admissible (§2).
- Each target carries its v4 pool keys. The ranking is unchanged: market
  value per route over the trailing days.
- The daily run already restarts the canary into new targets. A hot new
  token is armed within a day of its first measured day.

### 7. Evaluation and gates

- `armed-eval` and `armed-replay` read v4 logs and USDC gross. Fires are
  simulated against the new executor.
- **Gate A″ (shadow, at least 3 days, on v4/USDC targets):**
  - quote and simulation agree on at least 95% of fires;
  - every armed v4 pool's storage matches the book;
  - each fire is judged against its actual winner, by our per-gas bid
    against theirs and by position, giving would-have-won fires and net a
    day.
- **The live gate:** a would-have-won net a day that the operator sets
  (see the decisions). Then the owner deploy, sender authorization, and a
  pilot under R36's caps.

## Phasing

Each phase is its own group of plan tasks and reviewable on its own.

1. **The executor and its fork tests** (no deploy).
2. **v4 inventory, reads and pricing,** proven exact against simulation,
   offline.
3. **The feed, and USDC cycles and economics,** in shadow.
4. **Targets, arming and evaluation for v4/USDC** in shadow; gate A″.
5. **The owner deploy and authorization, then the live pilot.** Each step
   needs the operator's approval.

## Risks

- **Concentration and churn.** This week the lever is mostly AXIS, launched
  on 10-21 and swinging from $1.4k to $26.5k a day. Daily targeting is the
  answer, and a week without a hot token may show little.
- **Competition.** Eight bots in ten samples, tipping up to 97%. Gate A″
  measures whether we would win at all.
- **Upgradeable tokens and hooks.** AXIS's owner can change its code; a trade
  is atomic, so the worst case is a revert's gas. `V4AlphaHook` is not a
  proxy. Allowlisted hooks are re-checked when their code hash changes.
- **Request units and Denim,** as in R36.
- **The bid cap from the sender float.**
- **Lender depth:** PoolManager's and Morpho's USDC balances bound the size,
  as `Lenders` already does for WETH.

## What is left to the builder

- module layout;
- the exact hop and header encoding, within the codec's style;
- how the inventory's archive scan is scheduled;
- the hook allowlist's file format;
- the conversion price's source pool;
- test fixtures and the recorded fork block.

R26 and R36's verification list carries over:
- move-to-price exactness, golden predicate JSON, storage-word checks,
  evaluator cases and mutations;
- extended to v4 slots, the protocol-fee composition and the USDC start.

## Decisions that are the operator's

1. **Phase order.** R36 settled that v4 and USDC wait for gate B. Gate B
   needs 30 landings in 7 days, and the lane has about 2 fires a day, all lost
   so far, so it cannot finish. **I propose building this now, with gate A″
   and the live gate in place of gate B.**
2. **Sender funding and the bid cap.** 0.002 ETH a sender caps a bid near
   2.3 gwei a gas; AXIS winners paid up to 11. Raising the top-up to 0.01 ETH
   a sender (0.16 ETH for 16) lifts the cap about five times. Raising it is
   one run of the fund script.
3. **The live gate's threshold:** the would-have-won net a day in shadow
   that justifies going live.
4. **The daily loss budget:** whether wins offset losses, still open from
   R36.
5. **Token scope:** WETH and USDC only for now (USDT-gain value was $6.6k in
   9 days). **(my call unless overruled)**
