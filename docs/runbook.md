# Arbot Runbook (Multi-chain Ops)

## Fill `ops/inputs.yaml`

Minimum per-chain fields (repeat for each chain in `chains:`):

- `chain_name`, `chain_id`, `env_prefix`
- `rpc_http_urls` and `rpc_ws_urls`
- `executor_address` and `executor_owner`
- Venue addresses (at least one quoting path):
  - `venues` entry with `kind: univ3_like`, `quoter`, `factory`, `router`
  - Optional: `balancer_like` with `vault`
- Flash loan providers (Aave/Balancer) if you want live execution

Liquidations, backrun, and other features remain optional but must be fully specified when enabled.

## Config precedence + startup validation

Resolution order (highest → lowest): `ops/inputs.yaml` (non-secret runtime) → registry (shared metadata) → `.env` (secrets + deployment-local addresses).
Startup validation fails fast if any required RPC endpoints, addresses, or keys are missing or malformed.

## Deploy executor per chain

Use Foundry deploy script (per-chain):

```bash
CHAIN=<chain_name> \
RPC_URL=<rpc_http_url> \
PRIVATE_KEY=<deployer_key> \
forge script script/Deploy.s.sol:Deploy \
  --rpc-url "$RPC_URL" \
  --broadcast
```

Capture the deployed executor address and set in `ops/inputs.yaml`:

```yaml
chains:
  - chain_name: arbitrum
    executor_address: <DEPLOYED_EXECUTOR>
    executor_owner: <DEPLOYER_OR_MULTISIG>
```

## Run all six chains

Single binary, multi-chain:

```bash
CHAIN_LIST=arbitrum,optimism,base,linea,abstract,ink,ethereum \
cargo run --release
```

## Recommended process layout

**Option A (preferred for latency):** one process per chain, CPU pinned.

```bash
taskset -c 0 CHAIN=arbitrum cargo run --release &
taskset -c 1 CHAIN=base cargo run --release &
taskset -c 2 CHAIN=linea cargo run --release &
taskset -c 3 CHAIN=abstract cargo run --release &
taskset -c 4 CHAIN=ink cargo run --release &
taskset -c 5 CHAIN=ethereum cargo run --release &
```

**Option B:** single binary with `CHAIN_LIST` (less isolation, simpler ops).

## Integration smoke test

Run a dry integration check (RPC connectivity + quoting).

```bash
ARBOT_INTEGRATION_SMOKE=1 cargo test --test integration_smoke
```

`integration_smoke` now ignores unresolved `${ENV_VAR}` RPC placeholders and only fails when no concrete endpoint remains after filtering.

Optional executor simulation (requires `executor_address` + `executor_owner` set):

```bash
ARBOT_INTEGRATION_SMOKE=1 ARBOT_SMOKE_EXECUTOR=1 \
cargo test --test integration_smoke
```

## Circuit breaker operating mode

- **Selected mode:** manual-only on-chain circuit control.
- `tripCircuit()` opens cooldown immediately.
- `resetCircuit()` clears a manual trip and re-enables starts.
- There is no on-chain loss-counter threshold enforcement; operators must treat breaker activation as an explicit owner action.

## Dynamic token universe policy (profit-first defaults)

Set the universe controls in `ops/inputs.yaml`:

```yaml
universe:
  dynamic_top_tokens_30d: 200
  include_usd_stablecoins: true
  include_weth: true
  include_wbtc: true
  pair_prune_min_liquidity_usd: 100000
```

This keeps high-volume routing assets while hard-pruning thin pairs under `$100k` liquidity to reduce quote latency and bad-path noise.

Runtime guardrail: `TOKEN_WHITELIST_MAX` is clamped to a minimum of `64` at startup so aggressive tuning cannot silently zero out scanned edges.

## Doc validation checklist

- [ ] `.env.example` is restricted to env-only keys: secrets, API keys, and deployment-local addresses.
- [ ] `ops/inputs.yaml` carries non-secret runtime knobs (universe/risk/per-chain behavior) and venue topology.
- [ ] `docs/deployment_and_run.md` references prefixed keys (for example `<ENV_PREFIX>_EXECUTOR_OWNER`) and documents config ownership clearly.
