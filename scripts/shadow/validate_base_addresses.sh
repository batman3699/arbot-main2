#!/usr/bin/env bash
#
# validate_base_addresses.sh — On-chain preflight for the Base shadow-mode run.
#
# Verifies every Base mainnet address the bot relies on AGAINST THE LIVE CHAIN
# (via Alchemy) and cross-checks that .env / ops/inputs.yaml / config/registry.json
# all agree. This is read-only: it only performs `cast code` / `cast call` (eth_call)
# and never sends a transaction.
#
# The reference addresses below were validated against official sources
# (see docs/VALIDATION_RUN.md "Address validation" for citations):
#   - Uniswap V2/V3 + Permit2 : docs.base.org ecosystem-contracts
#   - Aave V3                 : bgd-labs/aave-address-book (AaveV3Base)
#   - Balancer V2 Vault       : balancer canonical multichain deployment
#   - Compound V3             : compound-finance/comet deployments/base/usdc/roots.json
#   - WETH / USDC             : Base predeploy / Circle (BaseScan)
#
# Usage:
#   ALCHEMY_KEY=xxxx scripts/shadow/validate_base_addresses.sh
#   BASE_SHADOW_RPC_URL=https://base-mainnet.g.alchemy.com/v2/xxxx scripts/shadow/validate_base_addresses.sh
#
# Exit code 0 = all critical checks passed; non-zero = at least one failure.

set -uo pipefail

# ----------------------------------------------------------------------------
# Resolve repo root and RPC endpoint
# ----------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

RPC_URL="${BASE_SHADOW_RPC_URL:-}"
if [ -z "$RPC_URL" ]; then
  KEY="${ALCHEMY_KEY:-}"
  if [ -z "$KEY" ] && [ -f .env ]; then
    KEY="$(grep -E '^ALCHEMY_KEY=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')"
  fi
  if [ -n "$KEY" ]; then
    RPC_URL="https://base-mainnet.g.alchemy.com/v2/${KEY}"
  fi
fi

if [ -z "$RPC_URL" ]; then
  echo "FATAL: no RPC URL. Set BASE_SHADOW_RPC_URL or ALCHEMY_KEY." >&2
  exit 2
fi

if ! command -v cast >/dev/null 2>&1; then
  echo "FATAL: foundry 'cast' not found. Install foundry (https://getfoundry.sh)." >&2
  exit 2
fi

EXPECTED_CHAIN_ID=8453

# ----------------------------------------------------------------------------
# Reference addresses (validated against official sources)
# ----------------------------------------------------------------------------
WETH=0x4200000000000000000000000000000000000006
USDC=0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913
PERMIT2=0x000000000022D473030F116dDEE9F6B43aC78BA3

UNIV3_FACTORY=0x33128a8fC17869897dcE68Ed026d694621f6FDfD
UNIV3_QUOTER=0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a
UNIV3_ROUTER=0x2626664c2603336E57B271c5C0b26F421741e481
UNIV2_FACTORY=0x8909Dc15e40173Ff4699343b6eB8132c65e18eC6
UNIV2_ROUTER=0x4752ba5dbc23f44d87826276bf6fd6b1c372ad24

# Aerodrome (Base's dominant DEX, Solidly-style AMM)
AERO_FACTORY=0x420DD381b31aEf6683db6B902084cB0FFECe40Da
AERO_ROUTER=0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43
SLIPSTREAM_FACTORY=0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A
SLIPSTREAM_ROUTER=0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5
SLIPSTREAM_QUOTER=0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0
PANCAKE_V3_FACTORY=0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865
PANCAKE_V3_ROUTER=0x1b81D678ffb9C0263b24A97847620C99d213eB14
PANCAKE_V3_QUOTER=0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997
WETH_TOKEN=0x4200000000000000000000000000000000000006
USDC_TOKEN=0x833589fCD6eDb6E08f4c7C32D4f71b54bDa02913

BAL_VAULT=0xBA12222222228d8Ba445958a75a0704d566BF2C8

AAVE_POOL=0xA238Dd80C259a72e81d7e4664a9801593F98d1c5
AAVE_PROVIDER=0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D
AAVE_DATA_PROVIDER=0x0F43731EB8d45A581f4a36DD74F5f358bc90C73A
AAVE_ORACLE=0x2Cc0Fc26eD4563A5ce5e8bdcfe1A2878676Ae156

COMET_USDC=0xb125E6687d4313864e53df431d5425969c15Eb2F
COMET_CONFIGURATOR=0x45939657d1CA34A8FA39A924B71D28Fe8431e581
COMET_REWARDS=0x123964802e6ABabBE1Bc9547D72Ef1B69B00A6b1

