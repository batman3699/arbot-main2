#!/usr/bin/env python3
"""Check Aerodrome v2's filtered inventory against the chain (R24 spec checks 1, 2, 3, 5).

For every record the shadow's filter admits (volatile, <= 3,000 ppm, >= $100k),
at one pinned block:
  1. inventory truth: the pool's factory() is the factory, isPool(pool) is
     true, stable() is false, and token0()/token1() are the record's;
  2. fee parity: getFee(pool, false) x 100 is the record's fee_ppm_onchain;
  3. quote parity: the R24 formula over getReserves() equals getAmountOut at
     0.01, 0.1 and 1 WETH and their equivalents in the other token, both
     directions, to the unit;
and over the last 200 blocks:
  5. Sync parity: each pool's last Sync in a block equals getReserves() at that
     block.
Exits non-zero on any failure.

Run from scripts/data: python3 verify_aerodrome_v2.py
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from arb_census.rpc import Rpc  # noqa: E402

FACTORY = "0x420dd381b31aef6683db6b902084cb0ffece40da"
WETH = "0x4200000000000000000000000000000000000006"
SYNC = "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"
INV = Path(__file__).resolve().parents[2] / "data" / "base" / "aerodrome_v2" / "pools.jsonl"


def w(x):
    return hex(x)[2:].rjust(64, "0") if isinstance(x, int) else x[2:].lower().rjust(64, "0")


def quote(a, fee_ppm, r_in, r_out):
    """The pool's getAmountOut: the fee floored off the input, then x*y=k floored."""
    a = a - a * fee_ppm // 1_000_000
    return a * r_out // (r_in + a)


def main():
    rpc = Rpc(max_per_second=8)
    head = int(rpc.call("eth_blockNumber", []), 16)
    b = hex(head - 2)

    def c(to, data, blk=b):
        return rpc.call("eth_call", [{"to": to, "data": data}, blk])

    recs = [json.loads(line) for line in INV.read_text().splitlines() if line.strip()]
    recs = [r for r in recs if not r["stable"] and r["fee_ppm_onchain"] <= 3000 and r["hub_usd_liquidity"] >= 100_000]
    bad = 0
    for r in recs:
        p = r["pool"]
        truth = ("0x" + c(p, "0xc45a0155")[-40:] == FACTORY and int(c(FACTORY, "0x5b16ebb7" + w(p)), 16) == 1
                 and int(c(p, "0x22be3de1"), 16) == 0 and "0x" + c(p, "0x0dfe1681")[-40:] == r["token0"]
                 and "0x" + c(p, "0xd21220a7")[-40:] == r["token1"])
        fee = int(c(FACTORY, "0xcc56b2c5" + w(p) + w(0)), 16) * 100
        res = c(p, "0x0902f1ac")
        r0, r1 = int(res[2:66], 16), int(res[66:130], 16)
        weth_is_0 = r["token0"] == WETH
        other_per_weth = (r1 / r0) if weth_is_0 else (r0 / r1)
        mismatches = 0
        for weth_amt in (10**16, 10**17, 10**18):
            for z, amt in ((weth_is_0, weth_amt), (not weth_is_0, max(1, int(weth_amt * other_per_weth)))):
                tok = r["token0"] if z else r["token1"]
                chain = int(c(p, "0xf140a35a" + w(amt) + w(tok)), 16)
                mine = quote(amt, fee, r0, r1) if z else quote(amt, fee, r1, r0)
                mismatches += chain != mine
        ok = truth and fee == r["fee_ppm_onchain"] and mismatches == 0
        bad += not ok
        print(f"{p} truth {truth} fee {fee} quotes off {mismatches}/6 {'OK' if ok else 'FAIL'}")
    if recs:
        logs = rpc.call("eth_getLogs", [{"address": [r["pool"] for r in recs], "topics": [SYNC],
                                         "fromBlock": hex(head - 200), "toBlock": hex(head - 2)}])
        last = {}
        for log in logs:
            last[(log["address"].lower(), int(log["blockNumber"], 16))] = log["data"]
        sync_bad = 0
        for (p, blk), data in last.items():
            res = c(p, "0x0902f1ac", hex(blk))
            sync_bad += res[2:130] != data[2:130]
        print(f"Sync parity: {len(last) - sync_bad}/{len(last)} pool-blocks equal getReserves")
        bad += sync_bad
    print(f"{len(recs)} pools checked at block {int(b, 16)}: {'all OK' if bad == 0 else f'{bad} FAILED'}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
