#!/usr/bin/env python3
"""
Check every pool in every Base inventory against the factory that actually
deployed it, and report the ones filed under the wrong venue.

WHY
---
A pool's venue decides which quoter prices it. Quote a PancakeSwap pool through
the Uniswap quoter and you do not get an error -- the quoter resolves ITS OWN
pool for that token pair and fee tier and prices a different pool entirely. The
inventory looks fine and the number is wrong.

Found 2026-09-11: 33 pools sat in BOTH data/base/uniswap_v3 and
data/base/pancakeswap_v3. `factory()` says PancakeSwap owns them -- including
0xc211e1f853 ($3.6M, 1bp) and 0x72ab388e ($2.8M, 1bp), two of the deepest cheap
pools on Base and both load-bearing in the cross-venue pair analysis.

Reads `factory()` per pool through Multicall3 aggregate3, so a ~1,350-pool
inventory costs about 7 requests.

DRY_RUN=1 reports only. Without it a misfiled pool is MOVED when its real
factory is a configured venue -- the record format is identical and the move is
what makes its quoter correct -- and REMOVED when the factory is not configured
at all, because nothing can price it correctly and leaving it means pricing it
wrongly. Backs up every file it rewrites.

Usage:  DRY_RUN=1 python3 scripts/data/verify_pool_venues.py
Env:    VERIFY_RPC_URLS, VERIFY_BATCH (200), VERIFY_PACE_S (1.0)
"""

import glob
import json
import os
import re
import shutil
import sys
import time
import urllib.request
from itertools import cycle

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from keccak_lite import selector  # noqa: E402

RPCS = [u.strip() for u in os.environ.get(
    "VERIFY_RPC_URLS",
    "https://mainnet.base.org,https://base.gateway.tenderly.co,https://base-rpc.publicnode.com"
).split(",") if u.strip()]
BATCH = int(os.environ.get("VERIFY_BATCH", "200"))
PACE_S = float(os.environ.get("VERIFY_PACE_S", "1.0"))
DRY_RUN = os.environ.get("DRY_RUN", "").lower() in ("1", "true", "yes")

MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
AGGREGATE3 = selector("aggregate3((address,bool,bytes)[])")
SEL_FACTORY = selector("factory()")
_rpc = cycle(RPCS)


def load_factories():
    txt = open("ops/inputs.yaml").read()
    m = re.search(r"- chain_name: base(.*?)(?=\n- chain_name:|\Z)", txt, re.S)
    out = {}
    for vm in re.finditer(r"- name: (\S+)\n(.*?)(?=\n  - name: |\n  [a-z_]+:\n|\Z)",
                          m.group(1), re.S):
        f = re.search(r"^\s*factory: '(0x[0-9a-fA-F]{40})'", vm.group(2), re.M)
        if f:
            out[f.group(1).lower()] = vm.group(1)
    return out


def encode_aggregate3(calls):
    n = len(calls)
    tuples = []
    for target, data in calls:
        raw = bytes.fromhex(data[2:])
        t = bytes(12) + bytes.fromhex(target[2:])
        t += (1).to_bytes(32, "big") + (96).to_bytes(32, "big")
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


def factories_of(pools):
    """-> {pool_lower: factory_lower}. Absent means UNREAD, never 'wrong'."""
    found = {}
    for i in range(0, len(pools), BATCH):
        chunk = pools[i:i + BATCH]
        payload = {"jsonrpc": "2.0", "id": 1, "method": "eth_call",
                   "params": [{"to": MULTICALL3,
                               "data": encode_aggregate3([(p, SEL_FACTORY) for p in chunk])},
                              "latest"]}
        for attempt in range(5):
            try:
                time.sleep(PACE_S)
                body = json.dumps(payload).encode()
                req = urllib.request.Request(next(_rpc), body, {
                    "Content-Type": "application/json", "User-Agent": "arbot-verify/1"})
                res = json.loads(urllib.request.urlopen(req, timeout=60).read()).get("result")
                if isinstance(res, str) and len(res) > 2:
                    for (ok, ret), p in zip(decode_aggregate3(res), chunk):
                        if ok and len(ret) >= 32:
                            found[p.lower()] = "0x" + ret[12:32].hex()
                    break
            except Exception:
                pass
            time.sleep(min(2.0 * (2 ** attempt), 30.0))
    return found


def main():
    facts = load_factories()
    venues = set(facts.values())
    print(f"{len(facts)} configured Base factories"
          f"{'  [DRY_RUN]' if DRY_RUN else ''}")

    inventories = {}
    misfiled = {}
    for src in sorted(glob.glob("data/base/*/pools.jsonl")):
        venue = src.split("/")[2]
        if venue == "uniswap_v2":   # constant-product pairs expose no factory()
            continue
        rows = [json.loads(l) for l in open(src) if l.strip()]
        if not rows:
            continue
        inventories[venue] = rows
        got = factories_of([r["pool"] for r in rows])
        wrong, unread, foreign = [], 0, {}
        for r in rows:
            f = got.get(r["pool"].lower())
            if f is None:
                unread += 1
                continue
            owner = facts.get(f)
            if owner != venue:
                wrong.append((r, owner, f))
                key = owner or f
                foreign[key] = foreign.get(key, 0) + 1
        misfiled[venue] = wrong
        print(f"\n  {venue:<28} {len(rows):>5} pools   "
              f"misfiled {len(wrong):<5} unread {unread}")
        for k, v in sorted(foreign.items(), key=lambda x: -x[1]):
            where = "-> move" if k in venues else "-> remove (factory not configured)"
            print(f"      {v:>4} belong to {k}  {where}")

    total = sum(len(v) for v in misfiled.values())
    if DRY_RUN or total == 0:
        print(f"\ntotal misfiled: {total}")
        return 0

    # Apply: strip from the wrong file, add to the right one where there is one.
    incoming = {}
    for venue, wrong in misfiled.items():
        if not wrong:
            continue
        bad_ids = {id(r) for r, _o, _f in wrong}
        inventories[venue] = [r for r in inventories[venue] if id(r) not in bad_ids]
        for r, owner, _f in wrong:
            if owner in venues:
                incoming.setdefault(owner, []).append(r)

    moved = 0
    for owner, recs in incoming.items():
        target = os.path.join("data", "base", owner, "pools.jsonl")
        existing = inventories.get(owner)
        if existing is None:
            existing = [json.loads(l) for l in open(target)] if os.path.exists(target) else []
        by_pool = {r["pool"].lower(): r for r in recs}
        # Existing wins: the venue's own record may carry fields this move cannot.
        for r in existing:
            by_pool[r["pool"].lower()] = r
        inventories[owner] = list(by_pool.values())
        moved += len(recs)

    stamp = int(time.time())
    for venue, rows in inventories.items():
        src = os.path.join("data", "base", venue, "pools.jsonl")
        if os.path.exists(src):
            shutil.copy2(src, f"{src}.bak.venue-{stamp}")
        tmp = src + ".tmp"
        with open(tmp, "w") as fh:
            for r in rows:
                fh.write(json.dumps(r) + "\n")
        os.replace(tmp, src)
        print(f"  {venue:<28} -> {len(rows):>5} records")
    print(f"\ntotal misfiled {total}: moved {moved}, removed {total - moved}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
