# Arbot Fork, Dry Run, and Integration Testing Guide

This guide is only for safe testing.

Use it when you want to:
- test Arbot on a local fork
- run smoke tests
- dry-run changes before trusting live use
- debug common fork errors such as `block out of range`

---

# 1. What a fork is

A fork is a local copy of a real blockchain.

Examples:
- Ethereum fork
- Base fork

A fork lets you:
- test quotes
- test simulations
- inspect local chain state
- run smoke tests
- debug safely

without risking real funds.

---

# 2. The 2 fork rules you must remember

## Rule A
Anvil may **listen** on `0.0.0.0`.

Your client should usually **connect** to `127.0.0.1`.

Good example:

```bash
anvil --host 0.0.0.0 --port 8545 ...
```

Then connect with:

```bash
http://127.0.0.1:8545
```

## Rule B
Do not mix:
- upstream block numbers
- local fork `eth_call` or simulation

That is the classic cause of:
- `block out of range`

If the error is usually only 1–2 blocks out, provider mixing is the first thing to suspect.

---

# 3. Required tools

Check these first:

```bash
anvil --version
cast --version
cargo --version
forge --version
```

If one fails, stop and fix that first.

---

# 4. Required upstream RPC variables

Set your upstream URLs first.

## Ethereum

```bash
export ALCHEMY_KEY="YOUR_ALCHEMY_KEY"
export ETH_UPSTREAM_RPC_URL="https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"
```

## Base

```bash
export BASE_UPSTREAM_RPC_URL="https://base-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"
```

Check them with:

```bash
echo "$ETH_UPSTREAM_RPC_URL"
echo "$BASE_UPSTREAM_RPC_URL"
```

---

# 5. Start a fork: Ethereum

## Terminal A

```bash
anvil --host 0.0.0.0 \
  --fork-url "$ETH_UPSTREAM_RPC_URL" \
  --port 8545 \
  --chain-id 1 \
  --block-time 1
```

Expected result:
- Anvil stays running
- it prints a listening line
- it does not exit with an error

## Check the fork is reachable

In another terminal:

```bash
cast block-number --rpc-url http://127.0.0.1:8545
```

and:

```bash
curl -s -X POST http://127.0.0.1:8545 \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
```

---

# 6. Start a fork: Base

## Terminal A

```bash
anvil --host 0.0.0.0 \
  --fork-url "$BASE_UPSTREAM_RPC_URL" \
  --port 8546 \
  --chain-id 8453 \
  --block-time 1
```

## Check the fork is reachable

```bash
cast block-number --rpc-url http://127.0.0.1:8546
```

and:

```bash
curl -s -X POST http://127.0.0.1:8546 \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
```

---

# 7. Check whether the fork process is listening

```bash
ss -ltnp | rg ':8545|:8546'
```

If nothing appears, the fork is not listening.

---

# 8. Integration smoke tests

These are your first real end-to-end checks.

## Ethereum integration smoke

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=ethereum \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://127.0.0.1:8545" \
CHAIN=ethereum \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

## Base integration smoke

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
- `missing fee for hop 1`
- `no univ3 quote found`
- `block out of range`
- `connection refused`

Each one means something different.
Do not treat all of them as the same error.

---

# 9. Fork dry run

A dry run means using the fork to validate the runtime path safely.

Use this when you changed:
- config
- venues
- token lists
- quote path logic
- risk controls
- search logic
- pool inventory

## Example dry-run shadow session: Ethereum

```bash
SHADOW_MODE=true \
SHADOW_LOG_PATH=logs/shadow.ethereum.fork.jsonl \
SHADOW_TAG=ethereum-fork \
CHAIN=ethereum \
cargo run --release
```

## Example dry-run shadow session: Base

```bash
SHADOW_MODE=true \
SHADOW_LOG_PATH=logs/shadow.base.fork.jsonl \
SHADOW_TAG=base-fork \
CHAIN=base \
cargo run --release
```

## Watch logs

```bash
tail -f logs/shadow.ethereum.fork.jsonl
```

or:

```bash
tail -f logs/shadow.base.fork.jsonl
```

---

# 10. Why fork runs sometimes seem to hang

