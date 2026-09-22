#!/usr/bin/env bash
# No source file may be invisible to git.
#
# `.gitignore` carries `*secret*` to keep credentials out of a public
# repository. It also swallowed `crates/apex-config/src/secret.rs` -- a module
# declared by `mod secret;` -- so `apex-config` did not compile from a clean
# checkout of any commit in Phase 0 or Phase 1. Every check had run against the
# working tree, where the file exists.
#
# This gate asks the question those checks could not: is there anything the
# build needs that a fresh clone would not get? It is cheap, it runs in
# milliseconds, and it fails loudly the moment a pattern eats a file again.
set -euo pipefail

roots=(crates scripts/ci .github)
offenders=""

for root in "${roots[@]}"; do
  [ -d "$root" ] || continue
  # Every file the build could plausibly need, minus build output.
  while IFS= read -r -d '' f; do
    case "$f" in
      */target/*) continue ;;
    esac
    if git check-ignore -q "$f"; then
      offenders="${offenders}${f}"$'\n'
    fi
  done < <(find "$root" -type f \
             \( -name '*.rs' -o -name '*.toml' -o -name '*.sh' -o -name '*.yml' \) -print0)
done

# Tracked-but-ignored is fine (git keeps tracking what it already tracks); it is
# UNtracked-and-ignored that never reaches a clone.
untracked=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  if ! git ls-files --error-unmatch "$f" >/dev/null 2>&1; then
    untracked="${untracked}  ${f}"$'\n'
  fi
done <<< "$offenders"

if [ -n "$untracked" ]; then
  echo "[no_ignored_sources] these source files are gitignored and untracked," >&2
  echo "[no_ignored_sources] so a clean checkout does not have them:" >&2
  printf '%s' "$untracked" >&2
  echo "[no_ignored_sources] add a per-path negation to .gitignore and commit them." >&2
  exit 1
fi

echo "[no_ignored_sources] ok: no untracked source hidden by .gitignore"
