# Arbot Tunable Settings Reference

This is a practical reference for the tunable settings surfaced in your current Arbot environment and main runtime docs.

It is designed to answer, for each setting:
- what it means
- what it does
- what it affects when adjusted
- when or why to adjust it
- how to adjust it
- a recommended starting point

Important:
- If the same setting appears more than once, the **last value usually wins** when the shell loads `.env`.
- Use **one value per key** unless you intentionally want later lines to override earlier lines.
- Keep **secrets and local/private overrides** in `.env`.
- Keep **non-secret runtime knobs** in `ops/inputs.yaml` where possible.

---

# 1. Before you tune anything

Tune in this order:

1. Confirm the right chain is selected.
2. Confirm RPC works.
3. Confirm pool inventory exists.
4. Confirm quotes succeed.
5. Confirm edges are built.
6. Confirm edges are scanned.
7. Only then tune search, risk, and execution settings.

If you skip this order, you can waste time tuning a system that is actually broken in an earlier stage.

---

# 2. Core selection and config loading

## CHAIN
What it means: the primary chain Arbot is operating on.

What it does: selects the active chain context for scanning, quoting, simulation, and execution.

What it affects:
- active RPC endpoints
- active venues
- active flashloan providers
- executor address resolution
- chain-specific logs and runtime behavior

When to adjust: whenever you want to switch focus between Ethereum, Base, Arbitrum, etc.

How to adjust:
```bash
export CHAIN=base
```

Recommended:
- `CHAIN=base` when tuning Base
- `CHAIN=ethereum` when tuning Ethereum

## CHAIN_LIST
What it means: the list of chains the runtime is allowed to consider in the same run.

What it does: controls multi-chain runtime scope.

What it affects:
- quote/search budget distribution
- concurrency
- log noise
- cross-chain confusion during debugging

When to adjust: when you want single-chain tuning or multi-chain operation.

How to adjust:
```bash
CHAIN_LIST=base
CHAIN_LIST=ethereum,base
```

Recommended: use one chain while tuning. Do not use `CHAIN_LIST=ethereum,base` while diagnosing one chain unless you truly need it.

## REGISTRY_FILE
What it means: path to the local registry file.

What it does: tells Arbot where to load shared chain and venue metadata from.

Recommended: keep the repo-local default unless you intentionally maintain multiple registries.

## REGISTRY_CACHE_PATH
What it means: path for the cached registry.

What it does: stores resolved registry state locally.

Recommended: leave as default.

## DOTENV_FILE
What it means: the env file the runtime expects to use.

What it does: helps document which env profile is intended.

Recommended: use a chain-specific env file when practical, such as `.env.base` or `.env.ethereum`.

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

Recommended: use `1` for config checks, `0` for real runtime.

---

# 3. RPC, provider, and chain transport settings

## ETH_RPC_URL / BASE_RPC_URL
Primary single HTTP RPC endpoint for a chain.

Use it when you want one clearly defined endpoint.

Recommended:
- local fork URL in fork-debug mode
- stable production provider in live mode

## ETH_RPC_URLS / BASE_RPC_URLS
Comma-separated HTTP RPC list.

Use it for provider redundancy and fallback.

Important warning: do **not** mix a local fork and upstream provider casually during fork-debug sessions unless you are sure the code will not mix block numbers between them.

Recommended:
- fork mode: only local fork, or local fork first with careful isolation
- live mode: upstream providers only

## ETH_RPC_URL_FALLBACKS
Fallback-only HTTP RPC list.

Recommended: use 1–2 good backups, not a huge list.

## ETH_WS_RPC_URLS / BASE_WS_RPC_URLS
Comma-separated websocket endpoints.

Used for subscriptions, event streaming, and low-latency updates.

Important warning: in fork-debug mode, mixing upstream WS with local HTTP can create `block out of range` if block numbers come from upstream and calls go to fork.

Recommended:
- fork-debug: disable WS or point WS to the same local fork if supported
- live mode: use good upstream WS

## ETH_PRIVATE_RPC_HTTP_URL / BASE_PRIVATE_RPC_HTTP_URL
Chain-specific private HTTP RPC.

Use it when you intentionally want a private or special endpoint.

Recommended: set it explicitly only when you know why.

## ETH_GAS_MODEL / BASE_GAS_MODEL
Chain gas pricing mode.

Recommended:
- Ethereum: `eip1559`
- Base: `l2`

## ETH_GAS_RPC_METHOD
RPC method used to estimate gas pricing on Ethereum.

Recommended: `eth_feeHistory`.