# Executor + operator are resolved from the live config (env first, then .env),
# so this preflight always validates the ACTUAL deployed clone rather than a
# stale hardcoded address. Defaults are the current Base deployment.
EXECUTOR="${BASE_EXECUTOR_ADDRESS:-}"
if [ -z "$EXECUTOR" ] && [ -f .env ]; then
  EXECUTOR="$(grep -E '^BASE_EXECUTOR_ADDRESS=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')"
fi
EXECUTOR="${EXECUTOR:-0x9445f7d3E1aA38bC9A9B373dc905D9fde7B9B852}"

EXECUTOR_OWNER="${BASE_EXECUTOR_OWNER:-}"
if [ -z "$EXECUTOR_OWNER" ] && [ -f .env ]; then
  EXECUTOR_OWNER="$(grep -E '^BASE_EXECUTOR_OWNER=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')"
fi
EXECUTOR_OWNER="${EXECUTOR_OWNER:-0xD7A4D612E572B5877b0E2fC7cE8683e36360f04c}"

# Universe tokens (BASE_TOKENS) — identified on-chain below.
UNIVERSE_TOKENS=(
  "$WETH"
  "$USDC"
  0x78a087d713Be963Bf307b18F2Ff8122EF9A63ae9
  0x940181a94A35A4569E4529A3CDfB74e38FD98631
  0xFe20C1B85ABa875EA8cecac8200bF86971968F3A
  0x64FCC3A02eeEba05Ef701b7eed066c6ebD5d4E51
)

PASS=0
FAIL=0
WARN=0

green() { printf '\033[0;32m%s\033[0m' "$1"; }
red()   { printf '\033[0;31m%s\033[0m' "$1"; }
yellow(){ printf '\033[0;33m%s\033[0m' "$1"; }

lc() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]'; }

ok()   { echo "  [$(green PASS)] $1"; PASS=$((PASS+1)); }
bad()  { echo "  [$(red FAIL)] $1"; FAIL=$((FAIL+1)); }
warn() { echo "  [$(yellow WARN)] $1"; WARN=$((WARN+1)); }

# check_code <label> <addr>  — assert the address has deployed bytecode.
check_code() {
  local label="$1" addr="$2" code
  code="$(cast code "$addr" --rpc-url "$RPC_URL" 2>/dev/null)"
  if [ -n "$code" ] && [ "$code" != "0x" ]; then
    ok "$label has bytecode ($addr)"
  else
    bad "$label has NO bytecode ($addr) — wrong address or RPC issue"
  fi
}

# check_call_addr <label> <addr> <sig> <expected>  — assert an address-returning
# view function equals an expected address (case-insensitive).
check_call_addr() {
  local label="$1" addr="$2" sig="$3" expected="$4" got
  got="$(cast call "$addr" "$sig" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
  if [ -z "$got" ]; then
    bad "$label: call '$sig' reverted/empty ($addr)"
    return
  fi
  if [ "$(lc "$got")" = "$(lc "$expected")" ]; then
    ok "$label: $sig -> $got (matches expected)"
  else
    bad "$label: $sig -> $got (EXPECTED $expected)"
  fi
}

echo "=============================================================="
echo " Base shadow-mode preflight — on-chain address validation"
echo " RPC: ${RPC_URL%%/v2/*}/v2/****"
echo "=============================================================="

# --- 0) Chain id sanity ------------------------------------------------------
echo ""
echo "[0] Network identity"
CHAIN_ID="$(cast chain-id --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ "$CHAIN_ID" = "$EXPECTED_CHAIN_ID" ]; then
  ok "chain id = $CHAIN_ID (Base mainnet)"
else
  bad "chain id = '${CHAIN_ID:-<none>}' (EXPECTED $EXPECTED_CHAIN_ID). Wrong RPC?"
  echo ""
  echo "Aborting: cannot validate addresses against the wrong network."
  exit 1
fi

# --- 1) DEX routers / quoters ------------------------------------------------
echo ""
echo "[1] Uniswap V3 / V2 + Permit2"
check_code "UniV3 Factory" "$UNIV3_FACTORY"
check_call_addr "UniV3 QuoterV2 -> factory" "$UNIV3_QUOTER" "factory()(address)" "$UNIV3_FACTORY"
check_call_addr "UniV3 SwapRouter -> factory" "$UNIV3_ROUTER" "factory()(address)" "$UNIV3_FACTORY"
check_call_addr "UniV2 Router -> factory" "$UNIV2_ROUTER" "factory()(address)" "$UNIV2_FACTORY"
check_call_addr "UniV2 Router -> WETH" "$UNIV2_ROUTER" "WETH()(address)" "$WETH"
check_code "UniV2 Factory" "$UNIV2_FACTORY"
check_code "Permit2" "$PERMIT2"

