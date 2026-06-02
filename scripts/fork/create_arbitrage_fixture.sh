#!/usr/bin/env bash
set -euo pipefail

# Creates a forced mispricing on a UniswapV2-style pair inside an Anvil/Hardhat fork
# by transferring extra reserves from whale accounts, then calling sync().
#
# Defaults mirror the WETH/USDC mainnet pool so the script is copy-pasteable.
# Override any env var to target a different pair.
# Required for non-default usage:
#   FORK_RPC_URL          - RPC URL of the fork (default http://127.0.0.1:8545)
#   PAIR_ADDRESS          - target pair contract address (default UniswapV2 WETH/USDC)
#   TOKEN_IN              - token0 address (default WETH)
#   TOKEN_OUT             - token1 address (default USDC)
#   TOKEN_IN_WHALE        - rich holder of token0 (default auto-selected)
#   TOKEN_OUT_WHALE       - rich holder of token1 (default auto-selected)
# Behavior:
#   If no listed whale can satisfy TOKEN_*_AMOUNT_WEI, the script auto-falls back
#   to the richest listed whale and scales amount down to the max available balance.
#   If no positive WETH whale is found on Ethereum defaults, a synthetic WETH whale
#   is created on the fork via WETH9 deposit() to keep fixture generation deterministic.
# Optional overrides:
#   TOKEN_IN_AMOUNT_WEI   - amount of token0 to add to reserves (default 1e21)
#   TOKEN_OUT_AMOUNT_WEI  - amount of token1 to add to reserves (default 1e24)
#   PRIVATE_KEY           - signer key for fixture txs (preferred for deterministic sender identity)
#   SIGNER_ADDRESS        - signer wallet address that matches PRIVATE_KEY (or WALLET_ADDRESS)
#   CAST_GAS_LIMIT        - gas limit override for cast send (default 500000)

FORK_RPC_URL=${FORK_RPC_URL:-http://127.0.0.1:8545}
PAIR_ADDRESS=${PAIR_ADDRESS:-0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc}
TOKEN_IN=${TOKEN_IN:-0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2}
TOKEN_OUT=${TOKEN_OUT:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}
TOKEN_IN_WHALE=${TOKEN_IN_WHALE:-}
TOKEN_OUT_WHALE=${TOKEN_OUT_WHALE:-}

IN_AMOUNT=${TOKEN_IN_AMOUNT_WEI:-1000000000000000000000}
OUT_AMOUNT=${TOKEN_OUT_AMOUNT_WEI:-1000000000000000000000000}

PRIVATE_KEY_VALUE="${PRIVATE_KEY:-${ETH_PRIVATE_KEY:-}}"
SIGNER_ADDRESS_VALUE="${SIGNER_ADDRESS:-${WALLET_ADDRESS:-${ETH_EXECUTOR_OWNER:-}}}"
CAST_GAS_LIMIT=${CAST_GAS_LIMIT:-500000}
CAST_TX_ARGS=(--gas-limit "$CAST_GAS_LIMIT")
CAST_CHAIN_OVERRIDE=${CAST_CHAIN_OVERRIDE:-mainnet}
CAST_ISOLATED_HOME=${CAST_ISOLATED_HOME:-/tmp/arbot-cast-home}
CAST_BIN=""
PYTHON_BIN=""
USE_PRIVATE_KEY=0
LAST_BALANCE_ERROR=""
WETH9_MAINNET=0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2
SYNTHETIC_WETH_WHALE=${SYNTHETIC_WETH_WHALE:-0x1000000000000000000000000000000000000001}
WHALE_CANDIDATES=(
  0x28C6c06298d514Db089934071355E5743bf21d60
  0x21a31Ee1afC51d94C2Efccaa2092aD1028285549
  0xDFd5293D8e347dFe59E90eFD55b2956a1343963d
  0x503828976D22510aad0201ac7EC88293211D23Da
  0x267be1c1d684f78cb4f6a176c4911b741e4ffdc0
)

cast_cmd() {
  # Arbot workflows often export chain selectors for the Rust runtime.
  # Foundry's cast binary also reads chain env vars and can reject values
  # like "ethereum" (expects "mainnet"). Isolate HOME to avoid inheriting
  # user-level Foundry config while preserving per-command --rpc-url behavior.
  local subcommand="$1"
  shift

  case "$subcommand" in
    call|send)
      env -i PATH="$PATH" HOME="$CAST_ISOLATED_HOME" USER="${USER:-}" LOGNAME="${LOGNAME:-}" \
        HTTP_PROXY="${HTTP_PROXY:-}" HTTPS_PROXY="${HTTPS_PROXY:-}" NO_PROXY="${NO_PROXY:-}" \
        SSL_CERT_FILE="${SSL_CERT_FILE:-}" SSL_CERT_DIR="${SSL_CERT_DIR:-}" \
        "$CAST_BIN" "$subcommand" --chain "$CAST_CHAIN_OVERRIDE" "$@"
      ;;
    *)
      env -i PATH="$PATH" HOME="$CAST_ISOLATED_HOME" USER="${USER:-}" LOGNAME="${LOGNAME:-}" \
        HTTP_PROXY="${HTTP_PROXY:-}" HTTPS_PROXY="${HTTPS_PROXY:-}" NO_PROXY="${NO_PROXY:-}" \
        SSL_CERT_FILE="${SSL_CERT_FILE:-}" SSL_CERT_DIR="${SSL_CERT_DIR:-}" \
        "$CAST_BIN" "$subcommand" "$@"
      ;;
  esac
}

