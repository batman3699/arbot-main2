# Arbot Tunable Settings Reference

This is a practical reference for the tunable settings surfaced in the current docs and env templates.

For each setting or setting family, this guide explains:
- what it means
- what it does
- what it affects
- when to adjust it
- how to adjust it
- a sensible starting point

Important:
- if the same setting appears more than once in an env file, the **last value usually wins**
- keep **one value per key** unless you deliberately want later lines to override earlier ones
- keep secrets and private overrides in `.env`
- keep non-secret runtime knobs in `ops/inputs.yaml` where practical

---

# 1. Before you tune anything

Tune in this order:

1. confirm the correct chain is selected
2. confirm RPC works
3. confirm pool inventory exists
4. confirm quotes succeed
5. confirm edges are built
6. confirm edges are scanned
7. only then tune search, risk, and execution settings

If you skip this order, you can waste time tuning a system that is actually broken earlier.

---

# 2. Beginner notes: how to read and use this reference

If you know almost nothing about these settings, use this method.

## Step 1: decide what kind of setting it is

Almost every setting belongs to one of these groups:
- chain selection
- RPC/provider
- venue/protocol addresses
- broadcast/relays
- search/sizing
- risk/health
- shadow/logging

If you know the group, the setting becomes much easier to understand.

## Step 2: change only one thing at a time

Bad approach:
- change ten settings
- run once
- hope it works

Good approach:
1. change one setting
2. run smoke test or shadow mode
3. compare before vs after
4. decide whether that setting helped

## Step 3: use the safest environment while learning

If you are still learning:
- use one chain only
- use shadow mode
- use a local fork for dangerous tests
- avoid live execution
- write down every change

## Step 4: know the difference between “broken” and “tuned badly”

A broken system has problems like:
- `connection refused`
- `block out of range`
- config parse errors
- missing required addresses
- no pool inventory

A badly tuned system is still functioning, but performs poorly:
- too few edges
- too few cycles
- quote queue saturation
- weak profitability
- too much long-tail noise

Do not tune a broken system.
Fix the break first.

## Step 5: understand what each kind of change usually does

### If you increase a time budget
Examples:
- `QUOTE_BUDGET_MS`
- `SIMULATION_BUDGET_MS`
- `CYCLE_SEARCH_TIMEOUT_MS`

Usually this means:
- more work is allowed to finish
- runtime may become slower
- queue pressure may increase

### If you increase a cap
Examples:
- `MAX_CANDIDATE_PATHS`
- `MAX_HOPS`
- `MAX_BELLMAN_CYCLES`

Usually this means:
- wider search surface
- more CPU and RPC work
- more noise if your token/venue set is weak

### If you tighten a threshold
Examples:
- `PROFIT_MARGIN_BPS`
- `EDGE_PRUNE_MAX_SLIPPAGE_BPS`
- `MAX_QUOTE_BLOCK_LAG`

Usually this means:
- fewer but higher-quality candidates
- lower false positives
- possible loss of borderline opportunities

### If you loosen a threshold
Usually this means:
- more candidate flow
- more noise
- higher risk of low-quality opportunities

## Step 6: use recommended settings as a starting point, not magic truth

“Recommended” means:
- safe enough to start
- easier to debug
- usually reasonable

It does **not** mean:
- guaranteed best forever
- guaranteed profit
- guaranteed fit for every chain and market condition

## Step 7: when in doubt, return to the simplest safe profile

That means:
- one chain only
- shadow mode on
- no mixed upstream/local providers
- sensible budgets
- sensible slippage and margin thresholds
- clean, deduplicated env file

That simple profile is the easiest to understand and debug.

---

# 3. Chain selection and config loading

## CHAIN
What it means: the main chain Arbot is operating on.

What it does: selects the active chain context for scanning, quoting, simulation, and execution.

What it affects:
- active RPC endpoints
- active venues
- active flashloan providers
- executor address resolution
- chain-specific runtime behavior

When to adjust: whenever you want to switch focus between Ethereum, Base, Arbitrum, etc.

