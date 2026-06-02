# Venue Onboarding Guide (Production)

This document explains exactly how to add a new DEX/venue to Arbot **without weakening profit safety rails**.

## Why this exists

Venue onboarding is not just “add contract addresses.” If metadata is wrong, the scanner discovers fake/non-routable edges, simulations diverge, and execution reverts. In live mode, that directly burns EV via missed blocks, failed bundles, and wasted engineering cycles.

Use this guide to make onboarding deterministic and repeatable.

---

## 1) Core concepts (simple terms)

- **Venue**: a protocol where swaps happen (Uniswap, Curve, Balancer, etc.).
- **Kind (`ops kind`)**: tells Arbot which adapter logic to use for quoting and execution.
- **Entrypoints**: contracts Arbot must know (factory/router/quoter/pool manager/vault).
- **Verification**: proving each address is the real deployed contract (not spoofed).
- **Seed pools**: high-liquidity pools added first so the graph has useful edges immediately.

If you remember one rule: **wrong addresses = wrong quotes = no profit**.

---

## 2) Venue-to-kind mapping

| Venue | Kind (ops `kind`) | Required addresses (ops fields) | Verification (per chain) | Flashloan support |
| --- | --- | --- | --- | --- |
| Uniswap v4 | `univ4` | `pool_manager` | Verify `PoolManager` on chain explorer; confirm code hash matches official deployment | None |
| Camelot v3 | `univ3_like` | `factory`, `router`, `quoter` | Verify Camelot V3 factory/router/quoter on explorer; confirm quoter ABI is V3-compatible | None |
| Fluid | `generic_router` | `router` | Verify Fluid router on explorer; confirm swap ABI for adapter | Protocol-specific (typically none) |
| GMX | `generic_router` | `router` | Verify GMX router on explorer; confirm swap ABI for adapter | None |
| Kyber | `univ3_like` | `factory`, `router`, `quoter` | Verify Kyber V3 factory/router/quoter on explorer | None |
| Aerodrome | `solidly_v2_like` | `factory`, `router`, `pool_init_code_hash` | Verify factory/router on explorer; confirm init code hash from factory bytecode | None |
| Velodrome | `solidly_v2_like` | `factory`, `router`, `pool_init_code_hash` | Verify factory/router on explorer; confirm init code hash from factory bytecode | None |
| SyncSwap | `univ2_like` | `factory`, `router`, `pool_init_code_hash` | Verify factory/router on explorer; confirm init code hash from factory bytecode | None |
| Nile | `univ2_like` | `factory`, `router`, `pool_init_code_hash` | Verify factory/router on explorer; confirm init code hash from factory bytecode | None |
| DyorSwap | `univ2_like` | `factory`, `router`, `pool_init_code_hash` | Verify factory/router on explorer; confirm init code hash from factory bytecode | None |
| Aborean v3 | `univ3_like` | `factory`, `router`, `quoter` | Verify factory/router/quoter on explorer; confirm quoter ABI is V3-compatible | None |
| Aborean v2 | `univ2_like` | `factory`, `router`, `pool_init_code_hash` | Verify factory/router on explorer; confirm init code hash from factory bytecode | None |

**Note:** for `generic_router`, adapter encoding must match the venue ABI exactly. Do not assume Uniswap-like calldata.

---

## 3) Production onboarding checklist (must pass all)

1. **Address authenticity**
   - Source addresses from official docs/governance repos.
   - Confirm verified bytecode on explorer.
   - Record explorer links in your onboarding PR notes.
2. **Adapter compatibility**
   - Confirm venue `kind` matches swap math + ABI surface.
   - For `generic_router`, confirm selector list and argument order.
3. **Liquidity relevance**
   - Add only pools with meaningful liquidity/volume.
   - Prefer pools connecting to base tokens (WETH/USDC/USDT on EVM chains).
4. **Simulation parity**
   - Fork-test at fixed block with deterministic inputs.
   - Ensure quote path can be simulated and executed without revert.
5. **Profit guardrails**
   - Verify min profit and max cost checks still hold with new venue edges.
   - Ensure no new route bypasses slippage floors.

If any item fails, do not enable the venue in live runtime.

---

## 4) Ethereum L1 canonical entrypoints

Use these as authoritative graph entrypoints in runtime config:

- Uniswap v3 factory: `0x1F98431c8aD98523631AE4a59f267346ea31F984`
- Uniswap v4 PoolManager: `0x000000000004444c5dc75cB358380D2e3dE08A90`
- Curve MetaRegistry: `0xF98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC`
- Balancer Vault: `0xBA12222222228d8Ba445958a75a0704d566BF2C8`
- SushiSwap v2 factory: `0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac`
- Uniswap v2 factory: `0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f`

## 5) Ethereum high-signal seed pools

Use these first so graph quality ramps immediately:

- UniV3:
  - WETH/USDC 0.05%: `0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640`
  - USDC/USDT: `0x7858E59e0C01EA06Df3aF3D20aC7B0003275D4Bf`
  - WBTC/WETH 0.3%: `0xCBCd9626bC03E24f779434178A73a0B4bad62eD`
