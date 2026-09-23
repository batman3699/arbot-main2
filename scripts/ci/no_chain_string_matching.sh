#!/usr/bin/env bash
# §20: "A CI grep fails the build on `match chain_name` / `if chain == "base"`
# outside `apex-chain`."
#
# The defect this closes is not stylistic. Chain identity in the legacy binary
# is a `&str` matched in `main.rs` -- `chain_hot_pool_base_cap(chain_name)`,
# `derive_chain_event_sampling_rate(chain_name)`, `derive_chain_time_budget_ms`
# -- so adding a chain means finding every such branch, and MISSING one means a
# new chain silently inherits Base's economics. §3.5 records 338 of these in the
# legacy crate; they retire with it in Phase 15/17, so this gate is scoped to
# the new crates exactly as `no_shared_mutable_state.sh` is.
#
# Two patterns, and the distinction between them matters:
#
#   1. A chain identifier compared against a string LITERAL. Looking a chain up
#      by a name held in a variable is fine and `apex-config` does it -- that is
#      a lookup, not a branch. Comparing against `"base"` in source is a branch.
#   2. A chain id compared against a `ChainId::` constant. Same defect wearing a
#      type: `if c.chain_id == ChainId::BASE { .. } else { .. }` is the `&str`
#      match with better spelling.
#
# What this does NOT catch: a behaviour branch keyed on a chain id held in a
# variable that was itself derived from a literal elsewhere. Stated so the gate
# is not mistaken for a proof.
set -euo pipefail
cd "$(dirname "$0")/../.."

scope=$(ls -d crates/apex-*/src 2>/dev/null | grep -v '^crates/apex-chain/' || true)
[ -n "$scope" ] || { echo "no apex-* crates to check"; exit 0; }

strip_comments() {
  grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|/\*|\*)'
}

literal='(chain|chain_name|chain_id|network)[[:space:]]*[!=]=[[:space:]]*"'
if hits=$(grep -rEn "$literal" $scope 2>/dev/null | strip_comments) && [ -n "$hits" ]; then
  echo "§20 VIOLATION: chain identity compared against a string literal." >&2
  echo "$hits" >&2
  echo >&2
  echo "Chain-specific BEHAVIOUR belongs on a ChainExecutionAdapter method, not" >&2
  echo "in a branch. Looking a chain up by a name held in a variable is fine;" >&2
  echo "this is a name written into the source." >&2
  exit 1
fi

constant='[!=]=[[:space:]]*ChainId::[A-Z]'
if hits=$(grep -rEn "$constant" $scope 2>/dev/null | strip_comments) && [ -n "$hits" ]; then
  echo "§20 VIOLATION: behaviour branched on a ChainId constant." >&2
  echo "$hits" >&2
  echo >&2
  echo "This is the &str match with better spelling. Put the difference behind" >&2
  echo "a ChainExecutionAdapter method so a new chain must answer for itself." >&2
  exit 1
fi

matcharm='match[[:space:]]+[a-z_.]*chain(_name|_id)?[[:space:]]*\{'
if hits=$(grep -rEn "$matcharm" $scope 2>/dev/null | strip_comments) && [ -n "$hits" ]; then
  echo "§20 VIOLATION: a match on chain identity." >&2
  echo "$hits" >&2
  exit 1
fi

echo "ok: no chain-identity branching outside apex-chain"
