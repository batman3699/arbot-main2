#!/usr/bin/env python3
"""Aerodrome v2's pools that pair with WETH, for the shadow's universe (R24).

Enumerates the factory (allPoolsLength, allPools(i)), keeps pools with WETH on
one side, and writes each with its tokens, stable flag, fee and depth to
data/base/aerodrome_v2/pools.jsonl, in the record format the live inventory
reads. Depth is the WETH reserve valued in USD, hub_symbol WETH, the hub-side
measure build_aerodrome_pools.py uses; the WETH price is the volatile WETH/USDC
pool's own. The shadow's filter then decides what loads.

Run from scripts/data: python3 build_aerodrome_v2_pools.py
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from arb_census.rpc import Rpc  # noqa: E402

FACTORY = "0x420dd381b31aef6683db6b902084cb0ffece40da"
WETH = "0x4200000000000000000000000000000000000006"
WETH_USDC = "0xcdac0d6c6c59727a65f871236188350531885c43"
OUT = Path(__file__).resolve().parents[2] / "data" / "base" / "aerodrome_v2" / "pools.jsonl"


def word(x):
    return hex(x)[2:].rjust(64, "0") if isinstance(x, int) else x[2:].lower().rjust(64, "0")


def addr(answer):
    return "0x" + answer[-40:] if answer and len(answer) >= 42 else None


def main():
    rpc = Rpc(max_per_second=8)
    block = hex(int(rpc.call("eth_blockNumber", []), 16))

    def call(to, data):
        return ("eth_call", [{"to": to, "data": data}, block])

    n = int(rpc.call(*call(FACTORY, "0xefde4e64")), 16)  # allPoolsLength()
    pools = [addr(r) for r in rpc.batch([call(FACTORY, "0x41d1de97" + word(i)) for i in range(n)])]  # allPools(i)
    pools = [p for p in pools if p]
    print(f"{n} pools at block {int(block, 16)}, {len(pools)} read", flush=True)
    toks = rpc.batch([c for p in pools for c in (call(p, "0x0dfe1681"), call(p, "0xd21220a7"))])
    weth_pools = []
    for i, p in enumerate(pools):
        t0, t1 = addr(toks[2 * i]), addr(toks[2 * i + 1])
        if t0 and t1 and WETH in (t0, t1):
            weth_pools.append((p, t0, t1))
    print(f"{len(weth_pools)} with WETH", flush=True)
    detail = rpc.batch([c for p, _, _ in weth_pools for c in (call(p, "0x22be3de1"), call(p, "0x0902f1ac"))])
    stable = [int(detail[2 * i], 16) == 1 if detail[2 * i] else None for i in range(len(weth_pools))]
    fees = rpc.batch([call(FACTORY, "0xcc56b2c5" + word(p) + word(1 if s else 0))
                      for (p, _, _), s in zip(weth_pools, stable)])
    r = rpc.call(*call(WETH_USDC, "0x0902f1ac"))
    weth_usd = int(r[66:130], 16) / 1e6 / (int(r[2:66], 16) / 1e18)
    rows = []
    for i, (p, t0, t1) in enumerate(weth_pools):
        res, fee = detail[2 * i + 1], fees[i]
        if stable[i] is None or not res or not fee:
            continue
        r0, r1 = int(res[2:66], 16), int(res[66:130], 16)
        weth_reserve = r0 if t0 == WETH else r1
        bps = int(fee, 16)
        rows.append({"pool": p, "token0": t0, "token1": t1, "stable": stable[i], "fee": bps,
                     "fee_ppm_onchain": bps * 100, "created_block": None,
                     "hub_usd_liquidity": round(weth_reserve / 1e18 * weth_usd, 2), "hub_symbol": "WETH"})
    rows.sort(key=lambda x: -x["hub_usd_liquidity"])
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("\n".join(json.dumps(x) for x in rows) + "\n")
    deep = [x for x in rows if not x["stable"] and x["fee_ppm_onchain"] <= 3000 and x["hub_usd_liquidity"] >= 100_000]
    print(f"wrote {len(rows)} to {OUT}; {len(deep)} volatile, <= 3,000 ppm, >= $100k (WETH at ${weth_usd:,.0f})")
    for x in deep[:15]:
        print(f"  {x['pool']} {x['token0'][:8]}/{x['token1'][:8]} fee {x['fee']} bps ${x['hub_usd_liquidity']:,.0f}")


if __name__ == "__main__":
    main()