How to adjust:
```bash
export CHAIN=base
```

Recommended:
- `CHAIN=base` when tuning Base
- `CHAIN=ethereum` when tuning Ethereum

## CHAIN_LIST
What it means: the list of chains Arbot is allowed to consider in one run.

What it does: controls multi-chain scope.

What it affects:
- quote/search budget distribution
- concurrency
- log noise
- cross-chain confusion

When to adjust: when you want single-chain tuning or multi-chain operation.

How to adjust:
```bash
CHAIN_LIST=base
CHAIN_LIST=ethereum,base
```

Recommended:
- use one chain while tuning
- do not use `CHAIN_LIST=ethereum,base` while diagnosing only one chain unless you truly need it

## REGISTRY_FILE
What it means: path to the local registry file.

What it does: tells Arbot where to load shared chain and venue metadata from.

Recommended: keep the repo-local default unless you intentionally maintain multiple registries.

## REGISTRY_CACHE_PATH
What it means: path for cached registry data.

What it does: stores resolved registry state locally.

Recommended: leave as default.

## DOTENV_FILE
What it means: the env file the runtime expects to use.

What it does: helps document which env profile is intended.

Recommended: use a chain-specific env file such as `.env.base` or `.env.ethereum`.

## OPS_INPUTS_STRICT
What it means: whether runtime config parsing should be strict.

What it does: controls how aggressively the runtime rejects config issues.

Recommended:
- `false` while iterating and debugging
- `true` only after config is stable and clean

## OFFLINE_MODE
What it means: run without live network execution.

What it does: useful for config validation and some startup checks.

How to adjust:
```bash
OFFLINE_MODE=1 cargo run --release
```

Recommended:
- `1` for config checks
- `0` for real runtime

---

# 4. RPC, provider, and transport settings

## ETH_RPC_URL / BASE_RPC_URL
Primary single HTTP RPC endpoint for a chain.

Use it when you want one clearly defined endpoint.

Recommended:
- local fork URL in fork-debug mode
- stable production provider in live mode

## ETH_RPC_URLS / BASE_RPC_URLS
Comma-separated HTTP RPC list.

Use it for provider redundancy and fallback.

Important warning:
do **not** mix a local fork and upstream provider casually during fork-debug sessions unless you are sure the code will not mix block numbers between them.

Recommended:
- fork mode: only local fork, or local fork first with careful isolation
- live mode: upstream providers only

## ETH_RPC_URL_FALLBACKS
Fallback-only HTTP RPC list.

Recommended: use 1–2 quality backups, not a giant list.

## ETH_WS_RPC_URL / ETH_WS_RPC_URLS / BASE_WS_RPC_URL / BASE_WS_RPC_URLS
Websocket endpoints.

Used for subscriptions and low-latency updates.

Important warning:
in fork-debug mode, mixing upstream WS with local HTTP can create `block out of range` if block numbers come from upstream and calls go to fork.

Recommended:
- fork-debug: disable WS or point it to the same local source if supported
- live mode: use strong upstream WS

## ETH_PRIVATE_RPC_HTTP_URL / BASE_PRIVATE_RPC_HTTP_URL
Chain-specific private or special HTTP RPC.

Use this only when you intentionally want a private or special endpoint.

Recommended: set it explicitly only when you know why.

## ETH_GAS_MODEL / BASE_GAS_MODEL
Gas pricing model for the chain.

Recommended:
- Ethereum: `eip1559`
- Base: `l2`

## ETH_GAS_RPC_METHOD
RPC method used for Ethereum gas estimation.

Recommended: `eth_feeHistory`.

## MAX_QUOTE_BLOCK_LAG
Maximum tolerated block lag when using quote data.

Affects:
- freshness
- edge acceptance
- pruning

Recommended:
- start with `2`
- tighten to `1` only if your pipeline is stable and fast
- loosen only if you understand the stale-data risk

## RPC_MAX_BACKOFF_SECS
Maximum backoff when retrying RPC failures.

