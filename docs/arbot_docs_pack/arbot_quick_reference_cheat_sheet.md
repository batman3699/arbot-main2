# Arbot Quick-Reference Cheat Sheet

This is the short version.

Use it when you do **not** want to read the full manuals.

Related full docs:
- `arbot_master_runbook.md`
- `arbot_fork_and_integration_testing.md`
- `arbot_ingestion_and_venue_onboarding.md`
- `arbot_troubleshooting_decision_tree.md`
- `arbot_base_tuning_sheet.md`
- `arbot_tunable_settings_reference.md`

---

# 1. The 3 things Arbot needs to work

## A. Correct config
Main files:
- `ops/inputs.yaml`
- `config/registry.json`
- `.env`

## B. Pool inventory
Important files:
- `data/<chain>/<venue>/pools.jsonl`

Examples:
- `data/ethereum/uniswap_v3/pools.jsonl`
- `data/base/uniswap_v3/pools.jsonl`

## C. Working RPC
Without working RPC:
- quotes fail
- fork tests fail
- simulations fail

---

# 2. The 3 address meanings you must never confuse

- **signer address** = wallet from `PRIVATE_KEY`
- **executor owner** = owner/admin wallet address
- **executor address** = clone address

Memory rule:

```text
Wallet signs -> wallet owns -> clone executes
```

---

# 3. Fastest health checks

Run from repo root:

```bash
bash ./scripts/ci/check_placeholder_endpoints.sh
bash ./scripts/ci/no_runtime_panics.sh
```

Check deploy env:

```bash
CHAIN=base bash ./scripts/check_deploy_env.sh
CHAIN=ethereum bash ./scripts/check_deploy_env.sh
```

Check builds:

```bash
cargo build --release
forge build
```

---

# 4. Most useful commands

## Check if fork is listening

```bash
ss -ltnp | rg ':8545|:8546'
```

## Check latest block from local fork

```bash
cast block-number --rpc-url http://127.0.0.1:8545
cast block-number --rpc-url http://127.0.0.1:8546
```

## Check chain ID

```bash
curl -s -X POST http://127.0.0.1:8545 \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
```

## Search repo for an error

```bash
rg -n "missing fee for hop" src tests
rg -n "block out of range" src tests
rg -n "ARBOT_FORK_RPC_URL" src tests .
rg -n "private_method_policy" .
```

## Show source lines with numbers

```bash
nl -ba tests/integration_smoke.rs | sed -n '330,380p'
```

## Parse YAML quickly

```bash
python3 - <<'PY'
import yaml
with open("ops/inputs.yaml","r",encoding="utf-8") as f:
    yaml.safe_load(f)
print("YAML OK")
PY
```

---

# 5. Environment variable rule that trips people up

This:

```bash
FOO=bar
```

does **not** export for later commands.

This does:

```bash
export FOO=bar
```

This passes only to one command:

```bash
FOO=bar some_command
```

---

# 6. Fork rules

## Rule 1
Anvil can **listen** on `0.0.0.0`.

Your client should usually **connect** to `127.0.0.1`.

## Rule 2
Do not mix:
- upstream block number
- local fork `eth_call`

That causes:

```text
block out of range
```

## Rule 3
If you restart the fork, restart the bot too.

---

# 7. Start local forks

## Ethereum fork

```bash
anvil --host 0.0.0.0 \
  --fork-url "$ETH_UPSTREAM_RPC_URL" \
  --port 8545 \
  --chain-id 1 \
  --block-time 1
```

## Base fork

```bash
anvil --host 0.0.0.0 \
  --fork-url "$BASE_UPSTREAM_RPC_URL" \
  --port 8546 \
  --chain-id 8453 \
  --block-time 1
```

---

# 8. Run integration smoke tests

## Ethereum

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=ethereum \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://127.0.0.1:8545" \
CHAIN=ethereum \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

## Base

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=base \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://127.0.0.1:8546" \
CHAIN=base \
BASE_PRIVATE_RPC_HTTP_URL="$BASE_UPSTREAM_RPC_URL" \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

Success looks like:

```text
test integration_smoke_per_chain ... ok
```

---

# 9. Start shadow mode

## Base

```bash
SHADOW_MODE=true \
SHADOW_LOG_PATH=logs/shadow.base.jsonl \
SHADOW_TAG=base-shadow \
CHAIN=base \
cargo run --release
```

## Watch shadow log

```bash
tail -f logs/shadow.base.jsonl
```

---

# 10. Ingesting cheat sheet

## What ingest does
Builds pool inventory files Arbot needs for UniV2/UniV3-style venues.

## Example ingest command

```bash
cargo run --bin ingest -- \
  --chain base \
  --venue uniswap_v3 \
  --from-block 1 \
  --to-block 25000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

## Check inventory exists

```bash
wc -l data/base/uniswap_v3/pools.jsonl
head -3 data/base/uniswap_v3/pools.jsonl
tail -3 data/base/uniswap_v3/pools.jsonl
```

---

# 11. Symptom -> likely meaning

## `missing fee for hop 1`
Usually a path encoding bug or wrong quote helper.

## `block out of range`
Usually upstream block numbers mixed with local fork calls.

## `connection refused`
Nothing is listening on the RPC port.

## `no univ3 quote found`
Probe exhausted candidates with no successful quote.

## `built_edges > 0 but edges scanned = 0`
Venue quoting probably worked, but downstream search/pruning failed.

---

# 12. Base tuning in one glance

For Base-only tuning:

```bash
export CHAIN=base
export CHAIN_LIST=base
```

Then check:
1. pool inventory exists
2. token universe is not tiny
3. venues are not too narrow
4. search budget is not too low
5. shadow mode is on

---

# 13. If something breaks, ask these 7 questions

1. did config load?
2. did pool inventory load?
3. did quote calls succeed?
4. were edges built?
5. were edges scanned?
6. did simulation succeed?
7. did broadcast happen?

If you cannot answer which step failed, stop and diagnose before changing settings.

---

# 14. Which full doc to read next

Read this when you need:
- full setup / deployment / runtime -> `arbot_master_runbook.md`
- forks / dry runs / smoke tests -> `arbot_fork_and_integration_testing.md`
- ingesting / venue onboarding / pool coverage -> `arbot_ingestion_and_venue_onboarding.md`
- symptom-based debugging -> `arbot_troubleshooting_decision_tree.md`
- Base-only tuning -> `arbot_base_tuning_sheet.md`
- setting explanations -> `arbot_tunable_settings_reference.md`
