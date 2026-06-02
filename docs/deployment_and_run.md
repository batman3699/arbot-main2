# Deployment & Runtime Checklist

This is the no-nonsense path to a live executor. Follow the steps in order and
use `ops/inputs.yaml` as the canonical non-secret runtime config (RPC, venues, flashloan, permit2, universe/risk knobs), `config/` for shared static artifacts, and `.env` only for secrets + deployment-local addresses.

## 0. Workstation prep

1. Use a Linux box (Ubuntu 22.04 or similar). Install Git and Rust (`rustup`).
2. Install Foundry binaries (`foundryup` + `forge/cast/anvil/chisel`) from the
   official installer:
   ```bash
   curl -L https://foundry.paradigm.xyz | bash
   source ~/.bashrc
   foundryup
   ```
3. Confirm `cargo`, `forge`, and `cast` all report versions without errors.
4. Verify Solidity compilation from repo root before any deploy step:
   ```bash
   forge build
   ```
5. Make sure the operator wallet has enough native gas token on every target
   chain. No balance, no trades.

## 1. Configure `.env`

1. From the repo root, copy the template:
   ```bash
   cp .env.example .env
   ```
   The template intentionally focuses on secrets and deployment-only settings. Canonical chain addresses and venue metadata belong in `ops/inputs.yaml`.
2. Point at your registry (choose one):
   - `REGISTRY_FILE=config/registry.json` for a local copy, or
   - `REGISTRY_IPFS_CID=<cid>` (plus optional `REGISTRY_IPFS_GATEWAY`), or
   - `REGISTRY_IPNS=<ipns-name>`.
   Add `REGISTRY_EXPECTED_HASH` if you know the SHA-256 to enforce integrity.
3. Config precedence (highest → lowest):
   1. `ops/inputs.yaml` (explicit per-chain overrides + venues/flashloans)
   2. Registry (file/IPFS/IPNS)
   3. Environment variables (`.env`, secrets + deployment-local addresses)
   Secrets (like `PRIVATE_KEY`) are **env-only** and are never pulled from the registry.
4. Pick your working chain and base size:
   - Set `CHAIN=` to match a key in the registry (e.g. `arbitrum`, `optimism`,
     `base`, `ethereum`, `abstract`, `ink`, `linea`).
   - Set `BASE_AMOUNT_WEI=` to the trial flash-loan amount (recommended explicit). If omitted, runtime now defaults to `1e18` wei to avoid zero/1-wei quote starvation.
5. Set the executor wallet secrets:
   - `PRIVATE_KEY` for the operator account broadcasting transactions
   - `<ENV_PREFIX>_EXECUTOR_OWNER` (e.g. `ARB_EXECUTOR_OWNER`) if governance lives on a different address than the signer
6. Put non-secret runtime knobs (search/time/risk/universe) in `ops/inputs.yaml` under `universe`/`risk`; reserve `.env` for secrets, relays, and deployment-local addresses only.
7. Optional extras in `.env`: relays (`PRIVATE_RELAY_URLS` or `<ENV_PREFIX>_PRIVATE_RELAY_URLS`) and external API keys. Keep canonical protocol addresses and performance knobs in `ops/inputs.yaml`.
8. Source the file into your shell:
   ```bash
   set -a && source .env && set +a
   ```
9. Run `OFFLINE_MODE=1 cargo run --release` to validate the config. The runtime
   will fetch the registry, cache it, and complain if anything is missing.

See `docs/environment_generation.md` for a concrete walkthrough of the manual
address entry process.


## Canonical flash-loan provider addresses (Aave V3 Pool)

Use these exact addresses in both `ops/inputs.yaml` (`chains[].flashloans[].pool`, `features.liquidation_markets[].aave_v3.pool`, and each `markets[].flash_loan_pool`) and `config/registry.json` (`chains.<chain>.aave_pool`). Keeping them identical avoids startup probe failures and liquidation misroutes.

| Chain | Canonical Aave V3 Pool |
| --- | --- |
| Ethereum | `0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2` |
| Arbitrum | `0x794a61358D6845594F94dc1DB02A252b5b4814aD` |
| Optimism | `0xa97684ead0e402dC232d5A977953DF7ECBaB3CDb` |
| Base | `0xA238Dd80C259a72e81d7e4664a9801593F98d1c5` |

Chains without Aave support in this stack should keep `aave_pool: null` in the registry and omit `aave_v3_like` flashloan entries.

## 2. Deploy contracts