detect_cast() {
  mkdir -p "$CAST_ISOLATED_HOME"
  if ! CAST_BIN=$(type -P cast); then
    echo "cast is required (install Foundry: https://book.getfoundry.sh/getting-started/installation)."
    exit 1
  fi
}

detect_python() {
  if command -v python3 >/dev/null 2>&1; then
    PYTHON_BIN="python3"
  elif command -v python >/dev/null 2>&1; then
    PYTHON_BIN="python"
  else
    echo "Python is required (python3 preferred). Install it or set up a compatible runtime."
    exit 1
  fi
}

configure_signing() {
  if [[ -n "$PRIVATE_KEY_VALUE" ]]; then
    USE_PRIVATE_KEY=1
    if [[ -z "$SIGNER_ADDRESS_VALUE" ]]; then
      SIGNER_ADDRESS_VALUE=$(cast_cmd wallet address --private-key "$PRIVATE_KEY_VALUE")
    fi
    if [[ -z "$SIGNER_ADDRESS_VALUE" ]]; then
      echo "Unable to resolve signer address from PRIVATE_KEY. Set SIGNER_ADDRESS explicitly."
      exit 1
    fi
    CAST_SIGNING_ARGS=(--private-key "$PRIVATE_KEY_VALUE")
    echo "Using explicit PRIVATE_KEY + SIGNER_ADDRESS=$SIGNER_ADDRESS_VALUE for fixture txs."
  else
    USE_PRIVATE_KEY=0
    CAST_SIGNING_ARGS=(--unlocked)
    echo "Using --unlocked with auto-impersonated whales; start Anvil with --auto-impersonate."
  fi
}

fund_address() {
  local addr="$1"
  cast_cmd rpc --rpc-url "$FORK_RPC_URL" anvil_setBalance "$addr" 0x021E19E0C9BAB2400000 >/dev/null # 10,000 ETH
}

fund_and_impersonate() {
  local addr="$1"
  fund_address "$addr"
  cast_cmd rpc --rpc-url "$FORK_RPC_URL" anvil_impersonateAccount "$addr" >/dev/null
}

transfer_to_pair() {
  local from="$1" token="$2" amount="$3"
  cast_cmd send --rpc-url "$FORK_RPC_URL" "${CAST_SIGNING_ARGS[@]}" "${CAST_TX_ARGS[@]}" --from "$from" "$token" \
    "transfer(address,uint256)" "$PAIR_ADDRESS" "$amount"
}

transfer_whale_to_signer() {
  local whale="$1" token="$2" amount="$3"
  cast_cmd send --rpc-url "$FORK_RPC_URL" --unlocked "${CAST_TX_ARGS[@]}" --from "$whale" "$token" \
    "transfer(address,uint256)" "$SIGNER_ADDRESS_VALUE" "$amount"
}

