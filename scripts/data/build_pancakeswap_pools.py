#!/usr/bin/env python3
"""
build_pancakeswap_pools.py — Enumerate liquid PancakeSwap V3 (Base) CL pools and emit
data/base/pancakeswap_v3/pools.jsonl for the bot.

PancakeSwap V3 on Base is UniV3-compatible (same getPool / fee tiers / tick math).
Official addresses: https://developer.pancakeswap.finance/contracts/v3/addresses
"""
import json
import os
import sys

import requests

ALCHEMY_KEY = os.environ.get("ALCHEMY_KEY", "").strip()
if not ALCHEMY_KEY:
    print("FATAL: ALCHEMY_KEY not set", file=sys.stderr)
    sys.exit(2)
RPC_URL = f"https://base-mainnet.g.alchemy.com/v2/{ALCHEMY_KEY}"

# PancakeSwap V3 on Base (canonical per PancakeSwap developer docs)
V3_FACTORY = "0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865"
V3_FEES = [100, 500, 2500, 10000]

WETH_USD = float(os.environ.get("RANK_WETH_USD", "2500"))
HUBS = {
    "0x4200000000000000000000000000000000000006": ("WETH", 18, WETH_USD),
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": ("USDC", 6, 1.0),
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": ("USDbC", 6, 1.0),
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb": ("DAI", 18, 1.0),
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": ("cbBTC", 8, float(os.environ.get("RANK_CBTC_USD", "95000"))),
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22": ("cbETH", 18, WETH_USD),
    "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452": ("wstETH", 18, WETH_USD),
    "0x940181a94a35a4569e4529a3cdfb74e38fd98631": ("AERO", 18, float(os.environ.get("RANK_AERO_USD", "0.35"))),
}
MAJORS = [
    "0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b",  # VIRTUAL
    "0x532f27101965dd16442e59d40670faf5ebb142e4",  # BRETT
    "0x4ed4e862860bed51a9570b96d89af5e1b0efefed",  # DEGEN
    "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2",  # USDT
    "0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42",  # EURC
    "0x1bc0c42215582d5a085795f4badbac3ff36d1bcb",  # CLANKER
]

ID_GETPOOL = "0x1698ee82"
ID_BALANCEOF = "0x70a08231"
ID_DECIMALS = "0x313ce567"
ID_TOKEN0 = "0x0dfe1681"
ID_TOKEN1 = "0xd21220a7"

MIN_USD = float(os.environ.get("PANCAKE_MIN_USD", "25000"))
OUT_PATH = os.environ.get("PANCAKE_OUT", "data/base/pancakeswap_v3/pools.jsonl")


def w(addr: str) -> str:
    return addr.lower().replace("0x", "").rjust(64, "0")


def rpc_batch(session, calls):
    payload = [
        {"jsonrpc": "2.0", "id": cid, "method": "eth_call",
         "params": [{"to": to, "data": data}, "latest"]}
        for (cid, to, data) in calls
    ]
    out = {}
    for attempt in range(4):
        try:
            r = session.post(RPC_URL, json=payload, timeout=30)
            r.raise_for_status()
            for item in r.json():
                out[item.get("id")] = item.get("result")
            return out
        except Exception:  # noqa: BLE001
            if attempt == 3:
                return out
    return out


def addr_from_word(hexword: str):
    if not hexword or len(hexword) < 66:
        return None
    a = "0x" + hexword[-40:]
    if int(a, 16) == 0:
        return None
    return a.lower()


def verify_majors(session):
    tokens = {}
    calls = [(i, a, ID_DECIMALS) for i, a in enumerate(MAJORS)]
    res = rpc_batch(session, calls)
    for i, a in enumerate(MAJORS):
        d = res.get(i)
        if isinstance(d, str) and len(d) >= 66:
            try:
                dec = int(d[:66], 16)
                if 0 < dec <= 36:
                    tokens[a] = dec
            except ValueError:
                pass
    return tokens


