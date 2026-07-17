#!/usr/bin/env bash
#
# base_shadow_run.sh — Launch Arbot in SHADOW MODE on Base via Alchemy.
#
# Shadow mode runs the FULL decision pipeline (scan -> simulate -> size -> plan)
# against live Base state, but STUBS the final broadcast: no bundle/tx is ever
# sent. It is the safe way to validate the system on real data before any live
# exposure.
#
# What this launcher guarantees:
#   * SHADOW_MODE=1            (no real broadcasts — hard requirement)
#   * Base only               (CHAIN=base, CHAIN_LIST=base)
#   * Chaos disabled           (clean baseline; use the chaos drill separately)
#   * Dedicated shadow log     (logs/shadow-base.jsonl)
#   * Non-interactive auto-start (no stdin `start` command required)
#
# Env overrides exported here win over .env (dotenvy does not overwrite existing
# process env), so your committed .env is left untouched.
#
# Usage:
#   scripts/shadow/base_shadow_run.sh                 # preflight + run
#   scripts/shadow/base_shadow_run.sh --skip-preflight
#   RUN_SECS=900 scripts/shadow/base_shadow_run.sh    # auto-stop after 15 min
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

SKIP_PREFLIGHT=0
for arg in "$@"; do
  case "$arg" in
    --skip-preflight) SKIP_PREFLIGHT=1 ;;
    *) echo "Unknown arg: $arg" >&2; exit 2 ;;
  esac
done

# --- Require an Alchemy key (paid tier strongly recommended) -----------------
KEY="${ALCHEMY_KEY:-}"
if [ -z "$KEY" ] && [ -f .env ]; then
  KEY="$(grep -E '^ALCHEMY_KEY=' .env | head -1 | cut -d= -f2- | tr -d '[:space:]')"
fi
if [ -z "$KEY" ]; then
  echo "FATAL: ALCHEMY_KEY is not set (env or .env). A Base archive/RPC endpoint is required." >&2
  exit 2
fi
export ALCHEMY_KEY="$KEY"

# --- Preflight on-chain address validation -----------------------------------
if [ "$SKIP_PREFLIGHT" -eq 0 ]; then
  echo ">>> Running on-chain address preflight..."
  if ! "$SCRIPT_DIR/validate_base_addresses.sh"; then
    echo "FATAL: preflight failed. Fix addresses before running. (--skip-preflight to override)" >&2
    exit 1
  fi
  echo ">>> Running live-config audit..."
  if ! "$SCRIPT_DIR/audit_base_live_config.sh"; then
    echo "FATAL: config audit failed. Fix .env/ops before running." >&2
    exit 1
  fi
fi

mkdir -p logs

# --- Stop stale instances (leftover shadow runs hold :9101 and RPC slots) ---
if pgrep -x arb-exec >/dev/null 2>&1; then
  echo ">>> Stopping stale arb-exec process(es) from a prior run..."
  pkill -x arb-exec 2>/dev/null || true
  sleep 2
  if pgrep -x arb-exec >/dev/null 2>&1; then
    echo "FATAL: could not stop existing arb-exec. Run: pkill -x arb-exec" >&2
    exit 1
  fi
fi

find_free_prometheus_port() {
  local p
  for p in 9101 9102 9103 9104 9105 9110 9111; do
    if ! ss -tln 2>/dev/null | grep -q ":${p} "; then
      echo "$p"
      return 0
    fi
  done
  echo "9199"
}

# --- Shadow + Base-only run controls (these win over .env) -------------------
export SHADOW_MODE=1
export ARBOT_NONINTERACTIVE=1
export SHADOW_LOG_PATH="${SHADOW_LOG_PATH:-logs/shadow-base.jsonl}"
export SHADOW_TAG="${SHADOW_TAG:-base-alchemy}"
export SHADOW_CONSOLE_LOG="${SHADOW_CONSOLE_LOG:-logs/shadow-base.console.log}"
export CHAIN=base
export CHAIN_LIST=base

# Clean baseline: no fault injection. (Chaos is rejected in production mode
# anyway; the resilience drill is a separate, explicitly non-production run.)
export CHAOS_RELAY_REJECT_BPS=0
export CHAOS_PUBLIC_REJECT_BPS=0
export CHAOS_BROADCAST_DELAY_MS=0
export CHAOS_DISABLE_WS=false

