# Arbot Troubleshooting Decision Tree

Use this when Arbot is broken and you need to decide what to check next.

This is the shortest path from **symptom** to **next action**.

Related docs:
- `arbot_master_runbook.md`
- `arbot_fork_and_integration_testing.md`
- `arbot_ingestion_and_venue_onboarding.md`
- `arbot_quick_reference_cheat_sheet.md`
- `arbot_base_tuning_sheet.md`
- `arbot_tunable_settings_reference.md`

---

# 1. Start here

Ask this first:

## Did the process crash immediately?

- **Yes** -> go to **Section 2**
- **No** -> go to **Section 3**

---

# 2. Crash / compile / parse failures

## Did `cargo build`, `cargo test`, or `cargo run` fail before runtime started?

- **Yes**
  - read the first real error
  - decide whether it is:
    - compile error
    - merge conflict marker
    - YAML / JSON parse error
    - missing env var

Use these commands:

```bash
cargo build --release
forge build
python3 - <<'PY'
import yaml
with open("ops/inputs.yaml","r",encoding="utf-8") as f:
    yaml.safe_load(f)
print("YAML OK")
PY
rg -n '<<<<<<<|=======|>>>>>>>' .
```

### If it says `missing required configuration`
Check:
- `ops/inputs.yaml`
- `config/registry.json`
- `.env`

Look for wrong key names like:
- `swaprouter02` vs `router`
- `aave_pool` vs `pool`
- `balancer_vault` vs `vault`

### If it says env values are missing
Check exported env vars:

```bash
env | sort | rg 'CHAIN|RPC|UNIV3|AAVE|BAL|PERMIT2|EXECUTOR'
```

If nothing appears, you probably forgot `export`.

---

# 3. Process is running but not working

Ask this next:

## Is it failing to connect to RPC?

- **Yes** -> go to **Section 4**
- **No** -> go to **Section 5**

---

# 4. RPC connection problems

## A. Error says `connection refused`

Meaning:
- nothing is listening at that host:port

Check:

```bash
ss -ltnp | rg ':8545|:8546'
cast block-number --rpc-url http://127.0.0.1:8545
cast block-number --rpc-url http://127.0.0.1:8546
```

### If you are on a local fork
Start Anvil:

```bash
anvil --host 0.0.0.0 --fork-url "$ETH_UPSTREAM_RPC_URL" --port 8545 --chain-id 1 --block-time 1
```

or:

```bash
anvil --host 0.0.0.0 --fork-url "$BASE_UPSTREAM_RPC_URL" --port 8546 --chain-id 8453 --block-time 1
```

Important:
- Anvil may **listen** on `0.0.0.0`
- your client should usually **connect** to `127.0.0.1`

## B. Error says `block out of range`

Meaning:
- the provider was asked for a block it cannot serve

Most common cause:
- block number came from upstream
- `eth_call` went to local fork

This is especially likely if it is only **1–2 blocks out**.

### What to do
1. stop the bot
2. restart the fork
3. point **all** chain RPC sources for that debug session at the same local provider
4. restart the bot

Example for Ethereum fork-debug mode:

```bash
export CHAIN=ethereum
export CHAIN_LIST=ethereum
export ETH_RPC_URL=http://127.0.0.1:8545
export ETH_RPC_URLS=http://127.0.0.1:8545
export ETH_PRIVATE_RPC_HTTP_URL=http://127.0.0.1:8545
export ETH_WS_RPC_URL=
export ETH_WS_RPC_URLS=
```

Then compare block heights:

```bash
cast block-number --rpc-url "$ETH_UPSTREAM_RPC_URL"
cast block-number --rpc-url http://127.0.0.1:8545
```

If upstream is ahead and your code mixes providers, that explains it.

---

# 5. Quotes are the problem

Ask this:

## Are all quotes failing?

- **No, some succeed** -> go to **Section 6**
- **Yes, all or almost all fail** -> go to **Section 7**

---

# 6. Some quotes succeed, but the system still finds nothing

This usually means the problem is **downstream of quoting**.

Look for logs like:
- `quote_success` is large
- `built_edges` is non-zero
- `edges scanned: 0`
- `search budget exceeded`

Meaning:
- venue quoting probably worked
- graph/search/pruning is probably the failing stage

### What to check

```bash
rg -n "edges scanned|search budget exceeded|reduced candidates|prune|health score|stale|quarantine" src
```

### Most likely causes
- search timeout too low
- edges pruned away after build
- stale/block-lag rejection
- health score filters too aggressive
- fork block mismatch poisoning downstream graph use

### Quick diagnostic move
Temporarily raise budgets:

```bash
export CYCLE_SEARCH_TIMEOUT_MS=3000
export QUOTE_BUDGET_MS=1500
export SIMULATION_BUDGET_MS=1500
```

If `edges scanned` becomes non-zero, the search stage is probably the issue.

---

# 7. All or nearly all quotes fail