Recommended: `30`.

## WS_CONNECT_TIMEOUT_SECS
WebSocket connection timeout.

Recommended: `10`.

---

# 5. Venue, validation, and chain-specific protocol settings

## ETH_UNIV3_FACTORY / BASE_UNIV3_FACTORY
UniV3 factory address.

Used to resolve pools and validate pool existence.

Recommended: keep canonical chain deployment addresses.

## ETH_UNIV3_QUOTER / BASE_UNIV3_QUOTER
UniV3 quoter address.

Used for quote estimation.

Recommended: keep canonical chain deployment addresses.

## ETH_UNIV3_ROUTER / BASE_UNIV3_ROUTER
UniV3 execution router.

Used in execution and integration assumptions.

Recommended: use the canonical router your adapter expects.

## BASE_UNIVERSAL_ROUTER
Optional newer Base universal router.

Recommended: keep recorded if useful, but do not switch execution assumptions casually.

## ETH_BAL_VAULT / BASE_BAL_VAULT
Balancer vault address.

Recommended: keep canonical vault address.

## ETH_AAVE_POOL / BASE_AAVE_POOL
Aave v3 pool address.

Recommended: keep canonical chain pool address.

## ETH_PERMIT2_ADDRESS / BASE_PERMIT2_ADDRESS
Permit2 address.

Recommended: use the canonical Permit2 address.

## ETH_UNIV3_VALIDATION_PATH
Known sanity-check path for Ethereum UniV3.

Recommended: use a liquid core pair only.

## ETH_UNIV3_VALIDATION_AMOUNT_WEI
Input amount for that validation quote.

Recommended: use a small but nontrivial amount.

## ETH_NATIVE_USD_PRICE
Reference price for native asset if your runtime uses it.

Recommended: keep updated if used.

---

# 6. Broadcast, relays, and send policy

## MEV_ROLE
Runtime role mode such as `searcher`.

Recommended: `searcher`.

## ENABLE_DEFAULT_PRIVATE_RELAYS
Whether default relay handling is enabled.

Warning:
Ethereum relay defaults are not automatically correct for Base.

Recommended:
- Ethereum: may be useful
- Base: prefer chain-specific relay settings

## ETH_PRIVATE_RELAY_URL / ETH_PRIVATE_RELAY_URLS
Ethereum-specific private relay endpoints.

Recommended: use for Ethereum only.

## PRIVATE_RELAY_URL / PRIVATE_RELAY_URLS
Generic relay endpoints.

Warning:
if these point to Ethereum relays and you are tuning Base, that can be misleading or wrong.

Recommended: use chain-specific relay settings where possible.

## PRIVATE_RELAY_TIMEOUT_MS
How long the runtime waits for relay submission before giving up.

Recommended:
- `1200` to `2000` ms is a reasonable starting range
- lower for fast-fail
- higher only if your relay is slow but still worthwhile

## ETH_PUBLIC_MEMPOOL_JITTER_BPS / PUBLIC_MEMPOOL_JITTER_BPS
Randomization/jitter around public mempool behavior.

Recommended: `35`.

## SEARCHER_PRIORITY_FEE_WEI
Priority fee for searcher sends.

Recommended: `1000000000` is a fair starting point, but should remain congestion-aware.

## FILLER_PRIORITY_FEE_WEI
Priority fee for filler/secondary mode.

Recommended: `2000000000` is acceptable if you want filler behavior more aggressive than searcher mode.

## CHAOS_RELAY_REJECT_BPS / CHAOS_PUBLIC_REJECT_BPS / CHAOS_BROADCAST_DELAY_MS / CHAOS_DISABLE_WS
Chaos or resilience testing toggles.

Recommended:
- keep zero/false during normal operation
- turn on only during deliberate failure-path tests

---

# 7. Shadow mode, logging, and observability

## SHADOW_MODE
Runs the full runtime without sending live transactions.

Recommended:
- `true` during tuning
- `false` only when you trust the setup

