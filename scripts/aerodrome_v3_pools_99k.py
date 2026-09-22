#!/usr/bin/env python3
"""
Fetch ALL Aerodrome Slipstream (= Aerodrome V3) pools on Base
with ≥ $99k liquidity and save them in the attached JSONL format.

Sources: GeckoTerminal + DexScreener
Output:  aerodrome_v3_pools_99k.jsonl
"""

# ---------------------------------------------------------------------------
# WARNING — aggregator output is NOT authoritative for a bot pool inventory.
#
# GeckoTerminal/DexScreener report a market-convention base/quote pair, not the
# pool's on-chain token0()/token1() ordering, and they do not report Slipstream
# tickSpacing at all. Records emitted here therefore carry an UNVERIFIED token
# order and, whenever the fee cannot be parsed, a placeholder fee of 0.
#
# An earlier run of this script produced data/base/aerodrome_slipstream/pools.jsonl
# with 59 Aerodrome Solidly V2 pairs mislabeled as concentrated-liquidity pools,
# fee=0 on all 187 records, and token0/token1 swapped on 101 of them.
#
# Prefer scripts/data/build_slipstream_pools.py, which enumerates the Slipstream
# CL factory directly. If you use this script, its output MUST pass
#     python3 scripts/data/verify_pool_inventory.py base <venue>
# before it is copied into data/<chain>/<venue>/pools.jsonl.
# ---------------------------------------------------------------------------


import requests
import json
import time
import random
from typing import Dict, List, Optional, Any
from datetime import datetime, timezone

# ====================== CONFIG ======================
MIN_LIQUIDITY_USD = 99_000
NETWORK = "base"
OUTPUT_FILE = "aerodrome_v3_pools_99k.jsonl"

GECKO_DEXES = [
    "aerodrome-slipstream",
    "aerodrome-slipstream-2",
]

DEXSCREENER_QUERIES = [
    "WETH USDC", "WETH", "USDC", "cbBTC", "AERO",
    "cbETH", "WETH cbBTC", "USDC cbBTC", "VIRTUAL",
    "BRETT", "DEGEN", "ZORA", "EURC", "wstETH",
    "AERO WETH", "AERO USDC",
]

BASE_URL = "https://api.geckoterminal.com/api/v2"
DEXSCREENER_SEARCH = "https://api.dexscreener.com/latest/dex/search"
HEADERS = {
    "Accept": "application/json",
    "User-Agent": "BaseAerodromeV3Scanner/1.0"
}

MIN_DELAY = 2.8
MAX_RETRIES = 5
BACKOFF_BASE = 4
MAX_PAGES = 8
# ====================================================

session = requests.Session()
session.headers.update(HEADERS)


def safe_get(url: str, params: dict = None) -> Optional[dict]:
    for attempt in range(MAX_RETRIES):
        try:
            r = session.get(url, params=params, timeout=25)
            if r.status_code == 429:
                wait = BACKOFF_BASE * (2 ** attempt) + random.uniform(0.5, 2.0)
                print(f"  429 – sleeping {wait:.1f}s")
                time.sleep(wait)
                continue
            r.raise_for_status()
            return r.json()
        except Exception as e:
            if attempt == MAX_RETRIES - 1:
                print(f"  [ERROR] {e}")
                return None
            time.sleep(BACKOFF_BASE * (2 ** attempt))
    return None


def fetch_gecko_dex(dex: str, page: int = 1) -> List[Dict]:
    data = safe_get(f"{BASE_URL}/networks/{NETWORK}/dexes/{dex}/pools", {"page": page})
    return data.get("data", []) if data else []


def fetch_gecko_top(page: int = 1) -> List[Dict]:
    data = safe_get(f"{BASE_URL}/networks/{NETWORK}/pools", {"page": page})
    return data.get("data", []) if data else []


def fetch_dexscreener(query: str) -> List[Dict]:
    data = safe_get(DEXSCREENER_SEARCH, {"q": query})
    if not data:
        return []
    pairs = data.get("pairs") or []
    return [
        p for p in pairs
        if str(p.get("chainId", "")).lower() == "base"
        and (
            "slipstream" in str(p.get("dexId", "")).lower()
            or "slipstream" in str(p.get("labels", [])).lower()
        )
    ]


