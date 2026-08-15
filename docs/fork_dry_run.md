# Fork Dry Run Guide (Live-System Safe)

This runbook validates Arbot end-to-end on a local fork before touching live capital.

A dry run proves:
- config is coherent,
- scanner/simulator/executor wiring works,
- logs and metrics are emitted,
- safety controls fail closed when expected.

---

## Why run this every time

In production arbitrage, tiny config mistakes can cause immediate loss (bad route data, wrong token decimals, stale RPCs, incorrect signer). Fork dry run catches these errors at low cost.

---

## 0) Prerequisites

From repo root:

```bash
cd /workspace/arbot-main
```

Check tools:

```bash
anvil --version
cast --version
cargo --version
```

Set key env:

```bash
export ALCHEMY_KEY="YOUR_ALCHEMY_KEY_HERE"
export FORK_RPC_URL="http://127.0.0.1:8545"
export PRIVATE_KEY="0xYOUR_FORK_ONLY_PRIVATE_KEY"
export SIGNER_ADDRESS="0xYOUR_FORK_WALLET_ADDRESS"
```

Safety note: use a dedicated test key only.

---

## 1) Start Ethereum fork (Terminal A)

```bash
anvil \
  --fork-url "https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}" \
  --fork-block-number 19600000 \
  --block-time 1 \
  --auto-impersonate \
  --prune-history \
  --steps-tracing
```

Expected output:
- `Listening on 127.0.0.1:8545`
- `Auto impersonate: true`

Keep this process running.

---

## 2) Prepare deterministic fixture (Terminal B)

```bash
cd /workspace/arbot-main
bash ./scripts/fork/create_arbitrage_fixture.sh
```

What this does:
- creates repeatable imbalance conditions,
- funds test signer path for transaction execution,
- makes integration checks deterministic.

If it fails, clear conflicting chain vars and rerun:

```bash
unset CHAIN CAST_CHAIN FOUNDRY_CHAIN DAPP_CHAIN ETH_CHAIN CHAIN_ID
bash ./scripts/fork/create_arbitrage_fixture.sh
```

---

## 3) Run Arbot in shadow mode (Terminal C)

```bash
cd /workspace/arbot-main
SHADOW_MODE=1 \
SHADOW_LOG_PATH=logs/shadow-mainnet.jsonl \
SHADOW_TAG=anvil-ethereum \
CHAOS_RELAY_REJECT_BPS=6000 \
CHAOS_PUBLIC_REJECT_BPS=2500 \
CHAOS_BROADCAST_DELAY_MS=120 \
RUST_LOG=info \
PRIVATE_RELAY_URLS=https://relay.flashbots.net,https://rpc.beaverbuild.org,https://builder0x69.io,https://rpc.rsync-builder.xyz \
CHAIN=ethereum \
PRIVATE_KEY="$PRIVATE_KEY" \
SIGNER_ADDRESS="$SIGNER_ADDRESS" \
cargo run --release
```

Why shadow mode:
- executes full decision pipeline,
- records opportunities and cost/profit estimates,
- avoids real onchain broadcasts.

---

## 4) Observe logs (Terminal D)

```bash
cd /workspace/arbot-main
tail -f logs/shadow-mainnet.jsonl
```

Quick parsed view:

```bash
jq 'select(.net_profit_wei!=null) | {ts:.timestamp_ms, net:.net_profit_wei, gas:.gas_cost_wei, hops:.hops, start:.cycle_start}' logs/shadow-mainnet.jsonl | head -n 5
```

Expected:
- JSON lines steadily appended,
- candidate evaluations with gas/net fields,
- no panic crashes.

---

## 5) Acceptance criteria (pass/fail)

A dry run is **pass** only if all are true:

1. Fork and fixture run successfully.
2. Bot starts and remains stable.
3. Logs contain evaluated opportunities.
4. No runtime panics or unwrap/expect crashes.
5. No evidence of public mempool broadcast path usage.
6. Rejections are explainable by safety guards (min profit/slippage/cost caps), not malformed config.

---

## 6) Optional chaos checks

WebSocket outage simulation:

```bash
CHAOS_DISABLE_WS=1 SHADOW_MODE=1 CHAIN=ethereum PRIVATE_KEY="$PRIVATE_KEY" SIGNER_ADDRESS="$SIGNER_ADDRESS" cargo run --release
```

What to expect:
- degraded data freshness,
- circuit-breaker/fail-closed behavior when critical signals are stale.

---

## 7) Stop all processes

- Stop bot in Terminal C with `Ctrl+C`.
- Stop anvil in Terminal A with `Ctrl+C`.

---

## 8) Troubleshooting quick table

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| `Loopback targets forbidden` | container/CI network policy | bind Anvil to `0.0.0.0` and use container IP |
| signer tx fails | key/address mismatch or zero funds | verify key/address pair, rerun fixture |
| no log output | wrong `SHADOW_LOG_PATH` or startup error | check process logs and file path permissions |
| repeated reverts | bad route metadata/decimals/adapter mismatch | re-check venue onboarding and registry |