## SHADOW_LOG_PATH
Where shadow mode logs are written.

Recommended: use chain-specific files such as:
- `logs/shadow.base.jsonl`
- `logs/shadow.ethereum.jsonl`

## SHADOW_TAG
Run tag for shadow mode.

Recommended: use descriptive values like:
- `base-shadow`
- `ethereum-shadow`
- `anvil-ethereum`

## PROMETHEUS_PORT
Metrics port.

Recommended: `9100` if unused.

## RUST_LOG
Log filter.

Recommended:
- normal runtime: `info,arb_exec=debug`
- deeper debugging: add module-specific targets such as `venue::univ3=debug`

---

# 8. Feature gates

## FEATURE_CYCLE_ARB
Enable cycle arbitrage.

Recommended: `true`.

## FEATURE_BACKRUN / FEATURE_SANDWICH / FEATURE_LIQUIDATIONS / FEATURE_BRIDGE
Enable optional strategy families.

Recommended:
- keep them `false` unless actively building and validating them
- if you enable one, do it in isolation first

---

# 9. Capital management

## BASE_AMOUNT_WEI
Base notional amount used for baseline sizing.

Recommended: keep it large enough to avoid trivial quote starvation, but aligned with your sizing model.

## MIN_FLASH_LOAN_WEI
Minimum flashloan size allowed.

Recommended: keep above dust.

## MAX_FLASH_LOAN_WEI
Maximum flashloan size allowed.

Recommended: start conservatively and raise only after proving execution quality.

## REINVEST_BPS
How much profit is reinvested.

Recommended:
- `10000` = reinvest everything
- lower it if you want to siphon gains

## SIPHON_BPS
How much profit is siphoned away.

Recommended:
- `0` if fully compounding
- raise only when you intentionally want extraction

## COMPOUND_GROWTH_UNIT_WEI
Unit step for compounding growth.

Recommended: keep modest and understandable.

## COMPOUND_MAX_BASE_WEI
Max base size allowed through compounding.

Recommended: use a cap you are genuinely comfortable with.

## SIPHON_THRESHOLD_WEI
Profit threshold before siphoning starts.

Recommended: `0` if siphoning is disabled.

---

# 10. Search and sizing constraints

## MIN_HOPS
Minimum hops in a cycle.

Recommended: `3`.

## MAX_HOPS / MAX_HOPS_CAP
Maximum allowed hop count.

Recommended:
- `5` or `6` is a sensible practical ceiling
- avoid pushing higher until coverage and stability are proven

## BELLMAN_MAX_RELAXATIONS
Maximum relaxations in Bellman-style search.

Recommended: `6`.

## MAX_BELLMAN_CYCLES
Upper bound on Bellman-discovered candidate cycles.

Recommended:
- `14` is conservative
- raise only if viable candidates are clearly being cut too early

## MAX_CANDIDATE_PATHS
Maximum candidate paths passed downstream.

Recommended: `12`.

## CYCLE_SEARCH_TIMEOUT_MS
Time budget for search.

Recommended:
- `1200` is okay to start
- `1500–3000` is a useful diagnostic range if search starvation is suspected

## QUOTE_BUDGET_MS
Time budget for quote work.

Recommended:
- `800–1500` is a more forgiving starting range
- lower values can be too tight during debugging

## SIMULATION_BUDGET_MS
Time budget for simulation.

Recommended:
- `1000–1500` is a good diagnostic range

## EDGE_SLIPPAGE_BPS
Per-edge slippage allowance.

Recommended:
- `35–40` is a strong starting range
- higher values let in lower-quality routes

## EDGE_MIN_HEALTH_SCORE_BPS
Minimum health score for an edge to survive.

Recommended: `7000`.

## MIN_EDGE_MAX_INPUT_WEI
Minimum edge max input threshold.

Recommended:
- `0` while diagnosing
- raise later if dust edges pollute the graph

## MIN_LIQUIDITY_TOKENS
Minimum token-side liquidity threshold.

