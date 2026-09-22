# Arbot Master Runbook

This is the main operating manual for Arbot.

It is written for someone with very little technical experience.
It explains the system in plain language and gives the steps in the order you should follow them.

---

# 1. What Arbot is

Arbot is a trading system that:

1. reads live blockchain market data
2. builds a graph of tradable routes
3. searches for profitable cycles
4. simulates those cycles
5. optionally executes them through a deployed contract

Simple meanings:

- **scanner** = finds possible trades
- **quoter** = asks what a swap would return
- **simulator** = checks whether a trade still works when executed
- **executor** = the deployed contract that performs the trade
- **fork** = a local copy of a live chain used for safe testing
- **shadow mode** = full runtime without sending live trades
- **ingest** = building pool inventory files such as `pools.jsonl`

---

# 2. The 3 configuration layers

Arbot normally uses 3 configuration layers.

## A. `ops/inputs.yaml`

Use this for:

- chain definitions
- venues
- flashloan providers
- non-secret runtime settings
- scan and risk controls

## B. `config/registry.json`

Use this for:

- shared chain metadata
- static route metadata
- non-secret registry information

## C. `.env`

Use this for:

- secrets
- private keys
- private relay URLs
- local RPC overrides
- local fork overrides
- deployment-only values

## Golden rule

- non-secret runtime behavior -> `ops/inputs.yaml`
- shared static metadata -> `config/registry.json`
- secrets and local/private overrides -> `.env`

Do not spread the same setting across all 3 unless you fully understand which layer wins.

---

# 3. Files and folders you must know

## Important files

- `ops/inputs.yaml`
- `config/registry.json`
- `.env`
- `script/Deploy.s.sol`

## Important pool inventory folder

- `data/<chain>/<venue>/pools.jsonl`

Examples:

- `data/ethereum/uniswap_v3/pools.jsonl`
- `data/base/uniswap_v3/pools.jsonl`
- `data/ethereum/uniswap_v2/pools.jsonl`

If these files are missing or weak, the graph can be weak.

Simple chain of failure:

- no or weak inventory
- weak route surface
- weak edges
- poor or zero cycles found

---

# 4. Before doing anything else

Open a terminal in the repo root:

```bash
cd ~/arbot-main2/arbot-main-main
```

Now run these checks.

## Check tool versions

```bash
cargo --version
forge --version
cast --version
anvil --version
```

If one fails, fix that first.

## Check that the repo builds

```bash
cargo build --release
forge build
```

If build fails, stop and fix that before moving into runtime testing.

---

# 5. Fast health checks

Run these from repo root.

## Placeholder endpoint check

```bash
bash ./scripts/ci/check_placeholder_endpoints.sh
```

This catches bad committed endpoints like:

- `127.0.0.1`
- `localhost`
- `${ALCHEMY_KEY}`

Important:

- local fork URLs are okay in your local env
- they are not okay in committed shared config files

## Runtime panic lint

```bash
bash ./scripts/ci/no_runtime_panics.sh
```

This helps catch risky runtime patterns.

## Deploy environment check

For Base:

```bash
CHAIN=base bash ./scripts/check_deploy_env.sh
```

For Ethereum:

```bash
CHAIN=ethereum bash ./scripts/check_deploy_env.sh
```

If it says required values are missing, the most common reason is that the values were not exported into the shell.

---

# 6. How environment variables actually work

This is one of the easiest things to get wrong.

This:

```bash
FOO=bar
```

creates a shell variable only.

This:

```bash
export FOO=bar
```

creates an environment variable that child processes can see.

This:

```bash
FOO=bar some_command
```

passes `FOO` only to `some_command`.

## Common mistake

Typing a long list like this:

```bash
CHAIN=base ENV_PREFIX=BASE UNIV3_ROUTER=0x...
```

and pressing Enter **without any command after it** does not export those values for later commands.

## Correct way

Either export values first:

```bash
export CHAIN=base
export ENV_PREFIX=BASE
export BASE_UNIV3_ROUTER=0x2626664c2603336E57B271c5C0b26F421741e481
export BASE_AAVE_POOL=0xA238Dd80C259a72e81d7e4664a9801593F98d1c5
export BASE_BAL_VAULT=0xBA12222222228d8Ba445958a75a0704d566BF2C8
export BASE_PERMIT2_ADDRESS=0x000000000022D473030F116dDEE9F6B43aC78BA3
export BASE_EXECUTOR_OWNER=0xYOUR_OWNER_WALLET
export PRIVATE_KEY=0xYOUR_PRIVATE_KEY
```

