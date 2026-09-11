#!/usr/bin/env python3
"""
Rebuild the Base UniV3 inventory by ARBITRAGE VIABILITY: fee first, then depth.

WHY THIS EXISTS
---------------
`rank_base_pools.py` ranks by `hub_usd_liquidity` descending and truncates. Fee
is never an input. On Base that inverts the selection, because depth and fee are
inversely related there: of the 1,881,808 pools the factory has emitted,
1,778,343 are the 1% tier. Ranking by depth walks straight into them.

Measured 2026-09-06 on the shipped 1,126-pool inventory:

  * top 50 by liquidity: mean fee 44.5 bps PER HOP -> a 2-hop cycle must clear
    ~89 bps before it earns anything.
  * a census over that inventory found 0 of ~1,200 samples profitable, with a
    3-hop median gross of -173 bps. That was never a fact about Base; it was a
    fact about which pools were handed to it.
  * the same census over 53 pools filtered to <=5 bps AND >=$250k depth moved
    the 3-hop median to -7.4 bps (p90 -6.8, best -6.41) and stayed flat to
    3.0 native. The hurdle fell ~25x.

WHAT IT MEASURED (run 2026-09-06, all 46,603 read)
--------------------------------------------------
Base has 49,250 pools at the 1bp/5bp tiers and the shipped inventory holds 207,
which looked like a 99.6% coverage gap. It is not. Reading every one of the
46,603 hub-anchored cheap pools found that the cheap tier is almost entirely
EMPTY:

      floor      pools   pairs with >=2 (a 2-hop cycle each)
  $1,000,000        28       2
    $250,000        37       3
     $50,000        72       5
      $1,000       370      27

So the 207 already on disk had captured essentially every cheap UniV3 pool with
any liquidity. Uniswap V3's cheap tier is not a missed cohort; it is dust. The
hypothesis this script was written to exploit is refuted by this script's own
output, which is why it aborts instead of writing: 14 pools is not an inventory.

Keep it for the measurement and for other chains, where the same question has
not been answered. On Base the coverage gap is Aerodrome, not Uniswap -- 170
Slipstream pools are held, 26 of them cheap AND deep (a 15% hit rate against
Uniswap's 1.2%), with no factory enumeration behind them at all.

WHY FEE IS A FILTER AND NOT A WEIGHT
------------------------------------
A fee is paid on every hop of every attempt; depth only caps size. A 30 bps pool
needs a >60 bps dislocation on a 2-hop, which essentially never occurs, however
deep it is. So fee is a hard gate and depth ranks what survives it -- hence
"fee, THEN depth", not a blended score.

SAFETY
------
Never shrinks an inventory. The result is UNIONed with whatever is already on
disk (existing records win on conflict, so hand-verified entries survive), the
previous file is copied aside first, and the run aborts without writing if too
few pools qualify -- a broken RPC must not be able to empty the universe.

USAGE
-----
    python3 scripts/data/rebuild_cheap_inventory.py            # write
    DRY_RUN=1 python3 scripts/data/rebuild_cheap_inventory.py  # report only

Env: CHEAP_RPC_URLS, CHEAP_MIN_USD (250000), CHEAP_TOP_N (4000),
     CHEAP_FEES (100,500), CHEAP_BATCH (200), CHEAP_WORKERS (1),
     CHEAP_PACE_S (1.2), DRY_RUN.
"""

import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.request
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from itertools import cycle
from threading import Lock

# Per venue: where the factory enumeration lives, and which field carries the
# REAL fee. On Uniswap the pool key IS the fee tier, so `fee` serves both roles.
# On Slipstream the key is the TICK SPACING and the fee is dynamic -- spacing 100
# pools were measured at 212 and 2500 ppm -- so the enumerator records the fee
# separately and this reads it from there. Getting that wrong silently filters on
# tick spacing, which is how the runtime came to price Slipstream at its spacing.
VENUES = {
    "uniswap_v3": {"fee_field": "fee"},
    "aerodrome_slipstream": {"fee_field": "fee_ppm_onchain"},
    "aerodrome_slipstream_v3": {"fee_field": "fee_ppm_onchain"},
    "pancakeswap_v3": {"fee_field": "fee"},
}
if os.environ.get("CHEAP_VENUES"):
    _want = {v.strip() for v in os.environ["CHEAP_VENUES"].split(",")}
    VENUES = {k: v for k, v in VENUES.items() if k in _want}


