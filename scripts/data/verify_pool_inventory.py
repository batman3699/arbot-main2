#!/usr/bin/env python3
"""verify_pool_inventory.py — assert a pool inventory matches on-chain reality.

Every `data/<chain>/<venue>/pools.jsonl` is consumed by the bot as if each record
were a pool of that venue's declared factory. Nothing enforced that until now, so
inventories built by scraping GeckoTerminal/DexScreener (scripts/*_99k.py) landed
Solidly AMM pairs in a concentrated-liquidity venue, wrote `fee: 0` for every
record, and stored the aggregator's base/quote pair rather than the pool's real
token0/token1 ordering.

This checks, per record, against chain state:

  * interface   — venue kind univ3_like/slipstream_like must expose slot0() and
                  tickSpacing(); a pool answering getReserves() instead is a V2
                  pair in a CL inventory.
  * factory     — pool.factory() must equal the venue's declared factory, so
                  pools cannot drift between sibling CL venues.
  * token order — token0/token1 must match the pool's own token0()/token1().
  * fee field   — Slipstream stores tickSpacing in PoolRecord.fee (see
                  quote_slipstream.rs and build_slipstream_pools.py); a real
                  UniV3 inventory stores the fee tier. Both are checked against
                  the pool's own tickSpacing()/fee().

Exit status is 0 only when every record passes.

Usage:
    python3 scripts/data/verify_pool_inventory.py base aerodrome_slipstream
    python3 scripts/data/verify_pool_inventory.py --all base
"""
import argparse
import json
import os
import sys
import time
from pathlib import Path

import requests
import yaml

REPO = Path(__file__).resolve().parents[2]
# Inventories live outside git (/data/ is gitignored), so allow the same
# POOL_DATA_ROOT override the builders use rather than assuming they sit
# beside this checkout — a git worktree has no data/ of its own.
DATA_ROOT = Path(os.environ.get("POOL_DATA_ROOT") or (REPO / "data"))
OPS_INPUTS = Path(os.environ.get("OPS_INPUTS") or (REPO / "ops" / "inputs.yaml"))
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
TRY_AGGREGATE = "bce38bd7"

SEL = {
    "slot0": "0x3850c7bd",
    "tickSpacing": "0xd0c93a7c",
    "getReserves": "0x0902f1ac",
    "factory": "0xc45a0155",
    "token0": "0x0dfe1681",
    "token1": "0xd21220a7",
    "fee": "0xddca3f43",
}
PROBES = list(SEL)
CL_KINDS = {"univ3_like", "slipstream_like"}
# Venues whose PoolRecord.fee holds tickSpacing rather than a UniV3 fee tier.
TICK_SPACING_VENUES = ("aerodrome_slipstream", "slipstream")


def resolve_rpc() -> str:
    for key in ("BASE_RPC_URL", "PROBE_RPC"):
        val = os.environ.get(key, "").strip()
        if val:
            return val
    urls = os.environ.get("BASE_RPC_URLS", "").strip()
    if urls and urls.split(",")[0].strip():
        return urls.split(",")[0].strip()
    return "https://mainnet.base.org"


def _w(v: int) -> str:
    return format(v, "064x")


def encode_try_aggregate(calls):
    """calls: [(target, 4-byte selector)] -> tryAggregate(false, Call[]) calldata."""
    n = len(calls)
    offsets = "".join(_w(n * 32 + i * 128) for i in range(n))
    elems = ""
    for target, sel in calls:
        elems += (target.lower().replace("0x", "").rjust(64, "0")
                  + _w(0x40) + _w(4) + sel.replace("0x", "").ljust(64, "0"))
    return "0x" + TRY_AGGREGATE + _w(0) + _w(0x40) + _w(n) + offsets + elems


def decode_try_aggregate(hexstr, n_expected):
    b = bytes.fromhex(hexstr[2:] if hexstr.startswith("0x") else hexstr)

    def word(off):
        return int.from_bytes(b[off:off + 32], "big")

    arr = word(0)
    n = word(arr)
    if n != n_expected:
        raise ValueError(f"expected {n_expected} results, got {n}")
    base = arr + 32
    out = []
    for i in range(n):
        t = base + word(base + i * 32)
        ok = bool(word(t))
        bo = t + word(t + 32)
        ln = word(bo)
        out.append((ok, "0x" + b[bo + 32: bo + 32 + ln].hex()))
    return out


