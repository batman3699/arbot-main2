#!/usr/bin/env python3
"""refresh_weth_pools.py -- find every WETH pool our five venues hold for the
tokens Base's arbitrage trades, measure each on chain, and merge them into the
inventories (Task 8.5 R30).

The runtime prices two-hop WETH cycles over pairs with two or more admitted
pools (fee at most 3,000 ppm, at least $100k on the hub side), so a WETH pool
it does not know is a cycle it never prices. The R23 census found $2.8k a week
of two-hop WETH arbitrage on our venues in pools missing from the
inventories, which were last rebuilt in September.

Nothing here is scraped: every pool is the one its venue's factory returns for
(WETH, token, tier), so an address cannot be invented, and every number is a
read at one pinned block:

- depth: WETH.balanceOf(pool) times the ETH price of Uniswap's 0.05% WETH/USDC
  pool at that block, as `hub_usd_liquidity` with `hub_symbol` WETH;
- fee: the pool's fee() (Uniswap, PancakeSwap), the Aerodrome factory's
  getFee(pool, false), or for Slipstream, whose fee is dynamic, the median of
  fee() at the pinned block and every 30 minutes for two hours before it: the
  reading decides admission only, and the book prices every swap at the fee
  then.

The candidate tokens are every token in a census arbitrage that touched one of
our venues, and every token already paired with WETH in the inventories in a
pool that held $10k or more.

Records already present keep their place and what they knew; one found again
takes the new WETH-side depth and fee, whatever hub it named before. A pool
holding under $1,000 of WETH is not kept: a new one is not added, and one
found drained is removed. New records follow, deepest
first. Each inventory is backed up before it is rewritten.

The RPC endpoint comes from BASE_RPC_URL (comma-separated for several) and is
never printed. Use public endpoints: the canary's key is the canary's.

Usage: BASE_RPC_URL=... scripts/data/refresh_weth_pools.py [--block N] [--out DIR | --write]
"""
import argparse
import collections
import dataclasses
import glob
import gzip
import json
import os
import shutil
import sys
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BASE_DATA = REPO / "data" / "base"
WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
WETH_USDC_UNI = "0xd0b53d9277642d899df5c87a3966a349a798f224"
MULTICALL3 = "0xca11bde05977b3631167028862be2a173976ca11"
CENSUS_VENUES = {"uniswap_v3", "pancakeswap_v3", "slipstream_5e7b", "slipstream_f8f2", "aerodrome_v2"}

# Each inventory directory: its factory, how a pool is asked for, and the
# tiers it is asked at (fee tiers; Slipstream's tick spacings are read from
# the factory). 10,000 ppm tiers are left out: the universe admits <= 3,000.
VENUES = {
    "uniswap_v3": dict(factory="0x33128a8fc17869897dce68ed026d694621f6fdfd", kind="fee", tiers=[100, 500, 3000]),
    "pancakeswap_v3": dict(factory="0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865", kind="fee", tiers=[100, 500, 2500]),
    "aerodrome_slipstream": dict(factory="0x5e7bb104d84c7cb9b682aac2f3d509f5f406809a", kind="spacing", tiers=None),
    "aerodrome_slipstream_v3": dict(factory="0xf8f2eb4940cfe7d13603dddd87f123820fc061ef", kind="spacing", tiers=None),
    "aerodrome_v2": dict(factory="0x420dd381b31aef6683db6b902084cb0ffece40da", kind="volatile", tiers=[None]),
}

SEL = {
    "getPool_fee": "1698ee82",       # getPool(address,address,uint24)
    "getPool_spacing": "28af8d0b",   # getPool(address,address,int24)
    "getPool_stable": "79bc57d5",    # getPool(address,address,bool)
    "tickSpacings": "9cbbbe86",      # tickSpacings()
    "fee": "ddca3f43",               # fee()
    "getFee": "cc56b2c5",            # getFee(address,bool)
    "balanceOf": "70a08231",         # balanceOf(address)
    "slot0": "3850c7bd",             # slot0()
    "aggregate3": "82ad56cb",        # aggregate3((address,bool,bytes)[])
}


@dataclasses.dataclass(frozen=True)
class Found:
    pool: str
    venue: str
    token: str
    tier: object  # fee tier, tick spacing, or None for Aerodrome v2
    fee_ppm: int
    weth_wei: int


@dataclasses.dataclass
class Summary:
    added: int = 0
    updated: int = 0
    kept: int = 0
    removed: int = 0


