#!/usr/bin/env python3
"""
rank_base_pools.py — Liquidity-rank a Base pool inventory using REAL on-chain
reserves, and rewrite the inventory sorted by hub-side USD liquidity descending.

WHY: the shipped inventories (data/base/<venue>/pools.jsonl) are enumerated by
creation block, not liquidity. The runtime hot-pool ranker only scores the first
`max_cold_pools` records, so a creation-ordered file feeds it the *oldest*
(mostly dead) pools. Re-sorting the file by real on-chain liquidity makes the
top records the genuinely liquid, hub-connected pools that arbitrage needs.

HOW: for every pool that pairs a known hub token, we read the hub token's
ERC-20 balanceOf(pool) (== that side's real reserve) via batched JSON-RPC
eth_call, convert to USD with live hub prices, filter by a minimum, sort
descending, and rewrite the file (original backed up to *.bak).

The USD weights are used ONLY to order the candidate set; the bot re-quotes
every pool with real on-chain math before sizing or executing anything.
"""
import json
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import requests

ALCHEMY_KEY = os.environ.get("ALCHEMY_KEY", "").strip()
if not ALCHEMY_KEY:
    print("FATAL: ALCHEMY_KEY not set", file=sys.stderr)
    sys.exit(2)
RPC_URL = os.environ.get(
    "BASE_RANK_RPC_URL",
    f"https://base-mainnet.g.alchemy.com/v2/{ALCHEMY_KEY}",
)

# hub address (lowercase) -> (symbol, decimals, usd_price)
# Prices are live QuoterV2 hub->USDC snapshots; ETH-LSTs use the WETH price
# (they are ETH-equivalent and their thin direct USDC pools misquote).
WETH_USD = 1638.12
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
# Preference when both tokens are hubs (deepest/most-canonical first).
HUB_PRIORITY = [
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",  # USDC
    "0x4200000000000000000000000000000000000006",  # WETH
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf",  # cbBTC
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",  # USDbC
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb",  # DAI
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22",  # cbETH
    "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452",  # wstETH
    "0x940181a94a35a4569e4529a3cdfb74e38fd98631",  # AERO
]
BALANCE_OF = "0x70a08231"  # balanceOf(address) selector

MIN_USD = float(os.environ.get("RANK_MIN_USD", "10000"))   # keep pools with >= this hub-side USD
TOP_N = int(os.environ.get("RANK_TOP_N", "1500"))          # cap records written
BATCH = int(os.environ.get("RANK_BATCH", "60"))            # eth_calls per JSON-RPC batch
WORKERS = int(os.environ.get("RANK_WORKERS", "16"))


def pick_hub(token0: str, token1: str):
    t0, t1 = token0.lower(), token1.lower()
    present = [h for h in HUB_PRIORITY if h in (t0, t1)]
    return present[0] if present else None


def balance_call(hub: str, pool: str):
    return {
        "to": hub,
        "data": BALANCE_OF + pool[2:].lower().rjust(64, "0"),
    }


def run_batch(session, jobs):
    """jobs: list of (index, hub, pool). Returns list of (index, balance_int)."""
    payload = [
        {
            "jsonrpc": "2.0",
            "id": idx,
            "method": "eth_call",
            "params": [balance_call(hub, pool), "latest"],
        }
        for (idx, hub, pool) in jobs
    ]
    for attempt in range(4):
        try:
            r = session.post(RPC_URL, json=payload, timeout=30)
            r.raise_for_status()
            data = r.json()
            out = []
            for item in data:
                idx = item.get("id")
                res = item.get("result")
                if isinstance(res, str) and len(res) >= 66:
                    try:
                        out.append((idx, int(res[:66], 16)))
                    except ValueError:
                        out.append((idx, 0))
                else:
                    out.append((idx, 0))
            return out
        except Exception as exc:  # noqa: BLE001
            if attempt == 3:
                print(f"  batch failed after retries: {exc}", file=sys.stderr)
                return [(idx, 0) for (idx, _h, _p) in jobs]
            time.sleep(0.5 * (attempt + 1))
    return [(idx, 0) for (idx, _h, _p) in jobs]


def rank_file(path: str):
    if not os.path.exists(path):
        print(f"  skip (missing): {path}")
        return
    records = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    total = len(records)
    print(f"  loaded {total} records from {path}")

    # Build jobs only for hub-anchored pools.
    jobs = []
    hub_for = {}
    for i, rec in enumerate(records):
        hub = pick_hub(rec.get("token0", ""), rec.get("token1", ""))
        if hub is None:
            continue
        hub_for[i] = hub
        jobs.append((i, hub, rec["pool"]))
    print(f"  {len(jobs)} hub-anchored pools to probe (batch={BATCH}, workers={WORKERS})")

    balances = {}
    batches = [jobs[k : k + BATCH] for k in range(0, len(jobs), BATCH)]
    done = 0
    with requests.Session() as session:
        with ThreadPoolExecutor(max_workers=WORKERS) as pool:
            for result in pool.map(lambda b: run_batch(session, b), batches):
                for idx, bal in result:
                    balances[idx] = bal
                done += 1
                if done % 50 == 0:
                    print(f"    probed {done}/{len(batches)} batches", flush=True)

    scored = []
    for i, rec in enumerate(records):
        if i not in hub_for:
            continue
        hub = hub_for[i]
        sym, dec, price = HUBS[hub]
        bal = balances.get(i, 0)
        if bal <= 0:
            continue
        usd = (bal / (10 ** dec)) * price
        if usd < MIN_USD:
            continue
        rec = dict(rec)
        rec["hub_usd_liquidity"] = round(usd, 2)
        rec["hub_symbol"] = sym
        scored.append(rec)

    scored.sort(key=lambda r: r["hub_usd_liquidity"], reverse=True)
    kept = scored[:TOP_N]
    print(f"  qualified (>= ${MIN_USD:,.0f}): {len(scored)} ; keeping top {len(kept)}")
    if kept:
        print("  top 8:")
        for r in kept[:8]:
            print(
                f"    ${r['hub_usd_liquidity']:>14,.0f}  {r['hub_symbol']:>6}  "
                f"{r['pool']}  fee={r.get('fee')}"
            )

    bak = path + ".bak"
    if not os.path.exists(bak):
        os.rename(path, bak)
        print(f"  backed up original -> {bak}")
    else:
        print(f"  backup already exists -> {bak} (left intact)")
    with open(path, "w") as fh:
        for r in kept:
            fh.write(json.dumps(r) + "\n")
    print(f"  wrote {len(kept)} liquidity-ranked records -> {path}")


def main():
    targets = sys.argv[1:] or [
        "data/base/uniswap_v2/pools.jsonl",
        "data/base/uniswap_v3/pools.jsonl",
    ]
    print(f"RPC: {RPC_URL.split('/v2/')[0]}/v2/****")
    print(f"MIN_USD={MIN_USD} TOP_N={TOP_N}")
    for path in targets:
        print(f"\n=== ranking {path} ===")
        rank_file(path)


if __name__ == "__main__":
    main()
