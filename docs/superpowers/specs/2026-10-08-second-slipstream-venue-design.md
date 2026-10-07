# Aerodrome's second Slipstream factory (Task 8.5 R22)

Approved by the operator 2026-10-08.

## Goal

Price Aerodrome's second concentrated-liquidity deployment, factory
`0xf8f2eB4940CFE7d13603DDDD87f123820Fc061Ef`, alongside the first
(`0x5e7BB104…`), which the shadow already prices.

## Evidence

Measured 2026-10-07/08 over 24 hours of every Uniswap v3-style, v2-style,
Uniswap v4 and Balancer swap on Base:

- Arbitrage needing this factory, and otherwise only our venues, grossed
  $3,716: $2,357 in the 02:07 UTC spike and about $1,400 a day in calm hours.
  That is the largest single venue addition measured. Uniswap v4 was
  comparable in total but almost all calm-hour cents.
- Two of its pools carried legs of spike trades: `0x3fe04a59` (WETH/USDC,
  $3.8M) in a $328 trade, and `0x7c7420dd` (WETH/cbBTC) in a $285 trade.
- Its pools are already in `data/base/aerodrome_slipstream_v3/pools.jsonl`
  (63 pools, 28 with a measured fee). Five pass the shadow's universe filter
  today, taking it from 72 to 110 two-hop routes.

## Verified on chain

- Router `0x698Cb2b6dd822994581fEa6eA4Fc755d1363A92F`: 9,908 bytes of code,
  `factory()` returns `0xf8f2…`, and its code holds `exactInputSingle`
  (`0xa026383e`), the call the first Slipstream router takes. Same code size
  as the first router.
- Its fee module is `0x87d8f999…`, a separate contract from the first factory's
  `0x090b2a6b…` and of the same size. The book already reads each factory's
  module and each pool's settings from the chain (`book.rs`), so nothing is
  assumed; verification check 2 confirms the result.

## What changes

1. **A venue.** `Venue::SlipstreamV3`, named after its inventory directory:
   - directory `aerodrome_slipstream_v3`; factory `0xf8f2…`;
   - venue id `AERODROME_SLIPSTREAM_V3 = VenueId(9)` in
     `apex_venues::adapter::venue_ids`;
   - a dynamic fee like Slipstream's: `fee_is_static` false, included in the
     book's TWAP refresh, admitted as `FeeBehavior::Dynamic`;
   - the same Swap topic in the feed;
   - Slipstream's gas model, because the router and pool code are the same.
     Verification check 4 measures it.
2. **An adapter.** `SLIPSTREAM_V3_ADAPTER = 3`, bound to the router above.
   `calls.rs` encodes its hops with `slipstream_exact_input_single` through
   adapter 3. `reachable_venues` admits the venue only when the executor's
   registry holds adapter 3 at that router and allows `0xa026383e`.
3. **Fee measurement.** `scripts/data/measure_slipstream_fees.py` gains
   `--dir`, defaulting to `aerodrome_slipstream`, and is run on
   `aerodrome_slipstream_v3` for the 35 pools without a measured fee.
4. **An owner transaction.** Register adapter 3 at the router, with selector
   `0xa026383e` allowed. I prepare it and dry-run it against Base; the
   operator broadcasts it with the owner's key. Until then, the shadow leaves
   the venue out by itself.

## What does not change

- The first Slipstream venue, its adapter and its inventory.
- Pricing code: Slipstream's concentrated-liquidity math and dynamic fee apply
  unchanged.
- No new contract is deployed.

## Verification

1. **Inventory truth.** Every new pool passes the book's load: tokens and
   factory read from the chain and matching `0xf8f2…`.
2. **Fee.** For each new pool that passes the filter, the book's computed fee
   equals the pool's `fee()` at three or more blocks.
3. **Quote parity.** At a pinned block where each pool has already swapped
   (so the after-first fee applies), `quote_exact_input_multi_tick` matches
   the shared quoter `0xCd2A7D98e82D6107eac1828ce8DeAA6acB65b555`, called with
   the tick spacing OR'd with `0x080000` (the factory flag the legacy engine
   measured on 2026-09-04). Both directions, at 0.01, 0.1 and 1 WETH and
   their USDC or cbBTC equivalents. A difference beyond a few units is a defect
   to fix before the restart.
4. **Execution.** `eth_simulateV1` of a swap through the router from an
   override-funded account succeeds. Its gas falls within the Slipstream gas
   model's range for the ticks crossed.
5. **Boot.** Once adapter 3 is registered, the shadow boots with the venue
   reachable, about 26 pools held, and the route count the filter gives.

## Risks

- The first swap of each block pays the module's initial fee. Quote parity is
  checked after the first swap, as in R20.
- A pool may sit near the fee ceiling and move across it with the dynamic fee,
  as in R20.
- Until the operator broadcasts the registration, R22 changes nothing in the
  run.

## Rollout

Ships with R21 in one release build and one restart, after the registration is
broadcast, so the restart picks up both. The restart resets the 14-day clock.