## MAX_QUOTE_BLOCK_LAG
Maximum tolerated block lag when using quote data.

What it affects:
- edge freshness
- graph pruning
- quote acceptance

Recommended:
- start with `2`
- tighten to `1` only if the pipeline is stable and fast
- loosen only if you understand the stale-data risk

## RPC_MAX_BACKOFF_SECS
Max backoff when retrying RPC failures.

Recommended: `30`.

## WS_CONNECT_TIMEOUT_SECS
WebSocket connect timeout.

Recommended: `10`.

---

# 4. Venue, validation, and chain-specific protocol settings

## ETH_UNIV3_FACTORY / BASE_UNIV3_FACTORY
Canonical UniV3 factory address for the chain.

Keep canonical chain deployment addresses.

## ETH_UNIV3_QUOTER / BASE_UNIV3_QUOTER
Quoter contract address for UniV3-style quoting.

Keep canonical chain deployment addresses.

## ETH_UNIV3_ROUTER / BASE_UNIV3_ROUTER
Swap router used for UniV3 execution pathing.

Use the canonical chain-specific router your adapter expects.

## BASE_UNIVERSAL_ROUTER
Optional newer Universal Router on Base.

Keep recorded, but do not switch core runtime assumptions casually.

## ETH_BAL_VAULT / BASE_BAL_VAULT
Balancer vault address.

Keep canonical vault address.

## ETH_AAVE_POOL / BASE_AAVE_POOL
Aave v3 pool address.

Keep canonical chain pool address.

## BASE_PERMIT2_ADDRESS / ETH_PERMIT2_ADDRESS
Permit2 contract address.

Use the canonical Permit2 address.

## ETH_UNIV3_VALIDATION_PATH
Known validation path for Ethereum UniV3.

Use a liquid core pair only.

## ETH_UNIV3_VALIDATION_AMOUNT_WEI
Input amount for the validation quote.

Use a small but nontrivial amount.

## ETH_NATIVE_USD_PRICE
Reference price for native asset when needed.

Keep updated if the runtime actually depends on it.

---

# 5. Broadcast, relay, and send policy

## MEV_ROLE
Runtime role mode, such as `searcher`.

Recommended: `searcher`.

## ENABLE_DEFAULT_PRIVATE_RELAYS
Whether default relay handling is enabled.

Warning: Ethereum relay defaults are not automatically correct for Base.

Recommended:
- Ethereum: may be useful
- Base: prefer chain-specific private relay settings instead of blindly reusing Ethereum relay URLs

## ETH_PRIVATE_RELAY_URL / ETH_PRIVATE_RELAY_URLS
Ethereum-specific private relay endpoint(s).

Recommended: use for Ethereum only.

## PRIVATE_RELAY_URL / PRIVATE_RELAY_URLS
Generic relay endpoint(s).

Warning: if these point to Ethereum relays and you are tuning Base, that can be misleading or wrong.

Recommended: use chain-specific relay settings wherever possible.

## PRIVATE_RELAY_TIMEOUT_MS
How long the runtime waits for relay submission before giving up.

Recommended:
- `1200` to `2000` ms is a reasonable starting range
- lower for fast-fail environments
- higher only if your relays are slow but still worthwhile

## ETH_PUBLIC_MEMPOOL_JITTER_BPS / PUBLIC_MEMPOOL_JITTER_BPS
Randomization/jitter applied around public mempool behavior.

Recommended: `35`.

## SEARCHER_PRIORITY_FEE_WEI
Priority fee for searcher sends.

Recommended: current `1000000000` (1 gwei) is a fair starting point, but it should remain congestion-aware.

## FILLER_PRIORITY_FEE_WEI
Priority fee for filler / secondary mode.

Recommended: `2000000000` is fine if you want filler behavior more aggressive than searcher mode.

## CHAOS_RELAY_REJECT_BPS / CHAOS_PUBLIC_REJECT_BPS / CHAOS_BROADCAST_DELAY_MS / CHAOS_DISABLE_WS
Deliberate chaos/test toggles.

Recommended:
- keep all at zero/false during normal operation
- turn on only during deliberate resilience tests

---

# 6. Shadow mode, logging, and observability

## SHADOW_MODE
Run the full runtime without sending live transactions.

Recommended:
- `true` during tuning
- `false` only when you trust the setup

## SHADOW_LOG_PATH
Where shadow mode logs are written.

Recommended: use chain-specific files like:
- `logs/shadow.base.jsonl`
- `logs/shadow.ethereum.jsonl`

## SHADOW_TAG
Tag for the shadow run.

