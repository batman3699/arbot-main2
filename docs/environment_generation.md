# Environment Generation Guide (Production + Beginner-Friendly)

This guide explains how to generate a safe `.env` and runtime config for Arbot.

Goal: make configuration deterministic, reproducible, and safe for a live production-grade arbitrage system.

---

## Why this matters

Most runtime incidents come from bad environment state:
- wrong chain selected,
- placeholder RPC endpoints in production,
- mismatched signer address and key,
- missing registry source,
- accidental public broadcast fallback.

A correct environment prevents expensive mistakes before code runs.

---

## 1) Configuration model (what controls what)

Arbot reads config from these sources, highest priority first:

1. `ops/inputs.yaml` (non-secret runtime behavior)
2. Registry source (`REGISTRY_FILE`, `REGISTRY_IPFS_CID`, or `REGISTRY_IPNS`)
3. `.env` (secrets and deployment-local overrides)

### Simple rule
- Put secrets in `.env`.
- Put non-secret strategy/runtime values in `ops/inputs.yaml`.
- Put canonical shared addresses in registry.

---

## 2) Generate `.env` from template

Run from repo root:

```bash
cp .env.example .env
```

Now edit `.env`.

---

## 3) Pick one registry source

Choose exactly one of these as your primary source:

### Option A: Local registry file (best for local dev/fork)
```bash
REGISTRY_FILE=config/registry.json
```

### Option B: IPFS CID (best for shared immutable release)
```bash
REGISTRY_IPFS_CID=<your_cid>
REGISTRY_IPFS_GATEWAY=https://ipfs.io
```

### Option C: IPNS name (best for updatable pointer)
```bash
REGISTRY_IPNS=<your_ipns_name>
REGISTRY_IPFS_GATEWAY=https://ipfs.io
```

Optional integrity lock:
```bash
REGISTRY_EXPECTED_HASH=<sha256_hex>
```

What to expect:
- Arbot caches resolved registry data at `cache/registry.json`.
- If remote resolution fails and cache exists, runtime can continue using cache.

---

## 4) Set chain + signer identity

In `.env` set:

```bash
CHAIN=ethereum
PRIVATE_KEY=0x...
SIGNER_ADDRESS=0x...
BASE_AMOUNT_WEI=100000000000000000
```

Supported examples: `ethereum`, `arbitrum`, `optimism`, `base`, `linea`, `abstract`, `ink`.

### Why these fields matter
- `CHAIN`: selects per-chain venues/tokens/contracts.
- `PRIVATE_KEY` + `SIGNER_ADDRESS`: transaction signer; mismatch causes runtime failure.
- `BASE_AMOUNT_WEI`: first sizing anchor for route evaluation.

---

## 5) Set RPC endpoints (required)

Define RPC URLs in `ops/inputs.yaml` and inject keys via env placeholders.

Minimum recommended:
- 2+ HTTPS RPC endpoints per chain.
- 1+ WSS endpoint per chain for subscriptions.
- provider diversity (avoid single-vendor outage risk).

Example env values:

```bash
ALCHEMY_KEY=...
ETHEREUM_PRIVATE_RPC_HTTP_URL=https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
ETHEREUM_PRIVATE_RPC_WS_URL=wss://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
```

---

## 6) Set execution safety knobs (must fail closed)

Confirm these are explicitly set (in `.env` and/or `ops/inputs.yaml`):

- `MIN_PROFIT_WEI` (or equivalent min net threshold)
- `MAX_GAS_WEI` / cost cap
- slippage controls (`minOut`/per-hop floors)
- private-only relay submission toggle/path
- circuit-breaker thresholds for revert rate and RPC health

Expected behavior in production:
- If a required signal is missing/stale, bot should not broadcast.
- If expected net profit is below threshold, route is rejected.

---

## 7) Validate environment before runtime

Run these checks from repo root:

```bash
./scripts/check_deploy_env.sh
./scripts/ci/check_placeholder_endpoints.sh
./scripts/ci/no_runtime_panics.sh
```

If chain-specific:

```bash
CHAIN=ethereum ./scripts/check_deploy_env.sh
CHAIN=base ./scripts/check_deploy_env.sh
```

---

## 8) Build and smoke test

```bash
cargo build --release
cargo test
forge test
```

Fork smoke (example):

```bash
ARBOT_INTEGRATION_SMOKE=1 \
ARBOT_INTEGRATION_CHAIN=ethereum \
ARBOT_FORK_RPC_URL=http://127.0.0.1:8545 \
CHAIN=ethereum \
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

---

## 9) Common mistakes and fixes

1. **Signer mismatch** (`PRIVATE_KEY` does not match `SIGNER_ADDRESS`)
   - Fix: derive address from key and update one side.
2. **Placeholder URLs in production**
   - Fix: run `./scripts/ci/check_placeholder_endpoints.sh` and replace all placeholders.
3. **Wrong chain selected**
   - Fix: set `CHAIN` explicitly and run chain-specific env check.
4. **Registry hash mismatch**
   - Fix: update source or hash; do not bypass unless intentional rollout.
5. **Public mempool path active in prod profile**
   - Fix: disable any public broadcast fallback and enforce private-only.

---

## 10) Production readiness exit criteria

Do not mark environment ready unless all are true:
- env checks pass,
- tests pass,
- fork smoke passes,
- no placeholder endpoints,
- signer configured and funded,
- private relay path validated,
- observability/log sink configured.

This is the minimum bar for a live production-grade deployment.
