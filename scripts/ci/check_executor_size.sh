#!/usr/bin/env bash
set -euo pipefail

MAX=24576
FORGE_BIN="${FORGE_BIN:-forge}"

if ! command -v "$FORGE_BIN" >/dev/null 2>&1; then
  echo "forge not found (set FORGE_BIN or add forge to PATH)"
  exit 127
fi

OUT="$($FORGE_BIN build --sizes 2>&1 || true)"
SIZE=$(printf '%s\n' "$OUT" | awk -F'|' '/\| MultiVenueArbImplementation[[:space:]]*\|/ {gsub(/[ ,]/,"",$3); print $3; exit}')

if [[ -z "${SIZE:-}" ]]; then
  echo "could not determine MultiVenueArbImplementation runtime size"
  exit 1
fi

if (( SIZE > MAX )); then
  echo "MultiVenueArbImplementation size ${SIZE} exceeds max ${MAX}"
  exit 1
fi

echo "MultiVenueArbImplementation size ${SIZE} bytes <= ${MAX} bytes"
