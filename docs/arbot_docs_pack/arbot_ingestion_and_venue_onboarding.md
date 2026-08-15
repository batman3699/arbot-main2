# Arbot Ingestion and Venue Onboarding Guide

This guide explains:
- what ingesting is
- why it matters
- how to build pool inventory
- how to onboard venues safely
- why poor edge population often comes from weak inventory or weak venue coverage

---

# 1. What “ingesting” means

Ingesting means collecting pool metadata and writing it into Arbot’s local data files.

These files are especially important for UniV2-like and UniV3-like venues.

Without ingestion, Arbot may know a venue exists but still have poor practical coverage.

## Where ingest writes data

The normal location is:

- `data/<chain>/<venue>/pools.jsonl`

Examples:

- `data/ethereum/uniswap_v3/pools.jsonl`
- `data/base/uniswap_v3/pools.jsonl`
- `data/ethereum/uniswap_v2/pools.jsonl`

---

# 2. Why ingest matters

Weak inventory can cause:
- no edges
- weak edge population
- missing routes
- stale routes
- quote spam on bad pairs
- poor search quality

Simple chain of failure:

- bad or empty inventory
- weak graph
- weak graph -> poor or zero cycles
- poor or zero cycles -> no profitable trades found

---

# 3. How to check whether inventory exists

## Example for Base UniV3

```bash
ls -l data/base/uniswap_v3/pools.jsonl
wc -l data/base/uniswap_v3/pools.jsonl
head -3 data/base/uniswap_v3/pools.jsonl
tail -3 data/base/uniswap_v3/pools.jsonl
```

## Example for Ethereum UniV3

```bash
ls -l data/ethereum/uniswap_v3/pools.jsonl
wc -l data/ethereum/uniswap_v3/pools.jsonl
```

If the file is missing or line count is tiny, that is a coverage problem.

---

# 4. How to ingest pools

## General pattern

```bash
cargo run --bin ingest -- \
  --chain <chain> \
  --venue <venue> \
  --from-block <start> \
  --to-block <end> \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

## Ethereum UniV3 example

```bash
cargo run --bin ingest -- \
  --chain ethereum \
  --venue uniswap_v3 \
  --from-block 12369621 \
  --to-block 22000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

## Ethereum UniV2 example

```bash
cargo run --bin ingest -- \
  --chain ethereum \
  --venue uniswap_v2 \
  --from-block 10000835 \
  --to-block 22000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

## Base UniV3 example

```bash
cargo run --bin ingest -- \
  --chain base \
  --venue uniswap_v3 \
  --from-block 1 \
  --to-block 25000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

Adjust the ending block to a suitable current range.

---

# 5. How to confirm ingest worked

## Count rows

```bash
wc -l data/base/uniswap_v3/pools.jsonl
```

## Parse a few rows

```bash
python3 - <<'PY'
import json
p = "data/base/uniswap_v3/pools.jsonl"
with open(p, "r", encoding="utf-8") as f:
    for i, line in enumerate(f, 1):
        if i > 3:
            break
        print(json.loads(line))
PY
```

## Search a known pool field quickly

```bash
head -3 data/base/uniswap_v3/pools.jsonl
```

If the file is empty or obviously malformed, fix ingestion before tuning runtime.

---

# 6. What runtime actually loads

Runtime pool loading is based on:
- chain name
- venue name
- `pool_data_path(chain, venue)`

That usually resolves to:

- `data/<chain>/<venue>/pools.jsonl`

Examples:
- Base + uniswap_v3 -> `data/base/uniswap_v3/pools.jsonl`
- Ethereum + uniswap_v3 -> `data/ethereum/uniswap_v3/pools.jsonl`

Important:
- grep results often mix runtime code and test fixtures
- example files are not runtime
- test fixture paths are not proof that runtime is using the wrong chain

---

# 7. Why a smoke test can still fail even if pools.jsonl exists

A smoke test may:
- use direct token-pair probing
- use factory lookups
- use quoter calls

without directly reading `pools.jsonl`.

So it is possible for:
- `data/base/uniswap_v3/pools.jsonl` to be healthy
- while a smoke probe still says `no univ3 quote found`

That does not automatically mean the inventory file is ignored everywhere.
It only means **that specific probe path** may be using a different discovery path.

---

# 8. Venue onboarding: the core idea

Adding a venue is not just “add addresses”.

