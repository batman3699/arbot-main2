#!/usr/bin/env python3
"""
build_slipstream_pools.py — Enumerate liquid Aerodrome Slipstream (Base) CL pools and emit
data/base/aerodrome_slipstream/pools.jsonl for the bot.

Slipstream uses tickSpacing (not UniV3 fee tiers). PoolRecord.fee stores tick spacing.
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
    key = os.environ.get("ALCHEMY_KEY", "").strip()
    if key:
        return f"https://base-mainnet.g.alchemy.com/v2/{key}"
    print(
        "FATAL: set BASE_RPC_URL (or BASE_RPC_URLS, or ALCHEMY_KEY) to a Base RPC endpoint",
        file=sys.stderr,
    )
    sys.exit(2)


RPC_URL = _resolve_rpc_url()

FACTORY = "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A"
ID_GETPOOL = "0x28af8d0b"  # getPool(address,address,int24)
ID_LIQUIDITY = "0x1a686502"  # liquidity()
ID_BALANCEOF = "0x70a08231"

TICK_SPACINGS = [1, 10, 50, 100, 200, 2000]

WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
UNIV3_QUOTER = "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a"

WETH_USD_FALLBACK = float(os.environ.get("RANK_WETH_USD", "2500"))


def eth_call(session: requests.Session, to: str, data: str) -> str | None:
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
    }
    r = session.post(RPC_URL, json=payload, timeout=20)
    r.raise_for_status()
    result = r.json().get("result")
    if not isinstance(result, str) or result in ("0x", "0x0"):
        return None
    return result


def fetch_weth_usd(session: requests.Session) -> float:
    amount_in = 10**15
    data = (
        "0xcdca1753"
        + WETH[2:].lower().rjust(64, "0")
        + USDC[2:].lower().rjust(64, "0")
        + format(amount_in, "064x")
        + format(500, "064x")
        + "0" * 64
    )
    try:
        result = eth_call(session, UNIV3_QUOTER, data)
        if result and len(result) >= 66:
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


def _extra_majors() -> list:
    """Extra non-hub tokens to pair against, comma-separated in EXTRA_MAJORS.

    These builders shipped with tiny hardcoded token sets, which is what kept
    the Base inventories at 15-17 pools. Entries are validated as 20-byte hex so
    a malformed address fails loudly here rather than silently yielding a pool
    that never resolves.
    """
    raw = os.environ.get("EXTRA_MAJORS", "").strip()
    if not raw:
        return []
    out, seen = [], set()
    for token in raw.split(","):
        token = token.strip().lower()
        if not token:
            continue
        if not (token.startswith("0x") and len(token) == 42):
            print(f"FATAL: malformed EXTRA_MAJORS entry {token!r}", file=sys.stderr)
            sys.exit(2)
        try:
            int(token, 16)
        except ValueError:
            print(f"FATAL: non-hex EXTRA_MAJORS entry {token!r}", file=sys.stderr)
            sys.exit(2)
        if token not in seen:
            seen.add(token)
            out.append(token)
    return out


def get_pool(session: requests.Session, token_a: str, token_b: str, tick_spacing: int) -> str | None:
    a = token_a.lower().replace("0x", "").rjust(64, "0")
    b = token_b.lower().replace("0x", "").rjust(64, "0")
    spacing = tick_spacing
    if spacing >= 0:
        spacing_hex = format(spacing, "064x")
    else:
        spacing_hex = format(spacing & ((1 << 256) - 1), "064x")
    data = ID_GETPOOL + a + b + spacing_hex
    result = eth_call(session, FACTORY, data)
    if not result:
        return None
    addr = "0x" + result[-40:]
    if int(addr, 16) == 0:
        return None
    return addr


def pool_liquidity(session: requests.Session, pool: str) -> int:
    result = eth_call(session, pool, ID_LIQUIDITY)
    if not result:
        return 0
    return int(result, 16)


def hub_balance(session: requests.Session, token: str, pool: str) -> int:
    data = ID_BALANCEOF + pool.lower().replace("0x", "").rjust(64, "0")
    result = eth_call(session, token, data)
    if not result:
        return 0
    return int(result, 16)


def hub_side_liquidity_usd(
    session: requests.Session, token0: str, token1: str, pool: str, hubs: dict
) -> float:
    total = 0.0
    for hub, (_, decimals, usd) in hubs.items():
        hub_l = hub.lower()
        if hub_l == token0.lower():
            bal = hub_balance(session, token0, pool)
            total += (bal / (10**decimals)) * usd
        elif hub_l == token1.lower():
            bal = hub_balance(session, token1, pool)
            total += (bal / (10**decimals)) * usd
    return total


def main() -> int:
    session = requests.Session()
    weth_usd = fetch_weth_usd(session)
    hubs = build_hubs(weth_usd)
    min_usd = float(os.environ.get("MIN_POOL_LIQ_USD", "25000"))

    hub_addrs = sorted(hubs.keys(), key=lambda x: int(x, 16))
    records: list[dict] = []
    seen: set[tuple[str, str, int]] = set()

    # Hub x hub alone is only C(8,2)=28 pairs, which capped this builder at 15
    # pools. Pair the hubs against additional tokens too, but keep at least one
    # hub on every pair: `hub_side_liquidity_usd` prices the pool from its hub
    # side, so a major/major pool has no measurable USD value here and would be
    # admitted unranked.
    hub_set = {a.lower() for a in hub_addrs}
    extra = _extra_majors()
    scan_addrs = hub_addrs + [a for a in extra if a.lower() not in hub_set]
    if extra:
        print(f"scanning {len(hub_addrs)} hubs + {len(scan_addrs) - len(hub_addrs)} extra tokens")
    for i, token0 in enumerate(scan_addrs):
        for token1 in scan_addrs[i + 1 :]:
            if token0.lower() not in hub_set and token1.lower() not in hub_set:
                continue
            t0, t1 = (
                (token0, token1)
                if int(token0, 16) < int(token1, 16)
                else (token1, token0)
            )
            for ts in TICK_SPACINGS:
                key = (t0.lower(), t1.lower(), ts)
                if key in seen:
                    continue
                pool = get_pool(session, t0, t1, ts)
                if not pool:
                    continue
                liq = pool_liquidity(session, pool)
                if liq == 0:
                    continue
                hub_usd = hub_side_liquidity_usd(session, t0, t1, pool, hubs)
                if hub_usd < min_usd:
                    continue
                seen.add(key)
                records.append(
                    {
                        "pool": pool,
                        "token0": t0,
                        "token1": t1,
                        "fee": ts,
                        "created_block": 0,
                        "hub_usd_liquidity": round(hub_usd, 2),
                    }
                )
                # Only one side is guaranteed to be a hub now that non-hub
                # tokens are scanned, so label by address when unknown.
                def _label(addr: str) -> str:
                    entry = hubs.get(addr.lower())
                    return entry[0] if entry else addr[:10]

                print(
                    f"  pool={pool} {_label(t0)}/{_label(t1)} ts={ts} liq={liq} hub_usd={hub_usd:.0f}",
                    file=sys.stderr,
                )

    out_dir = os.path.join(
        os.environ.get("POOL_DATA_ROOT", "data"),
        "base",
        "aerodrome_slipstream",
    )
    os.makedirs(out_dir, exist_ok=True)
    out_path = os.path.join(out_dir, "pools.jsonl")
    with open(out_path, "w", encoding="utf-8") as fh:
        for rec in sorted(records, key=lambda r: -(r.get("hub_usd_liquidity") or 0)):
            fh.write(json.dumps(rec) + "\n")

    print(f"Wrote {len(records)} Slipstream pools to {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