1. Inspect `script/Deploy.s.sol` and adjust fee / risk parameters if needed.
   The executor now treats `cycleSlippageBps` as a **hard slippage-cap declaration**,
   not a profit floor. If a submitted plan sets `cycleSlippageBps` above the on-chain
   configured `maxSlippageBps`, execution reverts with `InvalidMaxSlippage()` before
   any flash-loan side-effects are attempted.

   `minProfit` is intentionally independent from notional-size percentages. Keep
   `minProfit` as a pure economic threshold managed off-chain (gas costs, inclusion
   probability, relay competition, expected adverse selection), while per-hop swap
   slippage protection remains encoded in step-level `min_out` values from the planner.

   Access control is now role-separated on-chain:
   - `executors`: allowed to call `start`, `startLegacy`, and `startV2`.
   - `configAdmins`: allowed to modify runtime risk/config parameters (fee recipient,
     profit recipient, slippage/deadline config, circuit breaker controls, approvals).
   - `owner`: manages role assignment (`setExecutor`, `setConfigAdmin`) and ownership.

   For canonical profit accounting, optionally set `canonicalProfitToken` to force
   execution plans to borrow/settle in a single treasury asset. If unset (`address(0)`),
   profit accounting defaults to the borrowed token.

   Uniswap V3 step-level slippage is strict on-chain: `min_out` must be non-zero and
   is enforced via router `amountOutMinimum`.
2. Choose an HTTPS RPC endpoint that supports broadcasting transactions.
3. (Optional) Dry-run the script:
   ```bash
   forge script script/Deploy.s.sol:Deploy --rpc-url https://your-rpc --slow -vvvv
   ```
4. Deploy for real:
   ```bash
   forge script script/Deploy.s.sol:Deploy \
     --rpc-url https://your-rpc \
     --broadcast \
     --slow -vvvv
   ```
5. Run preflight env validation before broadcasting:
   ```bash
   bash scripts/check_deploy_env.sh
   ```
   The deploy script consumes these env keys exactly (prefixed and global forms):
   - Prefix derivation: `ENV_PREFIX` or `CHAIN`
   - Owner/signer: `PRIVATE_KEY`, `<PREFIX>_EXECUTOR_OWNER` (fallback `EXECUTOR_OWNER`)
     - Deploy flow is `clone -> router -> initialise(clone owner = router) -> transfer router ownership to configured executor owner`.
       This keeps `BatchRouter` as the only privileged caller for executor entrypoints while still handing governance to the configured owner.
   - UniV3 router: `<PREFIX>_UNIV3_ROUTER` (fallback `<PREFIX>_SWAPROUTER02`)
     - Deploy now accepts canonical short prefixes and long aliases interchangeably for ETH/ARB/OPT chains (for example, `ETH_UNIV3_ROUTER` or `ETHEREUM_UNIV3_ROUTER`).
     - Ethereum mainnet hard fallback is built-in: if both prefixed/global env values are unset on chain id `1`, Deploy automatically uses `SwapRouter02` at `0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45`. This avoids false-negative deploy failures during emergency rotations where only required secrets are present.
   - Balancer vault canonical first: `<PREFIX>_BAL_VAULT` (legacy fallback `<PREFIX>_BALANCER_VAULT`)
     - Ethereum mainnet hard fallback is built-in: if balancer env values are unset on chain id `1`, Deploy uses Balancer Vault `0xBA12222222228d8Ba445958a75a0704d566BF2C8`.
   - Aave pool (required on enabled chains): `<PREFIX>_AAVE_POOL`
     - Ethereum mainnet hard fallback is built-in: if Aave pool env values are unset on chain id `1`, Deploy uses Aave V3 Pool `0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2`.
   - Permit2 canonical first: `<PREFIX>_PERMIT2_ADDRESS` (legacy fallback `<PREFIX>_PERMIT2`)
     - Ethereum mainnet hard fallback is built-in: if permit2 env values are unset on chain id `1`, Deploy uses canonical Permit2 `0x000000000022D473030F116dDEE9F6B43aC78BA3`.
6. The deployment path is atomic: factory `deployAndInit(...)` now deploys the clone and calls `initialise(...)` in the same transaction, eliminating mempool front-run windows where an external account could seize ownership between transactions.
7. Note the emitted executor clone address and store it in `.env` (`<ENV_PREFIX>_EXECUTOR_ADDRESS`) or ops if you intentionally centralize non-secret addresses there.
   Re-source `.env` afterwards.

## 3. Run the bot

1. Ensure your websocket RPC URLs are hot and low-latency. Stale endpoints are
   foregone profit.
2. Start the executor:
   ```bash
   cargo run --release
   ```
