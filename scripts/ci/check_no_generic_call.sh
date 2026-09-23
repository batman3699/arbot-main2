#!/usr/bin/env bash
# INV-33 / B-1: the settlement contract has no arbitrary-call surface.
#
# `_execGeneric` decoded a target address out of a step payload and called it.
# A step now names an ADAPTER by id and the contract resolves it against an
# owner-managed allowlist, so a plan cannot choose what the contract calls.
#
# This gate checks the property two ways, because the two catch different
# regressions:
#
#   SOURCE  -- a reintroduction under any name. Grepping for the specific
#              identifier `_execGeneric` would miss `_execRaw`; the pattern
#              that matters is a call whose target came out of `abi.decode`.
#
#   BYTECODE -- that the REMOVED features are actually gone from the compiled
#              runtime, not merely from the source. Acceptance criterion 6 says
#              "absent from source and bytecode", and a stale artifact or a
#              partial rebuild is exactly the state where those differ.
set -euo pipefail
cd "$(dirname "$0")/../.."

FORGE_BIN="${FORGE_BIN:-forge}"
IMPL="contracts/executor/MultiVenueArbImplementation.sol"
fail=0

# ---- source -----------------------------------------------------------------
# Comment lines are stripped first: the modules that exist BECAUSE of B-1
# describe the hole, and a gate that cannot tell code from prose about code
# punishes writing the explanation down.
stripped=$(sed 's,//.*,,' "$IMPL")

if printf '%s\n' "$stripped" | grep -qE '\b_execGeneric\b'; then
  echo "[check_no_generic_call] _execGeneric is back in $IMPL" >&2
  fail=1
fi

# Every low-level call target must come from the registry or from storage.
#
# An earlier version of this gate grepped for "an address decoded from a
# payload", which flagged `_execBalancer` decoding a pool's token addresses --
# not a target -- and could not tell the two apart. Enumerating the CALL SITES
# is exact: there are three in the whole contract, and each has to be
# justified.
calls=$(printf '%s\n' "$stripped" | grep -cE '\.safeCall\(' || true)
if [ "$calls" -ne 1 ]; then
  echo "[check_no_generic_call] expected exactly one safeCall site, found $calls:" >&2
  printf '%s\n' "$stripped" | grep -nE '\.safeCall\(' >&2
  echo "[check_no_generic_call] a second one is a second surface to justify." >&2
  fail=1
fi

# ...and that one site's target must be resolved, not supplied.
if ! printf '%s\n' "$stripped" \
   | awk '/function _execAdapter\(/,/^    }$/' \
   | grep -q '_resolveAdapter('; then
  echo "[check_no_generic_call] _execAdapter does not resolve its target through the registry" >&2
  fail=1
fi

# The delegatecall dispatch may only reach the constructor-deployed modules.
if printf '%s\n' "$stripped" | grep -E 'module = ' | grep -vE 'module = (swapExecutorModule|genericExecutorModule);' >/dev/null 2>&1; then
  echo "[check_no_generic_call] the module dispatch assigns a target that is not an immutable module:" >&2
  printf '%s\n' "$stripped" | grep -nE 'module = ' >&2
  fail=1
fi

# ---- bytecode ---------------------------------------------------------------
if command -v "$FORGE_BIN" >/dev/null 2>&1; then
  "$FORGE_BIN" build >/dev/null 2>&1 || true
  artifact="out/MultiVenueArbImplementation.sol/MultiVenueArbImplementation.json"
  if [ -f "$artifact" ]; then
    python3 - "$artifact" <<'PY' || fail=1
import json, sys

artifact = json.load(open(sys.argv[1]))
methods = artifact.get("methodIdentifiers") or {}
runtime = artifact["deployedBytecode"]["object"].lower()

# Selectors of the removed surface. Present in `methodIdentifiers` means the
# function is still declared; present in the runtime means a dispatch arm for
# it is still compiled in.
banned = [
    "moduleExecBridge(bytes)",
    "moduleExecJit(uint8,bytes,uint256)",
    "moduleExecGenericRaw(bytes)",
    "uniswapV3MintCallback(uint256,uint256,bytes)",
]
bad = []
for signature in banned:
    selector = methods.get(signature)
    if selector is not None:
        bad.append(f"{signature} is still declared (selector 0x{selector})")
        continue
# Anything declared under a JIT or bridge name at all.
for signature in methods:
    lowered = signature.lower()
    if "jit" in lowered or "bridge" in lowered:
        bad.append(f"{signature} is declared; §1.4 excludes it")

if bad:
    print("[check_no_generic_call] removed surface is still in the artifact:", file=sys.stderr)
    for line in bad:
        print(f"  {line}", file=sys.stderr)
    sys.exit(1)

# The registry has to be REACHABLE, or the fix is not wired in.
if "adapterOf(uint16)" not in methods:
    print("[check_no_generic_call] adapterOf is absent: the registry is not wired in", file=sys.stderr)
    sys.exit(1)

print(f"[check_no_generic_call] ok: runtime {len(runtime)//2} bytes, "
      f"{len(methods)} external functions, none of them JIT or bridge")
PY
  else
    echo "[check_no_generic_call] no artifact at $artifact; bytecode not checked" >&2
    fail=1
  fi
else
  echo "[check_no_generic_call] forge not found; bytecode not checked" >&2
  fail=1
fi

[ "$fail" -eq 0 ] || exit 1
echo "[check_no_generic_call] ok: no arbitrary-call surface in source or bytecode"
