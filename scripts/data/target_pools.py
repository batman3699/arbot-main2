"""R37: the census's targeted pools that the inventories do not hold, added.

The census targets the routes the market's arbitrage took. Some run through
pools newer than the last inventory refresh: on 2026-10-29 the AXIS/USDC
Uniswap v3 pools under the top v4 routes were missing, so those routes could
not arm. A targeted pool loads below the depth floor, so a record without
depth is enough.

Each record is read from the pool itself (factory, token0, token1, and its
tier: fee, tick spacing or curve) and is kept only when the factory is one
of our five venues' and its own `getPool` names this pool: a contract that
claims a factory it was not made by is refused. Its depth is its hub side,
as R30's records carry it: its USDC, or its WETH at the WETH/USDC pool's
price, with `hub_symbol` (every record must rank in the legacy loader).
Idempotent; v4 pools (32-byte IDs) are `v4_inventory.py`'s.

Run from scripts/data:

    python3 target_pools.py --targets /opt/apex/var/armed/targets.json --data /opt/apex/data/base --rpc public
"""
from __future__ import annotations

import argparse
import json
import os
import sys

from v4_inventory import client

# Each inventory directory's factory, and how it names a pool.
FACTORIES = {
    "0x33128a8fc17869897dce68ed026d694621f6fdfd": ("uniswap_v3", "fee"),
    "0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865": ("pancakeswap_v3", "fee"),
    "0x5e7bb104d84c7cb9b682aac2f3d509f5f406809a": ("aerodrome_slipstream", "spacing"),
    "0xf8f2eb4940cfe7d13603dddd87f123820fc061ef": ("aerodrome_slipstream_v3", "spacing"),
    "0x420dd381b31aef6683db6b902084cb0ffece40da": ("aerodrome_v2", "stable"),
}
WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
# The deepest WETH/USDC pool (Uniswap v3, 500 ppm): WETH's price for a depth.
WETH_USDC_UNI = "0xd0b53d9277642d899df5c87a3966a349a798f224"
SEL = {
    "factory": "0xc45a0155", "token0": "0x0dfe1681", "token1": "0xd21220a7", "fee": "0xddca3f43",
    "tickSpacing": "0xd0c93a7c", "stable": "0x22be3de1",
    "getPool_fee": "0x1698ee82", "getPool_spacing": "0x28af8d0b", "getPool_stable": "0x79bc57d5",
    "getFee": "0xcc56b2c5", "balanceOf": "0x70a08231", "slot0": "0x3850c7bd",
}


def _word(v: int) -> str:
    return "%064x" % (v % (1 << 256))


def _read(call, to: str, data: str) -> int | None:
    out = str(call("eth_call", [{"to": to, "data": data}, "latest"])).lower().removeprefix("0x")
    return int(out[:64], 16) if len(out) >= 64 else None


def _address(v: int | None) -> str | None:
    return None if not v else "0x%040x" % v


def _signed24(v: int) -> int:
    v &= (1 << 24) - 1
    return v - (1 << 24) if v >= 1 << 23 else v


def depth(call, pool: str, t0: str, t1: str):
    """(USD, hub symbol) of `pool`'s hub side: its USDC, or its WETH at the
    WETH/USDC pool's mid price; None without either."""
    if USDC in (t0, t1):
        bal = _read(call, USDC, SEL["balanceOf"] + pool[2:].rjust(64, "0"))
        return None if bal is None else (round(bal / 1e6, 2), "USDC")
    if WETH in (t0, t1):
        bal = _read(call, WETH, SEL["balanceOf"] + pool[2:].rjust(64, "0"))
        sqrt = _read(call, WETH_USDC_UNI, SEL["slot0"])
        if bal is None or not sqrt:
            return None
        usd_per_weth = (sqrt & ((1 << 160) - 1)) ** 2 / 2**192 * 1e12
        return round(bal / 1e18 * usd_per_weth, 2), "WETH"
    return None