get_holder_balance() {
  local holder="$1" token="$2"
  local call_output
  local balance
  local balance_int

  if ! call_output=$(cast_cmd call --rpc-url "$FORK_RPC_URL" "$token" "balanceOf(address)(uint256)" "$holder" 2>&1); then
    LAST_BALANCE_ERROR="cast call failed for holder=$holder token=$token rpc=$FORK_RPC_URL: $call_output"
    return 1
  fi

  balance="${call_output//$'\n'/ }"
  if [[ -z "$balance" || "$balance" == 0x0 || "$balance" == 0x ]]; then
    LAST_BALANCE_ERROR="empty/zero balance response for holder=$holder token=$token"
    return 1
  fi

  if ! balance_int=$(BALANCE_RAW="$balance" "$PYTHON_BIN" - <<'PY2'
import os
import re
import sys
from decimal import Decimal
raw = os.environ['BALANCE_RAW'].strip()
match = re.search(r"0x[0-9a-fA-F]+|[0-9]+(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?", raw)
if not match:
    sys.exit(2)
token = match.group(0)
if token.startswith("0x"):
    print(int(token, 16))
else:
    print(int(Decimal(token)))
PY2
  ); then
    local rc=$?
    if [[ $rc -eq 2 ]]; then
      LAST_BALANCE_ERROR="unparseable balance output for holder=$holder token=$token: '$balance'"
    else
      LAST_BALANCE_ERROR="balance parse failed for holder=$holder token=$token"
    fi
    return 1
  fi

  LAST_BALANCE_ERROR=""
  echo "$balance_int"
  return 0
}

has_sufficient_balance() {
  local holder="$1" token="$2" amount="$3"
  local balance_int

  if ! balance_int=$(get_holder_balance "$holder" "$token"); then
    return 1
  fi

  if [[ "$balance_int" =~ ^[0-9]+$ ]] && is_int_ge "$balance_int" "$amount"; then
    LAST_BALANCE_ERROR=""
    return 0
  fi

  LAST_BALANCE_ERROR="insufficient balance for holder=$holder token=$token"
  return 1
}

compare_ints() {
  local lhs="$1" rhs="$2" op="$3"

  if [[ ! "$lhs" =~ ^[0-9]+$ || ! "$rhs" =~ ^[0-9]+$ ]]; then
    return 1
  fi

  LHS_INT="$lhs" RHS_INT="$rhs" COMPARE_OP="$op" "$PYTHON_BIN" - <<'PY2' >/dev/null
import os
import sys

lhs = int(os.environ["LHS_INT"])
rhs = int(os.environ["RHS_INT"])
op = os.environ["COMPARE_OP"]

if op == "gt":
    sys.exit(0 if lhs > rhs else 1)
if op == "ge":
    sys.exit(0 if lhs >= rhs else 1)

sys.exit(2)
PY2
}

is_int_gt() {
  compare_ints "$1" "$2" "gt"
}

is_int_ge() {
  compare_ints "$1" "$2" "ge"
}


prepare_synthetic_weth_whale() {
  local token="$1" amount="$2"

  if [[ "${token,,}" != "${WETH9_MAINNET,,}" ]]; then
    return 1
  fi

  if [[ ! "$amount" =~ ^[0-9]+$ ]]; then
    return 1
  fi

  echo "WARNING: No positive whale balance found for WETH; minting synthetic WETH on fork via deposit() from $SYNTHETIC_WETH_WHALE." >&2
  fund_and_impersonate "$SYNTHETIC_WETH_WHALE"
  cast_cmd send --rpc-url "$FORK_RPC_URL" --unlocked "${CAST_TX_ARGS[@]}" --from "$SYNTHETIC_WETH_WHALE" "$token" "deposit()" --value "$amount" >/dev/null
  echo "$SYNTHETIC_WETH_WHALE|$amount"
  return 0
}

require_balance() {
  local holder="$1" token="$2" amount="$3"
  if ! has_sufficient_balance "$holder" "$token" "$amount"; then
    echo "Balance check failed for $holder on $token. Lower amount or override whale."
    exit 1
  fi
}

print_runtime_context() {
  echo "Fixture RPC: $FORK_RPC_URL"
  if [[ "$USE_PRIVATE_KEY" -eq 1 ]]; then
    echo "Fixture signer address: $SIGNER_ADDRESS_VALUE"
  fi
}


detect_cast
detect_python
configure_signing
print_runtime_context

select_whale() {
  local token="$1" amount="$2"
  shift 2
  local candidate
  local balance_int
  local last_error=""
  local best_holder=""
  local best_balance=0

  for candidate in "$@"; do
    if balance_int=$(get_holder_balance "$candidate" "$token"); then
      if [[ "$balance_int" =~ ^[0-9]+$ ]] && is_int_gt "$balance_int" "$best_balance"; then
        best_balance="$balance_int"
        best_holder="$candidate"
      fi
      if [[ "$balance_int" =~ ^[0-9]+$ ]] && is_int_ge "$balance_int" "$amount"; then
        echo "$candidate|$amount"
        return 0
      fi
      last_error="insufficient balance for holder=$candidate token=$token"
    else
      last_error="$LAST_BALANCE_ERROR"
    fi
  done

  if [[ -n "$best_holder" ]] && is_int_gt "$best_balance" "0"; then
    echo "WARNING: No whale meets requested amount=$amount for token=$token. Falling back to holder=$best_holder amount=$best_balance." >&2
    echo "$best_holder|$best_balance"
    return 0
  fi

  if [[ -n "$last_error" ]]; then
    echo "Last balance probe error: $last_error" >&2
  fi
  return 1
}


