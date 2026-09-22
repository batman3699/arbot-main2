#!/usr/bin/env bash
# Every invariant in PLAN.md §8 names the test that enforces it. This checks
# those tests exist, and RATCHETS: coverage may rise, never fall.
#
# Most invariants belong to crates that do not exist yet (Phases 1-8), so a
# hard "all present" gate would be red for months and get switched off. A floor
# that only moves up keeps the signal without the noise -- and makes deleting an
# implemented invariant's test a build failure rather than a quiet regression.
set -euo pipefail
cd "$(dirname "$0")/../.."

FLOOR_FILE="docs/apex/.invariant-floor"

# Test names are the backticked `crate::test_name` entries in the §8 tables.
mapfile -t declared < <(
  sed -n '/^# 8\. Critical invariants/,/^# 9\./p' PLAN.md \
  | grep -oE '`[a-z_]+::[a-z_0-9]+`' \
  | tr -d '`' | sort -u
)

if [ "${#declared[@]}" -eq 0 ]; then
  echo "FAIL: parsed zero test names out of PLAN.md §8 -- the parser is broken," >&2
  echo "not the plan. A silently-empty gate is worse than no gate." >&2
  exit 1
fi

found=0
missing=()
for t in "${declared[@]}"; do
  name="${t##*::}"
  if grep -rq "fn ${name}\b" crates/ tests/ 2>/dev/null; then
    found=$((found + 1))
  else
    missing+=("$t")
  fi
done

floor=0
[ -f "$FLOOR_FILE" ] && floor=$(cat "$FLOOR_FILE")

echo "invariant tests: ${found}/${#declared[@]} implemented (floor ${floor})"

if [ "$found" -lt "$floor" ]; then
  echo >&2
  echo "FAIL: coverage dropped from ${floor} to ${found}." >&2
  echo "An invariant that was enforced no longer is. Missing:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  exit 1
fi

if [ "$found" -gt "$floor" ]; then
  echo "$found" > "$FLOOR_FILE"
  echo "floor raised to ${found} -- commit ${FLOOR_FILE}"
fi
