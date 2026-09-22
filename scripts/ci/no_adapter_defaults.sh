#!/usr/bin/env bash
# No method of `VenueAdapter` may have a default body.
#
# PLAN.md §10.4: "No adapter may hide a meaningful economic assumption from the
# core engine -- enforced by `gas_model` and `classify_revert` being required
# methods with no default implementation."
#
# Rust cannot express "this trait may never gain a default body", and the rule
# has to survive methods that do not exist yet: §10.4 sketches seven methods and
# two of them (simulate_call_graph, encode_exact) arrive with Phases 4 and 5.
# When they do, the path of least resistance is a default body so the existing
# adapters keep compiling -- which is the exact failure the rule guards against.
#
# So: read the trait block, and fail if any method signature ends in `{`
# instead of `;`.
set -euo pipefail

file="crates/apex-venues/src/adapter.rs"
[ -f "$file" ] || { echo "[no_adapter_defaults] $file is gone" >&2; exit 1; }

# The trait body: from `pub trait VenueAdapter` to its closing brace at column 0.
body=$(awk '
  /^pub trait VenueAdapter/ { inside = 1; next }
  inside && /^}/            { exit }
  inside                    { print }
' "$file")

[ -n "$body" ] || { echo "[no_adapter_defaults] trait VenueAdapter not found in $file" >&2; exit 1; }

# A method declaration is a line containing `fn <name>(`. Without a body it
# ends in `;`; with one it ends in `{`.
offenders=$(printf '%s\n' "$body" | grep -E '\bfn [a-z_]+\(' | grep -E '\{\s*$' || true)

if [ -n "$offenders" ]; then
  echo "[no_adapter_defaults] these VenueAdapter methods have default bodies:" >&2
  printf '%s\n' "$offenders" >&2
  echo "[no_adapter_defaults] every method must be required -- an adapter that" >&2
  echo "[no_adapter_defaults] inherits an answer is an adapter nobody checked." >&2
  exit 1
fi

count=$(printf '%s\n' "$body" | grep -cE '\bfn [a-z_]+\(' || true)
echo "[no_adapter_defaults] ok: $count required methods, no default bodies"