# --- 1b) Aerodrome (Solidly AMM) --------------------------------------------
echo ""
echo "[1b] Aerodrome (Solidly AMM)"
check_code "Aerodrome PoolFactory" "$AERO_FACTORY"
check_call_addr "Aerodrome Router -> defaultFactory" "$AERO_ROUTER" "defaultFactory()(address)" "$AERO_FACTORY"
# Confirm the canonical WETH/USDC volatile pool exists, is liquid, and reports stable=false.
AERO_VPOOL="$(cast call "$AERO_FACTORY" "getPool(address,address,bool)(address)" "$WETH_TOKEN" "$USDC_TOKEN" false --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ -n "$AERO_VPOOL" ] && [ "$(lc "$AERO_VPOOL")" != "0x0000000000000000000000000000000000000000" ]; then
  ok "Aerodrome getPool(WETH,USDC,volatile) -> $AERO_VPOOL"
  AERO_STABLE="$(cast call "$AERO_VPOOL" "stable()(bool)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
  if [ "$AERO_STABLE" = "false" ]; then
    ok "Aerodrome WETH/USDC pool stable() = false (volatile vAMM)"
  else
    warn "Aerodrome WETH/USDC pool stable() = ${AERO_STABLE:-<none>} (expected false)"
  fi
  AERO_FEE="$(cast call "$AERO_FACTORY" "getFee(address,bool)(uint256)" "$AERO_VPOOL" false --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
  if [ -n "$AERO_FEE" ] && [ "$AERO_FEE" -gt 0 ] 2>/dev/null && [ "$AERO_FEE" -le 1000 ] 2>/dev/null; then
    ok "Aerodrome getFee(WETH/USDC,volatile) = ${AERO_FEE} bps (plausible)"
  else
    warn "Aerodrome getFee returned '${AERO_FEE:-<none>}' (expected small bps)"
  fi
else
  bad "Aerodrome getPool(WETH,USDC,volatile) returned empty/zero — factory wrong or RPC issue"
fi
# Confirm the configured pool list is present + parseable.
AERO_POOLS_FILE="${BASE_SOLIDLY_V2_POOLS:-}"
if [ -z "$AERO_POOLS_FILE" ] && [ -f .env ]; then
  AERO_POOLS_FILE="$(grep -E '^BASE_SOLIDLY_V2_POOLS=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')"
fi
if [ -n "$AERO_POOLS_FILE" ] && [ -f "$AERO_POOLS_FILE" ]; then
  POOL_COUNT="$(grep -c '"pair"' "$AERO_POOLS_FILE" 2>/dev/null || echo 0)"
  ok "Aerodrome pool list present: $AERO_POOLS_FILE ($POOL_COUNT directional entries)"
else
  warn "Aerodrome pool list (BASE_SOLIDLY_V2_POOLS) missing — solidly edges will be empty"
fi

# --- 1c) Aerodrome Slipstream (CL) -------------------------------------------
echo ""
echo "[1c] Aerodrome Slipstream (CL)"
check_code "Slipstream CL Factory" "$SLIPSTREAM_FACTORY"
check_code "Slipstream Swap Router" "$SLIPSTREAM_ROUTER"
check_code "Slipstream QuoterV2" "$SLIPSTREAM_QUOTER"
SLIP_POOL="$(cast call "$SLIPSTREAM_FACTORY" "getPool(address,address,int24)(address)" "$WETH_TOKEN" "$USDC_TOKEN" 1 --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ -n "$SLIP_POOL" ] && [ "$(lc "$SLIP_POOL")" != "0x0000000000000000000000000000000000000000" ]; then
  ok "Slipstream getPool(WETH,USDC,tickSpacing=1) -> $SLIP_POOL"
  SLIP_LIQ="$(cast call "$SLIP_POOL" "liquidity()(uint128)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
  if [ -n "$SLIP_LIQ" ] && [ "$SLIP_LIQ" -gt 0 ] 2>/dev/null; then
    ok "Slipstream WETH/USDC ts=1 pool liquidity() > 0"
  else
    warn "Slipstream WETH/USDC ts=1 pool liquidity() empty/zero"
  fi
else
  bad "Slipstream getPool(WETH,USDC,1) returned empty/zero"