Or pass them on the same command line as the command you are running.

---

# 7. Address meanings you must never confuse

## Definitions

- **signer address** = the wallet from `PRIVATE_KEY`
- **executor owner** = the owner/admin wallet address
- **executor address** = the clone address
- **batch router** = a separate router contract address

## Memory rule

**Wallet signs -> wallet owns -> clone executes**

Do not confuse:

- clone address
- batch router address
- signer wallet
- owner wallet

They are not the same thing.

---

# 8. Deployment order

## Step 1: fill chain-specific env values

Example for Base:

```bash
export CHAIN=base
export ENV_PREFIX=BASE
export BASE_UNIV3_ROUTER=0x2626664c2603336E57B271c5C0b26F421741e481
export BASE_AAVE_POOL=0xA238Dd80C259a72e81d7e4664a9801593F98d1c5
export BASE_BAL_VAULT=0xBA12222222228d8Ba445958a75a0704d566BF2C8
export BASE_PERMIT2_ADDRESS=0x000000000022D473030F116dDEE9F6B43aC78BA3
export BASE_EXECUTOR_OWNER=0xYOUR_OWNER_WALLET
export PRIVATE_KEY=0xYOUR_PRIVATE_KEY
```

## Step 2: validate env

```bash
CHAIN=base bash ./scripts/check_deploy_env.sh
```

## Step 3: dry-run deploy script

```bash
forge script script/Deploy.s.sol:Deploy \
  --rpc-url https://your-rpc-url \
  --slow -vvvv
```

## Step 4: real deploy

```bash
forge script script/Deploy.s.sol:Deploy \
  --rpc-url https://your-rpc-url \
  --broadcast \
  --slow -vvvv
```

## Step 5: save the returned addresses

Record:

- implementation
- factory
- clone
- router

The important runtime ones are:

- executor address = clone
- executor owner = admin wallet
- signer = wallet from private key

---

# 9. Fork testing: why it matters

A fork is a safe local copy of a live chain.

Use forks before trusting live changes.

Fork testing helps prove:

- RPC wiring is correct
- quote calls work
- inventory is usable
- simulation matches chain state
- obvious bugs like `block out of range` or quote path bugs are gone

---

# 10. Local fork rules

## Rule 1

Anvil may **listen** on `0.0.0.0`, but your client should usually **connect** to `127.0.0.1`.

Good:

```bash
anvil --host 0.0.0.0 --port 8546 ...
```

and then:

```bash
ARBOT_FORK_RPC_URL=http://127.0.0.1:8546
```

Do not treat `0.0.0.0` like a normal client endpoint.

## Rule 2

Do not mix:

- upstream block numbers
- local fork `eth_call` or simulation

That is a common cause of:

- `block out of range`

## Rule 3

If you restart the fork, restart the bot too.

---

# 11. Integration smoke testing

## Ethereum smoke test

### Terminal A: start Ethereum fork

```bash
anvil --host 0.0.0.0 \
  --fork-url "$ETH_UPSTREAM_RPC_URL" \
  --port 8545 \
  --chain-id 1 \
  --block-time 1
```

### Terminal B: run Ethereum smoke test

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=ethereum \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://127.0.0.1:8545" \
CHAIN=ethereum \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

## Base smoke test

### Terminal A: start Base fork

```bash
anvil --host 0.0.0.0 \
  --fork-url "$BASE_UPSTREAM_RPC_URL" \
  --port 8546 \
  --chain-id 8453 \
  --block-time 1
```

### Terminal B: run Base smoke test

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=base \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://127.0.0.1:8546" \
CHAIN=base \
BASE_PRIVATE_RPC_HTTP_URL="$BASE_UPSTREAM_RPC_URL" \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

## What success looks like

```text
test integration_smoke_per_chain ... ok
```

## What failure looks like

Examples:

- `no univ3 quote found`
- `missing fee for hop 1`
- `block out of range`
- `connection refused`

Each one means something different.
Do not treat them as one generic “fork failed” problem.

---

# 12. Useful commands worth keeping

## Check whether fork is listening

```bash
ss -ltnp | rg ':8545|:8546'
```

## Ask a provider for latest block

```bash
cast block-number --rpc-url http://127.0.0.1:8545
cast block-number --rpc-url http://127.0.0.1:8546
```

## Ask chain identity directly

```bash
curl -s -X POST http://127.0.0.1:8545 \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
```

## Search repo for an error string

```bash
rg -n "missing fee for hop" src tests
rg -n "block out of range" src tests
rg -n "private_method_policy" .
rg -n "ARBOT_FORK_RPC_URL" src tests .
```

