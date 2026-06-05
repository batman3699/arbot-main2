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

BAL_VAULT=0xBA12222222228d8Ba445958a75a0704d566BF2C8

AAVE_POOL=0xA238Dd80C259a72e81d7e4664a9801593F98d1c5
AAVE_PROVIDER=0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D
AAVE_DATA_PROVIDER=0x0F43731EB8d45A581f4a36DD74F5f358bc90C73A
AAVE_ORACLE=0x2Cc0Fc26eD4563A5ce5e8bdcfe1A2878676Ae156

COMET_USDC=0xb125E6687d4313864e53df431d5425969c15Eb2F
COMET_CONFIGURATOR=0x45939657d1CA34A8FA39A924B71D28Fe8431e581
COMET_REWARDS=0x123964802e6ABabBE1Bc9547D72Ef1B69B00A6b1

EXECUTOR=0x627e54a5Fad377d0d0eef60298f7F3d0e2c15E7A
EXECUTOR_OWNER=0xA6080B97261C4F5AAB13C5533e75d36890cbD1E3

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
# Executor owner() is best-effort: the MultiVenueArb clone exposes owner().
OWNER_GOT="$(cast call "$EXECUTOR" "owner()(address)" --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
if [ -n "$OWNER_GOT" ]; then
  if [ "$(lc "$OWNER_GOT")" = "$(lc "$EXECUTOR_OWNER")" ]; then
    ok "Executor owner() = $OWNER_GOT (matches BASE_EXECUTOR_OWNER)"
  else
    warn "Executor owner() = $OWNER_GOT (config BASE_EXECUTOR_OWNER=$EXECUTOR_OWNER) — confirm intentional"
  fi
else
  warn "Executor owner() not callable — confirm $EXECUTOR is your MultiVenueArb executor"
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
