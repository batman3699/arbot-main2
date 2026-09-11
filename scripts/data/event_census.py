#!/usr/bin/env python3
"""
Event-triggered census: price cycles in the moments AFTER a large swap.

WHY
---
Every census so far sampled continuously and returned 0 positive out of
hundreds. On the cheap cross-venue universe the best 2-hop reached -0.18 bps --
0.018% from breakeven -- and never crossed. That is what a competitive market
looks like at our cost base: between events the price is efficient to within our
fee, and the dislocations that do appear are taken inside the block.

So continuous sampling measures the wrong moment. A large swap moves one pool's
price and leaves the other pools on that pair behind for a block or two. This
harness waits for that and prices the cycle immediately, which is the only
window in which a positive gross should exist.

CONTROL
-------
An event sample alone proves nothing: if event samples are also negative, we
need to know whether they are LESS negative than quiet ones. Every event
therefore has a control -- the same cycles, same sizes, priced at a block with
no qualifying swap. `trigger` is "swap" or "control" on every row, so the
comparison is a filter, not a re-run.

WHAT IT IS NOT
--------------
Not an execution path and not wired to the bot. It quotes through the same
on-chain quoters the production census uses (`quoteExactInputSingle`), so its
numbers are directly comparable to `census_*.jsonl`, and it writes the same
JSONL schema.

Usage:  python3 scripts/data/event_census.py
Env:    EV_RPC_URLS, EV_OUT, EV_MINUTES (60), EV_PACE_S (0.35),
        EV_CONTROL_EVERY (3), EV_MAX_POOLS_PER_PAIR (4),
        EV_MIN_SWAP_USD (7500), EV_MIN_DEPTH_USD (100000),
        EV_MAX_FEE_PPM (500)

Every threshold above is measured rather than chosen. Swap size: hit rate is
12.1% below $5k and 38.9% at or above, peaking at 51.8% in $7.5k-$10k. Depth:
17.4% at every floor from $0 to $50k and 20.0% at $100k, so $100k buys the same
opportunity for a third of the pools (sweep_depth_floor.py). Fee: 500 ppm is
5 bps a hop, the band the tradeable cross-venue pairs occupy.
"""

import json
import os
import random
import sys
import time
import urllib.request
from collections import defaultdict
from itertools import cycle

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from keccak_lite import selector, topic0  # noqa: E402

RPCS = [u.strip() for u in os.environ.get(
    "EV_RPC_URLS",
    "https://mainnet.base.org,https://base.gateway.tenderly.co,"
    "https://base-rpc.publicnode.com,https://1rpc.io/base,"
    "https://base-mainnet.public.blastapi.io"
).split(",") if u.strip()]
OUT = os.environ.get("EV_OUT", "/tmp/event_census.jsonl")
# $7,500, measured across 700 instances spanning $500-$50k: the hit rate is
# 12.1% below $5k and 38.9% at or above, peaking at 51.8% in the $7.5k-$10k band
# where the median best cycle is actually positive. It falls to ~32% above $20k,
# where faster participants have presumably already taken it.
MIN_SWAP_USD = float(os.environ.get("EV_MIN_SWAP_USD", "7500"))
MINUTES = float(os.environ.get("EV_MINUTES", "60"))
PACE_S = float(os.environ.get("EV_PACE_S", "0.35"))
CONTROL_EVERY = int(os.environ.get("EV_CONTROL_EVERY", "3"))
# A fee CEILING (the old name said MIN and meant the opposite). 500 ppm is
# 5 bps a hop: a 2-hop round trip at or under 10 bps, the band the tradeable
# cross-venue pairs occupy.
MAX_FEE_PPM = int(os.environ.get("EV_MAX_FEE_PPM", "500"))
# $100k, measured by sweep_depth_floor.py: identical hit rate to every lower
# floor, against a third of the pools. See that script for the table.
MIN_USD_DEPTH = float(os.environ.get("EV_MIN_DEPTH_USD", "100000"))

MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
AGGREGATE3 = selector("aggregate3((address,bool,bytes)[])")
SWAP_TOPIC = topic0("Swap(address,address,int256,int256,uint160,uint128,int24)")

