# Base arbitrage census (Task 8.5 R23)

Approved by the operator 2026-10-08.

## Goal

Measure, repeatably, what arbitrage winners actually earn on Base, by venue and
by pair, over 7 days. The measurement decides which venues earn an adapter and
which pairs earn promotion. Nothing in the live path changes.

## Why

A venue and pair expansion report (2026-10-08) ranked venues by DefiLlama
volume relative to liquidity: Metric, Tessera V, ElfomoFi, Hanji first. Volume
measures turnover, not arbitrage anyone can capture. One day of our own
measurement (2026-10-07, every Uniswap v2/v3/v4, Aerodrome, PancakeSwap and
Balancer swap) found:

- $23,747 of arbitrage across Base, $6,189 of it on the three venues we priced
  then; Uniswap v4 and Aerodrome's second Slipstream factory were the largest
  gaps.
- Calm-hour value concentrated in long-tail tokens, in lumps: one 10-hop trade
  through OMI and RISE banked 5.92 WETH (~$15,700), which the pool-movement
  estimate put at $3,500 because some legs were on venues it did not decode.
- The report's pairs mostly earned little: VIRTUAL $1,276, AERO and STONKEX
  ~$390 each; XDP's $2,523 was 65,529 trades averaging 4 cents.

The four venues the report puts first are all market-maker priced:

| Venue | Pricing | On Base (checked 2026-10-08) |
|---|---|---|
| Tessera V | proprietary AMM (Wintermute-associated) | `0x55555522005BcAE1c2424D474BfD5ed477749E3e`, `TesseraTrade` (`0x97ba0cd8…`), 417 trades in 30 min |
| ElfomoFi | signed quotes (`quoteId`, `partnerId` in its event) | `0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73`, `ElfomoTrade` (`0xbe65a3f1…`), 224 trades in 30 min |
| Metric | oracle-priced pools (`priceProvider` per pool) | factory `0xe22F9fc0f04486dE25ed6CF1800a4a47aFD82e0C`; no swaps under the adapter's `Swap` signature in 30 min |
| Hanji | on-chain order books with a designated market maker | CLOB factory `0xC7264dB7c78Dd418632B73A415595c7930A9EEA4`; 177 orders on 3 books in 30 min |

Addresses and events come from DefiLlama's open-source adapters and are leads,
not facts: each is verified on chain before the census counts it.

## Method

**An arbitrage** is a transaction whose decoded legs spend no token net (beyond
0.01% of that token's flow, which absorbs rounding) and gain at least one.
Native ETH counts as WETH.

**Detection by pool movements.** Each venue's trade event is decoded into the
pool-side change of each token. Transactions with two or more decoded legs are
grouped and tested for closure. This is the method of the 2026-10-07
measurement, and it scales to a week of Base.

**Measurement by transfers.** For every arbitrage whose pool-movement value is
$1 or more, and every arbitrage with a leg on a market-maker venue, the receipt
is read: the net ERC-20 transfers to the transaction's sender and its target
contract are what the bot banked, and `gasUsed × effectiveGasPrice + l1Fee` is
its gas. The banked figure is the census's value; the pool-movement figure is
kept beside it.

**Valuation.** WETH, USDC, USDbC and cbBTC from the window's own swaps against
WETH/USDC; other tokens likewise, from their swaps against WETH or USDC, with
at least three observations. A banked amount in a token with no price is
counted but reported unvalued.

**Spike hours** are those whose total gross exceeds 5× the median hour of the
census window. Every figure is reported calm and spike.

## Venues

Decoded today: Uniswap v3 and its forks (identified by `factory()`), Aerodrome
Slipstream (both factories), PancakeSwap v3, Uniswap v2 and its forks, Aerodrome
v2, Uniswap v4 (pool keys from the PositionManager), Balancer v2 and v3.

Added, each counted only once verified:

| Venue | Decoded from | Pools found by |
|---|---|---|
| Tessera V | `TesseraTrade(tokenIn, tokenOut, amountIn, amountOut, recipient)` | its one swap contract |
| ElfomoFi | `ElfomoTrade(quoteId, partnerId, executor, receiver, fromToken, toToken, fromAmount, toAmount)` | its one swap contract |
| Hanji | taker fills on each book | `OnchainCLOBCreated` from its factory, tokens from `getConfig()` |
| Metric | its pools' `Swap` | `PoolCreated` from its factory; the Base event version read from a live pool |
| QuickSwap | its pools' Algebra `Swap` | the pool's `factory()` |
| Fluid | its DEX pools' swap event | the Fluid DEX factory |
| Maverick | v2 pools' `PoolSwap` | the pool's `factory()` |
| Curve | `TokenExchange` | `coins(i)` per pool |

**Verification of a decoder**, before its venue counts: on at least three live
transactions, the decoded pool-side change of each token equals the pool's own
ERC-20 balance change in the same receipt (from its `Transfer` logs). A venue
that cannot be verified is reported as not covered, never estimated.

## Outputs

`data/census/` (outside git, as `data/` is): one file per day of collected
arbitrages, resumable, and a report.

The report (`data/census/report-<from>-<to>.md` and `.json`) has:

1. **Venues:** gross and net of gas, trades, typical (p50) and large (p90)
   trade value, distinct bots, calm and spike; and the value each would add to
   the venues the shadow prices, alone and combined.
2. **The market-maker venues:** trades that profit against each, their banked
   value, and the share taken by the top three accounts.
3. **Pairs:** the same, by unordered token pair, long-tail tokens included,
   each marked against the report's promotion gates the census can judge
   (two or more venues, a venue whose state we can reconstruct). The $100k
   depth gate is checked at promotion, from the pools' own balances: the
   census does not read them.
4. **Coverage:** what each venue's decoder verified, and what was not covered.

## Shape

A Python package, `scripts/data/arb_census/`, following the repository's data
scripts:

- `rpc.py`: the BlockPI endpoint from `.env`'s `BLOCKPI_KEY` (never printed),
  JSON-RPC batches, a request-rate limit.
- `venues.py`: one decoder per venue, log to pool-side token changes.
- `detect.py`: closure, and transfer accounting from a receipt.
- `collect.py`: one day at a time, resumable.
- `report.py`: rankings to Markdown and JSON.

Tests in `scripts/data/arb_census/test_*.py`, run with `python3 -m unittest`,
on recorded real logs and receipts: the $855 spike arbitrage, a Uniswap v4
arbitrage, the 10-hop OMI trade, a Tessera trade, a Hanji fill, and a user's
multi-hop swap that must not count.

## Running

Seven days, one at a time, in the background: about 3–4 hours. Requests are
rate-limited, and the shadow's read failures are watched while it runs; a rise
pauses the census.

## Out of scope

Adapters for any venue, any change to the shadow or its universe, and analysis
of Uniswap v4 hooks.

## Risks

- Long-tail prices from thin pools can be wrong. Transfers fix what was banked,
  not what it was worth; unvalued tokens are reported as such.
- Uniswap v4 pools whose keys the PositionManager does not hold stay
  unresolved, as on 2026-10-07 (42,114 transactions); counted and reported.
- Hanji's fills are order events, not swaps, and are the hardest decoder;
  verification decides whether it counts.
- A week can hold one spike or none. The report says how many spike hours it
  saw.
