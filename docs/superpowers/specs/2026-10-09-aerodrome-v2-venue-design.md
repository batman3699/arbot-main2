# Aerodrome v2 volatile pools (Task 8.5 R24)

Approved by the operator 2026-10-09.

## Goal

Price and settle Aerodrome v2's volatile (x·y=k) pools, factory
`0x420DD381b31aEf6683db6B902084cB0FFECe40Da`, as a fifth venue beside the four
concentrated-liquidity ones the shadow already prices. The frontier keeps its
shape: two pools of one WETH pair.

## Evidence

From the R23 census, blocks 52,018,886–52,321,285 (2026-10-01 to 10-08):

- In the frontier's shape, arbitrage needing Aerodrome v2 and otherwise only
  our venues grossed **$8,822** across 121 pools, $7,876 of it in trades of $10
  or more. The same measure for Uniswap v4 gives $2,107, nearly all in
  native-ETH pools. That is why the operator chose this venue first
  (2026-10-09). v4's larger share sits in multi-pair cycles, which this
  frontier does not search.
- Every pool that carried value is volatile, mostly at 30 bps. The stable
  curve carried none.
- The value is concentrated. WETH/USDC `0xcdac0d6c` holds about $4.9M a side
  and carried $6,131. Nearly all of that came in one episode: blocks
  52,021,670–52,029,153, about 90 minutes in and after spike hours. In it the
  pool lagged Slipstream repeatedly, with trades of $4,883, $407, $350 and
  $261. The rest is spread over long-tail pairs: WETH/DRV `0xac4e562d` ($808),
  TEVA/WETH, WETH/VVV and WETH/EDEL.

## Verified on chain (2026-10-09)

- **Factory `0x420dd381…`:** 3,516 bytes of code and 29,689 pools.
  `volatileFee()` is 30 and `stableFee()` is 5, in basis points over 10,000.
  `isPool(0xcdac0d6c…)` is true and `getFee(0xcdac0d6c…, false)` is 30.
- **Router `0xcf77a3ba9a5CA399B7c97c74d54e5b1Beb874E43`:** 23,581 bytes of code.
  `defaultFactory()` is the factory above and `weth()` is WETH.
  `getAmountsOut(1 WETH, [WETH→USDC, volatile, factory])` returned 2,482.154264
  USDC, equal to the pool's own `getAmountOut` at the same block. Its code holds
  `swapExactTokensForTokens` (`0xcac88ea9`) and `InsufficientOutputAmount()`
  (`0x42301c23`). It pulls input by `transferFrom` from the caller, so it takes
  a direct approval like the other adapters.
- **Pool events:** each swap emits `Swap` (`0xb3e27736…`), `Fees` (`0x112c2569…`)
  and `Sync(uint256,uint256)` (`0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a`),
  which carries the pool's new reserves. The WETH/USDC pool emitted 712 `Sync`
  and 706 `Swap` logs in 5,000 blocks.
- **Venue id:** `AERODROME_VOLATILE = VenueId(5)` already exists in
  `apex_venues::adapter::venue_ids`.

## What changes

1. **A venue.** `Venue::AerodromeV2`: directory `aerodrome_v2`, factory
   `0x420dd381…`, venue id 5. Only volatile pools load. A record with
   `stable: true` is refused at load, and the book refuses any pool whose
   `stable()` reads true. The fee is static between refreshes
   (`fee_is_static` true). It is admitted as a static-fee venue, with the
   existing transfer-semantics check excluding non-standard tokens.

2. **A second kind of pool state.** Today a book entry is a tick state and
   its ladder. An entry becomes one of two models:
   - **concentrated liquidity**, unchanged for the four existing venues;
   - **constant product**: `reserve0`, `reserve1` and `fee_bps`.

   Each hop of a cycle is quoted by its pool's model. A constant-product hop
   computes what the pool computes, in its order and rounding:
   `in' = in − floor(in · fee_bps / 10000)`, then
   `out = floor(in' · reserveOut / (reserveIn + in'))`. Note that
   `apex_math::quote_common::apply_swap_fee` rounds the other way: it can give
   one unit less of `in'`. The new quote follows the pool, and quote parity
   decides. A constant-product hop crosses no ticks.

