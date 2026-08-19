#!/usr/bin/env python3
"""Build the Optimism UniV3 pool inventory by enumerating factory.getPool().

Mirrors scripts/data/build_base_core_pools.py. Event-scan discovery is not an
option on Optimism with a rate-limited provider: BlockPI caps eth_getLogs at 500
blocks, and the factory's first queryable block (105,235,063) is ~50.5M blocks
behind head — about 101,000 requests. Enumerating getPool over a curated token
list is bounded (len(tokens)^2/2 * len(fees) calls) and needs no log access.

Every token address here was verified on-chain (symbol + decimals) before being
added. Pools are emitted only if the factory returns a non-zero address AND the
pool holds a non-trivial balance of the priced side.

Usage:
  python3 scripts/data/build_optimism_core_pools.py            # writes inventory
  python3 scripts/data/build_optimism_core_pools.py --dry-run  # print only
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

import requests

FACTORY = "0x1f98431c8ad98523631ae4a59f267346ea31f984"
# ops/inputs.yaml optimism uniswap_v3 fee_tiers
V3_FEES = [100, 500, 3000]
OUT_PATH = Path("data/optimism/uniswap_v3/pools.jsonl")

# Verified on-chain 2026-08-19 against chain_id 10: symbol() and decimals().
# usd is a ranking input only (hub_usd_liquidity), not a trading price.
ETH_USD = float(os.environ.get("OPT_ETH_USD", "1911"))
BTC_USD = float(os.environ.get("OPT_BTC_USD", "61601"))
TOKENS: dict[str, tuple[str, int, float | None]] = {
    "0x4200000000000000000000000000000000000006": ("WETH", 18, ETH_USD),
    "0x0b2c639c533813f4aa9d7837caf62653d097ff85": ("USDC", 6, 1.0),
    "0x7f5c764cbc14f9669b88837ca1490cca17c31607": ("USDC.e", 6, 1.0),
    "0x94b008aa00579c1307b0ef2c499ad98a8ce58e58": ("USDT", 6, 1.0),
    "0xda10009cbd5d07dd0cecc66161fc93d7c9000da1": ("DAI", 18, 1.0),
    "0x68f180fcce6836688e9084f035309e29bf0a2095": ("WBTC", 8, BTC_USD),
    "0x4200000000000000000000000000000000000042": ("OP", 18, None),
    "0x1f32b1c2345538c0c6f582fcb022739c4a194ebb": ("wstETH", 18, ETH_USD),
    "0x9bcef72be871e61ed4fbbc7630889bee758eb81d": ("rETH", 18, ETH_USD),
}

# Minimum USD on the priced side for a pool to be worth quoting. Base's
# inventory is dominated by pools far above this; the floor exists to drop the
# long tail of dust pools that cost a quote and never produce an edge.
MIN_HUB_USD = float(os.environ.get("OPT_MIN_HUB_USD", "25000"))


def rpc_url() -> str:
    url = os.environ.get("OPT_PRIVATE_RPC_HTTP_URL")
    if not url:
        sys.exit("OPT_PRIVATE_RPC_HTTP_URL is not set (source .env first)")
    return url


def call(session: requests.Session, url: str, to: str, data: str) -> str:
    body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
    }
    r = session.post(url, json=body, timeout=30)
    r.raise_for_status()
    out = r.json()
    if "error" in out:
        raise RuntimeError(out["error"])
    return out["result"]


def enc_addr(addr: str) -> str:
    return addr.lower().replace("0x", "").rjust(64, "0")


def get_pool(session, url, a: str, b: str, fee: int) -> str | None:
    # getPool(address,address,uint24) = 0x1698ee82
    data = "0x1698ee82" + enc_addr(a) + enc_addr(b) + f"{fee:064x}"
    res = call(session, url, FACTORY, data)
    pool = "0x" + res[-40:]
    return None if int(pool, 16) == 0 else pool


def balance_of(session, url, token: str, holder: str) -> int:
    # balanceOf(address) = 0x70a08231
    res = call(session, url, token, "0x70a08231" + enc_addr(holder))
    return int(res, 16)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    url = rpc_url()
    session = requests.Session()
    addrs = sorted(TOKENS)
    records, checked, skipped_dust = [], 0, 0

    for i, a in enumerate(addrs):
        for b in addrs[i + 1 :]:
            for fee in V3_FEES:
                checked += 1
                try:
                    pool = get_pool(session, url, a, b, fee)
                except Exception as exc:  # noqa: BLE001
                    print(f"  getPool({TOKENS[a][0]},{TOKENS[b][0]},{fee}) failed: {exc}")
                    continue
                if not pool:
                    continue
                # token0/token1 follow UniV3 address ordering.
                t0, t1 = (a, b) if int(a, 16) < int(b, 16) else (b, a)
                # Price the side we have a USD number for; prefer a stable.
                priced = [t for t in (t0, t1) if TOKENS[t][2] is not None]
                if not priced:
                    continue
                hub = min(priced, key=lambda t: 0 if TOKENS[t][2] == 1.0 else 1)
                sym, dec, usd = TOKENS[hub]
                try:
                    bal = balance_of(session, url, hub, pool)
                except Exception as exc:  # noqa: BLE001
                    print(f"  balanceOf({sym},{pool}) failed: {exc}")
                    continue
                hub_usd = round((bal / 10**dec) * usd, 2)
                if hub_usd < MIN_HUB_USD:
                    skipped_dust += 1
                    continue
                records.append(
                    {
                        "pool": pool,
                        "token0": t0,
                        "token1": t1,
                        "fee": fee,
                        # Not discoverable without log access (see module docstring).
                        # Only used as a tie-breaker on merge, so 0 is safe.
                        "created_block": 0,
                        "hub_usd_liquidity": hub_usd,
                        "hub_symbol": sym,
                    }
                )

    records.sort(key=lambda r: -r["hub_usd_liquidity"])
    print(f"checked {checked} (pair,fee) combos -> {len(records)} pools "
          f"(dropped {skipped_dust} below ${MIN_HUB_USD:,.0f})")
    for r in records:
        print(f"  {r['pool']}  fee={r['fee']:>5}  "
              f"{r['hub_symbol']:>6} ${r['hub_usd_liquidity']:,.0f}")

    if args.dry_run:
        return 0
    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    with OUT_PATH.open("w") as fh:
        for r in records:
            fh.write(json.dumps(r) + "\n")
    print(f"wrote {len(records)} pools -> {OUT_PATH}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