# Observability defaults (override by exporting before calling).
export PROMETHEUS_PORT="${PROMETHEUS_PORT:-$(find_free_prometheus_port)}"
export HOT_POOL_SKIP_VOLUME="${HOT_POOL_SKIP_VOLUME:-1}"
export HOT_POOL_RANK_CONCURRENCY="${HOT_POOL_RANK_CONCURRENCY:-32}"
export HOT_POOL_RPC_TIMEOUT_MS="${HOT_POOL_RPC_TIMEOUT_MS:-2000}"
export UNIV3_MAX_CONCURRENT_POOL_TASKS="${UNIV3_MAX_CONCURRENT_POOL_TASKS:-32}"
export UNIV3_QUOTE_CONCURRENCY="${UNIV3_QUOTE_CONCURRENCY:-24}"
export ARBOT_LOCAL_CL_QUOTES="${ARBOT_LOCAL_CL_QUOTES:-1}"
export ARBOT_SIM_REVM="${ARBOT_SIM_REVM:-1}"
export ARBOT_SIM_L1_FEE="${ARBOT_SIM_L1_FEE:-1}"
export ARBOT_SIM_PREFETCH="${ARBOT_SIM_PREFETCH:-1}"
export ARBOT_SCAN_IDLE_SLEEP_MS="${ARBOT_SCAN_IDLE_SLEEP_MS:-200}"

# Mempool + backrun: decode pending swaps and trigger same-block rescans.
export ARBOT_BF_SKIP_ON_STABLE_GRAPH="${ARBOT_BF_SKIP_ON_STABLE_GRAPH:-0}"
export BACKRUN_POST_STATE="${BACKRUN_POST_STATE:-1}"
export FEATURE_BACKRUN="${FEATURE_BACKRUN:-true}"
export BACKRUN_MONITOR="${BACKRUN_MONITOR:-true}"
export BACKRUN_MONITOR_ENABLED="${BACKRUN_MONITOR_ENABLED:-true}"
export RUST_LOG="${RUST_LOG:-info,arb_exec=info,arb_exec::venues=info,venue::univ3=warn,mempool=info}"

# --- Hard safety assertion ---------------------------------------------------
if [ "${SHADOW_MODE}" != "1" ]; then
  echo "FATAL: SHADOW_MODE != 1. Refusing to start (would broadcast live)." >&2
  exit 1
fi

ARB_EXEC="$REPO_ROOT/target/release/arb-exec"

echo "=============================================================="
echo " ARBOT — Base Shadow Validation Run"
echo "   chain          : base (chain id 8453)"
echo "   mode           : SHADOW (no broadcasts, auto-start)"
echo "   shadow log     : $SHADOW_LOG_PATH"
echo "   console log    : $SHADOW_CONSOLE_LOG"
echo "   shadow tag     : $SHADOW_TAG"
echo "   prometheus     : http://127.0.0.1:${PROMETHEUS_PORT}/metrics"
echo "   RUST_LOG       : $RUST_LOG"
echo "   auto-stop      : ${RUN_SECS:-<none, Ctrl+C to stop>}"
echo "=============================================================="
echo ">>> Building release binary (one-time; startup ranks hot pools ~30-60s)..."
if ! cargo build --release --bin arb-exec; then
  echo "FATAL: build failed." >&2
  exit 1
fi

if [ ! -x "$ARB_EXEC" ]; then
  echo "FATAL: missing $ARB_EXEC after build." >&2
  exit 1
fi

echo ">>> Starting shadow run. Tail would-be trades:"
echo "    tail -f $SHADOW_LOG_PATH | jq ."
echo ">>> Tail scan activity:"
echo "    tail -f $SHADOW_CONSOLE_LOG | grep -E 'STATUS|edges scanned|Executed'"
echo ""

if [ -n "${RUN_SECS:-}" ]; then
  timeout --preserve-status "${RUN_SECS}s" \
    "$ARB_EXEC" </dev/null 2>&1 | tee -a "$SHADOW_CONSOLE_LOG"
  code="${PIPESTATUS[0]}"
  if [ "$code" -eq 124 ]; then
    echo ">>> Shadow run stopped after ${RUN_SECS}s."
    exit 0
  fi
  exit "$code"
else
  "$ARB_EXEC" </dev/null 2>&1 | tee -a "$SHADOW_CONSOLE_LOG"
fi
