#!/usr/bin/env bash
# §6.1's dependency graph, enforced.
#
# "Cycle-free by construction" is what §6.1 claims, and cargo already refuses a
# cycle. **Cargo does not refuse a wrong-direction edge**, because a single one
# is not a cycle: `apex-search -> apex-runtime` compiles perfectly well as long
# as nothing goes back the other way. That is exactly the mistake Task 8.4 made
# -- `StateEvent` was defined in `apex-runtime::bus`, and the first crate that
# needed it was `apex-search`, four tiers above the runtime.
#
# It was cheap to fix at that moment and would not have stayed cheap. A type in
# the wrong crate does not announce itself; it announces itself the day someone
# needs it from the other side, and by then the fix is a move plus every call
# site.
#
# Scoped to `[dependencies]`. Dev-dependencies are deliberately NOT checked:
# `apex-obs`'s tests link `apex-capture`, `apex-chain`, `apex-econ`, `apex-sim`
# and `apex-venues` to walk their rejection types, which is the right way to
# write `every_rejection_path_records_a_miss` and creates no cycle in the lib
# graph. A gate that forbade it would be enforcing a rule §6.1 does not state.
set -euo pipefail
cd "$(dirname "$0")/../.."

# §6.1's order, one tier per line, lowest first. A crate may depend only on
# crates in a STRICTLY lower tier.
#
# `apex-obs` sits at tier 1 because §6.1 says it "is depended on by nearly
# everything but depends only on apex-types, so it never creates a cycle".
tiers=(
  "apex-types"
  "apex-config apex-math apex-obs"
  "apex-state"
  "apex-venues apex-chain"
  "apex-search"
  "apex-econ"
  "apex-sim"
  "apex-risk"
  "apex-exec"
  "apex-capture"
  "apex-strategy"
  "apex-runtime"
  "apex-tools"
)

tier_of() {
  local name="$1" i=0
  for row in "${tiers[@]}"; do
    for c in $row; do
      [ "$c" = "$name" ] && { echo "$i"; return 0; }
    done
    i=$((i + 1))
  done
  echo "-1"
}

violations=""
unplaced=""

for manifest in crates/*/Cargo.toml; do
  crate=$(basename "$(dirname "$manifest")")
  # arb-exec-legacy is outside the graph: it retires in Phase 17 and everything
  # still in it is by definition not yet placed.
  [ "$crate" = "arb-exec-legacy" ] && continue

  mine=$(tier_of "$crate")
  if [ "$mine" = "-1" ]; then
    unplaced="${unplaced}  ${crate}"$'\n'
    continue
  fi

  # Only the [dependencies] section, up to the next [section].
  deps=$(awk '/^\[dependencies\]/{f=1;next} /^\[/{f=0} f' "$manifest" \
         | grep -oE '^apex-[a-z-]+' || true)

  for dep in $deps; do
    theirs=$(tier_of "$dep")
    if [ "$theirs" = "-1" ]; then
      violations="${violations}  ${crate} -> ${dep}   (${dep} is not in §6.1's graph)"$'\n'
    elif [ "$theirs" -ge "$mine" ]; then
      violations="${violations}  ${crate} (tier ${mine}) -> ${dep} (tier ${theirs})"$'\n'
    fi
  done
done

if [ -n "$unplaced" ]; then
  echo "§6.1 VIOLATION: a crate exists that the dependency graph does not place." >&2
  printf '%s' "$unplaced" >&2
  echo "Add it to the tier list above, or to §6.1, whichever is actually wrong." >&2
  exit 1
fi

if [ -n "$violations" ]; then
  echo "§6.1 VIOLATION: a dependency runs the wrong way." >&2
  printf '%s' "$violations" >&2
  echo >&2
  echo "A crate may depend only on a STRICTLY lower tier. cargo will not catch" >&2
  echo "this -- one wrong-direction edge is not a cycle. Move the shared type" >&2
  echo "down to a crate both sides can see; apex_types::ack and" >&2
  echo "apex_state::feed::event are the two worked examples." >&2
  exit 1
fi

placed=0
for row in "${tiers[@]}"; do for c in $row; do [ -d "crates/$c" ] && placed=$((placed + 1)); done; done
echo "ok: §6.1's dependency direction holds across ${placed} crates"