A bad venue integration can cause:
- fake quotes
- non-routable edges
- revert-heavy simulation
- wasted search budget
- missed profit

---

# 9. How to onboard a venue safely

## Step 1: confirm the venue family

Ask:
- is it UniV2-like?
- UniV3-like?
- Solidly-like?
- Balancer-like?
- Curve-like?
- something custom?

Do not force a venue into the wrong adapter family.

## Step 2: get the correct addresses

You normally need some combination of:
- factory
- router
- quoter
- pool manager
- vault
- init code hash

## Step 3: add the venue to `ops/inputs.yaml`

Example shape for UniV3-like:

```yaml
- name: uniswap_v3
  kind: univ3_like
  factory: "0x..."
  router: "0x..."
  quoter: "0x..."
  fee_tiers:
    - 100
    - 500
    - 3000
    - 10000
```

Example shape for UniV2-like:

```yaml
- name: uniswap_v2
  kind: univ2_like
  factory: "0x..."
  router: "0x..."
  fee_bps: 30
  pool_init_code_hash: "0x..."
```

## Step 4: ingest pools for the venue

If it is UniV2-like or UniV3-like, ingest into:

- `data/<chain>/<venue>/pools.jsonl`

## Step 5: run smoke tests

Watch for:
- no pool found
- quote errors
- zero quotes
- path encoding errors
- revert-heavy behavior

## Step 6: run shadow mode

Do not trust a new venue without shadowing it first.

---

# 10. Base-specific venue advice

For Base, weak opportunity surface often comes from narrow venue coverage.

A healthier Base venue plan is usually:

1. Uniswap v3
2. PancakeSwap v3
3. Aerodrome / Slipstream
4. UniV2-like secondary venues
5. Balancer if useful
6. Curve if useful

Do not expect strong Base opportunity capture from only:
- Uniswap v2
- Uniswap v3
- a tiny static token set

---

# 11. Why token universe matters

If your chain uses only a tiny handpicked token set, the graph will be sparse.

That can directly cause:
- poor edge population
- missing profitable routes

A healthier chain scanner usually wants:
- core routing assets
- more than 5–6 static names
- chain-native high-volume assets
- stable pairs
- long-tail only after core coverage is strong

---

# 12. Recommended universe approach

## Tier 1
Core routing assets:
- WETH
- USDC
- major stablecoins
- major wrapped BTC or other major liquid assets

## Tier 2
High-volume chain-native ecosystem tokens

## Tier 3
Long-tail speculative assets

Do not let Tier 3 dominate scan budget before Tier 1 and Tier 2 are healthy.

---

# 13. How to diagnose poor edge population

Ask these in order:

1. do pool inventory files exist?
2. do venue addresses load correctly?
3. do quotes succeed?
4. are edges built?
5. are edges pruned away?
6. is search budget too small?
7. is the token universe too narrow?
8. are the right venues enabled?

---

# 14. Very useful commands for venue and inventory debugging

## Search runtime pool loading code

```bash
rg -n "pool_data_path|load_pool_records" src tests
```

## Search venue names

```bash
rg -n "uniswap_v3|uniswap_v2|solidly|balancer|curve" src tests ops config
```

## Show where a test gets tokens from

```bash
nl -ba tests/integration_smoke.rs | sed -n '280,340p'
```

## Validate JSONL rows

```bash
python3 - <<'PY'
import json
p = "data/base/uniswap_v3/pools.jsonl"
ok = 0
with open(p, "r", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        json.loads(line)
        ok += 1
print("parsed", ok, "rows")
PY
```

---

# 15. Best practice when adding a new venue

Do these in order:

1. identify the correct adapter family
2. collect real addresses
3. update `ops/inputs.yaml`
4. ingest pools if applicable
5. confirm inventory file exists
6. run smoke test
7. run shadow mode
8. only then trust live discovery

---

# 16. When not to trust a venue yet

Do not trust a new venue if you see:
- repeated quote reverts
- no pool inventory
- weird decimals or token ordering issues
- path encoding failures
- zero or fake edges
- strong built-edge counts but zero scanned edges downstream
- no successful shadow opportunities after enough runtime

---

# 17. Final rule

The best venue onboarding is boring.

If onboarding feels fast, vague, or magical, it is probably wrong.

Good onboarding is:
- explicit
- verified
- ingested
- smoke tested
- shadow tested
- then promoted to live use