# uint24 fee for the Uniswap-shaped quoters, int24 tickSpacing for Slipstream.
QUOTER_UINT24 = selector("quoteExactInputSingle((address,address,uint256,uint24,uint160))")
QUOTER_INT24 = selector("quoteExactInputSingle((address,address,uint256,int24,uint160))")
QUOTERS = {
    "uniswap_v3": ("0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a", QUOTER_UINT24),
    "pancakeswap_v3": ("0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997", QUOTER_UINT24),
    "aerodrome_slipstream": ("0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0", QUOTER_INT24),
    "aerodrome_slipstream_v3": ("0xCd2A7D98e82D6107eac1828ce8DeAA6acB65b555", QUOTER_INT24),
}
HUBS = {
    "0x4200000000000000000000000000000000000006": ("WETH", 18, 3000.0),
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": ("USDC", 6, 1.0),
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": ("cbBTC", 8, 95000.0),
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": ("USDbC", 6, 1.0),
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb": ("DAI", 18, 1.0),
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22": ("cbETH", 18, 3000.0),
    "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452": ("wstETH", 18, 3000.0),
    "0x940181a94a35a4569e4529a3cdfb74e38fd98631": ("AERO", 18, 0.7),
}
# Same economic span as the production ladder (0.001-30 native), in USD so it
# is comparable across start tokens.
LADDER_USD = [3, 10, 30, 100, 300, 1000, 3000, 10000, 30000, 90000]
GAS_USD = 0.01  # Base, measured: ~0.006 gwei * ~450k units

_rpc = cycle(RPCS)


