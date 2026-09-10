#!/usr/bin/env python3
"""
Enumerate every Aerodrome Slipstream pool from its factory, the way
`pools.factory-full.jsonl` was built for Uniswap V3.

WHY
---
Measured 2026-09-06/11. Base's Uniswap V3 cheap tier is a dead end: of the
46,603 hub-anchored pools at the 1bp/5bp tiers, only 37 hold more than $250k and
370 hold more than $1,000, so the 207 already on disk had captured essentially
all of it (see scripts/data/rebuild_cheap_inventory.py, which aborted rather
than write 14 records over a 1,126-pool inventory).

Aerodrome is where the coverage gap actually is. Of 170 Slipstream pools held,
26 are both cheap (<=5 bps) and deep (>=$250k) -- a 15% hit rate against Uniswap
V3's 1.2% -- and those 170 came from a GeckoTerminal scrape with no factory
enumeration behind them. The factories report 3,620 + 1,409 = 5,029 pools, so
the bot has been trading 3.4% of Base's dominant DEX.

HOW
---
`CLFactory` keeps an enumerable array, so this needs no log scan: read
`allPoolsLength()`, then `allPools(i)`, then token0/token1/tickSpacing/fee per
pool. Everything goes through Multicall3 `aggregate3` (200 sub-calls per
eth_call) because mainnet.base.org caps JSON-RPC batches at 10 and rate-limits.
~5,000 pools is roughly 130 requests.

FEE vs TICK SPACING
-------------------
`PoolRecord.fee` is the venue's POOL KEY, and for Slipstream that key is the
TICK SPACING, not the fee -- the router resolves a Slipstream path by spacing
(see the note in venues.rs and build_slipstream_pools.py). So `fee` carries the
spacing, exactly as the existing inventory does, and the real per-pool fee is
recorded separately as `fee_ppm_onchain`. Slipstream fees are dynamic and are
NOT derivable from the spacing: measured pools with spacing 100 charge 2500 ppm
and 212 ppm. `PoolRecordJson` ignores unknown fields, so the extra key is inert
to the runtime and available to the ranker.

Output: data/base/<venue>/pools.factory-full.jsonl  (never the live inventory)

Usage:  python3 scripts/data/enumerate_aerodrome_pools.py
Env:    AERO_RPC_URLS, AERO_BATCH (200), AERO_PACE_S (1.2), AERO_VENUES
"""

import json
import os
import sys
import time
import urllib.request
from itertools import cycle

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from keccak_lite import selector, selftest  # noqa: E402

DEFAULT_RPCS = "https://mainnet.base.org,https://base.gateway.tenderly.co"
RPCS = [u.strip() for u in os.environ.get("AERO_RPC_URLS", DEFAULT_RPCS).split(",") if u.strip()]
BATCH = int(os.environ.get("AERO_BATCH", "200"))
PACE_S = float(os.environ.get("AERO_PACE_S", "1.2"))

MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
AGGREGATE3 = selector("aggregate3((address,bool,bytes)[])")

VENUES = {
    "aerodrome_slipstream": "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A",
    "aerodrome_slipstream_v3": "0xf8f2eB4940CFE7d13603DDDD87f123820Fc061Ef",
}
if os.environ.get("AERO_VENUES"):
    want = {v.strip() for v in os.environ["AERO_VENUES"].split(",")}
    VENUES = {k: v for k, v in VENUES.items() if k in want}

SEL_LEN = selector("allPoolsLength()")
SEL_AT = selector("allPools(uint256)")
SEL_T0 = selector("token0()")
SEL_T1 = selector("token1()")
SEL_TS = selector("tickSpacing()")
SEL_FEE = selector("fee()")

_rpc = cycle(RPCS)


def post(payload, timeout=60):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        next(_rpc), body, {"Content-Type": "application/json", "User-Agent": "arbot-enum/1"}
    )
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def encode_aggregate3(calls):
    n = len(calls)
    tuples = []
    for target, data in calls:
        raw = bytes.fromhex(data[2:])
        t = bytes(12) + bytes.fromhex(target[2:])
        t += (1).to_bytes(32, "big")   # allowFailure: one odd pool must not void 199 reads
        t += (96).to_bytes(32, "big")
        t += len(raw).to_bytes(32, "big") + raw + bytes((-len(raw)) % 32)
        tuples.append(t)
    heads, off = b"", 32 * n
    for t in tuples:
        heads += off.to_bytes(32, "big")
        off += len(t)
    return AGGREGATE3 + ((32).to_bytes(32, "big") + n.to_bytes(32, "big")
                         + heads + b"".join(tuples)).hex()


def decode_aggregate3(result_hex):
    b = bytes.fromhex(result_hex[2:])
    arr = int.from_bytes(b[:32], "big")
    n = int.from_bytes(b[arr:arr + 32], "big")
    base = arr + 32
    out = []
    for i in range(n):
        o = base + int.from_bytes(b[base + 32 * i:base + 32 * i + 32], "big")
        ok = int.from_bytes(b[o:o + 32], "big") == 1
        d = o + int.from_bytes(b[o + 32:o + 64], "big")
        ln = int.from_bytes(b[d:d + 32], "big")
        out.append((ok, b[d + 32:d + 32 + ln]))
    return out