Recommended:
- `1` is light filtering
- raise if long-tail junk dominates

## POOL_DEPTH_REFRESH_SECS
How often pool depth is refreshed.

Recommended:
- `30` is a good starting point for active tuning
- longer intervals can go stale

## MAX_GAS_PRICE_WEI
Hard cap on acceptable gas price.

Recommended: keep as a safety ceiling and adjust by chain conditions.

## MAX_GAS_PRICE_CONGESTION_BPS
Congestion tolerance factor for gas pricing.

Recommended: current-style values are usually fine unless gas rejection is clearly too aggressive.

## PROFIT_MARGIN_BPS
Desired extra margin over raw breakeven.

Recommended:
- `35` is a safer starting point
- `25` is more aggressive
- do not set too low while still debugging

## OPPORTUNITY_COST_WEI
Manual extra cost floor.

Recommended: `0` unless you intentionally want that extra penalty.

## CROSS_CHAIN_PROFIT_BPS / CROSS_CHAIN_MIN_PROFIT_WEI
Cross-chain thresholds.

Recommended: leave as-is unless you are actively using cross-chain strategies.

## EDGE_PRUNE_MAX_SLIPPAGE_BPS
Maximum slippage allowed before pruning an edge.

Recommended:
- `50` is a cleaner starting point
- `65` is looser

## EDGE_PRUNE_MIN_SCORE
Minimum prune score threshold.

Recommended: `1`.

## EDGE_PRUNE_LIQUIDITY_WEIGHT / EDGE_PRUNE_PROFIT_WEIGHT / EDGE_PRUNE_SLIPPAGE_WEIGHT
Weights used in edge scoring.

Recommended: current balanced values are fine. Adjust only after evidence that pruning is systematically wrong.

## CONGESTION_EMA_ALPHA / COMPETITION_EMA_ALPHA
EMA smoothing factors for congestion and competition.

Recommended: current-style values are fine. Raise for faster reaction, lower for smoother behavior.

---

# 11. JIT LP controls

## JIT_LP_ENABLED
Enable JIT LP behavior.

Recommended: leave disabled until that strategy is intentionally developed.

## JIT_MIN_AMOUNT_WEI / JIT_SEED_BPS / JIT_TICK_RANGE / JIT_DISABLE_ON_MIN_OUT_FAIL
JIT LP execution controls.

Recommended: leave disabled unless working specifically on JIT LP.

---

# 12. ERC-3156 optional lender

## ERC3156_LENDER / ERC3156_FEE_BPS
Optional ERC-3156 lender settings.

Recommended: set only when you actually add a real ERC-3156 lender.

---

# 13. Circuit breaker and health controls

## CB_HOURLY_LOSS_LIMIT_WEI / CB_DAILY_LOSS_LIMIT_WEI
Loss ceilings.

Recommended: set them to amounts you are genuinely willing to lose.

## CB_MAX_CONSECUTIVE_FAILURES
Max consecutive failures before the breaker matters.

Recommended: `4`.

## ETH_HEALTH_EMA_ALPHA / HEALTH_EMA_ALPHA
Smoothing for health metrics.

Recommended: `0.35`.

## ETH_HEALTH_MAX_REJECT_RATE_EMA / HEALTH_MAX_REJECT_RATE_EMA
Maximum acceptable reject-rate trend.

Recommended: `0.10`.

## ETH_HEALTH_MAX_LATENCY_MS_EMA / HEALTH_MAX_LATENCY_MS_EMA
Maximum acceptable latency trend.

Recommended:
- Ethereum-specific: tighter such as `800`
- generic: around `1200–1500`
- tighten if you want stricter rejection
- loosen only if provider latency is acceptable but noisy

## ETH_HEALTH_MIN_SUCCESS_RATE_EMA / HEALTH_MIN_SUCCESS_RATE_EMA
Minimum acceptable success-rate trend.

Recommended: `0.95`.

---

# 14. Strategy-specific monitors

## BACKRUN_MONITOR / BACKRUN_MONITOR_ENABLED
Backrun monitoring toggles.

