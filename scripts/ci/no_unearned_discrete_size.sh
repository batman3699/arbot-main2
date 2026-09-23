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
  # The definition is not a call site.
  #
  # Integration tests are exempt, and the reason is structural rather than
  # convenience: `crates/*/tests/**` compiles to separate binaries that link the
  # library, so nothing in `src/` can call into them. A witness minted there can
  # never reach production code, which is what INV-18 is about. Inline
  # `#[cfg(test)]` modules under `src/` are NOT exempt -- they share the
  # compilation unit, and a helper there is one `cfg` edit away from being
  # reachable.
  case "$file" in
    crates/apex-types/src/candidate.rs) continue ;;
    crates/*/tests/*)                   continue ;;
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