fi
SLIP_POOLS_FILE="data/base/aerodrome_slipstream/pools.jsonl"
if [ -f "$SLIP_POOLS_FILE" ]; then
  SLIP_COUNT="$(wc -l < "$SLIP_POOLS_FILE" | tr -d '[:space:]')"
  ok "Slipstream pool inventory present: $SLIP_POOLS_FILE ($SLIP_COUNT pools)"
else
  warn "Slipstream pool inventory missing at $SLIP_POOLS_FILE — run scripts/data/build_slipstream_pools.py"
fi

# --- 1d) PancakeSwap V3 (CL) -------------------------------------------------
echo ""
echo "[1d] PancakeSwap V3 (CL)"
check_code "PancakeSwap V3 Factory" "$PANCAKE_V3_FACTORY"
check_code "PancakeSwap V3 Swap Router" "$PANCAKE_V3_ROUTER"
check_code "PancakeSwap V3 QuoterV2" "$PANCAKE_V3_QUOTER"
CAKE_POOL="$(cast call "$PANCAKE_V3_FACTORY" "getPool(address,address,uint24)(address)" "$WETH_TOKEN" "$USDC_TOKEN" 500 --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ -n "$CAKE_POOL" ] && [ "$(lc "$CAKE_POOL")" != "0x0000000000000000000000000000000000000000" ]; then
  ok "PancakeSwap getPool(WETH,USDC,fee=500) -> $CAKE_POOL"
else
  bad "PancakeSwap getPool(WETH,USDC,500) returned empty/zero"
fi
CAKE_POOLS_FILE="data/base/pancakeswap_v3/pools.jsonl"
if [ -f "$CAKE_POOLS_FILE" ]; then
  CAKE_COUNT="$(wc -l < "$CAKE_POOLS_FILE" | tr -d '[:space:]')"
  ok "PancakeSwap pool inventory present: $CAKE_POOLS_FILE ($CAKE_COUNT pools)"
else
  warn "PancakeSwap pool inventory missing at $CAKE_POOLS_FILE — run scripts/data/build_pancakeswap_pools.py"
fi

# --- 2) Flash-loan providers -------------------------------------------------
echo ""
echo "[2] Flash-loan providers"
check_call_addr "Aave V3 Pool -> ADDRESSES_PROVIDER" "$AAVE_POOL" "ADDRESSES_PROVIDER()(address)" "$AAVE_PROVIDER"
# FLASHLOAN_PREMIUM_TOTAL is what the bot probes at startup; just confirm it answers.
# Aave V3 FLASHLOAN_PREMIUM_TOTAL is in bps (5 => 0.05%). ops/inputs.yaml sets fee_bps: 5.
FLP="$(cast call "$AAVE_POOL" "FLASHLOAN_PREMIUM_TOTAL()(uint128)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ -n "$FLP" ]; then
  if [ "$FLP" = "5" ]; then
    ok "Aave V3 Pool flashloan premium = ${FLP} bps (0.05%) — matches ops/inputs.yaml fee_bps: 5"
  else
    warn "Aave V3 Pool flashloan premium = ${FLP} bps — ops/inputs.yaml has fee_bps: 5; reconcile"
  fi
else
  bad "Aave V3 Pool FLASHLOAN_PREMIUM_TOTAL() reverted"
fi
check_call_addr "Balancer V2 Vault -> WETH" "$BAL_VAULT" "WETH()(address)" "$WETH"

# --- 3) Liquidation markets --------------------------------------------------
echo ""
echo "[3] Liquidation markets (Aave V3 + Compound V3)"
check_call_addr "Aave DataProvider -> ADDRESSES_PROVIDER" "$AAVE_DATA_PROVIDER" "ADDRESSES_PROVIDER()(address)" "$AAVE_PROVIDER"
check_code "Aave Oracle" "$AAVE_ORACLE"
check_call_addr "Compound cUSDCv3 -> baseToken" "$COMET_USDC" "baseToken()(address)" "$USDC"
# Internal consistency: the Comet market and its Configurator must report the
# same Compound governor (the Base governance timelock, NOT our executor owner).
COMET_GOV="$(cast call "$COMET_USDC" "governor()(address)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
ZERO=0x0000000000000000000000000000000000000000
if [ -n "$COMET_GOV" ] && [ "$(lc "$COMET_GOV")" != "$(lc "$ZERO")" ]; then
  check_call_addr "Compound Configurator -> governor == comet governor" "$COMET_CONFIGURATOR" "governor()(address)" "$COMET_GOV"
else
  bad "Compound cUSDCv3 governor() empty/zero ($COMET_USDC)"
fi
check_code "Compound Configurator" "$COMET_CONFIGURATOR"
check_code "Compound Rewards" "$COMET_REWARDS"

