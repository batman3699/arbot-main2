#!/usr/bin/env bash
# Clear a canary halt (Task 8.5 R28). Run on the host with the service
# stopped: the running canary never re-reads its ledger, so a reset appended
# under it would take effect only at the next start, unannounced.
#   systemctl stop apex-canary && scripts/canary-reset.sh && systemctl start apex-canary
set -euo pipefail
ledger="${1:-var/apex/canary.jsonl}"
if systemctl is-active --quiet apex-canary 2>/dev/null; then
    echo "apex-canary is running: stop it first" >&2
    exit 1
fi
[ -f "$ledger" ] || { echo "no ledger at $ledger" >&2; exit 1; }
printf '{"kind":"reset","at_ms":%s}\n' "$(date +%s%3N)" >> "$ledger"
sync "$ledger"
echo "reset appended to $ledger; the cap counts from here"
