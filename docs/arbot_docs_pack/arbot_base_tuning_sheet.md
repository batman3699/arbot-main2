# Arbot Base-Specific Tuning Sheet

Use this when you are tuning **Base only**.

This is a focused one-page guide for:
- venue coverage
- token universe
- scan caps
- fork usage
- shadow mode

Related docs:
- `arbot_master_runbook.md`
- `arbot_fork_and_integration_testing.md`
- `arbot_ingestion_and_venue_onboarding.md`
- `arbot_quick_reference_cheat_sheet.md`
- `arbot_troubleshooting_decision_tree.md`
- `arbot_tunable_settings_reference.md`

---

# 1. First rule: isolate Base

When tuning Base, do **not** tune Ethereum at the same time.

Use:

```bash
export CHAIN=base
export CHAIN_LIST=base
```

Why:
- avoids wasting quote/search budget on Ethereum
- makes logs easier to read
- makes fork and smoke tests less confusing
- prevents mixed-chain conclusions

---

# 2. Base venue coverage: the biggest lever

If Base discovery is weak, the first thing to suspect is usually **insufficient venue coverage**, not a timer.

## Minimum worthwhile Base venue set

Tier 1:
- Uniswap v3
- PancakeSwap v3
- Aerodrome / Slipstream

Tier 2:
- UniV2-like secondary venues
- Balancer if it adds real route diversity
- Curve if it adds real route diversity

## Practical rule

If you only run:
- Uniswap v2
- Uniswap v3

then Base surface area is probably too small.

## Warning

Do not force a venue into the wrong adapter family.

Examples:
- Pancake v3 usually fits `univ3_like`
- Aerodrome classic often needs a Solidly-style adapter
- Slipstream may only fit `univ3_like` if your adapter truly matches its behavior

---

# 3. Base token universe: second biggest lever

A small token universe causes:
- poor edge population
- sparse graph
- missed routes
- weak cycle discovery

## Bad pattern
Only scanning a tiny handpicked set such as 5–6 tokens.

## Better pattern
Use a tiered chain-level universe.

### Tier 1
Always include strong routing assets:
- WETH
- USDC
- major stablecoins
- major wrapped BTC or other liquid majors
- AERO if it acts as a major routing asset

### Tier 2
High-volume Base ecosystem assets.

### Tier 3
Long-tail speculative assets.

## Practical rule

If long-tail tokens are present before a healthy core set exists, you often get:
- noisy quotes
- weak edge density
- wasted budget
- low-quality opportunities

---

# 4. Base pool inventory: do not skip ingesting

For UniV2 / UniV3-style venues, Base needs inventory files.

Important location:

- `data/base/<venue>/pools.jsonl`

Examples:
- `data/base/uniswap_v3/pools.jsonl`
- `data/base/uniswap_v2/pools.jsonl`

## Check inventory exists

```bash
ls -l data/base/uniswap_v3/pools.jsonl
wc -l data/base/uniswap_v3/pools.jsonl
head -3 data/base/uniswap_v3/pools.jsonl
tail -3 data/base/uniswap_v3/pools.jsonl
```

## If missing or tiny, ingest

```bash
cargo run --bin ingest -- \
  --chain base \
  --venue uniswap_v3 \
  --from-block 1 \
  --to-block 25000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

A weak or missing `pools.jsonl` file can be the entire reason your Base graph is sparse.

---

# 5. Base scan caps: recommended starting point

Conservative settings often cause:
- too few hot pools
- too few edges
- weak candidate sets
- `no cycles found`

## Better Base-focused starting point

Use something in this range:

- `max_hot_pools_per_chain_per_venue = 300 to 400`
- `max_edges_hot = 600 to 1000`
- `topk_per_token = 5`
- `hot_pool_refresh_secs = 30 to 45`

## Why
Base can move quickly.
If refresh is too slow and the graph is too narrow, you get stale or weak coverage.

## Warning
Do not widen blindly if quote queue starvation already exists.
If the queue is overloaded, fix:
- venue prioritization
- long-tail suppression
- quote concurrency hygiene

before widening much further.

---

# 6. Base time budgets: recommended starting point

These do not fix bad coverage, but they matter once coverage exists.

Suggested diagnostic range:

```text
CYCLE_SEARCH_TIMEOUT_MS = 1100 to 2500
QUOTE_BUDGET_MS        = 1200 to 1500
SIMULATION_BUDGET_MS   = 1150 to 1500
POOL_DEPTH_REFRESH_SECS = 30
MAX_QUOTE_BLOCK_LAG     = 2
```

If logs say:
- `search budget exceeded`
- `built_edges > 0`
- `edges scanned = 0`

then the issue is likely downstream search/pruning, not basic quote connectivity.

---

# 7. Base fork usage: correct way

## Correct pattern

Anvil may listen on `0.0.0.0`, but connect to `127.0.0.1`.

### Start Base fork

```bash
anvil --host 0.0.0.0 \
  --fork-url "$BASE_UPSTREAM_RPC_URL" \
  --port 8546 \
  --chain-id 8453 \
  --block-time 1