When a run seems stuck, it is usually one of these:
1. quote queue starvation
2. hot-pool refresh taking too long
3. search budget exceeded repeatedly
4. dead/blocking task after graph build
5. waiting on RPC/provider
6. mixed upstream/local block tags

A hang is not automatically a panic.
A panic usually crashes fast.
A hang usually means the process is still alive but blocked or waiting.

---

# 11. Common fork failure: block out of range

## Meaning
The code asked a provider for a block the provider cannot serve.

## Most common cause
- latest block number came from upstream
- quote or simulation call went to local fork

If the error is usually 1–2 blocks out, that strongly suggests provider mixing.

## What to do

### Step 1
Stop the runtime.

### Step 2
Restart the fork.

### Step 3
Point all chain RPC sources at the same local provider for that debug session.

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

### Step 4
Restart the bot.

## Compare local fork vs upstream

```bash
cast block-number --rpc-url "$ETH_UPSTREAM_RPC_URL"
cast block-number --rpc-url http://127.0.0.1:8545
```

If upstream is ahead and your code mixes providers, that explains the error.

## If it still happens
Search code for block-tagged calls:

```bash
rg -n "get_block_number|BlockId|BlockNumber|at_block|eth_call|simulate|quote_path" src tests
```

You are looking for any place where:
- one provider gets the block number
- another provider performs the actual call

---

# 12. Common fork failure: missing fee for hop 1

## Meaning
A path encoder or validator rejected the quote path before a real quote happened.

This usually means:
- wrong path shape
- wrong helper used for a single-hop quote
- off-by-one in fee encoding

## What to do

Search:

```bash
rg -n "missing fee for hop" src tests
rg -n "quote_path\\(|quote_exact_input_single" src tests
```

If it is a single-hop UniV3 probe, use a direct single-hop quote helper if one exists.

---

# 13. Common fork failure: no univ3 quote found

## Meaning
The smoke probe exhausted candidates and found no successful quote.

Possible reasons:
- no matching pool
- wrong fee tier
- quote returned zero
- token universe is weak
- path encoding is wrong
- smoke probe is too narrow

Helpful command:

```bash
nl -ba tests/integration_smoke.rs | sed -n '330,380p'
```

---

# 14. Common fork failure: connection refused

## Meaning
Nothing is listening at that host:port.

## What to do

Check:

```bash
ss -ltnp | rg ':8545|:8546'
```

If empty, restart Anvil.

---

# 15. Very useful diagnosis commands

## Show active endpoint values

```bash
echo "$ETH_UPSTREAM_RPC_URL"
echo "$BASE_UPSTREAM_RPC_URL"
echo "$ARBOT_FORK_RPC_URL"
```

## Check local fork latest block

```bash
cast block-number --rpc-url http://127.0.0.1:8545
cast block-number --rpc-url http://127.0.0.1:8546
```

## Compare local fork to upstream

```bash
cast block-number --rpc-url "$ETH_UPSTREAM_RPC_URL"
cast block-number --rpc-url http://127.0.0.1:8545
```

## Search repo for fork env usage

```bash
rg -n "ARBOT_FORK_RPC_URL" src tests .
```

## Show exact source lines

```bash
nl -ba tests/integration_smoke.rs | sed -n '150,240p'
nl -ba tests/integration_smoke.rs | sed -n '330,380p'
```

---

# 16. Acceptance criteria for a good fork test

A fork test is good only if:
1. local fork is reachable
2. chain identity is correct
3. smoke test passes
4. shadow mode runs without obvious blocking or panic
5. no unexplained block-range drift exists
6. quote path failures are understood, not ignored

---

# 17. When to stop immediately

Stop and fix the setup before continuing if you see:
- repeated `block out of range`
- repeated `connection refused`
- upstream/local provider mixing
- missing pool inventory
- `search budget exceeded` with `edges scanned: 0`
- repeated unhandled reverts with no diagnosis

---

# 18. Final fork-testing rule

Always isolate the failing stage:

1. fork reachable?
2. correct chain?
3. config loads?
4. inventory loads?
5. quote works?
6. edge build works?
7. search works?
8. simulation works?

If you do not know which stage is broken, you are not ready to trust the result.