def multicall(calls):
    """calls: [(target, data)] -> [(ok, bytes)]; [] means the read failed."""
    payload = {
        "jsonrpc": "2.0", "id": 1, "method": "eth_call",
        "params": [{"to": MULTICALL3, "data": encode_aggregate3(calls)}, "latest"],
    }
    for attempt in range(5):
        try:
            time.sleep(PACE_S)
            data = post(payload)
            res = data.get("result")
            if isinstance(res, str) and len(res) > 2:
                return decode_aggregate3(res)
            if attempt == 0 and data.get("error"):
                print(f"    rpc error: {str(data['error'])[:90]}", file=sys.stderr)
        except Exception as exc:
            if attempt == 0:
                print(f"    transport: {type(exc).__name__} {str(exc)[:70]}", file=sys.stderr)
        time.sleep(min(2.0 * (2 ** attempt), 30.0))
    return []


def single(target, data):
    payload = {"jsonrpc": "2.0", "id": 1, "method": "eth_call",
               "params": [{"to": target, "data": data}, "latest"]}
    for attempt in range(5):
        try:
            time.sleep(PACE_S)
            r = post(payload).get("result")
            if isinstance(r, str) and len(r) >= 66:
                return r
        except Exception:
            pass
        time.sleep(1.5 * (attempt + 1))
    return None


def addr_of(word):
    return "0x" + word[-20:].hex()


def i24(word):
    v = int.from_bytes(word[:32], "big")
    return v - (1 << 256) if v >= (1 << 255) else v


def enumerate_venue(venue, factory):
    print(f"\n=== {venue}  factory {factory} ===", flush=True)
    raw = single(factory, SEL_LEN)
    if raw is None:
        print("  ABORT: allPoolsLength() unreadable", file=sys.stderr)
        return None
    total = int(raw[:66], 16)
    print(f"  allPoolsLength() = {total:,}", flush=True)
    if total == 0 or total > 500_000:
        print(f"  ABORT: implausible pool count {total}", file=sys.stderr)
        return None

    pools = []
    for start in range(0, total, BATCH):
        idxs = range(start, min(start + BATCH, total))
        calls = [(factory, SEL_AT + i.to_bytes(32, "big").hex()) for i in idxs]
        for ok, ret in multicall(calls):
            if ok and len(ret) >= 32:
                pools.append(addr_of(ret[:32]))
        if (start // BATCH) % 5 == 0:
            print(f"    addresses {len(pools):,}/{total:,}", flush=True)
    print(f"  collected {len(pools):,} pool addresses", flush=True)

    # token0/token1/tickSpacing/fee, four sub-calls per pool in one stream.
    per = max(1, BATCH // 4)
    recs = []
    for start in range(0, len(pools), per):
        chunk = pools[start:start + per]
        calls = []
        for p in chunk:
            calls += [(p, SEL_T0), (p, SEL_T1), (p, SEL_TS), (p, SEL_FEE)]
        res = multicall(calls)
        if len(res) != len(calls):
            continue
        for i, p in enumerate(chunk):
            ok0, r0 = res[4 * i]
            ok1, r1 = res[4 * i + 1]
            oks, rs = res[4 * i + 2]
            okf, rf = res[4 * i + 3]
            if not (ok0 and ok1 and oks) or len(r0) < 32 or len(r1) < 32 or len(rs) < 32:
                continue
            spacing = i24(rs)
            if spacing <= 0:
                continue
            fee_ppm = int.from_bytes(rf[:32], "big") if (okf and len(rf) >= 32) else None
            recs.append({
                "pool": p,
                "token0": addr_of(r0[:32]),
                "token1": addr_of(r1[:32]),
                # POOL KEY, not the fee: Slipstream resolves paths by spacing.
                "fee": spacing,
                "created_block": 0,
                "hub_usd_liquidity": None,
                # The real fee, which the key cannot supply -- spacing 100 pools
                # were measured at both 2500 ppm and 212 ppm.
                "fee_ppm_onchain": fee_ppm,
            })
        if (start // per) % 5 == 0:
            print(f"    metadata {len(recs):,}/{len(pools):,}", flush=True)
    return recs


def main():
    selftest()
    grand = 0
    for venue, factory in VENUES.items():
        recs = enumerate_venue(venue, factory)
        if not recs:
            print(f"  {venue}: nothing collected; leaving files alone", file=sys.stderr)
            continue
        out_dir = os.path.join("data", "base", venue)
        os.makedirs(out_dir, exist_ok=True)
        out = os.path.join(out_dir, "pools.factory-full.jsonl")
        tmp = out + ".tmp"
        with open(tmp, "w") as fh:
            for r in recs:
                fh.write(json.dumps(r) + "\n")
        os.replace(tmp, out)
        fees = [r["fee_ppm_onchain"] for r in recs if r["fee_ppm_onchain"]]
        cheap = sum(1 for f in fees if f <= 500)
        print(f"  wrote {len(recs):,} records -> {out}")
        print(f"  on-chain fee read for {len(fees):,}; {cheap:,} at <= 500 ppm")
        grand += len(recs)
    print(f"\ntotal enumerated: {grand:,}")
    print("NOTE: this writes pools.factory-full.jsonl only. Rank it into the live")
    print("inventory with rebuild_cheap_inventory.py, which fails closed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
