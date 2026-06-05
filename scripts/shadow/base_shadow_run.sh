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
fi

mkdir -p logs

# --- Shadow + Base-only run controls (these win over .env) -------------------
export SHADOW_MODE=1
export SHADOW_LOG_PATH="${SHADOW_LOG_PATH:-logs/shadow-base.jsonl}"
export SHADOW_TAG="${SHADOW_TAG:-base-alchemy}"
export CHAIN=base
export CHAIN_LIST=base

# Clean baseline: no fault injection. (Chaos is rejected in production mode
# anyway; the resilience drill is a separate, explicitly non-production run.)
export CHAOS_RELAY_REJECT_BPS=0
export CHAOS_PUBLIC_REJECT_BPS=0
export CHAOS_BROADCAST_DELAY_MS=0
export CHAOS_DISABLE_WS=false

# Observability defaults (override by exporting before calling).
export RUST_LOG="${RUST_LOG:-info,arb_exec=info,arb_exec::venues=info,venue::univ3=warn}"
export PROMETHEUS_PORT="${PROMETHEUS_PORT:-9100}"

# --- Hard safety assertion ---------------------------------------------------
if [ "${SHADOW_MODE}" != "1" ]; then
  echo "FATAL: SHADOW_MODE != 1. Refusing to start (would broadcast live)." >&2
  exit 1
fi

echo "=============================================================="
echo " ARBOT — Base Shadow Validation Run"
echo "   chain          : base (chain id 8453)"
echo "   mode           : SHADOW (no broadcasts)"
echo "   shadow log     : $SHADOW_LOG_PATH"
echo "   shadow tag     : $SHADOW_TAG"
echo "   prometheus     : http://127.0.0.1:${PROMETHEUS_PORT}/metrics"
echo "   RUST_LOG       : $RUST_LOG"
echo "   auto-stop      : ${RUN_SECS:-<none, Ctrl+C to stop>}"
echo "=============================================================="
echo ">>> Building (release)..."
if ! cargo build --release --bin arb-exec; then
  echo "FATAL: build failed." >&2
  exit 1
fi

echo ">>> Starting shadow run. Tail the log in another terminal:"
echo "    tail -f $SHADOW_LOG_PATH | jq ."
echo ""

if [ -n "${RUN_SECS:-}" ]; then
  # Bounded run for unattended CI/smoke validation.
  timeout --preserve-status "${RUN_SECS}s" cargo run --release --bin arb-exec
  echo ">>> Shadow run stopped after ${RUN_SECS}s."
else
  exec cargo run --release --bin arb-exec
fi
