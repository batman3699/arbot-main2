#!/usr/bin/env bash
# INV-46: "Private keys and credentials never enter logs, metrics or telemetry"
# (§43). The three `secret::*_redacts` tests prove the `Secret<T>` wrapper hides
# its contents; this proves nothing got in ahead of the wrapper.
#
# **Written 2026-09-24, and it had never existed.** PLAN.md §8 named it as
# INV-46's enforcement and `.github/workflows/ci.yml` carried a comment
# explaining why it was "NOT wired yet -- red on arrival, but only on test
# fixtures". Both described a file that was never committed. Found by rewriting
# `invariant_coverage.sh` to check enforcement artefacts other than Rust tests,
# which is the only reason a missing security gate was visible at all.
#
# This repository keeps a plaintext `PRIVATE_KEY` and provider API keys in an
# untracked `.env`, so the failure this guards against is concrete: one `git add
# -A`, one debug `println!`, one fixture pasted from a real config.
#
# **No blanket test allowance.** "Tests may contain secrets" is how a real key
# hides in a fixture. A finding is suppressed only by a marker at the site --
#
#     secret-scan:allow <why>
#
# within two lines of it. That forces the claim to be made where the value is,
# keeps it greppable for an audit, and the count is printed on every run so
# suppressions cannot accumulate invisibly. A blanket rule is invisible by
# construction; a marker is a thing somebody wrote down.
set -euo pipefail
cd "$(dirname "$0")/../.."

fail=0
report() {
  echo "SECRET SCAN: $1" >&2; shift
  # Truncated per line and capped in count: a finding is a line to look at, and
  # printing a minified bundle buries it.
  printf '%s\n' "$@" | head -20 | cut -c1-200 | sed 's/^/  /' >&2
  echo >&2; fail=1
}

# Only tracked files: an untracked scratch file with a key in it is a local
# hazard, not a leak.
#
# Two trees are excluded, and the exclusion is itself checked below so a third
# cannot be added quietly:
#
#   lib/   vendored forge dependencies. `forge-std` ships anvil's well-known
#          test key as a literal, which is a real 64-hex private key and is
#          also the most published private key in Ethereum. Scanning it teaches
#          us nothing and training a person to skip one finding is how the next
#          one gets skipped.
#   docs/  vendored third-party documentation, including minified JavaScript
#          bundles and an Infura example URL. Same reasoning.
#
# Everything we write is scanned, including tests, fixtures and config.
EXCLUDED='^(lib|docs)/'
mapfile -t tracked < <(git ls-files | grep -Ev "$EXCLUDED")

# The exclusion is a liability, so it is pinned. A new vendored tree has to be
# added here deliberately, which is the moment to ask whether it should be.
unexpected=$(git ls-files | grep -E "$EXCLUDED" | awk -F/ '{print $1}' | sort -u \
             | grep -vE '^(lib|docs)$' || true)
if [ -n "$unexpected" ]; then
  echo "SECRET SCAN: an unexpected tree matched the exclusion pattern:" >&2
  printf '  %s\n' $unexpected >&2
  exit 1
fi

# --- 1. Secret-bearing files must never be tracked. ---------------------------
bad_files=()
for f in "${tracked[@]}"; do
  case "$f" in
    .env|.env.*|*.pem|*.p12|*.pfx|*/id_rsa|*/id_ed25519|*.keystore) bad_files+=("$f") ;;
    *.key) case "$f" in *.pub.key) ;; *) bad_files+=("$f") ;; esac ;;
  esac
done
[ "${#bad_files[@]}" -gt 0 ] && report "a credential file is tracked in git" "${bad_files[@]}"

