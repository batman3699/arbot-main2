#!/usr/bin/env python3
"""
rank_base_pools.py — Liquidity-rank a Base pool inventory using REAL on-chain
reserves, and rewrite the inventory sorted by hub-side USD liquidity descending.

WHY: the shipped inventories (data/base/<venue>/pools.jsonl) are enumerated by
creation block, not liquidity. The runtime hot-pool ranker only scores the first
`max_cold_pools` records, so a creation-ordered file feeds it the *newest*
(mostly illiquid) pools. Re-sorting the file by real on-chain liquidity makes the
top records the genuinely liquid, hub-connected pools that arbitrage needs.

HOW: for every pool that pairs a known hub token, we read the hub token's
ERC-20 balanceOf(pool) (== that side's real reserve) via batched JSON-RPC
eth_call, convert to USD with live hub prices, filter by a minimum, sort
descending, and rewrite the file (original backed up to *.bak).

The USD weights are used ONLY to order the candidate set; the bot re-quotes
every pool with real on-chain math before sizing or executing anything.

Base UniV3 note: concentrated-liquidity pools often have thin in-range hub
reserves. Default RANK_MIN_USD=500 qualifies ~170 pools on Base (vs ~40 at
$10k) and clears the rewrite safety floor (>=150 scored). Override with
RANK_MIN_USD if you need a stricter offline cut.
"""
import json
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import requests

# Prefer an explicit endpoint, then the URLs the bot itself uses, then Alchemy.
# Hardcoding Alchemy made this unrunnable once that key hit its monthly quota.
def _resolve_rpc_url():
    for var in ("BASE_RANK_RPC_URL", "BASE_RPC_URL"):
        val = os.environ.get(var, "").strip()
        if val:
            return val
    urls = os.environ.get("BASE_RPC_URLS", "").strip()
    if urls:
        first = urls.split(",")[0].strip()
        if first:
            return first
    key = os.environ.get("ALCHEMY_KEY", "").strip()
    if key:
        return f"https://base-mainnet.g.alchemy.com/v2/{key}"
    print(
        "FATAL: set BASE_RANK_RPC_URL (or BASE_RPC_URLS, or ALCHEMY_KEY)",
        file=sys.stderr,
    )
    sys.exit(2)


RPC_URL = _resolve_rpc_url()

WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
QUOTER = "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a"

MIN_USD = float(os.environ.get("RANK_MIN_USD", "500"))
TOP_N = int(os.environ.get("RANK_TOP_N", "1500"))
BATCH = int(os.environ.get("RANK_BATCH", "60"))
WORKERS = int(os.environ.get("RANK_WORKERS", "16"))
BALANCE_OF = "0x70a08231"

HUB_PRIORITY = [
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
    "0x4200000000000000000000000000000000000006",
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf",
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb",
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22",
    "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452",
    "0x940181a94a35a4569e4529a3cdfb74e38fd98631",
]


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
        print(f"  WARN: live WETH quote failed ({exc}); using env fallback", file=sys.stderr)
    return float(os.environ.get("RANK_WETH_USD", "2500"))


def build_hubs(weth_usd: float) -> dict:
    return {
        WETH: ("WETH", 18, weth_usd),
        "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": ("USDC", 6, 1.0),
        "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": ("USDbC", 6, 1.0),
        "0x50c5725949a6f0c72e6c4a641f24049a917db0cb": ("DAI", 18, 1.0),
        "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": (
            "cbBTC",
            8,
            float(os.environ.get("RANK_CBTC_USD", "95000")),
        ),
        "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22": ("cbETH", 18, weth_usd),
        "0xc1cba3fcea344f92d9239c08c0568f6f2f0ee452": ("wstETH", 18, weth_usd),
        "0x940181a94a35a4569e4529a3cdfb74e38fd98631": (
            "AERO",
            18,
            float(os.environ.get("RANK_AERO_USD", "0.35")),
        ),
    }


def pick_hub(token0: str, token1: str):
    t0, t1 = token0.lower(), token1.lower()
    present = [h for h in HUB_PRIORITY if h in (t0, t1)]
    return present[0] if present else None