if [[ -z "$TOKEN_IN_WHALE" ]]; then
  if ! IFS="|" read -r TOKEN_IN_WHALE IN_AMOUNT < <(select_whale "$TOKEN_IN" "$IN_AMOUNT" "${WHALE_CANDIDATES[@]}"); then
    if ! IFS="|" read -r TOKEN_IN_WHALE IN_AMOUNT < <(prepare_synthetic_weth_whale "$TOKEN_IN" "$IN_AMOUNT"); then
      echo "No whale found with positive balance for TOKEN_IN=$TOKEN_IN. Override TOKEN_IN_WHALE/TOKEN_IN_AMOUNT_WEI."
      exit 1
    fi
  fi
fi

if [[ -z "$TOKEN_OUT_WHALE" ]]; then
  IFS="|" read -r TOKEN_OUT_WHALE OUT_AMOUNT < <(select_whale "$TOKEN_OUT" "$OUT_AMOUNT" "${WHALE_CANDIDATES[@]}") || {
    echo "No USDC whale found with any positive balance. Override TOKEN_OUT_WHALE."
    exit 1
  }
fi

echo "Using TOKEN_IN_WHALE=$TOKEN_IN_WHALE amount=$IN_AMOUNT"
echo "Using TOKEN_OUT_WHALE=$TOKEN_OUT_WHALE amount=$OUT_AMOUNT"

fund_and_impersonate "$TOKEN_IN_WHALE"
fund_and_impersonate "$TOKEN_OUT_WHALE"

require_balance "$TOKEN_IN_WHALE" "$TOKEN_IN" "$IN_AMOUNT"
require_balance "$TOKEN_OUT_WHALE" "$TOKEN_OUT" "$OUT_AMOUNT"

if [[ "$USE_PRIVATE_KEY" -eq 1 ]]; then
  fund_address "$SIGNER_ADDRESS_VALUE"
  transfer_whale_to_signer "$TOKEN_IN_WHALE" "$TOKEN_IN" "$IN_AMOUNT"
  transfer_whale_to_signer "$TOKEN_OUT_WHALE" "$TOKEN_OUT" "$OUT_AMOUNT"
  require_balance "$SIGNER_ADDRESS_VALUE" "$TOKEN_IN" "$IN_AMOUNT"
  require_balance "$SIGNER_ADDRESS_VALUE" "$TOKEN_OUT" "$OUT_AMOUNT"
  transfer_to_pair "$SIGNER_ADDRESS_VALUE" "$TOKEN_IN" "$IN_AMOUNT"
  transfer_to_pair "$SIGNER_ADDRESS_VALUE" "$TOKEN_OUT" "$OUT_AMOUNT"
  cast_cmd send --rpc-url "$FORK_RPC_URL" "${CAST_SIGNING_ARGS[@]}" "${CAST_TX_ARGS[@]}" --from "$SIGNER_ADDRESS_VALUE" "$PAIR_ADDRESS" "sync()"
else
  transfer_to_pair "$TOKEN_IN_WHALE" "$TOKEN_IN" "$IN_AMOUNT"
  transfer_to_pair "$TOKEN_OUT_WHALE" "$TOKEN_OUT" "$OUT_AMOUNT"
  cast_cmd send --rpc-url "$FORK_RPC_URL" "${CAST_SIGNING_ARGS[@]}" "${CAST_TX_ARGS[@]}" --from "$TOKEN_IN_WHALE" "$PAIR_ADDRESS" "sync()"
fi

cast_cmd rpc --rpc-url "$FORK_RPC_URL" anvil_stopImpersonatingAccount "$TOKEN_IN_WHALE" >/dev/null
cast_cmd rpc --rpc-url "$FORK_RPC_URL" anvil_stopImpersonatingAccount "$TOKEN_OUT_WHALE" >/dev/null

echo "Injected imbalance into $PAIR_ADDRESS; reserves skewed for arbitrage simulation"