# --- 2. Private-key-shaped literals. -----------------------------------------
# 0x + exactly 64 hex. Hashes and codehashes are the same shape, so the filter
# is by CONTEXT: a literal on a line that also names a key, secret, mnemonic or
# wallet is a key; one next to `hash`, `B256`, `bytes32` or `keccak` is not.
keyish='(private[_ ]?key|privkey|secret|mnemonic|seed[_ ]?phrase|signing[_ ]?key|wallet)'
hashish='(hash|B256|H256|bytes32|keccak|root|parentHash|blockHash|codehash|selector|salt)'
#
# Windowed over three lines, not one. A multi-line assignment --
#
#     let private_key =
#         "0x<64 hex>";
#
# puts the name and the value on different lines, and a per-line grep sees a
# bare hex literal and a bare identifier and objects to neither. That is the
# shape the real mistake takes.
hits=$(for f in "${tracked[@]}"; do
  [ -f "$f" ] || continue
  awk -v F="$f" -v K="$keyish" -v H="$hashish" '
    { for (i = 5; i > 0; i--) w[i] = w[i-1]; w[0] = tolower($0); raw = $0 }
    /0[xX][0-9a-fA-F]{64}/ {
      # Detection looks back two lines: far enough for a multi-line assignment,
      # near enough that an unrelated mention does not implicate a hash.
      near = w[0] " " w[1] " " w[2]
      # Suppression looks back five, because a justification worth writing
      # takes more than one line and belongs above the value it excuses.
      far  = near " " w[3] " " w[4] " " w[5]
      if (near ~ K && near !~ H && far !~ /secret-scan:allow/)
        printf "%s:%d:%s\n", F, NR, raw
    }' "$f"
done || true)
[ -n "$hits" ] && report "a 64-hex literal sits on a line that names a key or secret" "$hits"

# --- 3. Credentials embedded in URLs. ----------------------------------------
# The provider endpoints this system uses put the API key in the path. A long
# opaque path segment on an rpc/provider URL is a credential.
#
# The allowed values are listed one by one on purpose. Each is a test fixture
# somebody has looked at; a fourth one appearing requires the same look.
allow_fixture='(test|test-key|abc123|adifferentkeyentirely|YOUR_KEY|<key>|\$\{[A-Z_]+\})'
url_hits=$(grep -rEn 'https?://[A-Za-z0-9.-]+/(v[0-9]+|rpc|ws)/[A-Za-z0-9_-]{8,}' -- "${tracked[@]}" 2>/dev/null \
           | grep -Ev "/(v[0-9]+|rpc|ws)/${allow_fixture}([\"'/[:space:],]|$)" || true)
[ -n "$url_hits" ] && report "a URL carries what looks like a real API key" "$url_hits"

# --- 4. A secret reaching a log, a metric or telemetry. ----------------------
# The invariant's actual words. `Secret<T>`'s redacting `Debug`/`Display` make
# the safe path safe; this catches the unsafe one -- reaching past the wrapper
# with `.expose()`/`.0`/`as_str()` inside a logging or metric call.
log_hits=$(grep -rEn '(tracing::)?(trace|debug|info|warn|error)!|println!|eprintln!|\.set\(|\.observe\(|\.inc_by\(' \
             -- "${tracked[@]}" 2>/dev/null \
           | grep -Ei "$keyish" \
           | grep -Ei '\.expose|\.expose_secret|\.secret\(\)|\.0\b|as_str\(\)' || true)
[ -n "$log_hits" ] && report "a secret is reached past its wrapper inside a log or metric call" "$log_hits"

if [ "$fail" -ne 0 ]; then
  echo "INV-46: private keys and credentials must never enter source, logs," >&2
  echo "metrics or telemetry (§43). Each finding above is a line to look at --" >&2
  echo "there is deliberately no blanket allowance for test files, because a" >&2
  echo "fixture is exactly where a real key hides." >&2
  exit 1
fi
suppressions=$(grep -rc 'secret-scan:allow' -- "${tracked[@]}" 2>/dev/null \
               | awk -F: '$2 > 0 {n += $2} END {print n + 0}')
echo "ok: no credential material in tracked source, logs or metrics (${suppressions} site suppressions)"
