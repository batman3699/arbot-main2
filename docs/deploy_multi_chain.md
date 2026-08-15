# Multi-chain executor deployment

This guide deploys a distinct executor per chain and records addresses back into `ops/inputs.yaml`.

## Prereqs

- Foundry installed (`forge --version`)
- `ops/inputs.yaml` created locally (do not commit) with per-chain `executor_address` placeholders
- RPC URLs exported per chain

## Environment

Set the deployer key once (global):

```bash
export PRIVATE_KEY=<deployer_private_key>
```

Provide per-chain params using the chain prefix (or set `ENV_PREFIX`/`CHAIN` to derive it):

```bash
# Example for Arbitrum
export CHAIN=arbitrum
export ARB_RPC_URL=https://arbitrum-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
export ARB_BAL_VAULT=<BALANCER_VAULT>                 # canonical (preferred)
export ARB_BALANCER_VAULT=<BALANCER_VAULT>            # legacy fallback
export ARB_UNIV3_ROUTER=<UNIV3_ROUTER_OR_SWAPROUTER02>
export ARB_AAVE_POOL=<AAVE_POOL_OR_ZERO>
export ARB_PERMIT2_ADDRESS=<PERMIT2>                  # canonical (preferred)
export ARB_PERMIT2=<PERMIT2>                          # legacy fallback
```

If a component is not present on a chain, set it to the zero address. The preflight script derives `REQUIRE_BALANCER` / `REQUIRE_AAVE` from `ops/inputs.yaml`, and `Deploy.s.sol` enforces those flags to avoid chain-ID drift.

### Preflight check (run before every deploy)

Use the one-command env validator before `forge script`:

```bash
bash scripts/check_deploy_env.sh
```

The script checks the exact keys consumed by `script/Deploy.s.sol`:

- Prefix derivation: `ENV_PREFIX` (preferred) or `CHAIN`
- Broadcast/deploy: `PRIVATE_KEY`, `<PREFIX>_EXECUTOR_OWNER` (or `EXECUTOR_OWNER` fallback)
- Core integrations: `<PREFIX>_UNIV3_ROUTER` (or `<PREFIX>_SWAPROUTER02` fallback)
- Canonical Balancer key (preferred): `<PREFIX>_BAL_VAULT` with legacy fallback `<PREFIX>_BALANCER_VAULT`
- Canonical Permit2 key (preferred): `<PREFIX>_PERMIT2_ADDRESS` with legacy fallback `<PREFIX>_PERMIT2`
- Balancer/Aave requirements are derived from `ops/inputs.yaml` flashloan kinds (`balancer*` / `aave*`) for the selected chain.

Unprefixed global fallbacks are also accepted (`BAL_VAULT`/`BALANCER_VAULT`, `PERMIT2_ADDRESS`/`PERMIT2`, etc.).

## Deploy commands (one per chain)

Use a deterministic CREATE2 salt per chain via `EXECUTOR_SALT` (or allow the script to derive one from `block.chainid`).

```bash
# Ethereum
export CHAIN=ethereum
export ETH_RPC_URL=https://ethereum-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
export ETH_BAL_VAULT=<BALANCER_VAULT>
export ETH_UNIV3_ROUTER=<UNIV3_ROUTER_OR_SWAPROUTER02>
export ETH_AAVE_POOL=<AAVE_POOL_OR_ZERO>
export ETH_PERMIT2_ADDRESS=<PERMIT2>
forge script script/Deploy.s.sol:Deploy --rpc-url "$ETH_RPC_URL" --broadcast

# Arbitrum
export CHAIN=arbitrum
export ARB_RPC_URL=https://arbitrum-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
export ARB_BAL_VAULT=<BALANCER_VAULT>                 # canonical (preferred)
export ARB_BALANCER_VAULT=<BALANCER_VAULT>            # legacy fallback
export ARB_UNIV3_ROUTER=<UNIV3_ROUTER_OR_SWAPROUTER02>
export ARB_AAVE_POOL=<AAVE_POOL_OR_ZERO>
export ARB_PERMIT2_ADDRESS=<PERMIT2>                  # canonical (preferred)
export ARB_PERMIT2=<PERMIT2>                          # legacy fallback
forge script script/Deploy.s.sol:Deploy --rpc-url "$ARB_RPC_URL" --broadcast

# Base
export CHAIN=base
export BASE_RPC_URL=https://base-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
export BASE_BAL_VAULT=<BALANCER_VAULT_OR_ZERO>
export BASE_UNIV3_ROUTER=<UNIV3_ROUTER_OR_SWAPROUTER02>
export BASE_AAVE_POOL=<AAVE_POOL_OR_ZERO>
export BASE_PERMIT2_ADDRESS=<PERMIT2>
forge script script/Deploy.s.sol:Deploy --rpc-url "$BASE_RPC_URL" --broadcast

# Linea
export CHAIN=linea
export LINEA_RPC_URL=https://linea-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
export LINEA_BAL_VAULT=<BALANCER_VAULT_OR_ZERO>
export LINEA_UNIV3_ROUTER=<UNIV3_ROUTER_OR_SWAPROUTER02>
export LINEA_AAVE_POOL=<AAVE_POOL_OR_ZERO>
export LINEA_PERMIT2_ADDRESS=<PERMIT2>
forge script script/Deploy.s.sol:Deploy --rpc-url "$LINEA_RPC_URL" --broadcast

# Abstract
export CHAIN=abstract
export ABSTRACT_RPC_URL=https://abstract-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
export ABSTRACT_BAL_VAULT=<BALANCER_VAULT_OR_ZERO>
export ABSTRACT_UNIV3_ROUTER=<UNIV3_ROUTER_OR_SWAPROUTER02>
export ABSTRACT_AAVE_POOL=<AAVE_POOL_OR_ZERO>
export ABSTRACT_PERMIT2_ADDRESS=<PERMIT2>
forge script script/Deploy.s.sol:Deploy --rpc-url "$ABSTRACT_RPC_URL" --broadcast

# Ink
export CHAIN=ink
export INK_RPC_URL=https://ink-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}
export INK_BAL_VAULT=<BALANCER_VAULT_OR_ZERO>
export INK_UNIV3_ROUTER=<UNIV3_ROUTER_OR_SWAPROUTER02>
export INK_AAVE_POOL=<AAVE_POOL_OR_ZERO>
export INK_PERMIT2_ADDRESS=<PERMIT2>
forge script script/Deploy.s.sol:Deploy --rpc-url "$INK_RPC_URL" --broadcast
```

## Record executor addresses

After each deploy, copy the emitted `MultiVenue executor clone deployed` address into `ops/inputs.yaml`:

```yaml
chains:
  - chain_name: arbitrum
    executor_address: <EXECUTOR_ADDRESS_FROM_DEPLOY>
```

Repeat for each chain so `CHAIN_LIST=arbitrum,base,linea,abstract,ink,ethereum` can run with distinct executors.
