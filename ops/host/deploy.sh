#!/usr/bin/env bash
# Package a committed revision and the pool inventory for the co-located host
# (Task 8.5 R25). Run on the operator's machine, from the repository:
#
#   ops/host/deploy.sh <revision>
#
# writes dist/apex-<rev>.tar.gz (git archive of <rev>), dist/data-base.tar.gz
# (each venue's pools.jsonl: data/ is outside git), dist/REVISION,
# dist/LeanExecutor2.runtime.hex (R37, only while <rev>'s config still has
# armed.executor2.runtime_code: the code the pre-deploy shadow simulates, its
# keccak checked against <rev>'s code_hash) and dist/MANIFEST.sha256, which
# the host checks before unpacking.
set -euo pipefail
cd "$(dirname "$0")/../.."
rev="$(git rev-parse --verify "${1:?usage: ops/host/deploy.sh <revision>}^{commit}")"
short="${rev:0:12}"
# The five venues the shadow loads (inventory::Venue::dir).
inventory=(uniswap_v3 aerodrome_slipstream pancakeswap_v3 aerodrome_slipstream_v3 aerodrome_v2)
files=()
for v in "${inventory[@]}"; do
  [[ -f "data/base/$v/pools.jsonl" ]] || { echo "deploy: data/base/$v/pools.jsonl is missing" >&2; exit 1; }
  files+=("data/base/$v/pools.jsonl")
done
# R37: the v4 inventory (ARMED.md §3e). Without it the host loads no v4 pool.
if [[ -f data/base/uniswap_v4/pools.jsonl ]]; then
  files+=("data/base/uniswap_v4/pools.jsonl")
else
  echo "deploy: no data/base/uniswap_v4/pools.jsonl: the host will load no v4 pool" >&2
fi
mkdir -p dist
rm -f dist/apex-*.tar.gz dist/data-base.tar.gz dist/MANIFEST.sha256 dist/REVISION dist/LeanExecutor2.runtime.hex
# R37: LeanExecutor2's runtime code, which must hash to the revision's
# armed.executor2.code_hash: the working tree's contracts must be <rev>'s.
# Only before the deploy: once deployed (ARMED.md §3c) the config has no
# runtime_code and the boot reads the chain's code. `forge inspect` builds the
# full set, whose via_ir bytecode differs from the deployed 18-file build's.
config="$(git show "$rev:ops/canary.base.yaml")"
want="$(awk '/^  executor2:/{f=1} f && /code_hash:/{gsub(/[" ]/, "", $2); print $2; exit}' <<<"$config")"
if [[ -n "$want" ]] && grep -q '^    runtime_code:' <<<"$config"; then
  forge inspect LeanExecutor2 deployedBytecode > dist/LeanExecutor2.runtime.hex
  got="$(cast keccak "$(cat dist/LeanExecutor2.runtime.hex)")"
  [[ "$got" == "$want" ]] || { echo "deploy: LeanExecutor2's runtime code hashes to $got, not $rev's code_hash $want" >&2; exit 1; }
fi
git archive --format=tar.gz --prefix=apex/ -o "dist/apex-$short.tar.gz" "$rev"
tar -czf dist/data-base.tar.gz "${files[@]}"
echo "$rev" > dist/REVISION
( cd dist && sha256sum "apex-$short.tar.gz" data-base.tar.gz REVISION $( [[ -f LeanExecutor2.runtime.hex ]] && echo LeanExecutor2.runtime.hex ) > MANIFEST.sha256 )
echo "packaged $rev"
echo "manifest sha256 (compare on the host): $(sha256sum dist/MANIFEST.sha256 | cut -d' ' -f1)"
