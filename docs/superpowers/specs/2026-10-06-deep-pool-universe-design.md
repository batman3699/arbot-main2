# Deep-pool universe (Task 8.5 R20)

Approved by the operator 2026-10-06.

## Goal

Grow the shadow run's universe to the deepest pools on the three venues the
executor already reaches: Uniswap v3, Aerodrome Slipstream and PancakeSwap v3.
Every real gap the shadow has priced since R17 (tickets 42–46) followed a large
swap on WETH/USDC. The largest such swaps land in pools the universe leaves out:

| Pool | Depth | Fee | Why it is out today |
|---|---|---|---|
| Uniswap WETH/USDC 0.3% | $60.0M | 3,000 ppm | over the 500 ppm ceiling |
| Slipstream WETH/cbBTC, spacing 100 | $17.0M | 2,500 ppm | no measured fee in the inventory |
| Slipstream WETH/USDC, spacing 100 | $8.8M | 564–579 ppm | no measured fee in the inventory |
| Uniswap WETH/cbBTC 0.3% | $6.3M | 3,000 ppm | over the 500 ppm ceiling |

## What changes

1. **The universe filter becomes a shadow setting.** The shadow config gains a
   `universe:` section with `max_fee_ppm` and `min_depth_usd`.
   `ops/shadow.base.yaml` sets 3,000 ppm and $100k. `UniverseFilter::default()`
   keeps the census's 500 ppm and $100k and its documentation. This follows
   R19's pattern: an operator decision recorded in the shadow's config, with
   the code defaults untouched.
2. **Skipped Slipstream fees are measured by a committed script.** The loader's
   rule is unchanged: a Slipstream record without `fee_ppm_onchain` is skipped,
   never guessed, because its `fee` field is the tick spacing.
   `scripts/data/measure_slipstream_fees.py` reads `fee()` at one pinned block
   for every Slipstream record that lacks a measured fee. It backs up
   `data/base/aerodrome_slipstream/pools.jsonl`, then writes `fee_ppm_onchain`
   and the block it was read at. The data lives outside git; the script makes
   the step repeatable.

## What does not change

- No new venue, adapter or on-chain action. Uniswap 0.3% pools run through the
  executor's `UNIV3` op, and Slipstream pools of any tick spacing through
  adapter 1.
- No change to the live run's defaults: a live config would still get 500 ppm.
- Pricing, gas and the risk gate are unchanged. The book prices every
  Slipstream pool at its live dynamic fee, recomputed from the tick, so the
  inventory's measured fee only decides admission.

## Expected universe

| | Pairs | Pools | Two-hop cycles |
|---|---|---|---|
| Today (500 ppm, $100k, WETH pairs) | 4 | 12 | 30 |
| 3,000 ppm, $100k, measured Slipstream fees | 8 | 24 | 74 |

WETH/USDC goes from 5 pools to 7, and WETH/cbBTC from 3 to 5. The dead
Uniswap WETH/bsdETH pool still loads as unreadable, as it does today.

## Verification

1. **Inventory truth.** Every new pool must pass the book's own load at boot:
   its tokens and factory are read from the chain and must match the venue's
   factory, or the pool is refused and logged. Expect no new refusals.
2. **Quote exactness.** A probe loads the book for each new pool at a pinned
   block and compares `quote_exact_input_multi_tick` with the venue's quoter at
   the same block. Both directions, at 0.01, 0.1 and 1 WETH and their USDC or
   cbBTC equivalents. This is the R4 check (2026-09-30), repeated for the new
   pools. Slipstream charges the first swap of a block a different fee, and
   the book prices the after-first fee. So each Slipstream pool is checked at
   a block in which it had already swapped. A difference beyond a few units is
   a defect to fix before the restart.
3. **Boot.** The shadow boots with about 24 pools held and 74 cycles.
4. **Run health.** The first reports show `unpriced 0`, no read failures, and
   a block named for Tier 2.

## Risks

- The 30 bps pools pay only when a dislocation exceeds their fee. Most of what
  they add will be near misses, which the near-miss bands will show.
- A Slipstream fee is dynamic. The measured fee is one moment's, so a pool near
  the ceiling (two read 3,000 and 3,012 ppm) may fall either side of it.
- An event touching WETH/USDC prices up to 42 templates instead of 20. That is
  still milliseconds of work, and `flashblock_engine` already sizes its cap to
  the resident set.

## Rollout

Tests first for the config section. Then mutation checks, all five CI gates,
the clean-worktree check of each commit, a release build, and a restart. The
restart resets the 14-day clock, and the report to the operator says where it
now counts from.
