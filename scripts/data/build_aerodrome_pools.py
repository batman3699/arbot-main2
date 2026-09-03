#!/usr/bin/env python3
"""
build_aerodrome_pools.py — Enumerate liquid Aerodrome (Base) AMM pools from the
PoolFactory and emit the BASE_SOLIDLY_V2_POOLS config the bot consumes.

Aerodrome AMM pools are Solidly-style (volatile vAMM + stable sAMM). The bot's
`collect_solidly_edges` path already quotes them with correct stable/volatile
math; it just needs a pool list: [{pair, tokenIn, tokenOut, stable, feeBps}].

For every hub x {hubs+majors} pair we query getPool(a,b,false|true), read the
real per-pool fee via factory.getFee(pool,stable), measure hub-side balanceOf as
real liquidity, and (for liquid pools) emit BOTH swap directions so arbitrage
cycles can traverse the pool either way. Fees/liquidity are read live on-chain.
"""
import json
import os
import sys

import requests

# RPC endpoint. Prefer an explicit BASE_RPC_URL (or the first entry of
# BASE_RPC_URLS, which is what the bot itself uses), and only fall back to
# Alchemy. The hardcoded Alchemy URL made these builders unrunnable once that
# key hit its monthly quota, which is why the pool inventories went stale.
def _resolve_rpc_url():
    explicit = os.environ.get("BASE_RPC_URL", "").strip()
    if explicit:
        return explicit
    urls = os.environ.get("BASE_RPC_URLS", "").strip()
    if urls:
        first = urls.split(",")[0].strip()
        if first:
            return first
    key = os.environ.get("BLOCKPI_KEY", "").strip()
    if key:
        return f"https://base.blockpi.network/v1/rpc/{key}"
    print(
        "FATAL: set BASE_RPC_URL (or BASE_RPC_URLS, or ALCHEMY_KEY) to a Base RPC endpoint",
        file=sys.stderr,
    )
    sys.exit(2)

RPC_URL = _resolve_rpc_url()

FACTORY = "0x420dd381b31aef6683db6b902084cb0ffece40da"

ID_GETPOOL = "0x79bc57d5"    # getPool(address,address,bool)
ID_GETFEE = "0xcc56b2c5"     # getFee(address,bool)
ID_BALANCEOF = "0x70a08231"  # balanceOf(address)

WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
QUOTER = "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a"

WETH_USD_FALLBACK = float(os.environ.get("RANK_WETH_USD", "2500"))


def fetch_weth_usd(session: requests.Session) -> float:
    """Live WETH/USDC quote from Base QuoterV2 (500 bps tier)."""
    amount_in = 10**15
    data = (
        "0xcdca1753"
        + WETH[2:].lower().rjust(64, "0")
        + USDC[2:].lower().rjust(64, "0")
        + format(amount_in, "064x")
        + format(500, "064x")
        + "0" * 64
    )
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [{"to": QUOTER, "data": data}, "latest"],
    }
    try:
        r = session.post(RPC_URL, json=payload, timeout=20)
        r.raise_for_status()
        result = r.json().get("result")
        if isinstance(result, str) and len(result) >= 66:
            usdc_out = int(result[:66], 16)
            return (usdc_out / 1e6) / (amount_in / 1e18)
    except Exception as exc:  # noqa: BLE001
        print(f"  WARN: live WETH quote failed ({exc}); using fallback", file=sys.stderr)
    return WETH_USD_FALLBACK


def build_hubs(weth_usd: float) -> dict:
    cbtc_usd = float(os.environ.get("RANK_CBTC_USD", "95000"))
    aero_usd = float(os.environ.get("RANK_AERO_USD", "0.35"))
    return {
        "0x4200000000000000000000000000000000000006": ("WETH", 18, weth_usd),
        "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": ("USDC", 6, 1.0),
        "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": ("USDbC", 6, 1.0),
        "0x50c5725949a6f0c72e6c4a641f24049a917db0cb": ("DAI", 18, 1.0),
        "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": ("cbBTC", 8, cbtc_usd),
        "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22": ("cbETH", 18, weth_usd),
        "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452": ("wstETH", 18, weth_usd),
        "0x940181a94a35a4569e4529a3cdfb74e38fd98631": ("AERO", 18, aero_usd),
    }


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

def _extra_majors() -> list:
    """Additional non-hub tokens to pair against, comma-separated.

    The hardcoded MAJORS list above is 8 tokens, which capped this builder at
    ~17 pools on Base's deepest DEX. The ranked UniV3 inventory already names
    500+ non-hub tokens that cleared a real USD liquidity bar, so the useful
    list is data, not a literal. Sourced addresses are deduped against MAJORS
    and validated as 20-byte hex so a malformed entry fails loudly here rather
    than silently producing a pool that never resolves.
    """
    raw = os.environ.get("AERO_EXTRA_MAJORS", "").strip()
    if not raw:
        return []
    out, seen = [], {m.lower() for m in MAJORS}
    for token in raw.split(","):
        token = token.strip().lower()
        if not token:
            continue
        if not (token.startswith("0x") and len(token) == 42):
            print(f"FATAL: malformed AERO_EXTRA_MAJORS entry {token!r}", file=sys.stderr)
            sys.exit(2)
        try:
            int(token, 16)
        except ValueError:
            print(f"FATAL: non-hex AERO_EXTRA_MAJORS entry {token!r}", file=sys.stderr)
            sys.exit(2)
        if token not in seen:
            seen.add(token)
            out.append(token)
    return out