Ask this:

## What is the exact error?

### `missing fee for hop 1`
Meaning:
- path encoding bug
- wrong helper used for single-hop quote
- generic multi-hop helper used where a direct single-hop helper should be used

Check:

```bash
rg -n "missing fee for hop" src tests
rg -n "quote_path\\(|quote_exact_input_single" src tests
```

### `no univ3 quote found`
Meaning:
- the probe exhausted candidates and found no successful quote

Possible reasons:
- no pool
- wrong fee tier
- quote returned zero
- token universe too weak
- path encoding problem
- smoke probe too narrow

Check:

```bash
nl -ba tests/integration_smoke.rs | sed -n '330,380p'
```

### `execution reverted: Unexpected error`
Meaning:
- the quote reached the venue, but the route reverted
- that can be a bad fee tier, bad pair, weird pool, long-tail junk, or invalid route

If some quotes still succeed, do **not** describe this as “all quotes fail”.

### Zero quote
Meaning:
- pool exists, quote returned zero
- can be invalid path, unsuitable pool, wrong amount, or pool-specific behavior

---

# 8. No edges or poor edge population

Ask this:

## Does pool inventory exist?

Check:

```bash
ls -l data/base/uniswap_v3/pools.jsonl
wc -l data/base/uniswap_v3/pools.jsonl
ls -l data/ethereum/uniswap_v3/pools.jsonl
wc -l data/ethereum/uniswap_v3/pools.jsonl
```

If missing or tiny, that is a coverage problem.

## If inventory is missing
Run ingest:

```bash
cargo run --bin ingest -- \
  --chain base \
  --venue uniswap_v3 \
  --from-block 1 \
  --to-block 25000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

## If inventory exists but coverage is still poor
Check:
- token universe too small
- venue set too narrow
- search caps too low
- too much long-tail, not enough core routing assets

For Base especially:
- Uniswap v3 alone is not enough
- weak venue coverage causes sparse edges

---

# 9. Shadow mode appears to hang

Ask:

## Did it crash?

- **Yes** -> go back to crash / parse / compile failures
- **No** -> likely blocked or stuck, not panicked

Look for:
- quote queue starvation
- semaphore wait timeout
- hot pool refresh blocked
- search budget exceeded
- dead/blocking task

Useful signs:
- CPU low + no logs = waiting/blocking
- CPU high + no progress = loop/heavy work

Check logs around:
- hot pool refresh
- quote attempts
- semaphore wait timeouts
- search budget exceeded
- no cycles found

---

# 10. Deploy script problems

## `MissingRequiredIntegration(...)`
Meaning:
- deploy script could not resolve a required address such as router, permit2, vault, or pool

Check:
- chain prefix
- env prefix
- exported vars

Example:

```bash
export CHAIN=base
export ENV_PREFIX=BASE
export BASE_UNIV3_ROUTER=0x...
export BASE_AAVE_POOL=0x...
export BASE_BAL_VAULT=0x...
export BASE_PERMIT2_ADDRESS=0x...
export BASE_EXECUTOR_OWNER=0x...
```

Then run:

```bash
CHAIN=base bash ./scripts/check_deploy_env.sh
```

## `OwnerNotConfigured()`
Meaning:
- deploy script did not get a valid owner address

Check:
- `EXECUTOR_OWNER`
- `BASE_EXECUTOR_OWNER`
- not empty
- not zero address

---

# 11. Quick “what should I do next?” table

## Symptom -> next action

### `connection refused`
-> check fork process and RPC port

### `block out of range`
-> stop mixing upstream block numbers with local fork calls

### `missing fee for hop 1`
-> inspect path encoder / single-hop quote helper

### `no univ3 quote found`
-> inspect token universe, fee tiers, and smoke probe logic

### `quote_success high but edges scanned 0`
-> inspect search/pruning stage

### `placeholder endpoint lint failed`
-> remove `127.0.0.1`, `localhost`, `${ALCHEMY_KEY}` from committed config

### deploy env check says missing values
-> export env vars properly

### shadow mode hangs after hot-pool stage
-> inspect semaphore starvation / blocked search path

---

# 12. Final emergency checklist

When confused, do these in order:

```bash
cargo build --release
forge build
bash ./scripts/ci/check_placeholder_endpoints.sh
bash ./scripts/ci/no_runtime_panics.sh
ss -ltnp | rg ':8545|:8546'
cast block-number --rpc-url http://127.0.0.1:8545
cast block-number --rpc-url http://127.0.0.1:8546
python3 - <<'PY'
import yaml
with open("ops/inputs.yaml","r",encoding="utf-8") as f:
    yaml.safe_load(f)
print("YAML OK")
PY
```

Then ask:
1. config loaded?
2. fork reachable?
3. pool inventory present?
4. quotes succeeding?
5. edges built?
6. edges scanned?
7. simulation succeeding?
8. broadcast working?

That is the shortest safe path to the real cause.
