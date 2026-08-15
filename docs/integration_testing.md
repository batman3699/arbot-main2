# Integration Testing Guide (Fork, Shadow Mode, and Chain Smoke)

This guide explains how to run full integration testing for Arbot in a way that matches real trading conditions.

The writing below is intentionally simple and step by step. It is designed so a new team member can follow it safely.

## Scope

This guide covers:

- local fork execution checks,
- shadow mode runtime validation,
- per chain smoke tests for Ethereum and Base,
- deployment readiness gates.

---

## Why this matters for profit

A unit test can prove one function works.
An integration test proves the full money path works.

For this trading system, integration testing checks that:

- graph discovery reads real chain state,
- simulation assumptions match execution assumptions,
- risk controls block bad trades,
- network and node settings do not silently reduce inclusion.

If these checks fail, expected value on paper will not become real profit in production.

---

## 1) Prerequisites

Run these commands first:

```bash
cd /workspace/arbot-main
anvil --version
cast --version
cargo --version
forge --version
```

Set required environment values:

```bash
export ALCHEMY_KEY="YOUR_ALCHEMY_KEY"
export ETH_UPSTREAM_RPC_URL="https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"
export BASE_UPSTREAM_RPC_URL="https://base-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"
```

### Where your fork remote procedure call Uniform Resource Locator comes from

A fork remote procedure call Uniform Resource Locator is the upstream chain endpoint that Anvil uses to copy real chain state.

Use a paid reliable endpoint provider key for this value. In this repository, the expected source is Alchemy.

Examples:

- Ethereum mainnet fork source: `https://eth-mainnet.g.alchemy.com/v2/<your-key>`
- Base mainnet fork source: `https://base-mainnet.g.alchemy.com/v2/<your-key>`

Why this should be the value:

- The fork must read current state from the real chain.
- A stable provider reduces failed calls and stale state.
- Reproducible testing requires one clearly defined upstream source.

---

## 2) Static preflight checks (must pass first)

Run:

```bash
./scripts/ci/check_placeholder_endpoints.sh
./scripts/ci/no_runtime_panics.sh
CHAIN=ethereum ./scripts/check_deploy_env.sh
CHAIN=base ./scripts/check_deploy_env.sh
```

Expected result: zero failures. If any command fails, stop and fix configuration or code before continuing.

---

## 3) Baseline test suites

Run:

```bash
cargo test
forge test
```

These are the fast safety checks before more expensive fork testing.

---

## 4) Ethereum fork integration smoke test

### Terminal A: start local fork server

```bash
anvil --host 0.0.0.0 --fork-url "$ETH_UPSTREAM_RPC_URL" --port 8545 --chain-id 1 --block-time 1
```

### Terminal B: run integration smoke test

If your environment does not allow local loopback, replace `<CONTAINER_IP>` with the real machine or container internet protocol address.

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=ethereum \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://<CONTAINER_IP>:8545" \
CHAIN=ethereum \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

Expected result:

- test process connects to fork,
- chain coverage checks pass,
- no panic or unresolved chain configuration errors.

---

## 5) Base fork integration smoke test

### Terminal A: start local fork server

```bash
anvil --host 0.0.0.0 --fork-url "$BASE_UPSTREAM_RPC_URL" --port 8546 --chain-id 8453 --block-time 1
```

### Terminal B: run integration smoke test

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=base \
ARBOT_REQUIRE_CHAIN_COVERAGE=1 \
ARBOT_FORK_RPC_URL="http://<CONTAINER_IP>:8546" \
CHAIN=base \
BASE_PRIVATE_RPC_HTTP_URL="$BASE_UPSTREAM_RPC_URL" \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

Expected result:

- Base specific coverage checks pass,
- no missing venue, token, or registry edge errors.

---

## 6) Most useful fork remote procedure call Uniform Resource Locator debugging commands

Use these commands when a fork test fails.

1) Show the value you are actually using:

```bash
echo "$ETH_UPSTREAM_RPC_URL"
echo "$BASE_UPSTREAM_RPC_URL"
echo "$ARBOT_FORK_RPC_URL"
```

2) Ask the endpoint for chain identity directly:

```bash
curl -s -X POST "$ETH_UPSTREAM_RPC_URL" \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
```

3) Read latest block height through Cast:

```bash
cast block-number --rpc-url "$ETH_UPSTREAM_RPC_URL"
cast block-number --rpc-url "$BASE_UPSTREAM_RPC_URL"
```

4) Confirm your local fork server is listening on the expected port:

```bash
ss -ltnp | rg ':8545|:8546'
```

5) Start a clean local fork with clear settings:

```bash
anvil --host 0.0.0.0 --fork-url "$BASE_UPSTREAM_RPC_URL" --port 8546 --chain-id 8453 --block-time 1
```

What these commands tell you:

- whether your environment variable is empty,
- whether the upstream endpoint is alive,
- whether the endpoint is on the expected chain,
- whether the local fork process is reachable,
- whether your test points to the right local endpoint.

---

## 7) Optional executor simulation mode

Use this only when executor owner and deployed addresses are correct for the fork state.

```bash
ARBOT_SMOKE_EXECUTOR=1 \
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=ethereum \
ARBOT_FORK_RPC_URL="http://<CONTAINER_IP>:8545" \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

---

## 8) Fork test for cycle reconstruction and profitable sizing

This test validates three critical points on a live fork remote procedure call endpoint:

- negative cycle detection is consistent,
- predecessor based path reconstruction produces expected token sequence,
- sizing chooses a non zero input with positive net output.

Run:

```bash
ARBOT_FORK_RPC_URL="http://<CONTAINER_IP>:8545" \
cargo test fork_negative_cycle_reconstruction_and_sizing_are_profitable -- --nocapture
```

Notes:

- If `ARBOT_FORK_RPC_URL` is not set, this test exits early safely.
- Keep this test in your release gate after graph or sizing changes.

---

## 9) Shadow mode runtime validation

Run the bot against fork state in shadow mode:

```bash
SHADOW_MODE=1 \
SHADOW_LOG_PATH=logs/shadow-mainnet.jsonl \
SHADOW_TAG=integration-check \
CHAIN=ethereum \
PRIVATE_KEY="$PRIVATE_KEY" \
SIGNER_ADDRESS="$SIGNER_ADDRESS" \
cargo run --release
```

Inspect output:

```bash
tail -f logs/shadow-mainnet.jsonl
jq 'select(.net_profit_wei!=null) | {ts:.timestamp_ms, net:.net_profit_wei, gas:.gas_cost_wei, hops:.hops}' logs/shadow-mainnet.jsonl | head -n 10
```

Expected result:

- stable runtime,
- opportunity evaluations include cost and profit fields,
- fail closed behavior when required inputs are stale or missing.

---

## 10) Common failures and what they mean

| Failure                          | Meaning in simple words                                                   | Action                                                  |
| -------------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------- |
| chain coverage assertion failure | required chain data is missing in configuration or registry               | update registry and inputs, then rerun                  |
| placeholder endpoint detection   | environment still has unsafe placeholder values                           | replace placeholders in environment or operations files |
| panic check failure              | runtime crash path still exists                                           | remove unwrap or expect in runtime path                 |
| fork connection refused          | Anvil is not running or address and port are wrong                        | restart Anvil and verify endpoint                       |
| wrong chain identity             | fork remote procedure call Uniform Resource Locator points to wrong chain | verify chain identifier with `eth_chainId` call         |

---

## 11) Deployment readiness gate

Only proceed toward live deployment when all items below pass:

- static checks pass,
- unit and integration suites pass,
- Ethereum and Base fork smoke tests pass,
- shadow mode logs are healthy,
- no public mempool broadcast path is enabled in production profile.

This gate is mandatory for production grade operation.
