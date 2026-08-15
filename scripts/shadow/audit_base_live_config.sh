#!/usr/bin/env bash
# audit_base_live_config.sh — fail-fast checks before live Base trading.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

fail() { echo "FAIL: $*" >&2; exit 1; }
warn() { echo "WARN: $*"; }
ok()   { echo "OK:   $*"; }

_preserve_shadow="${SHADOW_MODE-}"
if [ -f .env ]; then
  set -a
  # shellcheck source=/dev/null
  source .env
  set +a
fi
if [ -n "$_preserve_shadow" ]; then
  export SHADOW_MODE="$_preserve_shadow"
fi

# Audit the endpoint the RUNTIME uses. This previously hard-required
# ALCHEMY_KEY and built an Alchemy URL, so an exhausted Alchemy quota failed the
# audit — and blocked the run — even though Base runs on BASE_RPC_URLS (drpc)
# and never calls Alchemy. Same bug as validate_base_addresses.sh had.
RPC="${BASE_RPC_URL:-}"
if [ -z "$RPC" ]; then
  RPC="${BASE_RPC_URLS%%,*}"
fi
if [ -z "$RPC" ] && [ -n "${ALCHEMY_KEY:-}" ]; then
  RPC="https://base-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"
fi
[ -z "$RPC" ] && fail "no Base RPC configured (set BASE_RPC_URLS)"
# Expand any ${VAR} placeholders carried in from config.
RPC="$(eval "printf '%s' \"$RPC\"")"

EXEC="${BASE_EXECUTOR_ADDRESS:-0x9445f7d3E1aA38bC9A9B373dc905D9fde7B9B852}"
OWNER_CFG="${BASE_EXECUTOR_OWNER:-0xD7A4D612E572B5877b0E2fC7cE8683e36360f04c}"
ROUTER_CFG="${BASE_BATCH_ROUTER_ADDRESS:-0x46C6d9003FB9FBFE29d60ae6feF869F7CAe6f499}"

echo "=== Base live config audit ==="

# 1) Executor ownership chain (clone → router → operator wallet)
CLONE_OWNER="$(cast call "$EXEC" "owner()(address)" --rpc-url "$RPC")"
[ "$(echo "$CLONE_OWNER" | tr '[:upper:]' '[:lower:]')" = "$(echo "$ROUTER_CFG" | tr '[:upper:]' '[:lower:]')" ] \
  || fail "executor.owner()=$CLONE_OWNER expected BatchRouter $ROUTER_CFG"

ROUTER_OWNER="$(cast call "$CLONE_OWNER" "owner()(address)" --rpc-url "$RPC")"
[ "$(echo "$ROUTER_OWNER" | tr '[:upper:]' '[:lower:]')" = "$(echo "$OWNER_CFG" | tr '[:upper:]' '[:lower:]')" ] \
  || fail "BatchRouter.owner()=$ROUTER_OWNER does not match BASE_EXECUTOR_OWNER=$OWNER_CFG"
ok "ownership chain: executor → $CLONE_OWNER → operator $ROUTER_OWNER"

# 2) L2 relay misconfiguration (Flashbots on Base) — live mode only
if [ "${SHADOW_MODE:-false}" != "true" ] && [ "${SHADOW_MODE:-0}" != "1" ]; then
  RELAYS="${BASE_PRIVATE_RELAY_URLS:-}${PRIVATE_RELAY_URLS:-}${PRIVATE_RELAY_URL:-}"
  if echo "$RELAYS" | grep -Eiq 'flashbots|titanbuilder|beaverbuild|builder0x69'; then
    fail "Ethereum bundle relay in env while chain=base live; unset PRIVATE_RELAY_URL(S), set BASE_PRIVATE_RELAY_URLS to sequencer RPC"
  fi
  ok "no Ethereum bundle relays in Base live env"
else
  if echo "${PRIVATE_RELAY_URLS:-}${PRIVATE_RELAY_URL:-}" | grep -Eiq 'flashbots|titanbuilder|beaverbuild'; then
    warn "PRIVATE_RELAY_URL(S) points at Ethereum builders — remove before live (ops relays win for shadow scans)"
  fi
fi

# 3) Shadow vs live mode
if [ "${SHADOW_MODE:-false}" = "true" ] || [ "${SHADOW_MODE:-0}" = "1" ]; then
  warn "SHADOW_MODE is enabled — no live broadcasts"
else
  ok "SHADOW_MODE disabled (live mode)"
  BAL="$(cast balance "${ROUTER_OWNER}" --rpc-url "$RPC" 2>/dev/null || echo 0)"
  MIN_WEI=40000000000000000
  if [ "$(printf '%s\n' "$BAL" "$MIN_WEI" | sort -n | head -1)" != "$MIN_WEI" ]; then
    fail "operator wallet balance ${BAL} wei < ${MIN_WEI} wei (~0.04 ETH) required for live gas"
  fi
  ok "operator wallet funded for live (${BAL} wei)"
fi

# 4) Ops executor pins
if grep -q "executor_address: '0x9445f7d3E1aA38bC9A9B373dc905D9fde7B9B852'" ops/inputs.yaml; then
  ok "ops/inputs.yaml Base executor_address matches deployment"
else
  warn "ops/inputs.yaml Base executor_address may differ from $EXEC"
fi

echo "=== audit passed ==="
