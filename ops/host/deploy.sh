#!/usr/bin/env bash
# Package a committed revision and the pool inventory for the co-located host
# (Task 8.5 R25). Run on the operator's machine, from the repository:
#
#   ops/host/deploy.sh <revision>
#
# writes dist/apex-<rev>.tar.gz (git archive of <rev>), dist/data-base.tar.gz
# (each venue's pools.jsonl: data/ is outside git), dist/REVISION and
# dist/MANIFEST.sha256, which the host checks before unpacking.
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
mkdir -p dist
rm -f dist/apex-*.tar.gz dist/data-base.tar.gz dist/MANIFEST.sha256 dist/REVISION
git archive --format=tar.gz --prefix=apex/ -o "dist/apex-$short.tar.gz" "$rev"
tar -czf dist/data-base.tar.gz "${files[@]}"
echo "$rev" > dist/REVISION
( cd dist && sha256sum "apex-$short.tar.gz" data-base.tar.gz REVISION > MANIFEST.sha256 )
echo "packaged $rev"
echo "manifest sha256 (compare on the host): $(sha256sum dist/MANIFEST.sha256 | cut -d' ' -f1)"
