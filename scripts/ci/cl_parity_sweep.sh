#!/usr/bin/env bash
# Sweep cl_parity across the deepest pools at sizes chosen to cross ticks.
#
# A single cl_parity run proves little: on a deep pool, ordinary sizes never
# leave the current tick, so single-tick and multi-tick agree trivially and the
# run PASSes without testing anything. The handoff doc's acceptance criterion
# (§6.2) is deliberately about sizes that provably cross >=2 tick boundaries.
#
# One invocation per pool so the tick ladder is built once and reused across
# that pool's whole size ladder.
#
# Usage: scripts/ci/cl_parity_sweep.sh [pool_count] [sizes_per_pool]
set -uo pipefail

POOLS=${1:-8}
STEPS=${2:-14}
INV=data/base/uniswap_v3/pools.jsonl
BIN=./target/release/cl_parity

[ -x "$BIN" ] || { echo "build first: cargo build --release --bin cl_parity"; exit 1; }
: "${ARBOT_RPC_URL:?set ARBOT_RPC_URL}"

OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT

# Deepest pools first: they need the largest sizes to cross a tick, which is
# exactly where the single-tick model's error becomes economically relevant.
mapfile -t ROWS < <(python3 -c "
import json
p=[json.loads(l) for l in open('$INV') if l.strip()]
p.sort(key=lambda r:-(r.get('hub_usd_liquidity') or 0))
for r in p[:$POOLS]:
    print(r['pool'], r['fee'])
")

echo "sweeping ${#ROWS[@]} pools x $STEPS sizes"
for row in "${ROWS[@]}"; do
  pool=${row%% *}; fee=${row##* }
  t0=$(cast call "$pool" "token0()(address)" --rpc-url "$ARBOT_RPC_URL" 2>/dev/null | head -1)
  dec=$(cast call "$t0" "decimals()(uint8)" --rpc-url "$ARBOT_RPC_URL" 2>/dev/null | awk '{print $1}')
  [ -z "${dec:-}" ] && { echo "  $pool: token0 decimals unavailable, skipped"; continue; }

  # ladder is meaningless across pools that differ by orders of magnitude in
  # depth, and simply exhausts the tick ladder on thin ones.
  bal=$(cast call "$t0" "balanceOf(address)(uint256)" "$pool" --rpc-url "$ARBOT_RPC_URL" 2>/dev/null | awk '{print $1}')
  [ -z "${bal:-}" ] || [ "$bal" = "0" ] && { echo "  $pool: token0 balance unavailable, skipped"; continue; }
  sizes=$(python3 -c "
bal=int('$bal'); n=$STEPS
# 0.01% .. 50% of pool depth: small stays in-tick, large forces crossings.
print(' '.join(str(max(1,int(bal * (10 ** (-4 + i*(3.7)/(n-1)))))) for i in range(n)))
")
  # shellcheck disable=SC2086
  res=$("$BIN" "$pool" "$fee" $sizes 2>&1)
  echo "$res" | grep -E '^[0-9]+,' | sed "s|^|$pool,|" >> "$OUT"
  hdr=$(echo "$res" | grep -oP 'ticks=\K[0-9]+' | head -1)
  echo "  $pool fee=$fee dec=$dec ladder_ticks=${hdr:-?} $(echo "$res" | grep -c '^[0-9]*,') samples"
done

python3 - "$OUT" <<'PY'
import sys, csv
rows=[r for r in csv.reader(open(sys.argv[1])) if len(r)==8]
if not rows:
    print("\nNO SAMPLES"); raise SystemExit(1)
# Rows are the cl_parity CSV prefixed with the pool, so:
#   0 pool | 1 amount_in | 2 single | 3 multi | 4 ticks | 5 exhausted | 6 single_err | 7 multi_err
judged=[r for r in rows if r[5].strip().lower()=="false"]
crossed=[r for r in judged if int(r[4])>=1]
crossed2=[r for r in judged if int(r[4])>=2]
def worst(rs,i): return max((abs(int(r[i])) for r in rs), default=0)
print(f"\n=== cl_parity sweep ===")
print(f"  total samples          : {len(rows)}")
print(f"  judged (not exhausted) : {len(judged)}")
print(f"  crossed >=1 tick       : {len(crossed)}")
print(f"  crossed >=2 ticks      : {len(crossed2)}   <-- the acceptance population")
print(f"\n  worst |multi_tick err| , all judged : {worst(judged,7)} bps")
print(f"  worst |multi_tick err| , >=2 ticks  : {worst(crossed2,7)} bps")
print(f"  worst |single_tick err|, >=2 ticks  : {worst(crossed2,6)} bps  (what multi-tick replaces)")
bad=[r for r in crossed2 if abs(int(r[7]))>5]
print(f"\n  multi-tick samples over 5 bps: {len(bad)}")
for r in bad[:5]:
    print(f"    pool={r[0][:12]}.. amount_in={r[1]} ticks={r[4]} err={r[7]}bps")
print("\nRESULT:", "PASS" if not bad and crossed2 else ("NO TICK-CROSSING COVERAGE" if not crossed2 else "FAIL"))
PY
