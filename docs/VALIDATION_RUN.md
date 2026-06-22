# Base Shadow-Mode Validation Run

**Status:** authoritative runbook for validating Arbot on **Base mainnet** via
**Alchemy paid RPC**, in **shadow mode** (no on-chain broadcasts), before any
increase in live exposure.

Shadow mode runs the **entire** decision pipeline against live Base state —
`scan → simulate → size → plan` — but replaces the final broadcast with a stub
that records what *would* have been sent. It is the safe, real-data gate between
"compiles and passes tests" and "trades real capital."

> Scope: this is **not** the local-fork wiring test (see `docs/fork_dry_run.md`).
> The fork run uses a synthetic guaranteed-arb fixture on `anvil` to prove
> plumbing. This run uses **real Base state over a paid Alchemy endpoint** to
> validate realism, RPC stability, and the corrected economics.

---

## 0) TL;DR

```bash
# 1. Validate every Base address on-chain (read-only). Requires foundry `cast`.
ALCHEMY_KEY=<your_paid_key> scripts/shadow/validate_base_addresses.sh

# 2. Launch the shadow run (Base only, no broadcasts). Runs preflight first.
scripts/shadow/base_shadow_run.sh

# 3. In another terminal, watch what the bot would have executed.
tail -f logs/shadow-base.jsonl | jq .
```

---

## 1) BLOCKERS found during validation (must clear before a meaningful run)

These were discovered on-chain while building this runbook. **Both must be
resolved or the run will not validate the execution path.**

### B1 — Alchemy key is over its free-tier monthly quota (HTTP 429)

The key in `.env` returns:

```
Monthly capacity limit exceeded. ... upgrade your scaling policy ...
```

This is the same class of failure that produced ~40 days of `rpc_error` with
zero trades. The mission explicitly targets **Alchemy paid RPC** — you must
upgrade the Alchemy app to a paid tier (or use a paid/dedicated Base endpoint)
before the run. The preflight fails fast on this (it cannot read chain id).

### B2 — The Base executor is NOT deployed

`BASE_EXECUTOR_ADDRESS = 0x627e54a5Fad377d0d0eef60298f7F3d0e2c15E7A` has **no
bytecode on Base** (verified via `eth_getCode` on two independent RPCs → `0x`).

Consequence: `simulate_plan_execution` calls `start_v2(...)` on this address via
`eth_call`. With no code, **every candidate fails at the simulation stage**, so:
- the shadow log (`logs/shadow-base.jsonl`) will stay **empty**, and
- the candidate decision log will show `simulation_failed` for every opportunity.

You can still validate scanning/sizing/RPC stability with B2 open, but to
validate the **execution path** you must deploy the executor to Base
(`script/Deploy.s.sol`) and set `BASE_EXECUTOR_ADDRESS` (and the liquidation
`adapter` fields) to the deployed clone address.

> The preflight reports B2 as a hard `FAIL` by design — do not `--skip-preflight`
> past it unless you are deliberately validating only the scan/size path.

---

## 2) Prerequisites

| Requirement | Why |
|---|---|
| **Alchemy paid tier** (Base Mainnet app) | Free tier throttles within minutes under HFT quoting load (see B1). Growth/Scale tier or a dedicated node is required for realistic latency/throughput. |
| `cargo` (Rust 1.92+) | Build/run the engine. |
| foundry `cast` | On-chain preflight address validation. |
| `jq` | Parse the shadow JSONL log. |
| A **dedicated key** for `PRIVATE_KEY` | Shadow mode never broadcasts and sets the min-balance requirement to 0, so a fresh, **unfunded** key is fine. Do **not** reuse a key that holds funds. |

> **Security note:** `.env` currently contains a real, previously-exposed
> `PRIVATE_KEY` in plaintext. Treat it as compromised. For the shadow run, set
> `PRIVATE_KEY` to a fresh throwaway key (it is used only to derive a `from`
> address for `eth_call` simulation). It must not match the placeholder patterns
> (`replace`, `changeme`, `your_`, `0x`, empty) or production-mode validation
> rejects it.