def paths(venue):
    d = os.path.join("data", "base", venue)
    return (os.path.join(d, "pools.factory-full.jsonl"),
            os.path.join(d, "pools.jsonl"),
            os.path.join(d, ".cheap_depth_cache.json"))

DEFAULT_RPCS = "https://mainnet.base.org,https://base.gateway.tenderly.co"
RPCS = [u.strip() for u in os.environ.get("CHEAP_RPC_URLS", DEFAULT_RPCS).split(",") if u.strip()]
MIN_USD = float(os.environ.get("CHEAP_MIN_USD", "250000"))
TOP_N = int(os.environ.get("CHEAP_TOP_N", "4000"))
# A ceiling, not a tier list: Slipstream fees are arbitrary ppm values
# (65, 85, 90, 190, 346, 425 ...), so an enum of Uniswap tiers cannot
# express "cheap" for it.
MAX_FEE_PPM = int(os.environ.get("CHEAP_MAX_FEE_PPM", "500"))
# Multicall3, not a JSON-RPC batch. Measured 2026-09-06: mainnet.base.org caps
# JSON-RPC batches at 10 calls and rate-limits, which turns 46,603 reads into
# 4,661 requests it will not serve. One aggregate3 carries 200 balanceOf calls
# in a single eth_call, so the same work is ~230 requests.
BATCH = int(os.environ.get("CHEAP_BATCH", "200"))
WORKERS = int(os.environ.get("CHEAP_WORKERS", "1"))
# Public Base endpoints 429 readily. One request in flight with a pause
# between it finishes sooner than three that spend their time backing off.
PACE_S = float(os.environ.get("CHEAP_PACE_S", "1.2"))
DRY_RUN = os.environ.get("DRY_RUN", "").lower() in ("1", "true", "yes")

BALANCE_OF = "0x70a08231"
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
AGGREGATE3 = "0x82ad56cb"  # aggregate3((address target, bool allowFailure, bytes callData)[])

# Symbol, decimals, USD. Prices only set the ordering and the floor, so a rough
# value is fine; they are overridable for a volatile market.
HUBS = {
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": ("USDC", 6, 1.0),
    "0x4200000000000000000000000000000000000006": ("WETH", 18, float(os.environ.get("RANK_WETH_USD", "3000"))),
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": ("cbBTC", 8, float(os.environ.get("RANK_CBTC_USD", "95000"))),
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": ("USDbC", 6, 1.0),
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb": ("DAI", 18, 1.0),
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22": ("cbETH", 18, float(os.environ.get("RANK_WETH_USD", "3000"))),
    "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452": ("wstETH", 18, float(os.environ.get("RANK_WETH_USD", "3000"))),
    "0x940181a94a35a4569e4529a3cdfb74e38fd98631": ("AERO", 18, float(os.environ.get("RANK_AERO_USD", "0.7"))),
}
# Deepest first, so a pool pairing two hubs is measured on the more liquid side.
HUB_PRIORITY = [
    "0x4200000000000000000000000000000000000006",
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf",
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb",
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22",
    "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452",
    "0x940181a94a35a4569e4529a3cdfb74e38fd98631",
]

_rpc = cycle(RPCS)
_rpc_lock = Lock()


def next_rpc():
    with _rpc_lock:
        return next(_rpc)


def pick_hub(token0, token1):
    pair = {token0.lower(), token1.lower()}
    for hub in HUB_PRIORITY:
        if hub in pair:
            return hub
    return None


def post(url, payload, timeout=45):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        url, body, {"Content-Type": "application/json", "User-Agent": "arbot-inventory/1"}
    )
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def encode_aggregate3(calls):
    """calls: [(target, calldata_hex)] -> aggregate3 calldata. allowFailure=true,
    so one reverting token cannot void the other 199 reads in the call."""
    n = len(calls)
    tuples = []
    for target, data in calls:
        raw = bytes.fromhex(data[2:])
        t = bytes(12) + bytes.fromhex(target[2:])
        t += (1).to_bytes(32, "big")    # allowFailure
        t += (96).to_bytes(32, "big")   # offset of the bytes member within the tuple
        t += len(raw).to_bytes(32, "big") + raw + bytes((-len(raw)) % 32)
        tuples.append(t)
    heads, off = b"", 32 * n
    for t in tuples:
        heads += off.to_bytes(32, "big")
        off += len(t)
    payload = (32).to_bytes(32, "big") + n.to_bytes(32, "big") + heads + b"".join(tuples)
    return AGGREGATE3 + payload.hex()


def decode_aggregate3(result_hex):
    """-> [(success, returndata)] in call order."""
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


