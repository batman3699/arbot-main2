# Arbot Operator Runbook (Step-by-Step)

This guide walks a first-time operator from a clean machine to a running, monitored Arbot instance. Every step is explicit—follow them in order.

## 1) Prepare the machine

1. Use Ubuntu 22.04 (server or WSL). Install base tools:
   
   ```bash
   sudo apt update
   sudo apt install -y build-essential pkg-config libssl-dev curl git
   ```
2. Install Rust toolchain:
   
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
   source "$HOME/.cargo/env"
   ```
3. Install Foundry (for contract deploys):
   
   ```bash
   curl -L https://foundry.paradigm.xyz | bash
   source "$HOME/.foundry/bin/foundryup"
   ```
4. Verify binaries:
   
   ```bash
   cargo --version
   forge --version
   cast --version
   ```
   
   All commands should print versions without errors.

## 2) Get the code

1. Clone the repo and enter it:
   
   ```bash
   git clone https://github.com/your-org/arbot.git
   cd arbot
   ```
2. Build once to fetch dependencies and confirm the toolchain works:
   
   ```bash
   cargo build --release
   ```

## 3) Create the runtime settings

Arbot resolves runtime config in this order: `ops/inputs.yaml` -> registry -> `.env`. Use `ops/inputs.yaml` for non-secret runtime config (chain RPC, venues, flashloans, universe/risk). Use `.env` for secrets and deployment-local addresses only.

1. Copy the template:
   
   ```bash
   cp .env.example .env
   ```
2. Open `.env` in your editor and set the basics:
   - Choose a chain with `CHAIN=arbitrum` (default) or a comma list with `CHAIN_LIST=arbitrum,optimism,base` to run multiple scanners.
   - Set `PRIVATE_KEY` for the hot wallet that signs transactions.
   - Set `<ENV_PREFIX>_EXECUTOR_ADDRESS` and `<ENV_PREFIX>_EXECUTOR_OWNER` for each enabled chain.
   - Keep RPC/venue/flashloan/permit2 values in `ops/inputs.yaml`; production startup rejects placeholder RPC hosts (`.example`, `localhost`) and unresolved `${...}` templates, so every RPC endpoint must be concrete before launch.
   - CI enforces this for non-example config files via `scripts/ci/check_placeholder_endpoints.sh`; run it before committing registry updates.
3. Configure execution limits in `ops/inputs.yaml` (preferred for low drift):
   - Capital/universe/risk settings and per-chain behavior.
   - Feature gates: enable only what is live and tested. Defaults are conservative (cycle arb on, everything else off):
     - `FEATURE_CYCLE_ARB=1`
     - `FEATURE_BACKRUN=0`
     - `FEATURE_SANDWICH=0`
     - `FEATURE_LIQUIDATIONS=0`
     - `FEATURE_BRIDGE=0`
   - Optional modules (only effective if their feature gate is enabled): ERC-3156 lender (`ERC3156_LENDER`/`ERC3156_FEE_BPS`). `ERC3156_FEE_BPS` must be an integer in `0..=10000`; malformed or out-of-range values fail startup validation before runners launch. JIT LP toggles, bridge planner paths, and accounting/tax fields.
4. Load the variables into your shell:
   
   ```bash
   set -a && source .env && set +a
   ```
5. Dry-run the binary offline to validate config ownership and that secrets/addresses are complete:
   
   ```bash
   OFFLINE_MODE=1 cargo run --release
   ```
   
   Fix any missing/typo’d variable before proceeding.

For detailed address entry examples, see [`docs/environment_generation.md`](./environment_generation.md).

## 4) Deploy the executor contract

1. Inspect `script/Deploy.s.sol` and adjust fee/slippage defaults if needed.
2. Choose a reliable HTTPS RPC for the target chain.
3. Optional simulation (no broadcast):
   
   ```bash
   forge script script/Deploy.s.sol:Deploy --rpc-url https://your-rpc --slow -vvvv
   ```
4. Real deployment:
   
   ```bash
   forge script script/Deploy.s.sol:Deploy \
     --rpc-url https://your-rpc \
     --broadcast \
     --slow -vvvv
   ```
5. Copy the emitted executor clone address into `<ENV_PREFIX>_EXECUTOR_ADDRESS` inside `.env` (or `ops/inputs.yaml`), then re-source the file:
   
   ```bash
   set -a && source .env && set +a
   ```

## 5) Run Arbot

1. Ensure fast WebSocket RPC endpoints are set for live data.
2. Start the runtime with informative logs:
   
   ```bash
   RUST_LOG=info,rpc=info cargo run --release
   ```
3. Confirm startup output lists your configured chains and pools without errors. Any complaint about missing addresses means `.env` or `ops/inputs.yaml` is incomplete.
4. Leave this terminal open while the bot runs. Stop with `Ctrl+C` when needed.

## 6) Monitor health and profit

Choose one or both options:

- **Simple log watching (quick):**
  - Look for `EXEC` lines with positive `netWei` and reasonable `latencyMs`.
  - Watch for warnings like `all websocket endpoints failed` or repeated `relayReject=true`.
- **Grafana dashboard (full):** follow [`docs/monitoring_guide.md`](./monitoring_guide.md) and [`docs/grafana/quickstart.md`](./grafana/quickstart.md) to spin up Prometheus + Grafana and import the ready-made dashboard. Keep `PROMETHEUS_PORT` set (for example `9000`).

## 7) Troubleshooting checklist

Work through these in order when something breaks:

1. **Binary will not start:** rerun `OFFLINE_MODE=1 cargo run --release` and read the first error; fix missing env vars or typo’d addresses.
2. **No executable cycles for >10 minutes:** verify RPC is live, refresh token lists, and ensure pools are active for your pairs.
3. **`SimulationReverted` or failed bundles:** update on-chain addresses, tighten slippage settings, and try a different relay.
4. **Circuit breaker mode (manual-only):** on-chain safeguards are owner-operated only. Use `tripCircuit` to force cooldown and `resetCircuit` to resume. Do not rely on loss-limit auto-tripping from counter thresholds.
5. **`all websocket endpoints failed` messages:** rotate to fresh RPC URLs and widen `RPC_MAX_BACKOFF_SECS` if reconnects flap.
6. **Latency spikes:** move a faster relay to the front of `PRIVATE_RELAY_URLS` and prefer geographically close RPCs.
7. **Profit mismatch vs wallet:** sum `netWei` from logs, confirm gas burn, and reconcile with on-chain balance before resuming.

## 8) Daily operator routine

1. Start Arbot with `RUST_LOG=info` and confirm it scans pools without errors.
2. Check logs every few minutes for positive `netWei` and low `latencyMs`.
3. Export profit numbers at the end of the day; compare against wallet balance.
4. Rotate secrets per your policy and keep RPC credentials private.

Follow this runbook and you will have a reproducible path from setup to monitored profits with minimal guesswork.
