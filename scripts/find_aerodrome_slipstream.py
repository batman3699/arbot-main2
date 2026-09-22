#!/usr/bin/env python3
"""
Find ALL Aerodrome Slipstream (CL) pools on Base with liquidity >= $99k
and save pool + token addresses to a file.
"""

import requests
import time
import json
from pathlib import Path

# ── Config ──────────────────────────────────────────────────────────────────
MIN_LIQUIDITY_USD = 99_000
NETWORK = "base"
DEXES = [
    "aerodrome-slipstream",   # original / legacy
    "aerodrome-slipstream-2", # gauge-caps generation
    "aerodrome-slipstream-3", # Gauges V3 / MEV-resistant
]
OUTPUT_FILE = "aerodrome_slipstream_gauge_pools_99k.txt"
OUTPUT_JSON = "aerodrome_slipstream_gauge_pools_99k.json"
REQUEST_DELAY = 6.5          # stay under free-tier rate limit (~10 req/min)

BASE_URL = "https://api.geckoterminal.com/api/v2"

HEADERS = {
    "Accept": "application/json;version=20230203",
    "User-Agent": "aerodrome-slipstream-scanner/1.0",
}


def fetch_pools_page(dex: str, page: int = 1) -> dict | None:
    url = f"{BASE_URL}/networks/{NETWORK}/dexes/{dex}/pools"
    params = {
        "page": page,
        "include": "base_token,quote_token",
    }
    try:
        r = requests.get(url, params=params, headers=HEADERS, timeout=30)
        if r.status_code == 429:
            print("Rate limited – sleeping 30s…")
            time.sleep(30)
            return fetch_pools_page(dex, page)
        r.raise_for_status()
        return r.json()
    except Exception as e:
        print(f"Error fetching {dex} page {page}: {e}")
        return None


def main():
    all_pools = []          # full objects for JSON
    addresses = set()       # pool + token addresses (lower-case)

    for dex in DEXES:
        print(f"\n── Scanning {dex} ──")
        page = 1
        while True:
            print(f"  page {page}…", end=" ", flush=True)
            data = fetch_pools_page(dex, page)
            if not data or "data" not in data:
                print("no more data")
                break

            pools = data["data"]
            if not pools:
                print("empty page – done")
                break

            for p in pools:
                attrs = p.get("attributes", {})
                liq = float(attrs.get("reserve_in_usd") or 0)

                if liq < MIN_LIQUIDITY_USD:
                    continue

                pool_addr = attrs.get("address", "").lower()
                if not pool_addr:
                    continue

                # relationships → included tokens
                rel = p.get("relationships", {})
                base_id = rel.get("base_token", {}).get("data", {}).get("id")
                quote_id = rel.get("quote_token", {}).get("data", {}).get("id")

                token0 = token1 = None
                for inc in data.get("included", []):
                    if inc.get("id") == base_id:
                        token0 = inc.get("attributes", {}).get("address", "").lower()
                    if inc.get("id") == quote_id:
                        token1 = inc.get("attributes", {}).get("address", "").lower()

                # fallback: sometimes address is in the pool name / attributes
                if not token0 or not token1:
                    # try to pull from name if needed (rare)
                    pass

                addresses.add(pool_addr)
                if token0:
                    addresses.add(token0)
                if token1:
                    addresses.add(token1)

                all_pools.append({
                    "dex": dex,
                    "pool": pool_addr,
                    "name": attrs.get("name"),
                    "liquidity_usd": liq,
                    "token0": token0,
                    "token1": token1,
                    "volume_24h": float(attrs.get("volume_usd", {}).get("h24") or 0),
                    "created_at": attrs.get("pool_created_at"),
                })

            print(f"kept {len([x for x in pools if float(x.get('attributes',{}).get('reserve_in_usd') or 0) >= MIN_LIQUIDITY_USD])} pools")
            page += 1
            time.sleep(REQUEST_DELAY)

            # safety – GeckoTerminal usually caps around page 10-20 for most DEXes
            if page > 30:
                break

    # ── Sort by liquidity ────────────────────────────────────────────────────
    all_pools.sort(key=lambda x: x["liquidity_usd"], reverse=True)

    # ── Write text file (one address per line) ───────────────────────────────
    sorted_addrs = sorted(addresses)
    Path(OUTPUT_FILE).write_text("\n".join(sorted_addrs) + "\n")
    print(f"\nSaved {len(sorted_addrs)} unique addresses → {OUTPUT_FILE}")

    # ── Write detailed JSON ──────────────────────────────────────────────────
    Path(OUTPUT_JSON).write_text(json.dumps(all_pools, indent=2))
    print(f"Saved {len(all_pools)} pools with details → {OUTPUT_JSON}")

    # quick summary
    print("\nTop 10 by liquidity:")
    for p in all_pools[:10]:
        print(f"  ${p['liquidity_usd']:>12,.0f}  {p['name']:<40}  {p['pool']}")


if __name__ == "__main__":
    main()