def main():
    session = requests.Session()
    majors = verify_majors(session)
    print(f"verified majors: {len(majors)} of {len(MAJORS)}")

    others = list(HUBS.keys()) + list(majors.keys())
    pool_calls = []
    meta = {}
    cid = 0
    seen = set()
    for hub in HUBS:
        for other in others:
            if other == hub:
                continue
            key = tuple(sorted((hub, other)))
            if key in seen:
                continue
            seen.add(key)
            for fee in V3_FEES:
                data = ID_GETPOOL + w(hub) + w(other) + format(fee, "064x")
                pool_calls.append((cid, V3_FACTORY, data))
                meta[cid] = (hub, fee)
                cid += 1

    print(f"enumerating {len(pool_calls)} PancakeSwap V3 factory lookups...")
    found = {}
    for k in range(0, len(pool_calls), 60):
        res = rpc_batch(session, pool_calls[k:k + 60])
        for c, hexres in res.items():
            pool = addr_from_word(hexres)
            if pool is None:
                continue
            hub, fee = meta[c]
            found.setdefault(pool, (hub, fee))

    print(f"  found {len(found)} pools with code")

    bal_calls = []
    bmeta = {}
    for i, (pool, (hub, _fee)) in enumerate(found.items()):
        bal_calls.append((i, hub, ID_BALANCEOF + w(pool)))
        bmeta[i] = pool
    balances = {}
    for k in range(0, len(bal_calls), 60):
        res = rpc_batch(session, bal_calls[k:k + 60])
        for c, hexres in res.items():
            pool = bmeta[c]
            if isinstance(hexres, str) and len(hexres) >= 66:
                try:
                    balances[pool] = int(hexres[:66], 16)
                except ValueError:
                    balances[pool] = 0

    scored = []
    for pool, (hub, fee) in found.items():
        bal = balances.get(pool, 0)
        if bal <= 0:
            continue
        sym, dec, price = HUBS[hub]
        usd = (bal / (10 ** dec)) * price
        if usd < MIN_USD:
            continue
        scored.append((pool, hub, fee, usd))

    kept_pools = [p for (p, _h, _f, _u) in scored]
    t0_calls = [(i, p, ID_TOKEN0) for i, p in enumerate(kept_pools)]
    t1_calls = [(i, p, ID_TOKEN1) for i, p in enumerate(kept_pools)]
    t0, t1 = {}, {}
    for k in range(0, len(t0_calls), 60):
        r0 = rpc_batch(session, t0_calls[k:k + 60])
        for c, hexres in r0.items():
            t0[kept_pools[c]] = addr_from_word(hexres)
    for k in range(0, len(t1_calls), 60):
        r1 = rpc_batch(session, t1_calls[k:k + 60])
        for c, hexres in r1.items():
            t1[kept_pools[c]] = addr_from_word(hexres)

    records = []
    for pool, hub, fee, usd in sorted(scored, key=lambda x: x[3], reverse=True):
        a0, a1 = t0.get(pool), t1.get(pool)
        if not a0 or not a1:
            continue
        records.append({
            "pool": pool,
            "token0": a0,
            "token1": a1,
            "fee": fee,
            "created_block": 0,
            "hub_usd_liquidity": round(usd, 2),
        })

    os.makedirs(os.path.dirname(OUT_PATH), exist_ok=True)
    existing = []
    if os.path.exists(OUT_PATH):
        with open(OUT_PATH) as fh:
            for line in fh:
                line = line.strip()
                if line:
                    try:
                        existing.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
    by_pool = {r["pool"].lower(): r for r in existing}
    for r in records:
        by_pool[r["pool"].lower()] = r
    merged = sorted(by_pool.values(), key=lambda r: r.get("hub_usd_liquidity", 0), reverse=True)
    with open(OUT_PATH, "w") as fh:
        for r in merged:
            fh.write(json.dumps(r) + "\n")
    print(f"wrote {len(merged)} records -> {OUT_PATH}")


if __name__ == "__main__":
    main()