MAJORS = MAJORS + _extra_majors()
MIN_USD = float(os.environ.get("AERO_MIN_USD", "10000"))
OUT_PATH = os.environ.get("AERO_OUT", "config/base_aerodrome_pools.json")


def w(addr: str) -> str:
    return addr.lower().replace("0x", "").rjust(64, "0")


def wbool(b: bool) -> str:
    return ("1" if b else "0").rjust(64, "0")


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


def addr_from_word(hexword):
    if not hexword or len(hexword) < 66:
        return None
    a = "0x" + hexword[-40:]
    return None if int(a, 16) == 0 else a.lower()


def int_from_word(hexword):
    if not hexword or len(hexword) < 66:
        return 0
    try:
        return int(hexword[:66], 16)
    except ValueError:
        return 0



def main():
    session = requests.Session()
    weth_usd = fetch_weth_usd(session)
    HUBS = build_hubs(weth_usd)
    print(f"WETH/USD (live quoter): ${weth_usd:,.2f}")
    tokens = list(HUBS.keys()) + [m.lower() for m in MAJORS]
    hub_list = list(HUBS.keys())

    # 1) enumerate getPool(a,b,stable) for every hub x other pair, both flavors
    calls = []
    meta = {}
    cid = 0
    seen = set()
    for hub in hub_list:
        for other in tokens:
            if other == hub:
                continue
            key = tuple(sorted((hub, other)))
            if key in seen:
                continue
            seen.add(key)
            for stable in (False, True):
                calls.append((cid, FACTORY, ID_GETPOOL + w(hub) + w(other) + wbool(stable)))
                meta[cid] = (hub, other, stable)
                cid += 1
    print(f"enumerating {len(calls)} Aerodrome getPool lookups...")
    found = {}  # pool -> (hub, other, stable)
    for k in range(0, len(calls), 60):
        res = rpc_batch(session, calls[k:k + 60])
        for c, hexres in res.items():
            pool = addr_from_word(hexres)
            if pool and pool not in found:
                found[pool] = meta[c]
    print(f"  found {len(found)} existing pools")

    # 2) hub-side balanceOf + getFee
    bal_calls, fee_calls = [], []
    bmeta, fmeta = {}, {}
    cid = 0
    for pool, (hub, other, stable) in found.items():
        bal_calls.append((cid, hub, ID_BALANCEOF + w(pool)))
        fee_calls.append((cid, FACTORY, ID_GETFEE + w(pool) + wbool(stable)))
        bmeta[cid] = pool
        fmeta[cid] = pool
        cid += 1
    balances, fees = {}, {}
    for k in range(0, len(bal_calls), 60):
        for c, hx in rpc_batch(session, bal_calls[k:k + 60]).items():
            balances[bmeta[c]] = int_from_word(hx)
    for k in range(0, len(fee_calls), 60):
        for c, hx in rpc_batch(session, fee_calls[k:k + 60]).items():
            fees[fmeta[c]] = int_from_word(hx)

    # 3) score + emit liquid pools, both directions
    kept = []
    for pool, (hub, other, stable) in found.items():
        bal = balances.get(pool, 0)
        if bal <= 0:
            continue
        sym, dec, price = HUBS[hub]
        usd = (bal / (10 ** dec)) * price
        if usd < MIN_USD:
            continue
        fee_bps = fees.get(pool, 0)
        if fee_bps <= 0 or fee_bps > 1000:
            # sanity: Aerodrome fees are small bps; skip implausible reads
            continue
        # Aerodrome sorts token0 < token1 by address
        t0, t1 = (hub, other) if hub < other else (other, hub)
        kept.append({"pool": pool, "token0": t0, "token1": t1,
                     "stable": stable, "fee_bps": fee_bps, "hub": hub,
                     "usd": round(usd, 2)})

    kept.sort(key=lambda r: r["usd"], reverse=True)

    entries = []
    for r in kept:
        for (ti, to) in ((r["token0"], r["token1"]), (r["token1"], r["token0"])):
            entries.append({
                "pair": r["pool"],
                "tokenIn": ti,
                "tokenOut": to,
                "stable": r["stable"],
                "feeBps": r["fee_bps"],
            })

    os.makedirs(os.path.dirname(OUT_PATH), exist_ok=True)
    with open(OUT_PATH, "w") as fh:
        json.dump(entries, fh, indent=2)
        fh.write("\n")
    print(f"\nwrote {len(entries)} directional entries ({len(kept)} pools) -> {OUT_PATH}")
    print("top liquid Aerodrome pools:")
    for r in kept[:14]:
        kind = "stable " if r["stable"] else "volatile"
        print(f"  ${r['usd']:>14,.0f}  {kind}  fee={r['fee_bps']:>3}bps  {r['pool']}")


if __name__ == "__main__":
    main()
