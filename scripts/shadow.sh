#!/usr/bin/env bash
# Start `apex shadow` (Task 8.5): the capture path over Base, with the null
# dispatcher as its only lane. Nothing is sent.
#
#   scripts/shadow.sh [config]          # default ops/shadow.base.yaml
#   setsid nohup scripts/shadow.sh > var/apex/shadow.log 2>&1 < /dev/null &
#   scripts/shadow-status.sh            # what it is doing
#   kill -INT "$(cat var/apex/shadow.pid)"   # drain and stop
#
# The pid file is written here, not by the caller: this shell becomes `apex` at
# `exec`, so `$$` is the run's pid. A caller's `$!` is not -- under an
# interactive shell `setsid` forks, and `$!` is a parent that has already
# exited.
#
# `apex` reads only `APEX_SECRET_*` variables. This maps two of the operator's
# .env entries onto them for the run's process alone -- BLOCKPI_KEY to
# APEX_SECRET_BLOCKPI_KEY, and TRADER_PRIVATE_KEY to APEX_SECRET_TRADER_KEY.
# Nothing is echoed: the values pass through variables, never through argv or
# the terminal, and the rest of .env is not loaded at all.
#
# TRADER_PRIVATE_KEY, and never PRIVATE_KEY: since the Phase 5 deploy
# (2026-10-03), PRIVATE_KEY signs as the address that owns the executor's
# router. The owner's key has no business in a long-running trading process,
# so this script has no path that reads it. The run refuses to start unless
# the trader key signs as the config's signer.address.
set -euo pipefail
cd "$(dirname "$0")/.."

config="${1:-ops/shadow.base.yaml}"
bin=target/release/apex
[[ -f .env ]] || { echo "shadow: .env not found" >&2; exit 1; }
[[ -x "$bin" ]] || { echo "shadow: $bin not built; cargo build --release -p apex-runtime --bin apex" >&2; exit 1; }

# The last assignment of one variable, unquoted. Values never reach argv.
value() {
  local v
  v="$(grep -E "^$1=" .env | tail -n 1 | cut -d= -f2- || true)"
  v="${v%\"}"; v="${v#\"}"; v="${v%\'}"; v="${v#\'}"
  printf '%s' "$v"
}

APEX_SECRET_BLOCKPI_KEY="$(value BLOCKPI_KEY)"
APEX_SECRET_TRADER_KEY="$(value TRADER_PRIVATE_KEY)"
[[ -n "$APEX_SECRET_BLOCKPI_KEY" ]] || { echo "shadow: BLOCKPI_KEY is empty in .env" >&2; exit 1; }
[[ -n "$APEX_SECRET_TRADER_KEY" ]] || {
  echo "shadow: TRADER_PRIVATE_KEY is not set in .env -- the key of the trader the executor" >&2
  echo "        authorizes (signer.address in $config). Not PRIVATE_KEY, which is the owner's." >&2
  exit 1
}
export APEX_SECRET_BLOCKPI_KEY APEX_SECRET_TRADER_KEY

mkdir -p var/apex
pidfile=var/apex/shadow.pid
# One run at a time: two would share a journal.
if [[ -f "$pidfile" ]]; then
  running="$(cat "$pidfile" 2>/dev/null || true)"
  if [[ -n "$running" ]] && { tr '\0' ' ' < "/proc/$running/cmdline"; } 2>/dev/null | grep -q 'apex shadow'; then
    echo "shadow: already running (pid $running)" >&2
    exit 1
  fi
fi
echo $$ > "$pidfile"
exec "$bin" shadow --config "$config"