---

## 3) Address validation (against official sources)

Every Base address the bot uses was validated **twice**: against the official
source-of-truth, and **on-chain** via `scripts/shadow/validate_base_addresses.sh`
(interface probes, not just existence). Result: **38 pass / 0 fail / 0 warn**
(executor deployed + allowlisted, Aerodrome wired).

| Component | Address | Official source | On-chain probe |
|---|---|---|---|
| UniV3 Factory | `0x33128a8fC17869897dcE68Ed026d694621f6FDfD` | docs.base.org ecosystem-contracts | has code |
| UniV3 QuoterV2 | `0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a` | docs.base.org | `factory()` → V3 factory ✓ |
| UniV3 SwapRouter | `0x2626664c2603336E57B271c5C0b26F421741e481` | docs.base.org | `factory()` → V3 factory ✓ |
| UniV2 Factory | `0x8909Dc15e40173Ff4699343b6eB8132c65e18eC6` | docs.base.org | has code |
| UniV2 Router | `0x4752ba5dbc23f44d87826276bf6fd6b1c372ad24` | docs.base.org | `factory()`/`WETH()` ✓ |
| Aerodrome PoolFactory | `0x420DD381b31aEf6683db6B902084cB0FFECe40Da` | aerodrome-finance/contracts | has code; `getPool(WETH,USDC,false)` ✓ |
| Aerodrome Router | `0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43` | aerodrome-finance/contracts | `defaultFactory()` → PoolFactory ✓ |
| Permit2 | `0x000000000022D473030F116dDEE9F6B43aC78BA3` | docs.base.org / canonical | has code |
| Balancer V2 Vault | `0xBA12222222228d8Ba445958a75a0704d566BF2C8` | balancer canonical multichain | `WETH()` → WETH ✓ |
| Aave V3 Pool | `0xA238Dd80C259a72e81d7e4664a9801593F98d1c5` | bgd-labs/aave-address-book (AaveV3Base) | `ADDRESSES_PROVIDER()` ✓; flash premium = 5 (0.05%) |
| Aave V3 AddressesProvider | `0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D` | aave-address-book | resolved by pool/data-provider |
| Aave V3 DataProvider | `0x0F43731EB8d45A581f4a36DD74F5f358bc90C73A` | aave-address-book | `ADDRESSES_PROVIDER()` ✓ |
| Aave V3 Oracle | `0x2Cc0Fc26eD4563A5ce5e8bdcfe1A2878676Ae156` | aave-address-book | has code |
| Compound cUSDCv3 (Comet) | `0xb125E6687d4313864e53df431d5425969c15Eb2F` | comet `deployments/base/usdc/roots.json` | `baseToken()` → USDC ✓ |
| Compound Configurator | `0x45939657d1CA34A8FA39A924B71D28Fe8431e581` | comet roots.json | `governor()` == comet governor ✓ |
| Compound Rewards | `0x123964802e6ABabBE1Bc9547D72Ef1B69B00A6b1` | comet roots.json | has code |
| WETH | `0x4200000000000000000000000000000000000006` | Base predeploy (WETH9) | `symbol()=WETH`, 18 dec ✓ |
| USDC (native) | `0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913` | Circle / BaseScan FiatTokenProxy | `symbol()=USDC`, 6 dec ✓ |
| **Executor clone (yours)** | `0x9445f7d3E1aA38bC9A9B373dc905D9fde7B9B852` | your deployment (live on Base) | has code; `owner()`→BatchRouter; hot signer allowlisted ✓ |
| **BatchRouter (owner)** | `0x46C6d9003FB9FBFE29d60ae6feF869F7CAe6f499` | your deployment (live on Base) | `owner()`→operator hot wallet ✓ |