Recommended: use descriptive values like `base-shadow`, `anvil-ethereum`, or `integration-check`.

## PROMETHEUS_PORT
Metrics port.

Recommended: `9100` if unused by other processes.

## RUST_LOG
Rust log filter.

Recommended:
- general runtime: `info,arbot=debug`
- deeper debugging: add module-specific targets such as `venue::univ3=debug`

---

# 7. Feature gates

## FEATURE_CYCLE_ARB
Enable cycle arbitrage strategy.

Recommended: `true`.

## FEATURE_BACKRUN
## FEATURE_SANDWICH
## FEATURE_LIQUIDATIONS
## FEATURE_BRIDGE
Enable optional strategy families.

Recommended:
- keep them `false` unless you are actively building and validating those modes
- if you enable one, do it in isolation first

---

# 8. Capital management

## BASE_AMOUNT_WEI
Base notional amount used for trial size / baseline.

Recommended: keep it large enough to avoid trivial quote starvation, but aligned with your actual sizing model.

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
- lower it if you want to siphon off gains

## SIPHON_BPS
How much profit is siphoned away.

Recommended:
- `0` if fully compounding
- raise only when you want explicit profit extraction

## COMPOUND_GROWTH_UNIT_WEI
Unit step for compounding growth.

Recommended: keep modest and understandable.

## COMPOUND_MAX_BASE_WEI
Max base size allowed through compounding.

Recommended: use a cap you are comfortable with operationally.

## SIPHON_THRESHOLD_WEI
Profit threshold before siphoning starts.

Recommended: `0` is fine if siphoning is disabled.

---

# 9. Search and sizing constraints

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
- raise only if viable candidates are being cut too early

## MAX_CANDIDATE_PATHS
Maximum candidate paths sent downstream.

Recommended: `12`.

## CYCLE_SEARCH_TIMEOUT_MS
Time budget for search.

Recommended:
- current `1200` is okay for starting
- try `1500–3000` when diagnosing search starvation

## QUOTE_BUDGET_MS
Time budget for quote work.

Recommended:
- `600` is workable but somewhat tight
- `800–1500` is a more forgiving diagnostic range

## SIMULATION_BUDGET_MS
Time budget for simulation.

Recommended:
- `850` is okay to start
- `1000–1500` can help during debugging

## EDGE_SLIPPAGE_BPS
Per-edge slippage allowance.

Recommended:
- `35–40` is a strong starting range
- higher values can let in lower-quality routes

## EDGE_MIN_HEALTH_SCORE_BPS
Minimum health score for an edge to survive.

Recommended: `7000`.

## MIN_EDGE_MAX_INPUT_WEI
Minimum edge max input threshold.

Recommended: `0` while diagnosing; raise later if dust edges pollute the graph.

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

Recommended: current value is fine as a safety ceiling; adapt by chain and market.

## MAX_GAS_PRICE_CONGESTION_BPS
Congestion tolerance factor for gas pricing.

Recommended: current value is reasonable unless gas rejection is clearly too aggressive.

## PROFIT_MARGIN_BPS
Desired extra margin over raw breakeven.

Recommended:
- `35` is a good safer starting point
- `25` is more aggressive
- do not set too low while still debugging

## OPPORTUNITY_COST_WEI
Manual extra cost floor.

Recommended: `0` unless you intentionally want that extra penalty.

## CROSS_CHAIN_PROFIT_BPS / CROSS_CHAIN_MIN_PROFIT_WEI
Cross-chain opportunity thresholds.

Recommended: leave as-is unless you are actively using cross-chain modes.

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

Recommended: current values are balanced and acceptable. Adjust only after evidence that pruning is systematically wrong.

## CONGESTION_EMA_ALPHA / COMPETITION_EMA_ALPHA
EMA smoothing factors for congestion/competition signals.

Recommended: current values are fine. Raise for faster reaction, lower for smoother response.

---

# 10. JIT LP controls

## JIT_LP_ENABLED
Enable JIT LP behavior.

Recommended: leave disabled until that strategy is intentionally developed.

## JIT_MIN_AMOUNT_WEI / JIT_SEED_BPS / JIT_TICK_RANGE / JIT_DISABLE_ON_MIN_OUT_FAIL
JIT LP execution controls.

Recommended: leave commented/disabled unless working specifically on JIT LP.

---

# 11. ERC-3156 optional lender

## ERC3156_LENDER / ERC3156_FEE_BPS
Optional ERC-3156 flashloan integration settings.

Recommended: set only when you actually add a real ERC-3156 lender.

---

# 12. Circuit breaker and health controls