- Curve:
  - 3pool: `0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7`
  - stETH/ETH: `0xDC24316b9AE028F1497c275EB9192a3Ea0f67022`
  - TriCrypto2: `0xD51a44d3FaE010294C616388b506AcdA1bfAAE46`
  - crvUSD/USDC: `0x4dece678ceceb27446b35c672dc7d61f30bad69e`
  - frxETH/ETH: `0xa1f8a6807c402e4a15ef4eba36528a3fed24e577`
- Balancer:
  - USDC/WETH pool id: `0x96646936b91d6b9d7d0c47c496afbf3d6ec7b6f8000200000000000000000019`

---


## 5.1 Exactly where to put seed pools (important)

When this guide says “use seed pools first,” it means:

1. **For `univ2_like` / `univ3_like` venues**
   - Seed pools must exist in the on-disk pool inventory file:
     - `data/<chain>/<venue>/pools.jsonl`
   - Runtime loads this file during startup. If the file is missing/empty for a configured UniV2/UniV3 venue, startup fails by design (fail closed).

2. **For Curve/Balancer seed routes**
   - Seed routes belong in registry pools:
     - `config/registry.json` under `chains.<chain>.pools.curve` / `chains.<chain>.pools.balancer`.

So: **UniV2/UniV3 seeds go to `data/.../pools.jsonl`; Curve/Balancer seeds go to registry `pools` arrays.**

## 5.2 Exactly how to apply seed pools (copy/paste)

### A) Add/verify venue in `ops/inputs.yaml`

Ensure the venue exists under your chain’s `venues:` list with the correct `name`, `kind`, and addresses.

Example (Ethereum UniV3 already present in repo):

```yaml
- name: uniswap_v3
  kind: univ3_like
  factory: '0x1F98431c8aD98523631AE4a59f267346ea31F984'
  router: '0xE592427A0AEce92De3Edee1F18E0157C05861564'
  quoter: '0x61fFE014bA17989E743c5F6cB21bF9697530B21e'
  fee_tiers: [500, 3000, 10000]
```

### B) Build UniV2/UniV3 inventory with ingest (recommended)

Set chain env (example: Ethereum):

```bash
export ALCHEMY_KEY="YOUR_ALCHEMY_KEY"
```

Run ingest per Uni venue:

```bash
cargo run --bin ingest -- --chain ethereum --venue uniswap_v3 --from-block 12369621 --to-block 22000000 --chunk-size 5000 --query-timeout-secs 30
cargo run --bin ingest -- --chain ethereum --venue uniswap_v2 --from-block 10000835 --to-block 22000000 --chunk-size 5000 --query-timeout-secs 30
cargo run --bin ingest -- --chain ethereum --venue sushiswap_v2 --from-block 10794229 --to-block 22000000 --chunk-size 5000 --query-timeout-secs 30
```

This writes/merges records into:

```text
data/ethereum/uniswap_v3/pools.jsonl
data/ethereum/uniswap_v2/pools.jsonl
data/ethereum/sushiswap_v2/pools.jsonl
```

### C) Confirm seed pools are present in inventory

Use `jq` to verify specific pool addresses are present:

```bash
jq -r '.pool' data/ethereum/uniswap_v3/pools.jsonl | rg -i '88e6a0c2ddd26feeb64f039a2c41296fcb3f5640|7858e59e0c01ea06df3af3d20ac7b0003275d4bf|cbcd9626bc03e24f779434178a73a0b4bad62ed'
```

If no output appears, ingest range is too narrow or wrong venue/factory is configured.

### D) Add Curve/Balancer seed routes in registry

Edit `config/registry.json` and add route objects under:

- `chains.ethereum.pools.curve[]`
- `chains.ethereum.pools.balancer[]`

Use the same object shape as `config/registry.example.json`.

### E) Run validation + smoke

```bash
./scripts/check_deploy_env.sh
./scripts/ci/check_placeholder_endpoints.sh
cargo test --test integration_smoke integration_smoke_per_chain -- --nocapture
```

If these pass, your seed pool data is in the right place and consumable by runtime.

## 6) Long-tail token policy (profit preserving)

Do **not** hardcode huge long-tail lists.

- Build long-tail coverage from pools that meet liquidity/volume thresholds.
- UniV3: discover via `factory.getPool(tokenA, tokenB, fee)` anchored to base tokens.
- Curve: discover via MetaRegistry `find_pool_for_coins`.
- Balancer: enumerate vault pools, keep only swap-enabled pools above minimum depth.

Why: static long-tail lists rot quickly and produce stale/false-positive edges.

---

## 7) What to expect after onboarding

- Scanner edge count rises first, then stabilizes as filters/TTL settle.
- Initial candidate count may increase; realized inclusion should increase only if execution quality remains high.
- If revert rate rises after onboarding, suspect ABI mismatch, wrong decimals, or stale pool metadata.

Track these during rollout:
- `scan_ms_p95`
- `edges_updated`
- `candidate_count`
- `revert_rate_1h`
- `net_profit_wei/day`

If net profit does not improve, disable venue and investigate before re-enable.