# A WETH pool holding less than this is not inventoried: it cannot be admitted
# at any floor in use, and the legacy ranker refuses a record under $1.
MIN_RECORD_USD = 1_000.0


def fee_blocks(block: int) -> list:
    """Where a dynamic fee is read: the pinned block and every 30 minutes
    (900 blocks) back, two hours in all."""
    return [block - 900 * i for i in range(5)]


def median_fee(readings: list) -> int:
    """A Slipstream fee moves with the price's distance from its TWAP, so one
    reading can put a pool either side of the admission cap; the median of
    several does not swing on one."""
    xs = sorted(readings)
    return xs[(len(xs) - 1) // 2]


def record(f: Found, eth_usd: float, block: int) -> dict:
    """The inventory record for a pool found, in its venue's format."""
    t0, t1 = sorted([WETH, f.token])
    usd = round(f.weth_wei / 1e18 * eth_usd, 2)
    if f.venue == "aerodrome_v2":
        return {"pool": f.pool, "token0": t0, "token1": t1, "stable": False, "fee": f.fee_ppm // 100,
                "fee_ppm_onchain": f.fee_ppm, "created_block": None, "hub_usd_liquidity": usd, "hub_symbol": "WETH"}
    # created_block 0, "not read": the legacy loader (arb-exec-legacy
    # pool_store) needs a number, and the runtime does not read it.
    r = {"pool": f.pool, "token0": t0, "token1": t1, "fee": f.tier, "created_block": 0,
         "hub_usd_liquidity": usd, "hub_symbol": "WETH", "fee_ppm_onchain": f.fee_ppm}
    if VENUES[f.venue]["kind"] == "spacing":
        r["fee_measured_block"] = block
    return r


def merge(existing: list, found: list, eth_usd: float, block: int):
    """`existing` with every pool found again measured afresh on its WETH side,
    whatever hub the record named (38 named none: scraped depths), then the
    new pools, deepest first, each once."""
    by_pool = {}
    for f in found:
        by_pool.setdefault(f.pool.lower(), f)
    out, summary, seen = [], Summary(), set()
    for r in existing:
        key = r["pool"].lower()
        seen.add(key)
        f = by_pool.get(key)
        if f is not None:
            fresh = record(f, eth_usd, block)
            if fresh["hub_usd_liquidity"] < MIN_RECORD_USD:
                summary.removed += 1
                continue
            r = dict(r, hub_usd_liquidity=fresh["hub_usd_liquidity"], hub_symbol="WETH",
                     fee_ppm_onchain=fresh["fee_ppm_onchain"])
            if "fee_measured_block" in fresh:
                r["fee_measured_block"] = block
            summary.updated += 1
        else:
            summary.kept += 1
        out.append(r)
    new = [record(f, eth_usd, block) for key, f in by_pool.items() if key not in seen]
    new = [r for r in new if r["hub_usd_liquidity"] >= MIN_RECORD_USD]
    new.sort(key=lambda r: -r["hub_usd_liquidity"])
    summary.added = len(new)
    return out + new, summary


def universe(inventories: dict, max_fee_ppm: int, min_depth_usd: float):
    """The runtime's universe from `inventories` (directory -> records): the
    pools of WETH pairs with two or more admitted pools, and the two-hop
    cycles over them (n x (n - 1) for a pair of n). Follows
    live::inventory::load and live::frontier::cycles; on the inventories of
    2026-10-20 it gives the runtime's 41 pools and 158 cycles."""
    specs = []
    for venue, recs in inventories.items():
        static = VENUES[venue]["kind"] == "fee"
        for r in recs:
            if venue == "aerodrome_v2" and r.get("stable") is not False:
                continue
            fee = r.get("fee_ppm_onchain")
            if fee is None:
                if not static:
                    continue
                fee = r.get("fee")
            depth = r.get("hub_usd_liquidity") or 0.0
            if not fee or fee > max_fee_ppm or depth < min_depth_usd:
                continue
            specs.append((r["pool"].lower(), tuple(sorted((r["token0"].lower(), r["token1"].lower())))))
    by_pair = collections.defaultdict(list)
    for pool, pair in specs:
        by_pair[pair].append(pool)
    weth = [v for pair, v in by_pair.items() if len(v) >= 2 and WETH in pair]
    return [p for v in weth for p in v], sum(len(v) * (len(v) - 1) for v in weth)


# ---- chain reads ---------------------------------------------------------

class Rpc:
    def __init__(self, urls, per_second=4.0):
        self.urls = [u for u in urls if u]
        self.gap = 1.0 / per_second
        self.next = 0.0
        self.i = 0

    def call(self, method, params, tries=12):
        err = None
        for n in range(tries):
            now = time.monotonic()
            if self.next > now:
                time.sleep(self.next - now)
            self.next = time.monotonic() + self.gap
            url = self.urls[self.i % len(self.urls)]
            body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
            req = urllib.request.Request(url, body, {"Content-Type": "application/json", "User-Agent": "apex-refresh/1"})
            try:
                r = json.load(urllib.request.urlopen(req, timeout=60))
                if "result" in r:
                    return r["result"]
                err = str(r.get("error"))[:120]
            except Exception as e:  # rotate and back off; the URL is never printed
                err = type(e).__name__
            self.i += 1
            time.sleep(min(2 ** n, 20))
        raise SystemExit(f"{method} failed after {tries} tries: {err}")


def word(x) -> str:
    if isinstance(x, bool):
        return format(int(x), "064x")
    if isinstance(x, int):
        return format(x % (1 << 256), "064x")
    return x.lower().replace("0x", "").rjust(64, "0")


def encode_aggregate3(calls):
    """aggregate3((address target, bool allowFailure, bytes callData)[])."""
    n = len(calls)
    head = [word(0x20), word(n)]
    tails, offset = [], 32 * n
    for target, data in calls:
        data = bytes.fromhex(data)
        padded = data + b"\0" * (-len(data) % 32)
        tail = word(target) + word(True) + word(0x60) + word(len(data)) + padded.hex()
        head.append(word(offset))
        tails.append(tail)
        offset += len(tail) // 2
    return "0x" + SEL["aggregate3"] + "".join(head) + "".join(tails)


def decode_aggregate3(hexdata):
    """(bool success, bytes returnData)[] -> list of bytes or None."""
    b = bytes.fromhex(hexdata[2:])
    at = lambda o: int.from_bytes(b[o:o + 32], "big")
    base = at(0)
    n = at(base)
    out = []
    for i in range(n):
        e = base + 32 + at(base + 32 + 32 * i)
        ok = at(e)
        d = e + at(e + 32)
        ln = at(d)
        out.append(b[d + 32:d + 32 + ln] if ok else None)
    return out


def multicall(rpc, calls, block, chunk=250):
    out = []
    for i in range(0, len(calls), chunk):
        part = calls[i:i + chunk]
        res = rpc.call("eth_call", [{"to": MULTICALL3, "data": encode_aggregate3(part)}, hex(block)])
        out.extend(decode_aggregate3(res))
    return out


def as_int(b, signed=False):
    if not b or len(b) < 32:
        return None
    v = int.from_bytes(b[:32], "big")
    return v - (1 << 256) if signed and v >= 1 << 255 else v


def as_addr(b):
    v = as_int(b)
    return None if not v else "0x" + format(v, "040x")


def eth_usd_at(rpc, block):
    """ETH in USD from Uniswap's 0.05% WETH/USDC pool: token0 WETH (18), token1 USDC (6)."""
    s0 = multicall(rpc, [(WETH_USDC_UNI, SEL["slot0"])], block)[0]
    sqrt = as_int(s0)
    return (sqrt / 2**96) ** 2 * 1e12


def tick_spacings(rpc, factory, block):
    raw = multicall(rpc, [(factory, SEL["tickSpacings"])], block)[0]
    n = as_int(raw[32:64])
    return [as_int(raw[64 + 32 * i:96 + 32 * i], signed=True) for i in range(n)]


def discover(rpc, tokens, block):
    found = []
    for venue, v in VENUES.items():
        tiers = v["tiers"] if v["tiers"] is not None else tick_spacings(rpc, v["factory"], block)
        sel = {"fee": SEL["getPool_fee"], "spacing": SEL["getPool_spacing"], "volatile": SEL["getPool_stable"]}[v["kind"]]
        asks = [(t, tier) for t in tokens for tier in tiers]
        calls = [(v["factory"], sel + word(WETH) + word(t) + word(False if tier is None else tier)) for t, tier in asks]
        pools = [as_addr(r) for r in multicall(rpc, calls, block)]
        hits = [(p, t, tier) for p, (t, tier) in zip(pools, asks) if p]
        # The pool's WETH and its fee, at the same block.
        reads = []
        for p, _, _ in hits:
            reads.append((WETH, SEL["balanceOf"] + word(p)))
            reads.append((v["factory"], SEL["getFee"] + word(p) + word(False)) if v["kind"] == "volatile" else (p, SEL["fee"]))
        res = multicall(rpc, reads, block)
        # A dynamic fee: the median of its readings over two hours.
        earlier = {}
        if v["kind"] == "spacing":
            for b in fee_blocks(block)[1:]:
                earlier[b] = multicall(rpc, [(p, SEL["fee"]) for p, _, _ in hits], b)
        for k, (p, t, tier) in enumerate(hits):
            bal, fee = as_int(res[2 * k]), as_int(res[2 * k + 1])
            if bal is None or fee is None:
                continue
            if earlier:
                fees = [fee] + [as_int(earlier[b][k]) for b in earlier]
                fee = median_fee([x for x in fees if x is not None])
            fee_ppm = fee * 100 if v["kind"] == "volatile" else fee
            found.append(Found(pool=p, venue=venue, token=t.lower(), tier=tier, fee_ppm=fee_ppm, weth_wei=bal))
        print(f"{venue}: {len(asks)} asked, {len(hits)} pools", file=sys.stderr)
    return found


def candidate_tokens(inventories):
    toks = set()
    for f in sorted(glob.glob(str(REPO / "data" / "census" / "day-*.json.gz"))):
        for a in json.load(gzip.open(f))["arbs"]:
            if CENSUS_VENUES & set(a["venues"]):
                for pair in a["pairs"]:
                    toks.update(x.lower() for x in pair)
    for recs in inventories.values():
        for r in recs:
            pair = {r["token0"].lower(), r["token1"].lower()}
            # Aerodrome v2's inventory is its whole factory: thousands of
            # dead WETH pairs. A token is asked again if its pool held $10k.
            if WETH in pair and (r.get("hub_usd_liquidity") or 0) >= 10_000:
                toks |= pair
    toks.discard(WETH)
    return sorted(toks)


def read_inventory(venue):
    path = BASE_DATA / venue / "pools.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--write", action="store_true", help="rewrite the inventories (backed up first)")
    ap.add_argument("--block", type=int, help="pin the reads to this block (default: the head less 5)")
    ap.add_argument("--out", help="write the merged inventories under this directory instead, for review")
    a = ap.parse_args()
    urls = [u.strip() for u in os.environ.get("BASE_RPC_URL", "").split(",")]
    if not any(urls):
        raise SystemExit("set BASE_RPC_URL")
    rpc = Rpc(urls)
    block = a.block or int(rpc.call("eth_blockNumber", []), 16) - 5
    before = {v: read_inventory(v) for v in VENUES}
    tokens = candidate_tokens(before)
    eth_usd = eth_usd_at(rpc, block)
    print(f"block {block}: ETH ${eth_usd:,.2f}; {len(tokens)} candidate tokens", file=sys.stderr)
    found = discover(rpc, tokens, block)
    after, report = {}, {}
    for v in VENUES:
        after[v], s = merge(before[v], [f for f in found if f.venue == v], eth_usd, block)
        report[v] = dataclasses.asdict(s)
    p0, c0 = universe(before, 3000, 100_000)
    p1, c1 = universe(after, 3000, 100_000)
    added = sorted(set(p1) - set(p0))
    dropped = sorted(set(p0) - set(p1))
    print(json.dumps({"block": block, "eth_usd": round(eth_usd, 2), "venues": report,
                      "universe": {"before": {"pools": len(p0), "cycles": c0}, "after": {"pools": len(p1), "cycles": c1},
                                   "added": added, "dropped": dropped}}, indent=1))
    if a.out:
        for v in VENUES:
            path = Path(a.out) / v / "pools.jsonl"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("".join(json.dumps(r) + "\n" for r in after[v]))
        print(f"merged inventories under {a.out}", file=sys.stderr)
    if a.write:
        stamp = time.strftime("%Y%m%dT%H%M%S")
        for v in VENUES:
            path = BASE_DATA / v / "pools.jsonl"
            shutil.copy2(path, path.with_name(f"pools.jsonl.bak-{stamp}"))
            path.write_text("".join(json.dumps(r) + "\n" for r in after[v]))
        print(f"written; backups pools.jsonl.bak-{stamp}", file=sys.stderr)


if __name__ == "__main__":
    main()
