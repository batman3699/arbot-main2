#!/usr/bin/env python3
"""
Remove cheap pools too shallow to contribute an arbitrage opportunity.

WHY
---
Swept as a filter over one event-census window (sweep_depth_floor.py), so every
floor faced the same market:

     floor      cycles  instances  hit rate
         0       1,292         23     17.4%
    25,000         972         23     17.4%
   100,000         792         20     20.0%
   250,000         160          4      0.0%

A floor below $100k adds CYCLES but not INSTANCES: the best cycle within an
instance was already a deep one, so shallow pools add candidates to price and
not opportunities to take. $100k is the same hit rate against a third of the
cycles. The inventory was left carrying the sub-$100k pools because
rebuild_cheap_inventory.py never shrinks an inventory, deliberately -- so
shrinking gets its own script, with its own guards, rather than a flag that
would weaken that one.

SCOPE -- deliberately narrow
---------------------------
Removes ONLY pools that are all three of: cheap (<= 500 ppm), carrying a
MEASURED `hub_usd_liquidity`, and below the floor.

It does not touch expensive pools: the sweep says nothing about them, and depth
is what makes a 30 bps major usable at all. It does not touch pools with no
measured depth: absent is not small, and a pool nobody measured must not be
deleted on the assumption that it is shallow.

GUARDS
------
Backs up every file it rewrites. Refuses a venue if the prune would take more
than PRUNE_MAX_SHARE of it (default 60%) -- a bad floor or a corrupt depth
column should not be able to empty a venue. Reports which token pairs drop
below two pools, since those lose their 2-hop cycle entirely.

Usage:  DRY_RUN=1 python3 scripts/data/prune_shallow_cheap_pools.py
        python3 scripts/data/prune_shallow_cheap_pools.py
Env:    PRUNE_FLOOR_USD (100000), PRUNE_MAX_FEE_PPM (500),
        PRUNE_MAX_SHARE (0.6), DRY_RUN
"""

import glob
import json
import os
import shutil
import sys
import time
from collections import defaultdict

FLOOR = float(os.environ.get("PRUNE_FLOOR_USD", "100000"))
MAX_FEE_PPM = int(os.environ.get("PRUNE_MAX_FEE_PPM", "500"))
MAX_SHARE = float(os.environ.get("PRUNE_MAX_SHARE", "0.6"))
DRY_RUN = os.environ.get("DRY_RUN", "").lower() in ("1", "true", "yes")

KEY_IS_FEE = ("uniswap_v3", "pancakeswap_v3")


def real_fee(record, venue):
    """The pool's fee in ppm, or None if it was never measured.

    `fee` is the venue's POOL KEY: a fee tier on univ3/pancake where the two
    coincide, a TICK SPACING on Slipstream where they do not. Reading the key as
    a fee would mark every spacing-100 Slipstream pool cheap while it charges up
    to 2500 ppm, so Slipstream is judged only on the measured value.
    """
    fee = record.get("fee_ppm_onchain")
    if isinstance(fee, int):
        return fee
    if venue in KEY_IS_FEE:
        fee = record.get("fee")
        return fee if isinstance(fee, int) else None
    return None


def pair_of(record):
    return tuple(sorted((record["token0"].lower(), record["token1"].lower())))


def main():
    print(f"floor ${FLOOR:,.0f}, fee ceiling {MAX_FEE_PPM} ppm"
          f"{'  [DRY_RUN]' if DRY_RUN else ''}")
    before_pairs, after_pairs = defaultdict(int), defaultdict(int)
    plan = {}
    for src in sorted(glob.glob("data/base/*/pools.jsonl")):
        venue = src.split("/")[2]
        if venue == "uniswap_v2":
            continue
        rows = [json.loads(l) for l in open(src) if l.strip()]
        if not rows:
            continue
        keep, drop = [], []
        for r in rows:
            fee = real_fee(r, venue)
            depth = r.get("hub_usd_liquidity")
            cheap = isinstance(fee, int) and 0 < fee <= MAX_FEE_PPM
            measured = isinstance(depth, (int, float))
            if cheap and measured and depth < FLOOR:
                drop.append(r)
            else:
                keep.append(r)
        for r in rows:
            before_pairs[pair_of(r)] += 1
        for r in keep:
            after_pairs[pair_of(r)] += 1
        share = len(drop) / len(rows) if rows else 0.0
        note = ""
        if share > MAX_SHARE:
            note = f"  REFUSED: would take {share:.0%} (> {MAX_SHARE:.0%})"
            keep, drop = rows, []
        print(f"  {venue:<26} {len(rows):>5} -> {len(keep):>5}   "
              f"pruning {len(drop):>4}{note}")
        plan[venue] = (src, keep, drop)

    lost = [p for p, n in before_pairs.items()
            if n > 1 and after_pairs.get(p, 0) < 2]
    total = sum(len(d) for _s, _k, d in plan.values())
    print(f"\n  pruning {total} records")
    print(f"  token pairs that drop below two pools (lose their 2-hop): {len(lost)}")
    if not total:
        return 0
    if DRY_RUN:
        print("\nDRY_RUN set; nothing written.")
        return 0

    stamp = int(time.time())
    for venue, (src, keep, drop) in plan.items():
        if not drop:
            continue
        shutil.copy2(src, f"{src}.bak.prune-{stamp}")
        tmp = src + ".tmp"
        with open(tmp, "w") as fh:
            for r in keep:
                fh.write(json.dumps(r) + "\n")
        os.replace(tmp, src)
        print(f"  wrote {len(keep):>5} -> {src}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