3. Tail the logs. The runtime should enumerate pools and begin searching using
   the cached registry. Any error is a bug stealing alpha—fix it before
   continuing.

The registry stays as the single source of truth; the cache keeps the bot alive
through short network hiccups.


## Dynamic token whitelist behavior (edge-scan coverage)

To prevent Ethereum scans from collapsing to only a handful of edges when `ETH_TOKENS` is narrowly configured, runtime edge pruning now builds a **dynamic whitelist** per scan pass:

1. Start with the configured chain token set (`<CHAIN>_TOKENS` / registry tokens).
2. Union token endpoints from current hot `univ2_like` pools.
3. Union token endpoints from current hot `univ3_like` pools.
4. Union token endpoints from any low-liquidity scanner candidates.
5. Enforce hard cap with `TOKEN_WHITELIST_MAX` (default `512`).

This keeps pruning safety while allowing discovered venues/pools to contribute edges even if the operator initially seeded a small token set.

### Operator knobs

- `TOKEN_WHITELIST_MAX` (optional): max token addresses admitted into pruning whitelist during scan build.
  - Default: `512`
  - Set lower to reduce quote fan-out and latency.
  - Set higher to expand venue surface area at the cost of quote workload.

### Practical guidance

- If you observe logs showing only `2` active edges on Ethereum, verify:
  - pool ingestion is populated (`data/ethereum/<venue>/pools.jsonl` non-empty), and
  - `TOKEN_WHITELIST_MAX` is not set too low.
- Runtime now **fails startup** for configured `univ2_like` / `univ3_like` venues if `data/<chain>/<venue>/pools.jsonl` is missing or empty. Populate inventory first (for example with `cargo run --bin ingest -- --chain <chain> --venue <venue>`).
- Runtime now **fails fast during startup** for every configured `univ2_like`/`univ3_like` venue when `data/<chain>/<venue>/pools.jsonl` is missing or empty. This is intentional safety behavior for live mode: no inventory means no graph and therefore no profitable route search.
- Recovery path: run the ingestion/discovery job first (for example `cargo run --bin ingest -- --chain <chain> --venue <venue> ...`) and restart only after pool files are present and non-empty.
- Runtime pool inventory lookup order is now: `POOL_DATA_ROOT` (if set) -> `/data/<chain>/<venue>/pools.jsonl` (if present) -> repo-local `data/<chain>/<venue>/pools.jsonl`. This prevents cold-start misses when production mounts inventories under `/data`.
- `ingest` now scans logs in block windows instead of one giant RPC query, so ingestion keeps making visible progress instead of appearing stalled on large ranges. Override with:
  - `--chunk-size <blocks>` (default `10000`)
  - `--query-timeout-secs <seconds>` (default `45`)
  - Example: `cargo run --bin ingest -- --chain ethereum --venue uniswap_v3 --from-block 12369621 --to-block 22000000 --chunk-size 5000 --query-timeout-secs 30`
- You no longer need to manually over-expand `ETH_TOKENS` just to let discovered pool endpoints survive pruning.
- Curve venue quotes now auto-fallback to `latest` when the RPC returns block-out-of-range for `eth_call` at a pinned block, reducing noisy false negatives from lagging providers.
- Balancer quotes now auto-fallback to `latest` when pinned-block `queryBatchSwap` calls hit block-out-of-range RPC drift, preventing transient pool disablement from provider lag.
- Startup now permits baseline UniV3 quoting before any historical profitable pairs are learned, preventing all-edge hot-path skips during cold start.


### Edge-scan resilience knobs (Ethereum)

- `UNIV3_BOOTSTRAP_MAX_PAIRS` (default `256`): when hot UniV3 pool store is empty, runtime probes token-whitelist pairs (across configured fee tiers) up to this many token-pair checks to bootstrap tradable UniV3 pools directly from factory discovery.
- `UNIV3_BOOTSTRAP_MAX_POOLS` (default `512`): hard cap on discovered UniV3 pools admitted by bootstrap mode.
- `UNIV3_MIN_FORCED_QUOTES` (default `8`): guarantees at least this many UniV3 quote attempts per scan cycle even when hot-path gating marks all paths as cold/backed-off. This prevents repeated `built_edges=0` loops after transient quote failures.

### Balancer poolId compatibility

`<CHAIN>_BAL_POOLS` entries now accept either:
- canonical 32-byte Balancer `poolId`, or
- a Balancer pool contract address in the `poolId` field.

If a pool address is supplied, runtime resolves `poolId` via `getPoolId()` once and then quotes normally.