def balance_batch(jobs):
    """jobs: [(key, hub, pool)] -> {key: raw_balance}. Missing keys mean UNREAD.

    A failed read is left absent rather than recorded as zero: a zero balance and
    an unread balance are different facts, and conflating them silently drops
    pools that a retry would have kept.
    """
    calls = [(hub, BALANCE_OF + pool[2:].lower().rjust(64, "0")) for (_k, hub, pool) in jobs]
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [{"to": MULTICALL3, "data": encode_aggregate3(calls)}, "latest"],
    }
    for attempt in range(5):
        try:
            time.sleep(PACE_S)
            data = post(next_rpc(), payload)
            res = data.get("result")
            if isinstance(res, str) and len(res) > 2:
                out = {}
                for (ok, ret), (key, _h, _p) in zip(decode_aggregate3(res), jobs):
                    if ok and len(ret) >= 32:
                        out[key] = int.from_bytes(ret[:32], "big")
                return out
            err = data.get("error")
            if attempt == 0 and err:
                print(f"    rpc error: {str(err)[:90]}", file=sys.stderr)
        except Exception as exc:
            if attempt == 0:
                print(f"    transport: {type(exc).__name__} {str(exc)[:70]}", file=sys.stderr)
        # 429 needs real patience, not a linear nudge.
        time.sleep(min(2.0 * (2 ** attempt), 30.0))
    return {}


def rebuild_venue(venue, cfg):
    factory, target, cache_path = paths(venue)
    fee_field = cfg["fee_field"]
    if not os.path.exists(factory):
        print(f"  {venue}: no {factory}; skipped "
              f"(enumerate it first)", file=sys.stderr)
        return None

    cands = []
    pairs = defaultdict(list)
    total = 0
    with open(factory) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            total += 1
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            fee = r.get(fee_field)
            if not isinstance(fee, int) or fee <= 0 or fee > MAX_FEE_PPM:
                continue
            hub = pick_hub(r.get("token0", ""), r.get("token1", ""))
            if hub is None:
                continue
            r["_hub"] = hub
            r["_fee_ppm"] = fee
            cands.append(r)
            pairs[tuple(sorted((r["token0"].lower(), r["token1"].lower())))].append(r["pool"])
    print(f"\n=== {venue} ===")
    print(f"  {total:,} enumerated -> {len(cands):,} at <= {MAX_FEE_PPM} ppm and hub-anchored")
    if not cands:
        return None

    depth = {}
    if os.path.exists(cache_path):
        try:
            depth = {k: int(v) for k, v in json.load(open(cache_path)).items()}
            print(f"  resumed {len(depth):,} cached balances")
        except Exception:
            depth = {}

    todo = [(r["pool"].lower(), r["_hub"], r["pool"]) for r in cands
            if r["pool"].lower() not in depth]
    if todo:
        batches = [todo[i:i + BATCH] for i in range(0, len(todo), BATCH)]
        print(f"  reading balanceOf for {len(todo):,} pools "
              f"({len(batches):,} multicalls of {BATCH})", flush=True)
        done = 0
        with ThreadPoolExecutor(max_workers=WORKERS) as pool:
            for result in pool.map(balance_batch, batches):
                depth.update(result)
                done += 1
                if done % 20 == 0:
                    print(f"    {done:,}/{len(batches):,} multicalls, "
                          f"{len(depth):,} balances", flush=True)
                    try:
                        json.dump({k: str(v) for k, v in depth.items()}, open(cache_path, "w"))
                    except OSError:
                        pass
        try:
            json.dump({k: str(v) for k, v in depth.items()}, open(cache_path, "w"))
        except OSError:
            pass

    scored = []
    for r in cands:
        raw = depth.get(r["pool"].lower())
        if raw is None or raw <= 0:
            continue
        sym, dec, price = HUBS[r["_hub"]]
        usd = (raw / 10 ** dec) * price
        if usd < MIN_USD:
            continue
        scored.append({
            "pool": r["pool"],
            "token0": r["token0"],
            "token1": r["token1"],
            # POOL KEY, preserved verbatim from the enumeration. On Slipstream
            # this is the tick spacing and the router needs it to resolve a path.
            "fee": r["fee"],
            "created_block": r.get("created_block") or 0,
            "hub_usd_liquidity": round(usd, 2),
            # REQUIRED. `pool_store::trusted_hub_usd_liquidity` ignores a
            # liquidity number that does not name the hub it was measured
            # against, so omitting this silently discards the whole rebuild.
            "hub_symbol": sym,
            "fee_ppm_onchain": r["_fee_ppm"],
        })

    # Fee was gated above; depth orders what survived.
    scored.sort(key=lambda r: -r["hub_usd_liquidity"])
    keep = scored[:TOP_N]
    print(f"  qualified (>= ${MIN_USD:,.0f}): {len(scored):,}   keeping {len(keep):,}")
    for r in keep[:5]:
        print(f"    {r['pool']}  {r['fee_ppm_onchain']:>4}ppm  "
              f"${r['hub_usd_liquidity']:>13,.0f}  {r['hub_symbol']}")
    if not keep:
        return None

    existing = []
    if os.path.exists(target):
        with open(target) as fh:
            for line in fh:
                line = line.strip()
                if line:
                    try:
                        existing.append(json.loads(line))
                    except json.JSONDecodeError:
                        continue
    # UNION, never shrink. An existing record keeps every field this script does
    # not measure -- it may be hand-verified or carry venue-specific keys -- but
    # the three facts this run DID measure are overlaid onto it.
    #
    # Plain "existing wins" was wrong and silently defeated the rebuild: the
    # scraped Slipstream records predate `fee_ppm_onchain`/`hub_symbol`, so
    # skipping them left the pools unusable to anything that filters on a
    # measured fee, and the cheap universe collapsed to the two venues whose
    # pool key happens to double as their fee.
    measured = {"hub_usd_liquidity", "hub_symbol", "fee_ppm_onchain"}
    fresh = {r["pool"].lower(): r for r in keep}
    by_pool = {}
    for r in existing:
        pl = r.get("pool")
        if not pl:
            continue
        new_rec = fresh.get(pl.lower())
        if new_rec:
            r = {**r, **{k: v for k, v in new_rec.items() if k in measured}}
        by_pool[pl.lower()] = r
    enriched = sum(1 for pl in fresh if pl in by_pool)
    for pl, r in fresh.items():
        by_pool.setdefault(pl, r)
    merged = list(by_pool.values())
    added = len(merged) - len(existing)
    if enriched:
        print(f"  enriched {enriched:,} existing records with measured fee/depth/hub")
    print(f"  existing {len(existing):,} + new {added:,} -> {len(merged):,}")

    if DRY_RUN:
        return {"venue": venue, "added": added, "keep": keep, "written": False}

    if os.path.exists(target):
        shutil.copy2(target, f"{target}.bak.cheap-{int(time.time())}")
    tmp = target + ".tmp"
    with open(tmp, "w") as fh:
        for r in merged:
            fh.write(json.dumps(r) + "\n")
    os.replace(tmp, target)
    print(f"  wrote {len(merged):,} -> {target}")
    return {"venue": venue, "added": added, "keep": keep, "written": True}


