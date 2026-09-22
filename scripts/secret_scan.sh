#!/usr/bin/env bash
set -euo pipefail

failures=0

scan() {
  local label="$1"
  local pattern="$2"
  shift 2
  if rg --pcre2 -n "$pattern" "$@"; then
    echo "[secret_scan] ${label} detected" >&2
    failures=1
  fi
}

# Alchemy keys embedded in URLs (must use ${ALCHEMY_KEY}).
scan "alchemy-key" "alchemy\.com/v2/(?!\\$\\{ALCHEMY_KEY\\})[A-Za-z0-9_-]+" \
  --glob '!target/**' --glob '!.git/**' --glob '!scripts/secret_scan.sh'

# Explicit private key assignment
scan "private-key-assignment" "PRIVATE_KEY=" \
  --glob '!target/**' --glob '!.git/**' --glob '!scripts/secret_scan.sh'

# Likely raw private keys (0x + 64 hex) outside vendor/fixtures.
scan "raw-private-key" "0x[a-fA-F0-9]{64}" \
  --glob '!target/**' \
  --glob '!.git/**' \
  --glob '!scripts/secret_scan.sh' \
  --glob '!lib/**' \
  --glob '!config/**' \
  --glob '!tests/**' \
  --glob '!scripts/shadow/**' \
  --glob '!src/venues.rs'

if [[ $failures -ne 0 ]]; then
  echo "[secret_scan] failed" >&2
  exit 1
fi

echo "[secret_scan] no findings"