3. **Book load and updates.** At load: `token0`, `token1`, `stable()` and
   `factory()` are read from the pool, and the pool must be the factory's
   (`isPool`). Then `getReserves()` and `getFee(pool, false)`. After load,
   each `Sync(reserve0, reserve1)` replaces the pool's reserves outright, so an
   update needs no other state. The feed subscribes to `Sync` on admitted
   Aerodrome v2 pools beside the concentrated-liquidity `Swap` topics. Within
   a flashblock the last `Sync` per pool stands, as swaps are applied today.
   The fee is re-read with each head refresh, because the factory's fee
   manager can set a custom fee per pool.

4. **An inventory.** A builder in `scripts/data` enumerates the factory with
   `allPoolsLength()` and `allPools(i)` through Multicall3. For each pool it
   reads tokens, `stable()`, reserves and `getFee`. It writes
   `data/base/aerodrome_v2/pools.jsonl` in the existing record format
   (`pool`, `token0`, `token1`, `fee` in bps, `fee_ppm_onchain` = bps × 100,
   `hub_usd_liquidity`, `hub_symbol`), plus `stable`. Depth is the hub token's
   reserve valued in USD, the hub-side measure `build_aerodrome_pools.py`
   uses, and `hub_symbol` is always set with it, or the loader ignores it. The
   shadow's existing universe filter then applies unchanged: fee ≤ 3,000 ppm,
   which admits 30 bps exactly; depth ≥ $100k; and WETH pairs with two or more
   pools across reachable venues.

5. **An adapter.** `AERODROME_V2_ADAPTER = 4`, bound to the router above.
   `calls.rs` encodes a hop as
   `swapExactTokensForTokens(amountIn, minOut, [Route(tokenIn, tokenOut, false, factory)], executor, deadline)`
   through adapter 4, approving `amountIn` to the router. The deadline is the
   plan's, which the executor checks before any swap. `reachable_venues`
   admits the venue only when the executor's registry holds adapter 4 at that
   router and allows `0xcac88ea9`.

6. **Revert classes.** `InsufficientOutputAmount()` (`0x42301c23`) is
   classified `MinOutNotMet`.

7. **Gas.** A constant-product hop costs a fixed figure for a router swap of
   that kind, with no crossings: an expected value and a ceiling, measured by
   verification check 4. A two-hop cycle mixing the models adds its hops as
   today.

8. **An owner transaction.** Register adapter 4 at the router, with selector
   `0xcac88ea9` allowed. I prepare it as the owner's `BatchRouter.multicall`
   and dry-run it against Base; the operator broadcasts it. Until then, the
   shadow leaves the venue out by itself.

## What does not change

- The four concentrated-liquidity venues, their maths, adapters and inventories.
- The frontier's shape (two pools of one WETH pair), sizing and the lender cap.
- No contract is deployed. Stable pools, Uniswap v4 and multi-pair cycles are
  left for later specs.

## Verification

1. **Inventory truth.** Every record that passes the filter is the factory's
   (`isPool`, and the pool's `factory()`), volatile, and its tokens match the
   chain.
2. **Fee.** For each such pool the book's fee equals `getFee(pool, false)`.
3. **Quote parity.** At a pinned block, the local quote equals the pool's
   `getAmountOut(amountIn, tokenIn)` **to the unit**. It is checked in both
   directions at 0.01, 0.1 and 1 WETH and their equivalents, on the WETH/USDC
   pool and every other pool that passes the filter. Any difference is a defect
   to fix before the restart.
4. **Execution and gas.** `eth_simulateV1` of a router swap from an
   override-funded account succeeds and pays the quoted output. Its gas sets
   the constant-product figure. A full two-hop cycle through the executor (one
   Aerodrome v2 hop, one concentrated-liquidity hop) simulates with its gas
   inside the model's ceiling.
5. **Book updates.** Over a live window, the book's reserves after each
   `Sync` equal `getReserves()` read at that block.
6. **Boot.** Once adapter 4 is registered, the shadow boots with the venue
   reachable. The spec's rollout entry states the pool and route counts the
   filter gives.

## Risks

- **Concentration.** Most of the measured value came from one kind of event:
  a deep 0.3% pool lagging through a sharp move. In calm hours the 30 bps fee
  leaves little to take. Judge the venue by what it captures in spikes.
- **Custom fees** can change between head refreshes. A trade priced at the old
  fee meets its minimum output in simulation or fails there, never on chain
  unseen.
- **Reserve freshness** depends on `Sync` arriving with the pool's swaps.
  Verification check 5 tests it.
- **Restart.** Shipping it means a restart, which resets the 14-day clock.

## Rollout

The registration is broadcast first. Then one release build and one restart,
which picks up the venue. The restart resets the 14-day clock.
