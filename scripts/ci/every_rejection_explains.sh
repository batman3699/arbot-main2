#!/usr/bin/env bash
# INV-40: "Every economically attractive but unexecuted candidate receives a
# machine-readable reason code" (§33, gate 36).
#
# §27 asks for an exhaustive test enumerating "every `return Reject` / `None`
# path in the candidate pipeline". Enumerating return statements is textual and
# a refactor invalidates it silently. The enumeration is over TYPES instead: a
# rejection is a value of some enum, that enum implements
# `apex_types::miss::ExplainsMiss` with an exhaustive `match`, and
# `apex-obs`'s `every_rejection_path_records_a_miss` walks every variant.
#
# That covers two of the three ways coverage can rot. A new VARIANT breaks the
# `match` at compile time, and a rejection that produces no record cannot
# compile because `MissLedger::record` takes the rejection and derives the
# bucket itself. This closes the third: **a new rejection TYPE with no
# implementation**, which compiles perfectly and is simply never asked.
#
# §6.5 records this repository's "written but never wired" pattern four times
# over. A rejection enum nobody mapped is the same shape, and it would show up
# as a quiet gap in the one dataset that decides where engineering effort goes.
set -euo pipefail
cd "$(dirname "$0")/../.."

# A rejection type is one whose name says it declines something. Deliberately
# narrow: this is a naming convention the gate enforces, not a guess about
# semantics, and the error message says how to opt out.
pattern='^pub enum ([A-Za-z0-9]*(Reject|Refusal|Refused|AdmissionError|NoLane|Verdict|Clause|LastMileCheck)[A-Za-z0-9]*)'

missing=()
while IFS= read -r line; do
  file="${line%%:*}"
  name=$(printf '%s' "${line#*:}" | sed -E 's/^pub enum ([A-Za-z0-9]+).*/\1/')
  crate=$(printf '%s' "$file" | cut -d/ -f2)
  # `arb-exec-legacy` is out of scope: it retires in Phase 17 and has no miss
  # ledger to file into. Scoped the same way as the other new-crate gates.
  [ "$crate" = "arb-exec-legacy" ] && continue
  grep -rq "impl .*ExplainsMiss for ${name}\b" crates/ 2>/dev/null && continue
  # Opt out at the declaration, with the reason, the same way secret_scan.sh
  # takes a suppression: a type that declines something other than a candidate
  # says so where it is defined, and the claim is greppable.
  grep -B12 -E "^pub enum ${name}\b" "$file" 2>/dev/null \
    | grep -q 'not-a-candidate-rejection' && continue
  missing+=("${file}: ${name}")
done < <(grep -rhnE "$pattern" crates/apex-*/src --include='*.rs' \
         | sed -E 's/^[0-9]+://' \
         | while IFS= read -r decl; do
             f=$(grep -rlE "^$(printf '%s' "$decl" | sed 's/[][\.*^$/]/\\&/g')" crates/apex-*/src --include='*.rs' | head -1)
             printf '%s:%s\n' "$f" "$decl"
           done)

if [ "${#missing[@]}" -gt 0 ]; then
  echo "INV-40 VIOLATION: a rejection type does not say which miss it is." >&2
  printf '  %s\n' "${missing[@]}" >&2
  echo >&2
  echo "Implement apex_types::miss::ExplainsMiss for it, with an exhaustive" >&2
  echo "match, in the crate that owns it -- the code that knows why it" >&2
  echo "declined is the code that knows which bucket that is. If the type is" >&2
  echo "genuinely not a candidate rejection, rename it so it does not match" >&2
  echo "the convention above." >&2
  exit 1
fi
optouts=$(grep -rc 'not-a-candidate-rejection' crates/apex-*/src --include='*.rs' 2>/dev/null \
          | awk -F: '$2 > 0 {n += $2} END {print n + 0}')
echo "ok: every rejection type explains which miss it is (${optouts} declared non-rejections)"