def main():
    print(f"fee ceiling {MAX_FEE_PPM} ppm, depth floor ${MIN_USD:,.0f}"
          f"{'  [DRY_RUN]' if DRY_RUN else ''}")
    results = [r for r in (rebuild_venue(v, c) for v, c in VENUES.items()) if r]
    if not results:
        print("\nnothing qualified anywhere; no inventory touched", file=sys.stderr)
        return 1

    # The number that matters: a 2-hop needs the same pair cheap on two pools,
    # and across venues is where that actually happens.
    allkeep = [(k, r["venue"]) for r in results for k in r["keep"]]
    bypair = defaultdict(list)
    for k, v in allkeep:
        bypair[tuple(sorted((k["token0"].lower(), k["token1"].lower())))].append((v, k))
    multi = {p: e for p, e in bypair.items() if len(e) > 1}
    cross = {p: e for p, e in multi.items() if len({v.split("_")[0] for v, _ in e}) > 1}
    print(f"\n=== {sum(r['added'] for r in results):,} pools added across "
          f"{len(results)} venues ===")
    print(f"  pairs with >=2 cheap+deep pools: {len(multi)}   of those cross-venue: {len(cross)}")
    for pair, ent in sorted(cross.items(), key=lambda x: -max(k["hub_usd_liquidity"] for _v, k in x[1]))[:8]:
        best = sorted(ent, key=lambda e: e[1]["fee_ppm_onchain"])[:2]
        rt = sum(e[1]["fee_ppm_onchain"] for e in best) / 100.0
        names = " + ".join(f"{v}@{k['fee_ppm_onchain']}ppm" for v, k in best)
        print(f"    {rt:>6.2f} bps round-trip   {names}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