def balance_call(hub: str, pool: str):
    return {
        "to": hub,
        "data": BALANCE_OF + pool[2:].lower().rjust(64, "0"),
    }


def run_batch(session: requests.Session, jobs):
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


LIQUIDITY = "0x1a686502"  # UniV3 pool.liquidity()


def run_univ3_liquidity_batch(session: requests.Session, jobs):
    """jobs: list of (index, pool_address). Returns (index, liquidity_uint)."""
    payload = [
        {
            "jsonrpc": "2.0",
            "id": idx,
            "method": "eth_call",
            "params": [{"to": pool, "data": LIQUIDITY}, "latest"],
        }
        for (idx, pool) in jobs
    ]
    for attempt in range(4):
        try:
            r = session.post(RPC_URL, json=payload, timeout=30)
            r.raise_for_status()
            out = []
            for item in r.json():
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
                return [(idx, 0) for (idx, _p) in jobs]
            time.sleep(0.5 * (attempt + 1))
    return [(idx, 0) for (idx, _p) in jobs]


def rank_univ3_file(path: str, session: requests.Session):
    """Rank UniV3 pools by hub-side balanceOf USD (same metric as UniV2)."""
    weth_usd = fetch_weth_usd(session)
    hubs = build_hubs(weth_usd)
    rank_file(path, session, hubs)


def rank_file(path: str, session: requests.Session, hubs: dict):
    """Hub balanceOf ranking — appropriate for UniV2-style pools only."""
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
    with ThreadPoolExecutor(max_workers=WORKERS) as pool:
        for result in pool.map(lambda b: run_batch(session, b), batches):
            for idx, bal in result:
                balances[idx] = bal
            done += 1
            if done % 50 == 0:
                print(f"    probed {done}/{len(batches)} batches", flush=True)

    scored = []
    unscored = []
    for i, rec in enumerate(records):
        if i not in hub_for:
            unscored.append(rec)
            continue
        hub = hub_for[i]
        sym, dec, price = hubs[hub]
        bal = balances.get(i, 0)
        if bal <= 0:
            unscored.append(rec)
            continue
        usd = (bal / (10**dec)) * price
        if usd < MIN_USD:
            unscored.append(rec)
            continue
        rec = dict(rec)
        rec["hub_usd_liquidity"] = round(usd, 2)
        rec["hub_symbol"] = sym
        scored.append(rec)

    scored.sort(key=lambda r: r["hub_usd_liquidity"], reverse=True)
    kept = scored[:TOP_N] + unscored
    if len(scored) < min(total, max(50, TOP_N // 10)):
        print(
            f"  ABORT: only {len(scored)} pools qualified; leaving {path} unchanged",
            file=sys.stderr,
        )
        return

    print(f"  qualified (>= ${MIN_USD:,.0f}): {len(scored)} ; writing {len(kept)} records")
    if scored:
        print("  top 8:")
        for r in scored[:8]:
            print(
                f"    ${r['hub_usd_liquidity']:>14,.0f}  {r['hub_symbol']:>6}  "
                f"{r['pool']}  fee={r.get('fee')}"
            )

    bak = path + ".bak"
    if not os.path.exists(bak):
        os.rename(path, bak)
        print(f"  backed up original -> {bak}")
    with open(path, "w") as fh:
        for r in kept:
            fh.write(json.dumps(r) + "\n")
    print(f"  wrote {len(kept)} liquidity-ranked records -> {path}")


def main():
    targets = sys.argv[1:] or ["data/base/uniswap_v3/pools.jsonl"]
    print(f"RPC: {RPC_URL.split('/v2/')[0]}/v2/****")
    print(f"MIN_USD={MIN_USD} TOP_N={TOP_N}")
    with requests.Session() as session:
        for path in targets:
            print(f"\n=== ranking {path} ===")
            if "uniswap_v3" in path.replace("\\", "/") or "pancakeswap_v3" in path.replace("\\", "/"):
                rank_univ3_file(path, session)
            else:
                weth_usd = fetch_weth_usd(session)
                hubs = build_hubs(weth_usd)
                print(f"WETH/USD (live quoter): ${weth_usd:,.2f}")
                rank_file(path, session, hubs)


if __name__ == "__main__":
    main()