def multicall(session, rpc, calls, tries=8):
    """Per-call success comes back in-band, so a revert is never confused with
    an RPC/rate-limit failure: the latter fails the whole round-trip and retries."""
    payload = {"jsonrpc": "2.0", "id": 1, "method": "eth_call",
               "params": [{"to": MULTICALL3, "data": encode_try_aggregate(calls)}, "latest"]}
    for attempt in range(tries):
        try:
            r = session.post(rpc, json=payload, timeout=90)
            if r.status_code != 200 or r.json().get("error"):
                time.sleep(1.5 * (attempt + 1))
                continue
            return decode_try_aggregate(r.json()["result"], len(calls))
        except Exception:  # noqa: BLE001
            time.sleep(1.5 * (attempt + 1))
    return None


def load_venues(chain: str):
    data = yaml.safe_load(OPS_INPUTS.read_text())
    for entry in data.get("chains") or []:
        if entry.get("chain_name") == chain:
            return entry.get("venues") or []
    return []


def verify(chain: str, venue_cfg: dict, rpc: str) -> bool:
    name = venue_cfg["name"]
    kind = venue_cfg.get("kind")
    path = DATA_ROOT / chain / name / "pools.jsonl"
    if not path.exists():
        print(f"[{name}] no inventory at {path} — skipped")
        return True
    if kind not in CL_KINDS:
        print(f"[{name}] kind={kind} is not concentrated-liquidity — skipped")
        return True

    declared = (venue_cfg.get("factory") or "").lower()
    if not declared:
        print(f"[{name}] FAIL: venue declares no factory")
        return False

    records = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
    if not records:
        print(f"[{name}] FAIL: inventory is empty")
        return False

    session = requests.Session()
    flat = [(r["pool"].lower(), p) for r in records for p in PROBES]
    results = {}
    for k in range(0, len(flat), 150):
        part = flat[k:k + 150]
        got = multicall(session, rpc, [(a, SEL[p]) for a, p in part])
        if got is None:
            print(f"[{name}] FAIL: RPC never returned for chunk at {k}; "
                  f"cannot verify (not treating this as a pass)")
            return False
        results.update(dict(zip(part, got)))

    uses_tick_spacing = any(t in name for t in TICK_SPACING_VENUES)
    problems = []
    seen = set()
    for r in records:
        a = r["pool"].lower()
        if a in seen:
            problems.append((a, "duplicate record"))
        seen.add(a)

        def val(p):
            ok, d = results[(a, p)]
            return d if ok and len(d) >= 66 else None

        if val("slot0") is None or val("tickSpacing") is None:
            what = "Solidly/V2 pair" if val("getReserves") else "not a CL pool"
            problems.append((a, f"{what} in a {kind} inventory"))
            continue

        fac = "0x" + val("factory")[2:66][-40:] if val("factory") else None
        if fac != declared:
            problems.append((a, f"factory {fac} != venue factory {declared}"))

        t0 = "0x" + val("token0")[2:66][-40:]
        t1 = "0x" + val("token1")[2:66][-40:]
        if (r["token0"].lower(), r["token1"].lower()) != (t0, t1):
            problems.append((a, f"token order {r['token0']}/{r['token1']} != on-chain {t0}/{t1}"))

        ts = int(val("tickSpacing")[2:66], 16)
        want = ts if uses_tick_spacing else (
            int(val("fee")[2:66], 16) if val("fee") else None)
        label = "tickSpacing" if uses_tick_spacing else "fee tier"
        if r.get("fee") != want:
            problems.append((a, f"fee={r.get('fee')} != on-chain {label} {want}"))

    if problems:
        print(f"[{name}] FAIL: {len(problems)} problem(s) across {len(records)} records")
        for a, msg in problems[:25]:
            print(f"    {a}  {msg}")
        if len(problems) > 25:
            print(f"    ... and {len(problems) - 25} more")
        return False
    print(f"[{name}] OK: {len(records)} records match chain "
          f"(factory {declared}, fee={'tickSpacing' if uses_tick_spacing else 'fee tier'})")
    return True


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("chain")
    ap.add_argument("venue", nargs="?")
    ap.add_argument("--all", action="store_true", help="verify every CL venue on the chain")
    args = ap.parse_args()

    venues = load_venues(args.chain)
    if not venues:
        print(f"no venues for chain {args.chain} in ops/inputs.yaml", file=sys.stderr)
        return 2
    if not args.all:
        if not args.venue:
            print("give a venue name or --all", file=sys.stderr)
            return 2
        venues = [v for v in venues if v.get("name") == args.venue]
        if not venues:
            print(f"venue {args.venue} not found on {args.chain}", file=sys.stderr)
            return 2

    rpc = resolve_rpc()
    print(f"verifying against {rpc}\n")
    ok = all([verify(args.chain, v, rpc) for v in venues])
    print("\n" + ("ALL INVENTORIES MATCH CHAIN" if ok else "INVENTORY VERIFICATION FAILED"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
