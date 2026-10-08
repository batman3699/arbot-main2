"""Does a venue's decoder read its trades right? Checked against the
receipt's own transfers before the venue counts in the census (R23)."""
import argparse
import json
import random
from pathlib import Path

from arb_census.detect import legs_of
from arb_census.venues import TRANSFER, WETH, holds_at_emitter



def _transfers(receipt):
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) == 3 and t[0] == TRANSFER and log["data"] not in ("0x", ""):
            yield log["address"].lower(), "0x" + t[1][-40:], "0x" + t[2][-40:], int(log["data"][:66], 16)


def check_leg(leg, receipt):
    if holds_at_emitter(leg.venue):
        pool = leg.pool.lower()
        net, weth_moved = {}, False
        for token, src, dst, x in _transfers(receipt):
            if dst == pool:
                net[token] = net.get(token, 0) + x
            if src == pool:
                net[token] = net.get(token, 0) - x
            weth_moved |= token == WETH and pool in (src, dst)
        # A WETH side the pool moved no WETH for was native ETH (see
        # `detect.native_flow`): checked by the other side alone.
        return all(net.get(t, 0) == x for t, x in leg.deltas.items()
                   if not (t == WETH and (leg.native or not weth_moved)))
    seen = {}
    for token, _src, _dst, x in _transfers(receipt):
        seen.setdefault(token, set()).add(x)
    # A native-ETH side has no Transfer log: it is checked by the other side
    # alone (and counted in `detect.banked` from the leg's own figure).
    return all(abs(x) in seen.get(t, set()) for t, x in leg.deltas.items()
               if x and not (t == WETH and leg.native))


def _per_pool(legs):
    """A pool that trades twice in one transaction is checked once, on the
    sum of its legs: its transfers are only known for the whole transaction."""
    by = {}
    for l in legs:
        if l.pool in by:
            prev = by[l.pool]
            deltas = dict(prev.deltas)
            for t, x in l.deltas.items():
                deltas[t] = deltas.get(t, 0) + x
            by[l.pool] = type(l)(l.tx, l.block, l.venue, l.pool, deltas, prev.native + l.native)
        else:
            by[l.pool] = l
    return list(by.values())


def verify_venue(rpc, venue, txs, meta):
    checked, passed, failures = 0, 0, []
    for tx in txs:
        receipt = rpc.call("eth_getTransactionReceipt", [tx])
        legs = [l for l in legs_of(receipt, meta) if l.venue == venue]
        if not legs:
            continue
        checked += 1
        if all(check_leg(l, receipt) for l in _per_pool(legs)):
            passed += 1
        else:
            failures.append(tx)
    return {"venue": venue, "checked": checked, "passed": passed, "failures": failures[:5]}


def main():
    from arb_census.collect import Resolver
    from arb_census.rpc import Rpc
    ap = argparse.ArgumentParser()
    ap.add_argument("--venue", required=True)
    ap.add_argument("--topic", required=True)
    ap.add_argument("--address")
    ap.add_argument("--blocks", type=int, default=3000)
    ap.add_argument("--sample", type=int, default=12)
    ap.add_argument("--out", default=str(Path(__file__).resolve().parents[3] / "data" / "census" / "coverage.json"))
    a = ap.parse_args()
    rpc = Rpc()
    head = int(rpc.call("eth_blockNumber", []), 16)
    q = {"topics": [a.topic]}
    if a.address:
        q["address"] = a.address
    logs, lo = [], head - a.blocks
    while lo <= head:
        hi = min(lo + 499, head)
        logs += rpc.call("eth_getLogs", [dict(q, fromBlock=hex(lo), toBlock=hex(hi))])
        lo = hi + 1
    meta = Resolver(rpc).meta_for_logs(logs)
    txs = sorted({l["transactionHash"] for l in logs})
    random.Random(7).shuffle(txs)
    result = verify_venue(rpc, a.venue, txs[: a.sample], meta)
    result["counts"] = result["checked"] >= 3 and result["passed"] == result["checked"]
    out = Path(a.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    cov = json.loads(out.read_text()) if out.exists() else {}
    cov[a.venue] = result
    out.write_text(json.dumps(cov, indent=1))
    print(json.dumps(result))


if __name__ == "__main__":
    main()
