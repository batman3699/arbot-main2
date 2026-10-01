#!/usr/bin/env bash
# Start `apex shadow` (Task 8.5): the capture path over Base, with the null
# dispatcher as its only lane. Nothing is sent.
#
#   scripts/shadow.sh [config]          # default ops/shadow.base.yaml
#   nohup scripts/shadow.sh > var/apex/shadow.log 2>&1 &
#
# `apex` reads only `APEX_SECRET_*` variables. This maps the operator's .env
# onto them -- BLOCKPI_KEY to APEX_SECRET_BLOCKPI_KEY, PRIVATE_KEY to
# APEX_SECRET_TRADER_KEY -- for the run's process alone. Nothing is echoed:
# the values pass through variables, never through argv or the terminal, and
# the rest of .env is not loaded at all.
set -euo pipefail
cd "$(dirname "$0")/.."

config="${1:-ops/shadow.base.yaml}"
bin=target/release/apex
[[ -f .env ]] || { echo "shadow: .env not found" >&2; exit 1; }
[[ -x "$bin" ]] || { echo "shadow: $bin not built; cargo build --release -p apex-runtime --bin apex" >&2; exit 1; }

# The last assignment of one variable, unquoted. Values never reach argv.
value() {
  local v
  v="$(grep -E "^$1=" .env | tail -n 1 | cut -d= -f2-)"
  v="${v%\"}"; v="${v#\"}"; v="${v%\'}"; v="${v#\'}"
  printf '%s' "$v"
}

APEX_SECRET_BLOCKPI_KEY="$(value BLOCKPI_KEY)"
APEX_SECRET_TRADER_KEY="$(value PRIVATE_KEY)"
[[ -n "$APEX_SECRET_BLOCKPI_KEY" ]] || { echo "shadow: BLOCKPI_KEY is empty in .env" >&2; exit 1; }
[[ -n "$APEX_SECRET_TRADER_KEY" ]] || { echo "shadow: PRIVATE_KEY is empty in .env" >&2; exit 1; }
export APEX_SECRET_BLOCKPI_KEY APEX_SECRET_TRADER_KEY

mkdir -p var/apex
exec "$bin" shadow --config "$config"