### Universe tokens (`BASE_TOKENS`) — identified on-chain

| Address | Symbol | Decimals | Note |
|---|---|---|---|
| `0x4200…0006` | WETH | 18 | hub / flash-loan asset |
| `0x8335…2913` | USDC | 6 | hub / flash-loan / liq debt asset |
| `0x78a0…3ae9` | BSWAP | 18 | Baseswap gov token — thin liquidity |
| `0x9401…8631` | AERO | 18 | Aerodrome — liquid |
| `0xFe20…68F3A` | BALD | 18 | **defunct 2023 memecoin — very thin; consider removing** |
| `0x64FC…4E51` | SPECTRA | 18 | thin liquidity |

> **Tuning observation (not a blocker):** BSWAP, BALD, and SPECTRA are
> low-liquidity tokens. They add phantom-cycle noise and revert risk. For a
> profit-focused Base run, consider trimming the universe to WETH/USDC/AERO/cbETH/
> cbBTC-class assets. This is a config change in `BASE_TOKENS` / `registry.json`.

### DEX venue coverage (incl. Aerodrome)

The Base scan graph is built from these venues (see `ops/inputs.yaml` → `chains.base.venues`):

| Venue | Kind | Pools source | Notes |
|---|---|---|---|
| Uniswap V3 | `univ3_like` | `data/base/uniswap_v3/pools.jsonl` (liquidity-ranked) | concentrated liquidity, fee tiers 100/500/3000/10000 |
| Uniswap V2 | `univ2_like` | `data/base/uniswap_v2/pools.jsonl` | thin on Base (few liquid pools) |
| **Aerodrome** | `solidly_v2_like` | **`config/base_aerodrome_pools.json`** via `BASE_SOLIDLY_V2_POOLS` | **Base's dominant DEX**; vAMM (volatile) + sAMM (stable) |

**Aerodrome integration:**
- Pool list is enumerated on-chain from the Aerodrome `PoolFactory.getPool(tokenA,tokenB,stable)`
  across hub × {hub, major} pairs, keeping pools with real liquidity. Each pool's
  fee is read live from `PoolFactory.getFee(pool,stable)` (e.g. 30 bps volatile,
  varies per pool), and both swap directions are emitted.
- Quoting uses the correct Solidly curves: constant-product for volatile, the
  `x³y + xy³` stableswap (decimals-normalized to 1e18) for stable pools
  (`src/quote_solidly.rs`). The executor swaps Aerodrome pools via the generic
  transfer-then-`pair.swap` step (same `swap(uint,uint,address,bytes)` ABI as V2).
- Regenerate the pool list any time:

```bash
ALCHEMY_KEY=<paid_key> python3 scripts/data/build_aerodrome_pools.py
# writes config/base_aerodrome_pools.json (directional {pair,tokenIn,tokenOut,stable,feeBps})
```

- Runtime confirmation: startup logs `venue::solidly: Loaded Solidly/Aerodrome edges
  configured_pools=N edges_built=M`. Wiring this venue is what enables the
  cross-venue edge (Aerodrome ↔ Uniswap V3) where most Base arbitrage lives.

> **Remaining venue gap (next phase):** Aerodrome **Slipstream** (concentrated-liquidity,
> factory `0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A`) is not yet wired — it needs a
> tickSpacing-keyed CL path distinct from Uniswap's fee-tier model. The AMM (vAMM/sAMM)
> pools above are fully integrated.

### Known dead config (harmless)

- `BASE_UNIVERSAL_ROUTER = 0x6fF5693b99212Da76ad316178A184AB56D299b43` differs
  from the current Base UniversalRouter (`0x198EF79F…02fC`). It is **not read by
  any code path** (grep-verified), so it does not affect the run. Remove or fix
  it to avoid confusion.

To re-run validation at any time:

```bash
ALCHEMY_KEY=<paid_key> scripts/shadow/validate_base_addresses.sh
# or against any Base RPC:
BASE_SHADOW_RPC_URL=https://mainnet.base.org scripts/shadow/validate_base_addresses.sh
```

---

## 4) Safety model — what shadow mode does and does not do

| Action | Live mode | Shadow mode |
|---|---|---|
| Scan graph, find cycles | ✅ | ✅ |
| Quote via on-chain QuoterV2 (`eth_call`) | ✅ | ✅ (real RPC load) |
| Optimal sizing | ✅ | ✅ |
| Build plan / calldata | ✅ | ✅ |
| Pre-broadcast simulation (`eth_call`) | ✅ | ✅ (executor deployed + signer allowlisted) |
| **Submit bundle / raw tx to relay** | ✅ | ❌ **stubbed** (`shadow_dispatch`) |
| Nonce consumption | ✅ | ❌ (no real tx) |
| Wallet balance requirement | enforced | **0** (unfunded key OK) |
| Writes `logs/shadow-base.jsonl` | — | ✅ records would-be executions |

Code anchors (for auditability):
- Dispatch short-circuits before any relay use: `src/main.rs` `dispatch_call` →
  `if self.shadow.enabled { return self.shadow_dispatch(...) }`.
- Relay providers are still *constructed* at startup, but for Base the relay
  (`api.blxrbdn.com`) is built via URL-parse only (no network handshake) and is
  **never used** in shadow mode.
- `base_shadow_run.sh` hard-asserts `SHADOW_MODE=1` and refuses to start otherwise.

---

## 5) Run procedure

### Step 1 — Upgrade Alchemy + set env