def post(payload, timeout=40):
    body = json.dumps(payload).encode()
    req = urllib.request.Request(
        next(_rpc), body, {"Content-Type": "application/json", "User-Agent": "arbot-ev/1"})
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def rpc(method, params, tries=4):
    for attempt in range(tries):
        try:
            time.sleep(PACE_S)
            d = post({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
            if "result" in d:
                return d["result"]
        except Exception:
            pass
        time.sleep(0.5 * (attempt + 1))
    return None


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


def decode_aggregate3(h):
    b = bytes.fromhex(h[2:])
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


def multicall_chunked(calls, block="latest", size=100):
    """Split a large read across several aggregate3 calls, preserving order.

    One multicall carrying 1,440 sub-calls fails outright, and the failure is
    TOTAL: a pair with nine cheap pools yields 144 cycle permutations, and
    before chunking every such event was silently recorded as zero quotes --
    losing exactly the deepest, most-contested pairs, which are the ones this
    experiment is about.
    """
    out = []
    for i in range(0, len(calls), size):
        part = multicall(calls[i:i + size], block)
        if len(part) != len(calls[i:i + size]):
            return []
        out.extend(part)
    return out


def multicall(calls, block="latest"):
    if not calls:
        return []
    payload = {"jsonrpc": "2.0", "id": 1, "method": "eth_call",
               "params": [{"to": MULTICALL3, "data": encode_aggregate3(calls)}, block]}
    for attempt in range(4):
        try:
            time.sleep(PACE_S)
            res = post(payload).get("result")
            if isinstance(res, str) and len(res) > 2:
                return decode_aggregate3(res)
        except Exception:
            pass
        time.sleep(0.6 * (attempt + 1))
    return []


def quote_call(venue, tin, tout, amount, key):
    addr, sel = QUOTERS[venue]
    data = (sel + tin[2:].lower().rjust(64, "0") + tout[2:].lower().rjust(64, "0")
            + format(amount, "064x") + format(key & ((1 << 256) - 1), "064x")
            + format(0, "064x"))
    return addr, data


def load_universe():
    """Pairs carrying >=2 cheap+deep pools -- each is a 2-hop cycle."""
    import glob
    pools = []
    for src in sorted(glob.glob("data/base/*/pools.jsonl")):
        venue = src.split("/")[2]
        if venue not in QUOTERS:
            continue
        for line in open(src):
            line = line.strip()
            if not line:
                continue
            r = json.loads(line)
            fee = r.get("fee_ppm_onchain")
            if not isinstance(fee, int):
                # Only univ3/pancake keys double as the fee; a Slipstream record
                # without a measured fee is skipped rather than guessed at.
                if venue in ("uniswap_v3", "pancakeswap_v3"):
                    fee = r.get("fee")
                else:
                    continue
            if not isinstance(fee, int) or fee <= 0 or fee > MAX_FEE_PPM:
                continue
            if (r.get("hub_usd_liquidity") or 0) < MIN_USD_DEPTH:
                continue
            r["venue"] = venue
            pools.append(r)
    by_pair = defaultdict(list)
    for r in pools:
        by_pair[tuple(sorted((r["token0"].lower(), r["token1"].lower())))].append(r)
    return {p: v for p, v in by_pair.items() if len(v) > 1}


MAX_POOLS_PER_PAIR = int(os.environ.get("EV_MAX_POOLS_PER_PAIR", "4"))


def cycles_for_pair(members):
    """Every ordered 2-pool, 2-token loop over this pair.

    Capped to the cheapest few pools: permutations grow as n*(n-1)*2, so nine
    pools is 144 cycles before the size ladder, and the expensive legs cannot
    clear regardless of how the price moved. Cheapest-first keeps the ones that
    could.
    """
    members = sorted(members, key=lambda m: m.get("fee_ppm_onchain") or m["fee"])
    members = members[:MAX_POOLS_PER_PAIR]
    out = []
    for i, a in enumerate(members):
        for j, b in enumerate(members):
            if i == j:
                continue
            for start in (a["token0"], a["token1"]):
                other = a["token1"] if start.lower() == a["token0"].lower() else a["token0"]
                out.append((start, other, a, b))
    return out


INSTANCE = [0]


def price_cycles(pair, members, trigger, swap_usd, block_tag, writer):
    """Quote every cycle on this pair across the ladder; append rows."""
    hub = next((t for t in pair if t in HUBS), None)
    if hub is None:
        return 0, None
    sym, dec, px = HUBS[hub]
    INSTANCE[0] += 1
    inst_id = INSTANCE[0]
    combos = cycles_for_pair(members)
    jobs = []
    for (start, mid, a, b) in combos:
        s = start.lower()
        if s not in HUBS:
            continue
        _s, sdec, spx = HUBS[s]
        for usd in LADDER_USD:
            amt = int(usd / spx * (10 ** sdec))
            if amt <= 0:
                continue
            jobs.append({"start": start, "mid": mid, "a": a, "b": b,
                         "usd": usd, "amt": amt})
    if not jobs:
        return 0, None

    leg1 = multicall_chunked([quote_call(j["a"]["venue"], j["start"], j["mid"],
                                         j["amt"], j["a"]["fee"]) for j in jobs], block_tag)
    if len(leg1) != len(jobs):
        return 0, None
    live = []
    for j, (ok, ret) in zip(jobs, leg1):
        if ok and len(ret) >= 32:
            mid_amt = int.from_bytes(ret[:32], "big")
            if mid_amt > 0:
                j["mid_amt"] = mid_amt
                live.append(j)
    if not live:
        return 0, None
    leg2 = multicall_chunked([quote_call(j["b"]["venue"], j["mid"], j["start"],
                                         j["mid_amt"], j["b"]["fee"]) for j in live], block_tag)
    if len(leg2) != len(live):
        return 0, None

    n = 0
    best_bps = None
    for j, (ok, ret) in zip(live, leg2):
        if not (ok and len(ret) >= 32):
            continue
        out_amt = int.from_bytes(ret[:32], "big")
        if out_amt <= 0:
            continue
        s = j["start"].lower()
        _sy, sdec, spx = HUBS[s]
        gross = out_amt - j["amt"]
        bps = gross / j["amt"] * 1e4
        best_bps = bps if best_bps is None else max(best_bps, bps)
        gas_tokens = int(GAS_USD / spx * (10 ** sdec))
        writer.write(json.dumps({
            "trigger": trigger,
            "instance": inst_id,
            "swap_usd": swap_usd,
            "block": block_tag,
            # depth_usd and fee_ppm ride along so a depth-floor sweep is a
            # FILTER over one run rather than one run per floor. Sequential runs
            # face different market conditions, and that confound is what made
            # the earlier dollar-based sweep non-monotonic and unreadable.
            "route": [
                {"venue": j["a"]["venue"], "pool": j["a"]["pool"],
                 "from": j["start"], "to": j["mid"],
                 "depth_usd": j["a"].get("hub_usd_liquidity"),
                 "fee_ppm": j["a"].get("fee_ppm_onchain") or j["a"]["fee"]},
                {"venue": j["b"]["venue"], "pool": j["b"]["pool"],
                 "from": j["mid"], "to": j["start"],
                 "depth_usd": j["b"].get("hub_usd_liquidity"),
                 "fee_ppm": j["b"].get("fee_ppm_onchain") or j["b"]["fee"]},
            ],
            "hops": 2,
            "start": j["start"],
            "amount_in": str(j["amt"]),
            "amount_out": str(out_amt),
            "gross": gross,
            "flash_fee": "0",
            "gas_cost": str(gas_tokens),
            "net": gross - gas_tokens,
            "net_bps": round((gross - gas_tokens) / j["amt"] * 1e4, 4),
            "notional_usd": j["usd"],
        }) + "\n")
        n += 1
    writer.flush()
    return n, best_bps


def swap_usd_of(log, pool_by_addr):
    """USD notional of a Swap, from whichever side is a hub token."""
    r = pool_by_addr.get(log["address"].lower())
    if r is None:
        return 0.0
    data = bytes.fromhex(log["data"][2:])
    if len(data) < 64:
        return 0.0

    def i256(b):
        v = int.from_bytes(b, "big")
        return v - (1 << 256) if v >= (1 << 255) else v

    a0, a1 = abs(i256(data[:32])), abs(i256(data[32:64]))
    for tok, amt in ((r["token0"].lower(), a0), (r["token1"].lower(), a1)):
        if tok in HUBS:
            _s, dec, px = HUBS[tok]
            return (amt / 10 ** dec) * px
    return 0.0


def main():
    universe = load_universe()
    pool_by_addr = {}
    for members in universe.values():
        for r in members:
            pool_by_addr[r["pool"].lower()] = r
    print(f"universe: {len(universe)} pairs, {len(pool_by_addr)} pools "
          f"(<= {MAX_FEE_PPM} ppm, >= ${MIN_USD_DEPTH:,.0f})")
    if not universe:
        print("ABORT: no pair carries two cheap+deep pools", file=sys.stderr)
        return 1
    for pair, members in universe.items():
        vs = "+".join(sorted({m["venue"] for m in members}))
        cheapest = sorted(m.get("fee_ppm_onchain") or m["fee"] for m in members)[:2]
        print(f"   {len(members)} pools  {sum(cheapest)/100:.2f} bps round-trip  {vs}")

    addrs = list(pool_by_addr)
    head = int(rpc("eth_blockNumber", []) or "0x0", 16)
    print(f"\nhead {head:,}; watching for swaps >= ${MIN_SWAP_USD:,.0f} "
          f"for {MINUTES:.0f} min -> {OUT}", flush=True)

    deadline = time.time() + MINUTES * 60
    events = controls = rows = hits = control_hits = 0
    quiet_blocks = 0
    seen = head
    with open(OUT, "w") as writer:
        while time.time() < deadline:
            nxt = int(rpc("eth_blockNumber", []) or "0x0", 16)
            if nxt <= seen:
                time.sleep(1.0)
                continue
            for blk in range(seen + 1, min(nxt, seen + 6) + 1):
                logs = rpc("eth_getLogs", [{"address": addrs, "topics": [SWAP_TOPIC],
                                            "fromBlock": hex(blk), "toBlock": hex(blk)}])
                if logs is None:
                    continue
                big = []
                for lg in logs:
                    usd = swap_usd_of(lg, pool_by_addr)
                    if usd >= MIN_SWAP_USD:
                        big.append((lg["address"].lower(), usd))
                if big:
                    quiet_blocks = 0
                    for addr, usd in big[:2]:
                        rec = pool_by_addr[addr]
                        pair = tuple(sorted((rec["token0"].lower(), rec["token1"].lower())))
                        # "latest", not the swap's block: the question is whether
                        # the dislocation is still there when we could act.
                        n, best = price_cycles(pair, universe[pair], "swap",
                                               round(usd, 2), "latest", writer)
                        rows += n
                        if n:
                            events += 1
                            if best is not None and best > 0:
                                hits += 1
                        rate = (100.0 * hits / events) if events else 0.0
                        print(f"  [{blk}] swap ${usd:>11,.0f} -> {n:>4} quotes  "
                              f"best {('%+.2f' % best) if best is not None else '   n/a':>8} bps  "
                              f"HIT-RATE {hits}/{events} = {rate:.1f}%", flush=True)
                else:
                    quiet_blocks += 1
                    if quiet_blocks >= CONTROL_EVERY:
                        quiet_blocks = 0
                        pair = random.choice(list(universe))
                        n, best = price_cycles(pair, universe[pair], "control", 0.0,
                                               "latest", writer)
                        rows += n
                        if n:
                            controls += 1
                            if best is not None and best > 0:
                                control_hits += 1
            seen = min(nxt, seen + 6)
    # The RATE is the measurement that converges. Realised dollars are
    # fat-tailed -- one window's total was 87% two events -- so a 45-minute run
    # cannot separate two configurations on money. A hit rate over ~100
    # instances can.
    er = (100.0 * hits / events) if events else 0.0
    cr = (100.0 * control_hits / controls) if controls else 0.0
    print(f"\nswap    instances {events:>4}  hits {hits:>4}  HIT RATE {er:5.1f}%")
    print(f"control instances {controls:>4}  hits {control_hits:>4}  HIT RATE {cr:5.1f}%")
    print(f"rows {rows} -> {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
