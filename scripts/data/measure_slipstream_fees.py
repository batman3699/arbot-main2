#!/usr/bin/env python3
"""measure_slipstream_fees.py -- record the fee of every Slipstream pool the
inventory has no measured fee for.

The shadow's loader skips a Slipstream record without `fee_ppm_onchain`
rather than guess one: its `fee` field is the tick spacing, and reading that
as a fee would admit a 200-spacing pool as a 200 ppm one. So 29 WETH pools of
$100k or more were never priced, Base's deepest WETH/USDC and WETH/cbBTC
Slipstream pools among them (R20, 2026-10-06). This reads each such pool's
fee() at one pinned block and writes it, with the block, into the record.

A Slipstream fee is dynamic: the reading is one moment's, and decides only
whether the pool is admitted. The book prices every swap at the fee the
module charges then.

The RPC endpoint comes from BASE_RPC_URL (or PROBE_RPC) and is never printed.
The inventory is backed up before it is rewritten. Re-running measures
nothing new.

Usage: BASE_RPC_URL=... scripts/data/measure_slipstream_fees.py [--dry-run]
"""
import argparse
import json
import os
import shutil
import sys
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
INVENTORY = REPO / "data" / "base" / "aerodrome_slipstream" / "pools.jsonl"
FEE = "0xddca3f43"  # fee()


def rpc_url() -> str:
    for key in ("BASE_RPC_URL", "PROBE_RPC"):
        if os.environ.get(key, "").strip():
            return os.environ[key].strip()
    sys.exit("set BASE_RPC_URL")


def call(url: str, method: str, params: list):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(
        url,
        data=body,
        headers={"Content-Type": "application/json", "User-Agent": "measure-slipstream-fees/1"},
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        out = json.loads(r.read())
    if "error" in out:
        raise RuntimeError(out["error"].get("message", "rpc error"))
    return out["result"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true", help="report the fees, write nothing")
    args = ap.parse_args()
    url = rpc_url()
    records = [json.loads(l) for l in INVENTORY.read_text().splitlines() if l.strip()]
    block = int(call(url, "eth_blockNumber", []), 16)
    measured, unreadable = 0, []
    for r in records:
        if r.get("fee_ppm_onchain") is not None:
            continue
        try:
            fee = int(call(url, "eth_call", [{"to": r["pool"], "data": FEE}, hex(block)]), 16)
        except (RuntimeError, ValueError) as e:
            unreadable.append((r["pool"], str(e)))
            continue
        print(f"{r['pool']} spacing {r.get('fee')} fee {fee} ppm")
        r["fee_ppm_onchain"], r["fee_measured_block"] = fee, block
        measured += 1
    print(f"{measured} measured at block {block}; {len(unreadable)} unreadable")
    for pool, why in unreadable:
        print(f"  unreadable {pool}: {why}")
    if args.dry_run or measured == 0:
        return 0
    backup = INVENTORY.with_name(f"pools.jsonl.bak.fees-{int(time.time())}")
    shutil.copy2(INVENTORY, backup)
    INVENTORY.write_text("".join(json.dumps(r) + "\n" for r in records))
    print(f"written; the previous inventory is {backup.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
