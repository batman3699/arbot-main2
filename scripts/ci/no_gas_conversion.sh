#!/usr/bin/env bash
# INV-19: gas limit (a SCHEDULING variable) and gas used (a COST variable) must
# never be interconvertible. Blueprint §23.4: conflating them "is incorrect on
# systems where gas limit affects scheduling" -- on Base the limit decides which
# Flashblock a transaction is eligible for.
#
# Rust has no negative trait bounds, so the absence of a conversion cannot be
# asserted in-language except via a compile-fail fixture whose expected stderr is
# pinned to a rustc version. This grep fails just as loudly, costs no dependency,
# and matches the existing scripts/ci/ convention.
set -euo pipefail
cd "$(dirname "$0")/../.."

pattern='impl[[:space:]]+(From|Into)<[[:space:]]*Gas(Limit|Used)[[:space:]]*>[[:space:]]+for[[:space:]]+Gas(Limit|Used)'
if hits=$(grep -rEn "$pattern" crates/ 2>/dev/null); then
  echo "INV-19 VIOLATION: a conversion between GasLimit and GasUsed was added." >&2
  echo "$hits" >&2
  echo >&2
  echo "These are different quantities. If you need the number, unwrap the .0 at" >&2
  echo "the call site so the reader can see which one you meant." >&2
  exit 1
fi
echo "ok: no GasLimit <-> GasUsed conversion"
