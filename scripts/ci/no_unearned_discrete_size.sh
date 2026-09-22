#!/usr/bin/env bash
# INV-18: the final trade size must be an exact integer candidate produced by
# discrete refinement (Blueprint §14.3). A continuous optimum may warm-start the
# search; it may never become the executed size.
#
# DiscreteSize can only be built from a DiscreteRefined witness, and this script
# is what keeps that witness earned: the minter is `pub` because apex-econ has to
# call it across a crate boundary, and Rust has no "visible to exactly these
# crates".
set -euo pipefail
cd "$(dirname "$0")/../.."

allowed='^crates/apex-econ/src/sizing/'
violations=""
while IFS= read -r line; do
  file="${line%%:*}"
  # The definition and its own tests are not call sites.
  case "$file" in
    crates/apex-types/src/candidate.rs) continue ;;
    crates/apex-types/tests/*)          continue ;;
  esac
  if ! echo "$file" | grep -qE "$allowed"; then
    violations="${violations}${line}\n"
  fi
done < <(grep -rn 'DiscreteRefined::new()' crates/ 2>/dev/null || true)

if [ -n "$violations" ]; then
  echo "INV-18 VIOLATION: DiscreteRefined::new() called outside the refinement path." >&2
  printf "%b" "$violations" >&2
  echo "Only crates/apex-econ/src/sizing/ may mint a refinement witness." >&2
  exit 1
fi
echo "ok: DiscreteSize is only minted from the refinement path"
