#!/usr/bin/env bash
set -euo pipefail

# Deterministic fork-based dry-run for integration_smoke test.
# Runs quote -> plan -> encode (+ optional executor eth_call simulation) against a local anvil fork.

UPSTREAM_RPC_URL=${UPSTREAM_RPC_URL:-${FORK_UPSTREAM_RPC_URL:-}}
FORK_RPC_URL=${FORK_RPC_URL:-http://127.0.0.1:8545}
FORK_PORT=${FORK_PORT:-8545}
CHAIN_NAME=${CHAIN_NAME:-ethereum}
ANVIL_BLOCK_TIME=${ANVIL_BLOCK_TIME:-1}
TEST_CMD=${TEST_CMD:-"cargo test --test integration_smoke -- --nocapture"}
RUN_EXECUTOR_SIM=${RUN_EXECUTOR_SIM:-0}

cast_rpc_cmd() {
  # Keep Foundry RPC calls independent from ambient chain aliases (e.g. CHAIN=ethereum)
  # because cast may try to resolve them as network presets even when --rpc-url is provided.
  # Also ensure loopback RPC calls bypass global proxy settings in CI/containers.
  env \
    -u CHAIN -u ETH_CHAIN -u FOUNDRY_CHAIN \
    NO_PROXY="localhost,127.0.0.1,::1,${NO_PROXY:-}" \
    no_proxy="localhost,127.0.0.1,::1,${no_proxy:-}" \
    cast "$@"
}

chain_id_for_name() {
  case "$1" in
    ethereum) echo 1 ;;
    arbitrum) echo 42161 ;;
    optimism) echo 10 ;;
    base) echo 8453 ;;
    polygon) echo 137 ;;
    linea) echo 59144 ;;
    abstract) echo 2741 ;;
    ink) echo 763373 ;;
    mantle) echo 5000 ;;
    scroll) echo 534352 ;;
    *)
      echo "ERROR: unsupported CHAIN_NAME='$1' for local fork harness" >&2
      return 1
      ;;
  esac
}

CHAIN_ID=$(chain_id_for_name "$CHAIN_NAME")

chain_name_for_id() {
  case "$1" in
    1) echo "ethereum" ;;
    42161) echo "arbitrum" ;;
    10) echo "optimism" ;;
    8453) echo "base" ;;
    137) echo "polygon" ;;
    59144) echo "linea" ;;
    2741) echo "abstract" ;;
    763373) echo "ink" ;;
    5000) echo "mantle" ;;
    534352) echo "scroll" ;;
    *) echo "unknown" ;;
  esac
}

if [[ -z "$UPSTREAM_RPC_URL" ]]; then
  echo "ERROR: set UPSTREAM_RPC_URL (or FORK_UPSTREAM_RPC_URL) to a live archive RPC endpoint."
  exit 1
fi

for dep in anvil cast cargo; do
  if ! command -v "$dep" >/dev/null 2>&1; then
    echo "ERROR: missing dependency '$dep' in PATH"
    exit 1
  fi
done

echo "[0/5] Validating upstream RPC chain id..."
UPSTREAM_CHAIN_ID=$(cast_rpc_cmd chain-id --rpc-url "$UPSTREAM_RPC_URL" 2>/dev/null || true)
if [[ -z "$UPSTREAM_CHAIN_ID" ]]; then
  echo "ERROR: unable to read chain id from UPSTREAM_RPC_URL. Verify endpoint reachability and auth."
  exit 1
fi

if [[ "$UPSTREAM_CHAIN_ID" != "$CHAIN_ID" ]]; then
  DETECTED_CHAIN_NAME=$(chain_name_for_id "$UPSTREAM_CHAIN_ID")
  echo "ERROR: CHAIN_NAME='$CHAIN_NAME' expects chain_id=$CHAIN_ID, but UPSTREAM_RPC_URL reports chain_id=$UPSTREAM_CHAIN_ID ($DETECTED_CHAIN_NAME)."
  echo "Hint: set CHAIN_NAME=$DETECTED_CHAIN_NAME or provide an upstream RPC for '$CHAIN_NAME'."
  exit 1
fi

echo "[1/5] Starting local fork on $FORK_RPC_URL (chain=$CHAIN_NAME chain_id=$CHAIN_ID)..."
anvil --fork-url "$UPSTREAM_RPC_URL" --port "$FORK_PORT" --chain-id "$CHAIN_ID" --block-time "$ANVIL_BLOCK_TIME" >/tmp/arbot-anvil.log 2>&1 &
ANVIL_PID=$!
cleanup() {
  if kill -0 "$ANVIL_PID" >/dev/null 2>&1; then
    kill "$ANVIL_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

for _ in {1..40}; do
  if cast_rpc_cmd block-number --rpc-url "$FORK_RPC_URL" >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

if ! cast_rpc_cmd block-number --rpc-url "$FORK_RPC_URL" >/dev/null 2>&1; then
  echo "ERROR: anvil fork did not become ready; inspect /tmp/arbot-anvil.log"
  exit 1
fi

echo "[2/5] Fork ready."

echo "[3/5] Running deterministic integration dry-run test suite..."
export ARBOT_INTEGRATION_SMOKE=1
export ARBOT_INTEGRATION_CHAIN="$CHAIN_NAME"
export CHAIN="$CHAIN_NAME"
export ARBOT_REQUIRE_CHAIN_COVERAGE=1
export ARBOT_FORK_RPC_URL="$FORK_RPC_URL"
export NO_PROXY="localhost,127.0.0.1,::1,${NO_PROXY:-}"
export no_proxy="localhost,127.0.0.1,::1,${no_proxy:-}"
if [[ "$RUN_EXECUTOR_SIM" == "1" ]]; then
  export ARBOT_SMOKE_EXECUTOR=1
fi

# shellcheck disable=SC2086
bash -lc "$TEST_CMD"

echo "[4/5] Dry-run completed successfully."
echo "[5/5] Anvil logs: /tmp/arbot-anvil.log"