Recommended: keep `false` unless actively working on backruns.

## BACKRUN_MIN_AMOUNT_WEI / BACKRUN_MIN_PRICE_IMPACT_BPS / BACKRUN_POLL_INTERVAL_MS
Backrun screening controls.

Recommended: leave at low/no-op values until backrun mode is intentionally enabled.

## SANDWICH_MONITOR / SANDWICH_MONITOR_ENABLED / SANDWICH_MIN_PROFIT_WEI
Sandwich strategy controls.

Recommended: keep disabled unless explicitly building that strategy.

## BRIDGE_MAX_TIME_SECS / BRIDGE_ROUTES / BRIDGE_ROUTES_FILE
Bridge strategy controls.

Recommended: leave disabled or zero unless actively using bridge mode.

---

# 15. Low-liquidity and pool monitors

## LOW_LIQUIDITY_FACTORIES
Factories treated as low-liquidity-sensitive.

Recommended: keep only relevant factories.

## LOW_LIQUIDITY_LOOKBACK_BLOCKS
How far back to inspect for low-liquidity signals.

Recommended: `120`.

## LOW_LIQUIDITY_MAX_TOTAL_TOKENS
How many tokens low-liquidity analysis should consider.

Recommended: `20`.

## LOW_LIQUIDITY_PRICE_DEVIATION_BPS
Threshold for price deviation in low-liquidity detection.

Recommended: `75`.

## POOL_MONITOR_POLL_MS
How often pool monitor checks run.

Recommended: `60000`.

## POOL_MONITOR_STALE_MS
How long until pool monitor data is considered stale.

Recommended: `300000`.

---

# 16. Accounting and alerts

These are usually commented out until intentionally needed.

## ACCOUNTING_ENABLED
Turns accounting on.

## ACCOUNTING_DIR
Accounting folder.

## ACCOUNTING_TRADE_LOG
Trade log path.

## ACCOUNTING_DAILY_SUMMARY
Daily summary path.

## ACCOUNTING_EVENT_LOG
Event log path.

## TAX_RESERVE_BPS
Tax reserve percentage.

## TAX_WALLET_ADDRESS
Tax wallet address.

## TAX_STABLECOIN_SYMBOL
Stablecoin symbol used for accounting assumptions.

## TAX_EXCHANGE_API_URL / TAX_EXCHANGE_API_KEY
External accounting integration.

## ALERT_WEBHOOK_URL
Alert destination.

## ALERT_DAILY_NET_TARGET_WEI
Daily target threshold.

## ALERT_REVERT_RATE_THRESHOLD
Alert on excessive revert rate.

## ALERT_RPC_ERROR_RATE_THRESHOLD
Alert on excessive RPC errors.

Recommended:
leave these disabled until you are ready to build real accounting and alerting workflows.

---

# 17. Recommended baseline profiles

## Safe tuning profile

Use while debugging:
- one chain only
- `SHADOW_MODE=true`
- `FEATURE_CYCLE_ARB=true`
- other major strategy gates off
- `QUOTE_BUDGET_MS=800 to 1500`
- `SIMULATION_BUDGET_MS=1000 to 1500`
- `CYCLE_SEARCH_TIMEOUT_MS=1500 to 3000`
- `PROFIT_MARGIN_BPS=35`
- `EDGE_SLIPPAGE_BPS=35 to 40`
- `EDGE_PRUNE_MAX_SLIPPAGE_BPS=50`
- no mixed upstream/local providers in fork-debug mode

## More aggressive profile

Use only after stability:
- lower `PROFIT_MARGIN_BPS`
- allow wider `EDGE_PRUNE_MAX_SLIPPAGE_BPS`
- enable more venues and tokens
- keep health and circuit breaker protections intact

---

# 18. Final operator rule

If you change a setting, write down:
1. the old value
2. the new value
3. why you changed it
4. what symptom you expected to improve
5. what actually happened after the change

That is how you stop random tuning and start making progress.