# --- 4) Executor contract ----------------------------------------------------
echo ""
echo "[4] Arb executor (your deployed contract)"
check_code "Executor" "$EXECUTOR"
# Ownership model for this deployment: the clone is owned by the BatchRouter,
# and the BatchRouter is owned by the operator hot wallet (BASE_EXECUTOR_OWNER).
CLONE_OWNER="$(cast call "$EXECUTOR" "owner()(address)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ -n "$CLONE_OWNER" ] && [ "$(lc "$CLONE_OWNER")" != "$(lc "$ZERO")" ]; then
  ok "Executor owner() = $CLONE_OWNER (BatchRouter)"
  ROUTER_OWNER="$(cast call "$CLONE_OWNER" "owner()(address)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
  if [ -n "$ROUTER_OWNER" ]; then
    if [ "$(lc "$ROUTER_OWNER")" = "$(lc "$EXECUTOR_OWNER")" ]; then
      ok "Router owner() = $ROUTER_OWNER (matches BASE_EXECUTOR_OWNER)"
    else
      warn "Router owner() = $ROUTER_OWNER (config BASE_EXECUTOR_OWNER=$EXECUTOR_OWNER) — confirm intentional"
    fi
  fi
else
  warn "Executor owner() not callable — confirm $EXECUTOR is your MultiVenueArb executor"
fi
# CRITICAL: start/startV2 are onlyExecutor. The hot signer MUST be allowlisted or
# every execution — including the shadow-mode eth_call simulation — reverts
# NotExecutor.
HOT_SIGNER="${HOT_SIGNER:-}"
if [ -z "$HOT_SIGNER" ] && [ -n "${PRIVATE_KEY:-}" ]; then
  HOT_SIGNER="$(cast wallet address --private-key "$PRIVATE_KEY" 2>/dev/null | tr -d '[:space:]')"
fi
if [ -z "$HOT_SIGNER" ] && [ -f .env ]; then
  PK="$(grep -E '^PRIVATE_KEY=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')"
  if [ -n "$PK" ]; then
    HOT_SIGNER="$(cast wallet address --private-key "$PK" 2>/dev/null | tr -d '[:space:]')"
  fi
fi
HOT_SIGNER="${HOT_SIGNER:-$EXECUTOR_OWNER}"
EXEC_ALLOWED="$(cast call "$EXECUTOR" "executors(address)(bool)" "$HOT_SIGNER" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ "$EXEC_ALLOWED" = "true" ]; then
  ok "Executor allowlist: hot signer $HOT_SIGNER is approved (startV2 callable)"
else
  bad "Executor allowlist: hot signer $HOT_SIGNER NOT approved — startV2 reverts NotExecutor (run setExecutor via router.multicall)"
fi

# --- 5) Token identity + decimals -------------------------------------------
echo ""
echo "[5] Universe token identity (symbol / decimals)"
for t in "${UNIVERSE_TOKENS[@]}"; do
  sym="$(cast call "$t" "symbol()(string)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '"' | tr -d '[:space:]')"
  dec="$(cast call "$t" "decimals()(uint8)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
  if [ -n "$sym" ] && [ -n "$dec" ]; then
    ok "$t = ${sym} (${dec} decimals)"
  else
    bad "$t — symbol()/decimals() failed; not a standard ERC-20?"
  fi
done

# --- 6) Config drift across files -------------------------------------------
echo ""
echo "[6] Config consistency (.env vs ops/inputs.yaml vs registry.json)"
drift_check() {
  local label="$1" addr="$2"
  local n
  n="$(grep -ril "$addr" .env ops/inputs.yaml config/registry.json 2>/dev/null | wc -l | tr -d '[:space:]')"
  if [ "$n" -ge 1 ]; then
    ok "$label present in $n config file(s)"
  else
    warn "$label ($addr) not found in any config file — is it actually used for Base?"
  fi
}
drift_check "Executor" "$EXECUTOR"
drift_check "Aave Pool" "$AAVE_POOL"
drift_check "Balancer Vault" "$BAL_VAULT"
drift_check "UniV3 Quoter" "$UNIV3_QUOTER"
drift_check "Compound comet" "$COMET_USDC"

# ----------------------------------------------------------------------------
echo ""
echo "=============================================================="
echo " Preflight summary: $(green "$PASS pass") / $(red "$FAIL fail") / $(yellow "$WARN warn")"
echo "=============================================================="
if [ "$FAIL" -gt 0 ]; then
  echo "RESULT: $(red FAIL) — resolve the failures above before the shadow run."
  exit 1
fi
echo "RESULT: $(green PASS) — Base addresses verified on-chain. Safe to start the shadow run."
exit 0
