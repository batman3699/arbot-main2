"""How the arbitrage taken in its trigger's own flashblock gets there
(Task 8.5 R35).

R34: 79-86% of arbitrage value on Base is taken in the flashblock of the
trade that opened it, where a bot that reacts to what it sees can never be.
This reads, for those arbitrages (R34's, $1 or more, on our venues or v4 with
them), the signs of how each followed its trigger:

  outbid_trigger  it paid more priority than its trigger and still ran after
                  it, in the same flashblock. The builder orders a flashblock
                  by priority fee (checked here, on sampled flashblocks), so an
                  ordinary transaction that outbid the trigger would have run
                  before it: one that could not run until the trigger had is a
                  validity transaction (Cobalt's base_sendRawTransactionValidity,
                  admitted when a watched slot changes), or one held for it.
  adjacent        it ran right after the trigger.
  same_bot        trigger and arbitrage share a sender or a contract.

and, per bot, over sampled blocks: how many transactions it sends to its
contract a block, and how many of them did anything (speculative spam
lands after a trigger by being there already).

Runs on the host (R34's data and the ledger are there), public endpoints only.

Usage (on the host, from /root/r34):
  python3 -m arb_census.offset0 --race /root/r34 --days 2026-10-20,2026-10-21 --out /root/r35
"""
import argparse
import glob
import gzip
import json
import os
from collections import defaultdict
from pathlib import Path

from arb_census import race

DEPOSIT = "0x7e"
SAMPLE = ((10_000, 50), (30_000, 50), (60_000, 50))   # (blocks into the window, how many): three stretches
RECEIPTS = 400         # a bot's sampled transactions whose receipts are read


def tx_priority(t, base):
    """What a transaction pays a gas over the base fee; None for a deposit."""
    if t.get("type") == DEPOSIT:
        return None
    if t.get("maxPriorityFeePerGas") is not None:
        return max(0, min(int(t["maxPriorityFeePerGas"], 16), int(t["maxFeePerGas"], 16) - base))
    return max(0, int(t["gasPrice"], 16) - base)


def mechanism(arb, trig):
    low = lambda x: (x or "").lower()
    return {"adjacent": arb["txi"] == trig["txi"] + 1,
            "outbid_trigger": arb["prio"] > trig["prio"],
            "same_bot": low(arb.get("sender")) == low(trig.get("from")) or low(arb.get("to")) == low(trig.get("to"))}


def inversions(prios):
    """Adjacent pairs, deposits skipped, where the later paid more."""
    p = [x for x in prios if x is not None]
    return sum(1 for a, b in zip(p, p[1:]) if b > a)


def bots(rows):
    """Per contract, by value taken: what it took and how."""
    by, total = defaultdict(list), sum(r["usd"] for r in rows)
    for r in rows:
        by[r["to"]].append(r)
    out = []
    for to, rs in by.items():
        n, usd = len(rs), sum(r["usd"] for r in rs)
        prio, gas = sorted(r["prio"] for r in rs), sorted(r["gas"] for r in rs)
        out.append({"to": to, "arbs": n, "usd": usd, "share": usd / total if total else 0.0,
                    "outbid_trigger": sum(r["outbid_trigger"] for r in rs) / n,
                    "adjacent": sum(r["adjacent"] for r in rs) / n, "same_bot": sum(r["same_bot"] for r in rs) / n,
                    "prio_p50": prio[n // 2], "gas_p50": gas[n // 2], "senders": len({r.get("sender") for r in rs})})
    return sorted(out, key=lambda b: -b["usd"])


# ---- the host run ---------------------------------------------------------

def sample_blocks(lo):
    return [lo + k + i for k, n in SAMPLE for i in range(n)]


def main():
    import race_analysis as RA
    ap = argparse.ArgumentParser()
    ap.add_argument("--race", required=True, help="R34's output directory")
    ap.add_argument("--days", required=True)
    ap.add_argument("--var", default="/opt/apex/var/apex")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    rpc = race.Public(6.0)
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    days = [json.load(gzip.open(os.path.join(a.race, f"race-{d}.json.gz"), "rt")) for d in a.days.split(",")]
    rows = [x for d in days for x in d["arbs"] if x["offset"] == 0 and "prio" in x and x["category"] in ("ours", "v4_with_ours")]
    print(f"{len(rows)} arbitrages of $1 or more taken in their trigger's flashblock", flush=True)

    trig = rpc.batch([("eth_getTransactionByBlockNumberAndIndex", [hex(x["trigger"][0]), hex(x["trigger"][1])]) for x in rows])
    read = []
    for x, t in zip(rows, trig):
        if not t or x["trigger"][0] != x["block"]:
            continue
        tp = tx_priority(t, x["base_fee"])
        if tp is None:
            continue
        x.update(mechanism(x, {"txi": x["trigger"][1], "prio": tp, "from": t["from"], "to": t.get("to")}),
                 trigger_prio=tp, trigger_from=t["from"].lower(), trigger_to=(t.get("to") or "").lower())
        read.append(x)
    ranked = bots(read)
    usd = sum(x["usd"] for x in read)
    overall = {k: sum(x["usd"] for x in read if x[k]) / usd for k in ("outbid_trigger", "adjacent", "same_bot")} if usd else {}

    # The builder's order, and each top bot's traffic, over sampled blocks.
    blocks = sample_blocks(days[0]["lo"])
    ledger = RA.load_ledger(sorted(glob.glob(os.path.join(a.var, "flashblocks-*.jsonl"))), set(blocks))
    bodies = rpc.batch([("eth_getBlockByNumber", [hex(b), True]) for b in blocks])
    top = {b["to"] for b in ranked[:10]}
    pairs = inv = 0
    traffic = defaultdict(list)
    for b, body in zip(blocks, bodies):
        if not body:
            continue
        base = int(body["baseFeePerGas"], 16)
        txs = body["transactions"]
        for f in ledger.by_block.get(b, []):
            if f["first_tx"] is None:
                continue
            p = [tx_priority(t, base) for t in txs[f["first_tx"]: f["first_tx"] + f["tx_count"]]]
            q = [x for x in p if x is not None]
            pairs += max(0, len(q) - 1)
            inv += inversions(p)
        for t in txs:
            if (t.get("to") or "").lower() in top:
                traffic[t["to"].lower()].append(t["hash"])
    seen = len([x for x in bodies if x])
    for b in ranked[:10]:
        hs = traffic.get(b["to"], [])
        rs = [r for r in rpc.batch([("eth_getTransactionReceipt", [h]) for h in hs[:RECEIPTS]]) if r]
        b["sample"] = {"txs_per_block": round(len(hs) / seen, 2) if seen else None, "receipts_read": len(rs),
                       "reverted": round(sum(r["status"] != "0x1" for r in rs) / len(rs), 3) if rs else None,
                       "no_logs": round(sum(not r["logs"] for r in rs) / len(rs), 3) if rs else None,
                       "gas_used_p50": sorted(int(r["gasUsed"], 16) for r in rs)[len(rs) // 2] if rs else None}
    summary = {"arbitrages": len(rows), "read": len(read), "usd": round(usd, 2),
               "value_share": {k: round(v, 3) for k, v in overall.items()},
               "builder_order": {"blocks": seen, "pairs": pairs, "inversions": inv, "share": round(inv / pairs, 4) if pairs else None},
               "bots": [dict(b, usd=round(b["usd"], 2), share=round(b["share"], 3)) for b in ranked[:15]]}
    (out / "offset0.json").write_text(json.dumps(summary, indent=1))
    json.dump(read, gzip.open(out / "offset0-arbs.json.gz", "wt"))
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
