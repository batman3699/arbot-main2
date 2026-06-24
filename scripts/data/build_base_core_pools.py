#!/usr/bin/env python3
"""
build_base_core_pools.py — Build a REAL, liquidity-anchored Base pool inventory
by enumerating pools directly from the Uniswap V2/V3 factories for every
hub-anchored token pair, probing on-chain reserves, and keeping the liquid ones.

The shipped inventories are creation-ordered dumps that miss Base's actual
liquid pools (and Base UniV2 is essentially dead). Enumerating factory.getPool /
getPair across {hubs} x {hubs + majors} x {fee tiers} guarantees we capture the
genuinely liquid venues (WETH/USDC, WETH/cbBTC, USDC/cbBTC, ...). Liquidity is
measured as the hub token's real ERC-20 balanceOf(pool); USD weights only order
the set — the bot re-quotes everything on-chain before sizing/execution.
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

V3_FACTORY = "0x33128a8fc17869897dce68ed026d694621f6fdfd"
V2_FACTORY = "0x8909dc15e40173ff4699343b6eb8132c65e18ec6"
V3_FEES = [100, 500, 3000, 10000]

WETH_USD = 1638.12
# hub address(lower) -> (symbol, decimals, usd_price)
HUBS = {
    "0x4200000000000000000000000000000000000006": ("WETH", 18, WETH_USD),
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": ("USDC", 6, 1.0),
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": ("USDbC", 6, 1.0),
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb": ("DAI", 18, 1.0),
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": ("cbBTC", 8, 61601.79),
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22": ("cbETH", 18, WETH_USD),
    "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452": ("wstETH", 18, WETH_USD),
    "0x940181a94a35a4569e4529a3cdfb74e38fd98631": ("AERO", 18, 0.332132),
}
# Candidate non-hub majors (verified on-chain below; bad ones dropped).
MAJORS = [
    "0x0b3e328455c4059eeb9e3f84b5543f74e24e7e1b",  # VIRTUAL
    "0x532f27101965dd16442e59d40670faf5ebb142e4",  # BRETT
    "0x4ed4e862860bed51a9570b96d89af5e1b0efefed",  # DEGEN
    "0xac1bd2486aaf3b5c0fc3fd868558b082a531b2b4",  # TOSHI
    "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2",  # USDT
    "0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42",  # EURC
    "0x9a26f5433671751c3276a065f57e5a02d2817973",  # KEYCAT
    "0x1bc0c42215582d5a085795f4badbac3ff36d1bcb",  # CLANKER
]

ID_GETPOOL = "0x1698ee82"   # getPool(address,address,uint24)
ID_GETPAIR = "0xe6a43905"   # getPair(address,address)
ID_BALANCEOF = "0x70a08231"  # balanceOf(address)
ID_SYMBOL = "0x95d89b41"     # symbol()
ID_DECIMALS = "0x313ce567"   # decimals()

MIN_USD_V3 = float(os.environ.get("CORE_MIN_USD_V3", "25000"))
MIN_USD_V2 = float(os.environ.get("CORE_MIN_USD_V2", "5000"))


def w(addr: str) -> str:
    return addr.lower().replace("0x", "").rjust(64, "0")


def rpc_batch(session, calls):
    """calls: list of (id, to, data). Returns dict id -> result hex (or None)."""
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
    calls = []
    for i, a in enumerate(MAJORS):
        calls.append((i, a, ID_DECIMALS))
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

    # token decimals map
    token_dec = {a: d for a, (s, d, p) in HUBS.items()}
    token_dec.update(majors)
    others = list(HUBS.keys()) + list(majors.keys())

    # 1) enumerate candidate pools (getPool/getPair) for every hub x other pair
    pool_calls = []
    meta = {}  # cid -> (venue, hub, other, fee)
    cid = 0
    seen_pairs = set()
    hub_list = list(HUBS.keys())
    for hub in hub_list:
        for other in others:
            if other == hub:
                continue
            key = tuple(sorted((hub, other)))
            if (key, "scan") in seen_pairs:
                continue
            seen_pairs.add((key, "scan"))
            for fee in V3_FEES:
                data = ID_GETPOOL + w(hub) + w(other) + format(fee, "064x")
                pool_calls.append((cid, V3_FACTORY, data))
                meta[cid] = ("uniswap_v3", hub, other, fee)
                cid += 1
            data = ID_GETPAIR + w(hub) + w(other)
            pool_calls.append((cid, V2_FACTORY, data))
            meta[cid] = ("uniswap_v2", hub, other, 30)
            cid += 1

    print(f"enumerating {len(pool_calls)} factory lookups...")
    found = {}  # pool_addr -> (venue, hub, fee)
    for k in range(0, len(pool_calls), 60):
        res = rpc_batch(session, pool_calls[k:k + 60])
        for c, hexres in res.items():
            pool = addr_from_word(hexres)
            if pool is None:
                continue
            venue, hub, other, fee = meta[c]
            if pool not in found:
                found[pool] = (venue, hub, fee)
    print(f"  found {len(found)} existing pools")

    # 2) probe hub-side balanceOf for each found pool
    bal_calls = []
    bmeta = {}
    cid = 0
    for pool, (venue, hub, fee) in found.items():
        bal_calls.append((cid, hub, ID_BALANCEOF + w(pool)))
        bmeta[cid] = pool
        cid += 1
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

    # 3) score + split by venue
    rows = {"uniswap_v2": [], "uniswap_v3": []}
    for pool, (venue, hub, fee) in found.items():
        bal = balances.get(pool, 0)
        if bal <= 0:
            continue
        sym, dec, price = HUBS[hub]
        usd = (bal / (10 ** dec)) * price
        floor = MIN_USD_V3 if venue == "uniswap_v3" else MIN_USD_V2
        if usd < floor:
            continue
        # token0/token1 by address sort order (Uniswap convention)
        venue_hub, other = hub, None
        # recover the other token: we stored only hub+fee; re-derive from meta not kept,
        # so read token0/token1 ordering from the pair we know (hub, ?) — instead, we
        # record token0/token1 by sorting hub with the counterparty captured below.
        rows[venue].append((pool, hub, fee, usd))

    # We need token0/token1; recover counterparty by re-scanning meta via found-origin.
    # Simpler: re-derive counterparty by mapping pool->(hub,other) during enumeration.
    # (Re-run enumeration mapping is avoided; instead read token0() per kept pool.)
    kept_pools = [p for v in rows for (p, _h, _f, _u) in rows[v]]
    t0_calls = [(i, p, "0x0dfe1681") for i, p in enumerate(kept_pools)]  # token0()
    t1_calls = [(i, p, "0xd21220a7") for i, p in enumerate(kept_pools)]  # token1()
    t0 = {}
    t1 = {}
    for k in range(0, len(t0_calls), 60):
        r0 = rpc_batch(session, t0_calls[k:k + 60])
        for c, hexres in r0.items():
            t0[kept_pools[c]] = addr_from_word(hexres)
    for k in range(0, len(t1_calls), 60):
        r1 = rpc_batch(session, t1_calls[k:k + 60])
        for c, hexres in r1.items():
            t1[kept_pools[c]] = addr_from_word(hexres)

    def emit(venue):
        recs = []
        for (pool, hub, fee, usd) in sorted(rows[venue], key=lambda x: x[3], reverse=True):
            a0, a1 = t0.get(pool), t1.get(pool)
            if not a0 or not a1:
                continue
            recs.append({
                "pool": pool, "token0": a0, "token1": a1,
                "fee": fee, "created_block": 0,
                "hub_usd_liquidity": round(usd, 2),
            })
        return recs

    for venue in ("uniswap_v2", "uniswap_v3"):
        core = emit(venue)
        path = f"data/base/{venue}/pools.jsonl"
        os.makedirs(os.path.dirname(path), exist_ok=True)
        # MERGE, never overwrite. A previous version of this script replaced the
        # whole inventory with this ~16-token core set, which silently destroyed
        # a 491-pool factory ingest and collapsed the live universe to 24 pools
        # (graph pruned to ~95 edges, zero arbs). Union by pool address instead:
        # the core can only ADD the guaranteed-liquid majors, never shrink the
        # discovered long tail.
        existing = []
        if os.path.exists(path):
            with open(path) as fh:
                for line in fh:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        existing.append(json.loads(line))
                    except json.JSONDecodeError:
                        continue
        by_pool = {}
        for r in existing + core:  # core last so its enriched fields win on conflict
            pool = r.get("pool")
            if not pool:
                continue
            by_pool[pool.lower()] = r
        merged = list(by_pool.values())
        bak = path + ".core.bak"
        if os.path.exists(path) and not os.path.exists(bak):
            import shutil
            shutil.copy2(path, bak)  # COPY (non-destructive), don't move
        with open(path, "w") as fh:
            for r in merged:
                fh.write(json.dumps(r) + "\n")
        print(
            f"\n=== {venue}: merged {len(core)} liquid-core pools into "
            f"{len(existing)} existing -> {len(merged)} total at {path} ==="
        )
        for r in core[:10]:
            print(f"  ${r['hub_usd_liquidity']:>14,.0f}  {r['pool']}  fee={r['fee']}")


if __name__ == "__main__":
    main()