def record_for(call, pool: str):
    """(venue directory, record) for `pool`, or (None, why it is refused)."""
    pool = pool.lower()
    factory = _address(_read(call, pool, SEL["factory"]))
    if factory not in FACTORIES:
        return None, "not one of our venues' factories"
    venue, kind = FACTORIES[factory]
    t0, t1 = _address(_read(call, pool, SEL["token0"])), _address(_read(call, pool, SEL["token1"]))
    if not t0 or not t1:
        return None, "no tokens"
    pair = t0[2:].rjust(64, "0") + t1[2:].rjust(64, "0")
    if kind == "fee":
        tier = _read(call, pool, SEL["fee"])
        named = _read(call, factory, SEL["getPool_fee"] + pair + _word(tier or 0))
        fee_ppm = tier
    elif kind == "spacing":
        raw = _read(call, pool, SEL["tickSpacing"])
        tier = None if raw is None else _signed24(raw)
        named = _read(call, factory, SEL["getPool_spacing"] + pair + _word(tier or 0))
        fee_ppm = _read(call, pool, SEL["fee"])
    else:
        tier = _read(call, pool, SEL["stable"])
        named = _read(call, factory, SEL["getPool_stable"] + pair + _word(tier or 0))
        bps = _read(call, factory, SEL["getFee"] + pool[2:].rjust(64, "0") + _word(tier or 0))
        fee_ppm = None if bps is None else bps * 100
    if tier is None or fee_ppm is None:
        return None, "no tier or fee"
    if _address(named) != pool:
        return None, "not its factory's pool"
    if kind == "stable":
        rec = {"pool": pool, "token0": t0, "token1": t1, "stable": bool(tier), "fee": fee_ppm // 100,
               "fee_ppm_onchain": fee_ppm, "created_block": None, "targeted": True}
    else:
        rec = {"pool": pool, "token0": t0, "token1": t1, "fee": tier, "created_block": 0,
               "fee_ppm_onchain": fee_ppm, "targeted": True}
    hub = depth(call, pool, t0, t1)
    if hub is not None:
        rec["hub_usd_liquidity"], rec["hub_symbol"] = hub
    return venue, rec


def _held(data: str) -> set[str]:
    held = set()
    for venue, _ in FACTORIES.values():
        path = os.path.join(data, venue, "pools.jsonl")
        if os.path.exists(path):
            with open(path) as f:
                held |= {json.loads(l)["pool"].lower() for l in f if l.strip()}
    return held


def missing(targets: dict, data: str) -> list[str]:
    """The targeted 20-byte pools no inventory holds, in route order."""
    held, out = _held(data), []
    for r in targets.get("routes", []):
        for p in r.get("pools", []):
            p = p.lower()
            if len(p) == 42 and p not in held and p not in out:
                out.append(p)
    return out


def add(call, targets: dict, data: str) -> dict:
    """Add each missing targeted pool's record to its venue's inventory."""
    added, refused = {}, {}
    for pool in missing(targets, data):
        venue, rec = record_for(call, pool)
        if venue is None:
            refused[pool] = rec
            continue
        path = os.path.join(data, venue, "pools.jsonl")
        rows = []
        if os.path.exists(path):
            with open(path) as f:
                rows = [l if l.endswith("\n") else l + "\n" for l in f if l.strip()]
        rows.append(json.dumps(rec) + "\n")
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            f.writelines(rows)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
        added.setdefault(venue, []).append(pool)
    return {"added": added, "refused": refused}


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--targets", required=True)
    p.add_argument("--data", required=True, help="the inventory root, data/base")
    p.add_argument("--rpc", required=True, help="an endpoint URL, or `public`")
    a = p.parse_args(argv)
    with open(a.targets) as f:
        targets = json.load(f)
    print(json.dumps(add(client(a.rpc)[0], targets, a.data)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
