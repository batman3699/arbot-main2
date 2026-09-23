#!/usr/bin/env bash
# Every invariant in PLAN.md §8 names what enforces it. This checks those things
# exist, and RATCHETS: coverage may rise, never fall.
#
# Phase 8's acceptance criterion 5 is "all 46 invariants have passing named
# tests; invariant_coverage.sh is green with no allowances", so this has to be
# able to see all 46. The version before 2026-09-24 could not, for three
# reasons found while auditing against that criterion:
#
#   1. It greped `crates/ tests/`, and `tests/` does not exist at the repository
#      root. Whether that produced a false answer depended on which `grep` was
#      on PATH -- GNU grep and ugrep disagree about the exit status when one
#      argument is unreadable and another matched. A gate whose answer depends
#      on the grep implementation is not a gate.
#
#   2. It scraped `crate::name` from the WHOLE table, catching the Enforcement
#      column's FUNCTION names -- `revalidate::last_mile`, `gas::choose_limit`,
#      `sizing::refine_discrete`, `allocation::certify` -- and counting four
#      functions as four missing tests.
#
#   3. It only understood Rust test names. Thirteen invariants are enforced by
#      `forge` tests, a CI grep, or a process gate, and every one of them was
#      invisible: they contributed to neither side of the ratio, so the gate
#      reported a fraction of a fraction and called it coverage.
#
# Four kinds of enforcement are recognised, and anything a row names that is not
# one of them is reported as UNCHECKABLE rather than passed over -- an invariant
# whose enforcement this script cannot verify is exactly the thing a coverage
# gate must not hide.
set -euo pipefail
cd "$(dirname "$0")/../.."

FLOOR_FILE="docs/apex/.invariant-floor"

section() { sed -n '/^# 8\. Critical invariants/,/^# 9\./p' PLAN.md; }

rows=$(section | grep -c '^| \*\*INV-' || true)
if [ "${rows:-0}" -eq 0 ]; then
  echo "FAIL: parsed zero invariants out of PLAN.md §8 -- the parser is broken," >&2
  echo "not the plan. A silently-empty gate is worse than no gate." >&2
  exit 1
fi

covered=0
uncheckable=()
missing=()

while IFS= read -r row; do
  inv=$(printf '%s' "$row" | awk -F'|' '{gsub(/\*|[[:space:]]/,"",$2); print $2}')
  # Column 4 of 7: Invariant | Statement | Enforcement | Test | Metric |
  # Remediation | Gate. Field 5 in awk, because the leading `|` empties field 1.
  test_col=$(printf '%s' "$row" | awk -F'|' '{print $5}')

  artefacts=0
  gaps=()

  # Rust: `crate::snake_case_name` -> `fn name` somewhere in crates/.
  while IFS= read -r t; do
    [ -z "$t" ] && continue
    artefacts=$((artefacts + 1))
    name="${t##*::}"
    grep -rq "fn ${name}\b" crates/ 2>/dev/null || gaps+=("rust ${t}")
  done < <(printf '%s' "$test_col" | grep -oE '`[a-z_]+::[a-z_0-9]+`' | tr -d '`' | sort -u)

  # Solidity: `testCamelCase` -> `function testCamelCase` somewhere in test/.
  while IFS= read -r t; do
    [ -z "$t" ] && continue
    artefacts=$((artefacts + 1))
    grep -rq "function ${t}\b" test/ contracts/ 2>/dev/null || gaps+=("forge ${t}")
  done < <(printf '%s' "$test_col" | grep -oE '`test[A-Z][A-Za-z0-9]*`' | tr -d '`' | sort -u)

  # CI greps: a `scripts/ci/x.sh` that exists and can run.
  while IFS= read -r t; do
    [ -z "$t" ] && continue
    artefacts=$((artefacts + 1))
    [ -x "$t" ] || gaps+=("script ${t}")
  done < <(printf '%s' "$test_col" | grep -oE '`scripts/ci/[a-z_]+\.sh`' | tr -d '`' | sort -u)

  if [ "$artefacts" -eq 0 ]; then
    uncheckable+=("${inv}:$(printf '%s' "$test_col" | cut -c1-60 | sed 's/^ *//;s/ *$//')")
  elif [ "${#gaps[@]}" -eq 0 ]; then
    covered=$((covered + 1))
  else
    for g in "${gaps[@]}"; do missing+=("${inv} ${g}"); done
  fi
done < <(section | grep '^| \*\*INV-')

floor=0
[ -f "$FLOOR_FILE" ] && floor=$(cat "$FLOOR_FILE")

echo "invariants fully enforced: ${covered}/${rows} (floor ${floor}); ${#uncheckable[@]} name no checkable artefact"

if [ "$covered" -lt "$floor" ]; then
  echo >&2
  echo "FAIL: coverage dropped from ${floor} to ${covered}." >&2
  echo "An invariant that was enforced no longer is:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  exit 1
fi

if [ "$covered" -gt "$floor" ]; then
  echo "$covered" > "$FLOOR_FILE"
  echo "floor raised to ${covered} -- commit ${FLOOR_FILE}"
fi

if [ "${1:-}" = "--list" ]; then
  [ "${#missing[@]}" -gt 0 ] && { echo "missing artefacts:"; printf '  %s\n' "${missing[@]}"; }
  [ "${#uncheckable[@]}" -gt 0 ] && { echo "uncheckable rows:"; printf '  %s\n' "${uncheckable[@]}"; }
fi
exit 0