## Show file lines with numbers

```bash
nl -ba tests/integration_smoke.rs | sed -n '330,380p'
```

## Find merge markers or tabs in YAML

```bash
grep -nP '\t' ops/inputs.yaml
rg -n '<<<<<<<|=======|>>>>>>>' ops/inputs.yaml
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

## Check exported values really exist

```bash
env | grep -E '^(CHAIN|ENV_PREFIX|BASE_UNIV3_ROUTER|BASE_AAVE_POOL|BASE_BAL_VAULT|BASE_PERMIT2_ADDRESS|BASE_EXECUTOR_OWNER)='
```

---

# 13. Ingesting: what it is and why it matters

Ingesting means building the pool inventory files Arbot uses for UniV2/UniV3 style venues.

Without ingesting, you can get:

- poor edge population
- missing pool coverage
- weak route surface
- poor cycle discovery

## Where ingest writes data

- `data/<chain>/<venue>/pools.jsonl`

## Example ingest command

```bash
cargo run --bin ingest -- \
  --chain ethereum \
  --venue uniswap_v3 \
  --from-block 12369621 \
  --to-block 22000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

Another example:

```bash
cargo run --bin ingest -- \
  --chain base \
  --venue uniswap_v3 \
  --from-block 1 \
  --to-block 25000000 \
  --chunk-size 5000 \
  --query-timeout-secs 30
```

## Confirm ingest worked

```bash
wc -l data/base/uniswap_v3/pools.jsonl
head -3 data/base/uniswap_v3/pools.jsonl
tail -3 data/base/uniswap_v3/pools.jsonl
```

If the file is missing or empty, venue coverage will usually be poor.

---

# 14. Why “no edges scanned” can happen

You can have:

- pool inventory present
- quote successes present
- built edges present

and still end with:

- `edges scanned: 0`

That usually means the failure is **downstream of quoting**, not necessarily in the venue layer.

Common causes:

1. search budget exceeded
2. pruning removed everything
3. stale/block-lag checks rejected edges
4. health-score filtering rejected edges
5. fork block mismatch poisoned downstream checks

If you see:

- `quote_success` large
- `built_edges` non-zero
- `edges scanned: 0`

then the likely issue is search/pruning, not basic quote connectivity.

---

# 15. Shadow mode: what it is for

Shadow mode runs the full trading logic without sending live trades.

Use it when:

- tuning a chain
- testing a venue
- changing risk controls
- expanding the token universe
- checking runtime behavior safely

## Start shadow mode

```bash
SHADOW_MODE=true \
SHADOW_LOG_PATH=logs/shadow.base.jsonl \
SHADOW_TAG=base-shadow \
CHAIN=base \
cargo run --release
```

## Watch the output

```bash
tail -f logs/shadow.base.jsonl
```

If shadow mode seems to hang, inspect logs around:

- hot pool refresh
- quote queue starvation
- search budget exceeded
- no cycles found

---

# 16. How to tune scan quality and edge population

If a chain is weak, do not only change one timer.
Work in this order:

1. confirm the correct chain is selected
2. confirm pool inventory exists
3. confirm the token universe is not tiny
4. add missing high-flow venues
5. widen scan caps moderately
6. run smoke test
7. run shadow mode
8. only then trust live quality

---

# 17. Troubleshooting by symptom

## `missing fee for hop 1`

Usually means:

- path encoding bug
- wrong helper used for a single-hop quote
- generic multi-hop helper used where a direct single-hop quote helper should be used

## `block out of range`

Usually means:

- provider was asked for a block it cannot serve
- most often because upstream block numbers got mixed with local fork calls

## `connection refused`

Means:

- nothing is listening on the target RPC port

## `no univ3 quote found`

Means:

- the probe exhausted candidates and found no successful quote

## `built_edges > 0` but `edges scanned = 0`

Means:

- venue quoting probably worked
- search/pruning is likely the real issue

---

# 18. Minimal daily operator checklist

1. open repo root
2. confirm fork/live RPCs are correct
3. run placeholder and panic checks
4. confirm chain selection
5. confirm pool inventory files exist
6. run smoke test if something important changed
7. run shadow mode before live use on changed setups
8. only then consider live runtime

---

# 19. Final rule

When something breaks, ask these in order:

1. did config load?
2. did pool inventory load?
3. did quote calls succeed?
4. were edges built?
5. were edges scanned?
6. did simulation succeed?
7. did broadcast happen?

That order prevents wrong conclusions and wasted tuning.