def parse_gecko(pool: Dict) -> Optional[Dict[str, Any]]:
    try:
        attrs = pool.get("attributes") or {}
        rel = pool.get("relationships") or {}

        liquidity = float(attrs.get("reserve_in_usd") or 0)
        if liquidity < MIN_LIQUIDITY_USD:
            return None

        dex_id = ((rel.get("dex") or {}).get("data") or {}).get("id", "").lower()
        # "aerodrome" alone also matches the Solidly AMM ("aerodrome-base"),
        # whose pairs have no slot0()/tickSpacing(). Require Slipstream.
        if "slipstream" not in dex_id:
            return None

        def clean_addr(token_id: str) -> str:
            if not token_id:
                return ""
            return token_id.split("_")[-1].lower()

        token0 = clean_addr(((rel.get("base_token") or {}).get("data") or {}).get("id", ""))
        token1 = clean_addr(((rel.get("quote_token") or {}).get("data") or {}).get("id", ""))

        fee = 0
        if "fee" in attrs:
            try:
                raw = float(attrs["fee"])
                fee = int(raw) if raw > 10 else int(raw * 10000)
            except Exception:
                pass

        pool_addr = str(attrs.get("address", "")).lower()
        if not pool_addr:
            return None

        return {
            "pool": pool_addr,
            "token0": token0,
            "token1": token1,
            "fee": fee,
            "created_block": 0,
            "hub_usd_liquidity": round(liquidity, 2),
        }
    except Exception:
        return None


def parse_dexscreener(pair: Dict) -> Optional[Dict[str, Any]]:
    try:
        liquidity = float((pair.get("liquidity") or {}).get("usd") or 0)
        if liquidity < MIN_LIQUIDITY_USD:
            return None

        base = pair.get("baseToken") or {}
        quote = pair.get("quoteToken") or {}

        fee = 0
        for lab in (pair.get("labels") or []):
            if isinstance(lab, str) and lab.endswith("%"):
                try:
                    fee = int(float(lab.replace("%", "")) * 10000)
                except Exception:
                    pass

        pool_addr = str(pair.get("pairAddress", "")).lower()
        if not pool_addr:
            return None

        return {
            "pool": pool_addr,
            "token0": str(base.get("address", "")).lower(),
            "token1": str(quote.get("address", "")).lower(),
            "fee": fee,
            "created_block": 0,
            "hub_usd_liquidity": round(liquidity, 2),
        }
    except Exception:
        return None


def main():
    print(f"Scanning Aerodrome Slipstream / V3 pools on Base ≥ ${MIN_LIQUIDITY_USD:,}\n")

    pools: Dict[str, Dict] = {}

    print("→ GeckoTerminal network top…")
    for page in range(1, MAX_PAGES + 1):
        for p in fetch_gecko_top(page):
            parsed = parse_gecko(p)
            if parsed:
                pools[parsed["pool"]] = parsed
        time.sleep(MIN_DELAY + random.uniform(0.3, 1.0))

    for dex in GECKO_DEXES:
        print(f"→ GeckoTerminal: {dex}")
        empty = 0
        for page in range(1, MAX_PAGES + 1):
            data = fetch_gecko_dex(dex, page)
            if not data:
                empty += 1
                if empty >= 2:
                    break
                time.sleep(MIN_DELAY)
                continue
            empty = 0
            for p in data:
                parsed = parse_gecko(p)
                if parsed:
                    pools[parsed["pool"]] = parsed
            time.sleep(MIN_DELAY + random.uniform(0.4, 1.2))

    print("→ DexScreener…")
    for q in DEXSCREENER_QUERIES:
        print(f"  {q}")
        for p in fetch_dexscreener(q):
            parsed = parse_dexscreener(p)
            if parsed:
                existing = pools.get(parsed["pool"])
                if existing is None or parsed["hub_usd_liquidity"] > existing["hub_usd_liquidity"]:
                    pools[parsed["pool"]] = parsed
        time.sleep(0.9)

    results = sorted(pools.values(), key=lambda x: x["hub_usd_liquidity"], reverse=True)

    print("\n" + "=" * 70)
    print(f"Found {len(results)} Aerodrome Slipstream/V3 pools ≥ ${MIN_LIQUIDITY_USD:,}")
    print("=" * 70)

    if results:
        print(f"\n{'Liquidity':>14}  {'Fee':>6}  Pool")
        print("-" * 70)
        for r in results[:20]:
            print(f"${r['hub_usd_liquidity']:>13,.0f}  {r['fee']:>6}  {r['pool']}")

    skipped_no_fee = 0
    with open(OUTPUT_FILE, "w") as f:
        for r in results:
            if not r["fee"]:
                skipped_no_fee += 1
                continue
            out = {
                "pool": r["pool"],
                "token0": r["token0"],
                "token1": r["token1"],
                "fee": r["fee"],
                "created_block": r["created_block"],
                "hub_usd_liquidity": r["hub_usd_liquidity"],
            }
            f.write(json.dumps(out) + "\n")

    if skipped_no_fee:
        print(f"SKIPPED {skipped_no_fee} pools with no parsable fee "
              f"(a placeholder 0 would be unquotable)")
    print(f"\nSaved {len(results) - skipped_no_fee} pools → {OUTPUT_FILE}")
    print(f"Scanned at {datetime.now(timezone.utc).isoformat()}")


if __name__ == "__main__":
    main()
