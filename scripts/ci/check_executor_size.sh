#!/usr/bin/env bash
# No DEPLOYABLE contract may exceed EIP-170.
#
# `forge build --sizes` fails the build when ANY contract is over the limit,
# test harnesses included, and that is the wrong question. `JitRemoveHarness`
# in test/MultiVenueArbExecutor.t.sol extends `MultiVenueArbImplementation` and
# adds scaffolding; since the parent already sits 173 bytes under the ceiling,
# every subclass of it is structurally over. Nothing deploys that harness.
#
# So: check the contracts that can actually be deployed, which means excluding
# the ones declared under test/ and contracts/mocks/. A new production contract
# over the limit still fails here.
set -euo pipefail

MAX=24576
FORGE_BIN="${FORGE_BIN:-forge}"

if ! command -v "$FORGE_BIN" >/dev/null 2>&1; then
  echo "forge not found (set FORGE_BIN or add forge to PATH)" >&2
  exit 127
fi

# `--sizes --json` reports name -> {runtime_size, runtime_margin, ...} with no
# source path, so the test/mock names are collected from the sources.
SIZES="$($FORGE_BIN build --sizes --json 2>/dev/null || true)"
[ -n "$SIZES" ] || { echo "forge produced no size report" >&2; exit 1; }

# `|| true`: grep exits non-zero when it matches nothing, and under
# `set -e` that would kill the gate with a bare exit code instead of a
# message. An empty skip list is a valid state -- it just means every
# contract is checked.
NON_DEPLOYABLE=$(grep -rhoE '^[[:space:]]*(abstract[[:space:]]+)?contract[[:space:]]+[A-Za-z0-9_]+' \
                   test contracts/mocks 2>/dev/null || true)
NON_DEPLOYABLE=$(printf '%s\n' "$NON_DEPLOYABLE" | awk 'NF {print $NF}' | sort -u)

printf '%s' "$SIZES" | NON_DEPLOYABLE="$NON_DEPLOYABLE" MAX="$MAX" python3 -c '
import json, os, sys

report = json.load(sys.stdin)
skip = {n for n in os.environ["NON_DEPLOYABLE"].split() if n}
limit = int(os.environ["MAX"])

over, tight = [], []
for name, m in sorted(report.items()):
    if name in skip:
        continue
    size = m.get("runtime_size", 0)
    margin = limit - size
    if margin < 0:
        over.append((name, size, margin))
    elif margin < 1024:
        tight.append((name, size, margin))

for name, size, margin in tight:
    print(f"[check_executor_size] {name}: {size} bytes, {margin} to spare")
for name, size, margin in over:
    print(f"[check_executor_size] {name}: {size} bytes, {-margin} OVER EIP-170", file=sys.stderr)

if over:
    print("[check_executor_size] a deployable contract exceeds EIP-170", file=sys.stderr)
    sys.exit(1)

print(f"[check_executor_size] ok: {len(report) - len(skip & report.keys())} deployable "
      f"contracts, all within {limit} bytes")
'