```

### Check Base fork

```bash
ss -ltnp | rg ':8546'
cast block-number --rpc-url http://127.0.0.1:8546
```

### Base smoke test

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=base \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://127.0.0.1:8546" \
CHAIN=base \
BASE_PRIVATE_RPC_HTTP_URL="$BASE_UPSTREAM_RPC_URL" \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

## Biggest fork mistake

Mixing:
- upstream block numbers
- local fork quote/sim calls

That causes:
- `block out of range`

If you restart the fork, restart the bot too.

---

# 8. Base shadow mode: how to use it properly

Shadow mode is the best way to tune Base without risking live sends.

## Start Base shadow mode

```bash
SHADOW_MODE=true \
SHADOW_LOG_PATH=logs/shadow.base.jsonl \
SHADOW_TAG=base-shadow \
CHAIN=base \
cargo run --release
```

## Watch log

```bash
tail -f logs/shadow.base.jsonl
```

## What you want to see
- hot pools refreshing
- quotes succeeding
- edges being built
- edges being scanned
- candidate cycles showing up
- simulations running

## Bad signs
- repeated semaphore wait timeouts
- repeated `search budget exceeded`
- `built_edges > 0` but `edges scanned = 0`
- repeated `block out of range`
- constant long-tail revert spam with no useful candidates

---

# 9. Base quality tuning: suggested values

These are reasonable starting values while tuning quality.

## Risk / quality

```text
PROFIT_MARGIN_BPS           = 35
EDGE_SLIPPAGE_BPS           = 35 to 40
EDGE_PRUNE_MAX_SLIPPAGE_BPS = 50
EDGE_MIN_HEALTH_SCORE_BPS   = 7000
MAX_QUOTE_BLOCK_LAG         = 2
```

Why:
- these values are usually safer than very loose settings while you are still proving the chain setup

---

# 10. Base troubleshooting quick map

## Symptom: no quotes
Check:
1. correct chain selected?
2. RPC working?
3. pool inventory exists?
4. venue addresses correct?
5. path encoding correct?

## Symptom: no edges
Check:
1. inventory exists?
2. token universe too small?
3. venues too narrow?
4. hot-pool caps too low?

## Symptom: quotes succeed but no cycles
Check:
1. search budget too low?
2. downstream pruning too aggressive?
3. edges scanned zero?
4. stale/block-lag rejection?

## Symptom: block out of range
Check:
1. are you mixing upstream latest block with local fork calls?
2. did you restart the bot after restarting the fork?
3. are all Base providers pointing to the same fork in debug mode?

---

# 11. Base tuning checklist

Do these in order:

1. set `CHAIN=base`
2. set `CHAIN_LIST=base`
3. verify Base RPC works
4. verify Base pool inventory exists
5. verify Base token universe is not tiny
6. add missing high-flow venues
7. increase hot-pool and edge caps moderately
8. run Base smoke test
9. run Base shadow mode
10. only then trust live opportunity quality

---

# 12. Final rule for Base

If Base looks weak, do not immediately blame:
- RPC
- quotes
- slippage
- one timer

The most common real causes are:
1. too few venues
2. too few good tokens
3. weak or missing pool inventory
4. graph/search caps too tight
5. fork/provider misuse during debugging
