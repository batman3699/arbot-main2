#!/usr/bin/env bash
# Tier 0 makes no network call, by construction.
#
# Task 4.1 asks for a test asserting a mock provider recorded zero requests.
# That test exists and it is the weaker half: it proves the current code path
# did not call out, not that none can.
#
# This is the other half. `tier0.rs` may not name a provider type, may not be
# async, and may not reach for a client -- so there is nothing to call with and
# no point at which to await. A change that wanted the network would have to
# alter the signature, which is a visible act rather than an added line.
#
# Comment lines are skipped: the module explains WHY it may not name a provider,
# and a guard that cannot tell code from prose about code punishes writing the
# explanation down.
set -euo pipefail
cd "$(dirname "$0")/../.."

file="crates/apex-sim/src/tier0.rs"
[ -f "$file" ] || { echo "[tier0_is_pure] $file is gone" >&2; exit 1; }

banned='\b(Provider|Middleware|JsonRpcClient|reqwest|async fn|\.await|tokio::)'
hits=$(sed -e 's,//.*,,' "$file" | grep -nE "$banned" || true)

if [ -n "$hits" ]; then
  echo "[tier0_is_pure] Tier 0 reached for the network:" >&2
  printf '%s\n' "$hits" >&2
  echo "[tier0_is_pure] screening runs before anything expensive; it may not" >&2
  echo "[tier0_is_pure] be the expensive thing." >&2
  exit 1
fi
echo "[tier0_is_pure] ok: tier0 names no provider and is not async"
