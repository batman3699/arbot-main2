#!/usr/bin/env bash
# B-13: `vm.setEnv` writes the *forge process* environment. It is global, shared
# by every test in the run, and it persists -- there is no `unsetEnv` cheatcode
# in forge 1.7. So two test files that write the same key are racing over one
# mutable variable, and forge does not promise an order. That is not a flaky
# test; it is a test suite with shared mutable state, and it is why
# `test/Deploy*` sat behind `--no-match-path` from Phase 0 until Task 5.7.
#
# The rule this enforces is the narrowest one that closes the mechanism: a key
# may be written by at most one file. Within a file the writes are ordered by
# the code, so a single owner can reason about its own key; across files nobody
# can.
#
# This does not need to also check readers. `script/Deploy.s.sol` is the only
# file in the Solidity tree that calls `vm.env*` at all (the check below asserts
# that, so the reasoning cannot go stale), and the only test that reaches a read
# does so through a harness in the one file that owns `CHAIN`.
set -euo pipefail
cd "$(dirname "$0")/../.."

readers=$(grep -rl 'vm\.env[A-Z]' test/ script/ contracts/ 2>/dev/null | grep -v '^script/Deploy.s.sol$' || true)
if [[ -n "$readers" ]]; then
  echo "B-13 VIOLATION: a second file reads the process environment." >&2
  echo "$readers" >&2
  echo >&2
  echo "script/Deploy.s.sol::configFromEnv is meant to be the only env reader," >&2
  echo "which is what makes the one-writer-per-key rule below sufficient. A new" >&2
  echo "reader means this gate must start checking reads against writes too." >&2
  exit 1
fi

# key<TAB>file, deduplicated, then any key with more than one distinct file.
contended=$(
  grep -rHo 'vm\.setEnv("[A-Za-z0-9_]*"' test/ 2>/dev/null \
    | sed 's/^\([^:]*\):vm\.setEnv("\(.*\)"$/\2\t\1/' \
    | sort -u \
    | awk -F'\t' '{n[$1]++; where[$1] = where[$1] "  " $2 "\n"} END {for (k in n) if (n[k] > 1) printf "%s written by %d files:\n%s", k, n[k], where[k]}'
)
if [[ -n "$contended" ]]; then
  echo "B-13 VIOLATION: two test files contend for one process-global env key." >&2
  echo "$contended" >&2
  echo "Give the key one owning file, or have the other file take the value as" >&2
  echo "an argument -- script/Deploy.s.sol::runWith and ::resolveConfig exist so" >&2
  echo "that a test can deploy without the environment reaching in at all." >&2
  exit 1
fi
echo "ok: every env key written by test/ has exactly one owning file"