Upgrade your Alchemy Base app to a paid tier, then either export the key or put
it in `.env` (already present, just ensure it's the paid key):

```bash
export ALCHEMY_KEY=<your_paid_base_key>
export PRIVATE_KEY=0x<fresh_throwaway_key>   # unfunded; simulation `from` only
```

### Step 2 — On-chain preflight

```bash
scripts/shadow/validate_base_addresses.sh
```

Expect `RESULT: PASS` once the executor is deployed (B2). Until then you'll see
the single executor `FAIL`.

### Step 3 — Launch the shadow run

```bash
scripts/shadow/base_shadow_run.sh           # interactive; Ctrl+C to stop
# or bounded:
RUN_SECS=900 scripts/shadow/base_shadow_run.sh   # auto-stop after 15 min
```

The launcher forces: `SHADOW_MODE=1`, `CHAIN=base`, `CHAIN_LIST=base`, chaos off,
`SHADOW_LOG_PATH=logs/shadow-base.jsonl`. These override `.env` (exported env wins
over dotenv), so your committed `.env` is left untouched.

### Step 4 — Monitor (separate terminals)

```bash
# Would-be executions (only populated once the executor is deployed):
tail -f logs/shadow-base.jsonl | jq '{ts:.timestamp_ms, start:.cycle_start, in:.amount_in_wei, net:.net_profit_wei, gas:.gas_cost_wei, hops:.hops}'

# Candidate decision log — WHY each opportunity was taken/rejected:
# (default path; override with CANDIDATE_LOG_PATH)
tail -f logs/candidates.jsonl 2>/dev/null | jq '{stage, reason:.rejection_reason, sim:.simulation_status, start:.cycle_start_token}'

# Live counters:
curl -s localhost:9100/metrics | grep -E 'opportunities_seen_total|simulations_(passed|failed)_total|tx_sent_total|rpc_errors_total|scan_ms'

# Accounting event stream (the original 'all rpc_error' signal lives here):
tail -f accounting/events.csv
```

---

## 6) Monitoring & interpretation guide

### Shadow log fields (`logs/shadow-base.jsonl`)

Each line is one would-be execution (written only when a candidate passes
simulation and reaches dispatch):

| Field | Meaning |
|---|---|
| `cycle_start` | start token of the cycle |
| `amount_in_wei` | optimal flash-loan size chosen by sizing |
| `est_gross_after_fee_wei` | gross profit after flash-loan fee, before gas |
| `net_profit_wei` | **net** profit estimate after gas |
| `gas_cost_wei` | gas cost in native units |
| `min_profit_wei` | the on-chain `minProfit` floor that would be enforced |
| `max_slippage_bps` | worst per-hop slippage in the route |
| `hops` | hop count |

### Key Prometheus metrics (`:9100/metrics`)

| Metric | Healthy signal |
|---|---|
| `rpc_errors_total{chain="base"}` | **flat / near-zero** — the #1 thing this run validates. Rising = RPC under-provisioned (B1). |
| `scan_ms` (histogram) | p95 well under the search budget; not climbing |
| `opportunities_seen_total` | > 0 — the scanner is finding candidate cycles |
| `simulations_passed_total` | > 0 — candidates survive real simulation (requires executor deployed) |
| `simulations_failed_total` | high relative to passed = bad economics, stale data, or B2 |
| `tx_sent_total` | in shadow this counts would-be sends; > 0 means the full path reached dispatch |
| `net_profit_native` / `net_profit_usd` | trend of would-be net profit |

### Candidate rejection reasons (decision log)

These are *expected* and explain why a candidate didn't execute:

| `rejection_reason` | Meaning | Action |
|---|---|---|
| `no_quote_available` | sizing found no profitable size | normal; market not dislocated |
| `below_min_profit_threshold` | gross/net under the floor | normal; raise/lower floor per strategy |
| `no_liquidity` | cycle start has zero capacity after slippage | normal for thin tokens |
| `no_flashloan_provider` | no provider supports the start token | expected unless WETH/USDC |
| `simulation_failed` / `simulation_timeout` | the `eth_call` reverted or timed out | **with B2 open, this is universal**; after deploy, indicates stale/competitive state |
| `plan_build_failed` | could not build calldata | investigate (edge/venue specific) |
| `gas_estimation_failed` | estimate reverted | usually downstream of a bad route |

### What "good" looks like (with executor deployed + paid RPC)

- `rpc_errors_total` stays low and flat for the whole run.
- `scan_ms` p95 stable (no unbounded growth).
- `opportunities_seen_total` climbs steadily; a fraction reach
  `simulations_passed_total`.
- `logs/shadow-base.jsonl` accumulates records with **positive `net_profit_wei`**.
- No panics; no `RunnerState::Error` storms in stdout.
- Accounting `events.csv` shows `tx_sent` / `tx_confirmed` (shadow synthesises a
  success receipt) — **not** an endless `rpc_error` stream.

---

## 7) Base-specific logic validated

- **Flash-loan providers:** Aave V3 Pool (`0xA238Dd80…`, premium 5 bps confirmed
  on-chain) and Balancer V2 Vault (`0xBA12…`, 0 bps). Both flash WETH/USDC per
  `ops/inputs.yaml`. Aave is additionally probed at startup by
  `validate_aave_pool_probes` (`ADDRESSES_PROVIDER()` + `FLASHLOAN_PREMIUM_TOTAL()`).
- **Venues:** UniV3 (fee tiers 100/500/3000/10000) and UniV2 — both routers
  confirmed to point at the correct Base factories on-chain.
- **Liquidations** (`FEATURE_LIQUIDATIONS=true`, enabled for base): Aave V3 and
  Compound V3 cUSDCv3, debt USDC / collateral WETH. Comet `baseToken()` confirmed
  to be USDC on-chain; configurator/rewards confirmed against Compound's official
  `roots.json`. **Note:** the liquidation `adapter` is the executor — blocked by
  B2 until deployed.
- **Gas model:** `op_stack` (correct for Base; includes L1 data fee).
- **Phantom-edge safety (from Phase 2):** UniV4 fixed-price edges are disabled by
  default; Solidly stable pools use the correct stableswap curve. Neither is wired
  for Base today, so no effect on this run, but the guards are active.

---

## 8) Pass / fail acceptance criteria

A Base shadow validation run is a **PASS** only if all hold over a ≥ 30-minute run:

1. Preflight `RESULT: PASS` (B1 + B2 cleared).
2. `rpc_errors_total{chain="base"}` remains low/flat (no 429 storm).
3. No runtime panics; no sustained `RunnerState::Error`.
4. `opportunities_seen_total > 0` (scanner productive on real state).
5. `simulations_passed_total > 0` and `logs/shadow-base.jsonl` has records.
6. Recorded `net_profit_wei` values are **positive** and survive the on-chain
   `min_profit_wei` floor.
7. Zero evidence of any real broadcast (shadow stubs all dispatch).

If 1–4 pass but 5–6 do not, the system is *stable* but not yet *profitable* on
Base — expected early; proceed to economics tuning, not live capital.

---

## 9) Optional — chaos / resilience drill

Chaos injection is **rejected in production mode**, so unset production first and
run a separate, clearly-labelled drill (never on the path to live):

```bash
APP_ENV=staging \
CHAOS_RELAY_REJECT_BPS=6000 \
CHAOS_BROADCAST_DELAY_MS=150 \
SHADOW_MODE=1 CHAIN=base CHAIN_LIST=base \
SHADOW_LOG_PATH=logs/shadow-base-chaos.jsonl \
cargo run --release --bin arb-exec
```

Validate that the circuit breaker / fail-closed paths engage under induced relay
rejection and latency. Also test RPC degradation: `CHAOS_DISABLE_WS=true` (forces
polling) and confirm the engine fails closed rather than trading on stale data.

---

## 10) Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Preflight: `chain id = <none>`, 429 | Alchemy over quota (B1) | upgrade to paid tier |
| Preflight: `Executor has NO bytecode` | executor not deployed (B2) | deploy via `script/Deploy.s.sol`, update `BASE_EXECUTOR_ADDRESS` |
| Every candidate `simulation_failed` | B2, or stale/competitive state | deploy executor; re-check |
| `shadow-base.jsonl` empty | nothing reached dispatch (B2, or no profitable ops) | resolve B2; widen universe / lower floor for the test |
| `rpc_errors_total` climbing | RPC under-provisioned | paid tier / dedicated node; lower `UNIV3_QUOTE_CONCURRENCY` |
| Refuses to start: `SHADOW_MODE != 1` | safety guard tripped | use the launcher, don't override |
| Startup: "no pool inventory" | missing `data/base/**/pools.jsonl` | present in repo (141k UniV2 + 491 UniV3); re-run ingestion if deleted |

---

## 11) Exit criteria & next steps

1. **Clear B1** (paid Alchemy) and **B2** (deploy Base executor) → preflight PASS.
2. Run shadow for ≥ 30 min; confirm acceptance criteria §8.
3. If stable but unprofitable: this is the signal to measure real RPC quote load
   (decide multicall batching vs. local quoter) and tune the universe/floors —
   **not** to add capital.
4. Only after a clean shadow run with positive recorded net profit should live
   exposure be considered, and then with the smallest viable `BASE_AMOUNT_WEI`
   and tight circuit-breaker loss limits.

---

## Appendix — files

| File | Purpose |
|---|---|
| `scripts/shadow/validate_base_addresses.sh` | On-chain (read-only) address + interface preflight |
| `scripts/shadow/base_shadow_run.sh` | Base-only shadow launcher with safety guards |
| `logs/shadow-base.jsonl` | Would-be executions (shadow output) |
| `logs/candidates.jsonl` | Per-candidate decision log (`CANDIDATE_LOG_PATH`) |
| `docs/fork_dry_run.md` | Complementary local-fork wiring test |