## CB_HOURLY_LOSS_LIMIT_WEI / CB_DAILY_LOSS_LIMIT_WEI
Loss ceilings over different windows.

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
- Ethereum-specific: `800`
- generic/current: `1200`
- tighten if you want stricter health rejection
- loosen only if provider latency is acceptable but noisy

## ETH_HEALTH_MIN_SUCCESS_RATE_EMA / HEALTH_MIN_SUCCESS_RATE_EMA
Minimum acceptable success-rate trend.

Recommended: `0.95`.

---

# 13. Strategy-specific monitors

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

Recommended: leave disabled/zero unless actively using bridge mode.

---

# 14. Low-liquidity and pool monitors

## LOW_LIQUIDITY_FACTORIES
Factories treated as low-liquidity-sensitive.

Recommended: keep known relevant factories only.

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

# 15. Accounting and alerts

These are mostly commented out in your current template.

## ACCOUNTING_ENABLED
Turns accounting on.

## ACCOUNTING_DIR
Where accounting files live.

## ACCOUNTING_TRADE_LOG
Trade log path.

## ACCOUNTING_DAILY_SUMMARY
Daily summary path.

## ACCOUNTING_EVENT_LOG
Event log path.

## TAX_RESERVE_BPS
Tax reserve percentage.

## TAX_WALLET_ADDRESS
Destination for tax reserve handling.

## TAX_STABLECOIN_SYMBOL
Stablecoin symbol used for tax/accounting assumptions.

## TAX_EXCHANGE_API_URL / TAX_EXCHANGE_API_KEY
External tax/accounting integration.

## ALERT_WEBHOOK_URL
Alert destination.

## ALERT_DAILY_NET_TARGET_WEI
Daily target threshold.

## ALERT_REVERT_RATE_THRESHOLD
Alert on excessive revert rate.

## ALERT_RPC_ERROR_RATE_THRESHOLD
Alert on excessive RPC errors.

Recommended: leave these disabled until you are ready to build a real accounting and alerting workflow.

---

# 16. Recommended baseline profiles

## Safe tuning profile
Use while debugging:
- `CHAIN_LIST=base` or `CHAIN_LIST=ethereum`
- `SHADOW_MODE=true`
- `FEATURE_CYCLE_ARB=true`
- all other major strategy gates off
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

# 17. Final operator rule

If you change a setting, write down:
1. the old value
2. the new value
3. why you changed it
4. what exact symptom you expected to improve
5. what actually happened after the change

That is how you stop random tuning and start making progress.

---

# 18. Beginner notes: how to read and use this reference

If you know almost nothing about these settings, use this method.

## Step 1: decide what kind of setting it is

Almost every setting belongs to one of these groups:

- **chain selection**: which chain Arbot works on
- **RPC/provider**: where Arbot gets blockchain data from
- **venue/protocol addresses**: where Uniswap, Aave, Balancer, etc. live
- **broadcast/relays**: how transactions get sent
- **search/sizing**: how hard Arbot looks for opportunities
- **risk/health**: how cautious Arbot is
- **shadow/logging**: how you observe Arbot without risking funds

If you know the group, the setting becomes much easier to understand.

## Step 2: change only one thing at a time

Do **not** change ten settings at once.

Bad approach:
- change quote budget
- change search timeout
- change token universe
- change chain list
- change relays
- then hope it works

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

## Step 5: understand what each change usually does

### If you increase a time budget
Examples:
- `QUOTE_BUDGET_MS`
- `SIMULATION_BUDGET_MS`
- `CYCLE_SEARCH_TIMEOUT_MS`

Usually this means:
- more work is allowed to finish
- runtime may become slower
- queue pressure may increase
- you may get more candidates, or you may just wait longer for the same bad setup

### If you increase a cap
Examples:
- `MAX_CANDIDATE_PATHS`
- `MAX_HOPS`
- `MAX_BELLMAN_CYCLES`

Usually this means:
- wider search surface
- more CPU and RPC work
- more noise if your token/venue set is poor

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
- higher risk of bad or stale opportunities

## Step 6: use recommended settings as a starting point, not magic truth

“Recommended” means:
- safe enough to start
- common-sense values
- easier to debug

It does **not** mean:
- universally best forever
- guaranteed profit
- guaranteed fit for every chain or market condition

## Step 7: when in doubt, return to the simplest safe profile

That means:
- one chain only
- shadow mode on
- no mixed upstream/local providers
- sane search budgets
- sensible slippage and profit thresholds
- clean, deduplicated env file

That simple profile is the easiest to understand and debug.
