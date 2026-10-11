#!/usr/bin/env python3
"""The contest analyzer (Task 8.5 R25). Per contested gap: the trigger's
flashblock k, the rival's, and the flashblock our transaction would have landed
in, at proposal (step 2's fast path) and after Tier 2 (today). Gates G1 and G2.

A gap is contested when a later transaction swapped through every pool of our
route within four blocks of the trigger: the rival. Offsets count flashblocks
from k, across blocks, as the flashblock ledger saw them arrive.

  G1  detection: flashblock k's arrival -> the capture loop took the event,
      p50 <= 80 ms and p90 <= 130 ms.
  G2  our transaction, sent at proposal, would land by k+1 on >= 50% of
      contested gaps at eps = 20 ms.

Run from scripts/data, against a copy of the host's var/apex:
  python3 race_analysis.py --dir ../../var/apex --delta-ms <half the sequencer RTT>

The RPC key is APEX_SECRET_BLOCKPI_KEY when set (the host), else .env's
BLOCKPI_KEY; it is never printed.
"""
import argparse
import bisect
import glob
import json
import os
import sys
from pathlib import Path

SWAP_TOPICS = ["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67",  # Uniswap v3, Slipstream
               "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83",  # PancakeSwap v3
               "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"]  # Aerodrome v2's Sync
EPSILONS_MS = (0, 20, 40)
RIVAL_BLOCKS = 4
# Blocks loaded around each trigger: one before (a late flashblock), and the
# rival's window plus one (where our transaction could still have landed).
WINDOW = (-1, RIVAL_BLOCKS + 1)


class Ledger:
    """The flashblock ledger, or the part of it near the triggers."""

    def __init__(self, lines):
        self.by_block = {}
        for x in lines:
            self.by_block.setdefault(x["block"], []).append(x)
        for v in self.by_block.values():
            v.sort(key=lambda x: x["index"])
        self.ordered = sorted((x for v in self.by_block.values() for x in v), key=lambda x: x["arrived_at_ns"])
        self._arrivals = [x["arrived_at_ns"] for x in self.ordered]
        self._position = {(x["block"], x["index"]): i for i, x in enumerate(self.ordered)}

    @staticmethod
    def index_of(x):
        return None if x is None else x["index"]

    def locate(self, block, tx_index):
        """The flashblock that added `tx_index` to `block`, if seen whole."""
        for x in self.by_block.get(block, []):
            if x["first_tx"] is not None and x["first_tx"] <= tx_index < x["first_tx"] + x["tx_count"]:
                return x
        return None

    def would_land(self, t_ns, delta_ns, eps_ns):
        """The first flashblock to arrive at or after t + delta + eps."""
        i = bisect.bisect_left(self._arrivals, t_ns + delta_ns + eps_ns)
        if i == len(self.ordered):
            return None
        return (self.ordered[i]["block"], self.ordered[i]["index"])

    def offset(self, frm, to):
        """Flashblocks from `frm` to `to`, both (block, index), counting across blocks."""
        if frm is None or to is None or frm not in self._position or to not in self._position:
            return None
        return self._position[to] - self._position[frm]


def load_ledger(paths, trigger_blocks):
    """The ledger lines within WINDOW of a trigger block: two weeks of the
    stream is millions of lines, and only these are asked about."""
    wanted = {b + d for b in trigger_blocks for d in range(WINDOW[0], WINDOW[1] + 1)}
    lines = []
    for p in paths:
        with open(p) as f:
            for line in f:
                if line.strip():
                    x = json.loads(line)
                    if x["block"] in wanted:
                        lines.append(x)
    return Ledger(lines)


def rival(rpc, pools, block, tx_index, blocks=RIVAL_BLOCKS):
    """The first transaction after (block, tx_index), within `blocks`, that
    swapped through every one of `pools`; None when none did."""
    logs = rpc.call("eth_getLogs", [{"address": pools, "topics": [SWAP_TOPICS],
                                     "fromBlock": hex(block), "toBlock": hex(block + blocks)}])
    by_tx = {}
    for l in logs:
        b, t = int(l["blockNumber"], 16), int(l["transactionIndex"], 16)
        if (b, t) <= (block, tx_index):
            continue
        by_tx.setdefault((b, t, l["transactionHash"]), set()).add(l["address"].lower())
    want = {p.lower() for p in pools}
    for (b, t, tx), touched in sorted(by_tx.items()):
        if touched >= want:
            r = rpc.call("eth_getTransactionByHash", [tx])
            prio = r.get("maxPriorityFeePerGas") or r.get("gasPrice") or "0x0"
            return {"block": b, "tx_index": t, "tx": tx, "priority_gwei": int(prio, 16) / 1e9}
    return None


def pct(v, p):
    """Nearest rank."""
    v = sorted(v)
    return v[max(0, min(len(v) - 1, -(-len(v) * p // 100) - 1))] if v else None


def share(rows, ok):
    return {e: (sum(1 for r in rows if ok(r, e)) / len(rows)) if rows else None for e in EPSILONS_MS}


def analyze(race, ledger, journal, rpc, route_pools, delta_ns):
    rows = []
    receipts, rivals = {}, {}
    for line in race:
        pools = route_pools.get(line["route_hash"])
        tx = line["trigger"]["tx"]
        if not pools or not tx:
            continue
        if tx not in receipts:
            receipts[tx] = rpc.call("eth_getTransactionReceipt", [tx])
        rc = receipts[tx]
        if not rc:
            continue
        tb, ti = int(rc["blockNumber"], 16), int(rc["transactionIndex"], 16)
        k = ledger.locate(tb, ti)
        if k is None:
            continue
        kk = (k["block"], k["index"])
        key = (tuple(sorted(p.lower() for p in pools)), tb, ti)
        if key not in rivals:
            rivals[key] = rival(rpc, pools, tb, ti)
        riv = rivals[key]
        if riv is None:
            continue
        rf = ledger.locate(riv["block"], riv["tx_index"])
        j = journal.get(line.get("ticket")) or {}
        end = j.get("closed_ns")
        rows.append({
            "route_hash": line["route_hash"], "ticket": line.get("ticket"), "trigger_flashblock": kk,
            "rival_tx": riv["tx"],
            "rival_offset": ledger.offset(kk, (rf["block"], rf["index"])) if rf else None,
            "rival_priority_gwei": riv["priority_gwei"],
            "ours_at_proposed": {e: ledger.offset(kk, ledger.would_land(line["proposed_ns"], delta_ns, e * 1_000_000)) for e in EPSILONS_MS},
            "ours_after_tier2": {e: ledger.offset(kk, ledger.would_land(end, delta_ns, e * 1_000_000)) if end else None for e in EPSILONS_MS},
            "g1_ms": (line["received_ns"] - k["arrived_at_ns"]) // 1_000_000,
            "stage_ms": {"observed_to_received": (line["received_ns"] - line["observed_ns"]) // 1_000_000,
                         "received_to_proposed": (line["proposed_ns"] - line["received_ns"]) // 1_000_000,
                         "tier2": ((end - j["exacting_ns"]) // 1_000_000) if end and j.get("exacting_ns") else None},
            "outcome": line["outcome"],
        })
    g1 = [r["g1_ms"] for r in rows]
    offsets = {}
    for r in rows:
        if r["rival_offset"] is not None:
            offsets[r["rival_offset"]] = offsets.get(r["rival_offset"], 0) + 1
    summary = {
        "contested": len(rows),
        "g1_ms": {"p50": pct(g1, 50), "p90": pct(g1, 90)},
        "g1_pass": bool(g1) and pct(g1, 50) <= 80 and pct(g1, 90) <= 130,
        # Landed by k+1, sent at proposal.
        "g2_share": share(rows, lambda r, e: r["ours_at_proposed"][e] is not None and r["ours_at_proposed"][e] <= 1),
        # Landed in an earlier flashblock than the rival: won outright, before
        # any priority-fee ordering inside a flashblock.
        "ahead_share": share(rows, lambda r, e: None not in (r["ours_at_proposed"][e], r["rival_offset"])
                             and r["ours_at_proposed"][e] < r["rival_offset"]),
        "rival_offsets": dict(sorted(offsets.items())),
    }
    summary["g2_pass"] = bool(rows) and summary["g2_share"][20] >= 0.5
    return rows, summary


def load_journal(path):
    """ticket id -> {"exacting_ns", "closed_ns"} from the shadow journal."""
    out = {}
    with open(path) as f:
        for line in f:
            r = json.loads(line)
            if "Advanced" in r and r["Advanced"].get("to") == "Exacting":
                out.setdefault(r["Advanced"]["id"], {})["exacting_ns"] = r["Advanced"]["at"]
            elif "Closed" in r:
                out.setdefault(r["Closed"]["id"], {})["closed_ns"] = r["Closed"]["at"]
    return out


def route_pools_from_journal(path):
    """route hash -> its pools, from each admitted ticket's commitment."""
    out = {}
    with open(path) as f:
        for line in f:
            r = json.loads(line)
            if "Admitted" in r:
                rc = r["Admitted"]["ticket"]["route_commitment"]
                out[rc["route_hash"]] = [h["pool"]["address"].lower() for h in rc["hops"]]
    return out


def main():
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from arb_census.rpc import Rpc
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default="../../var/apex")
    ap.add_argument("--delta-ms", type=float, required=True)
    ap.add_argument("--out", default="race_rows.jsonl")
    a = ap.parse_args()
    d = Path(a.dir)
    race = [json.loads(l) for f in sorted(glob.glob(str(d / "race-*.jsonl"))) for l in open(f) if l.strip()]
    journal_path = d / "shadow.journal"
    route_pools = route_pools_from_journal(journal_path)
    ledger = load_ledger(sorted(glob.glob(str(d / "flashblocks-*.jsonl"))),
                         {l["trigger"]["block"] for l in race if l["route_hash"] in route_pools})
    key = os.environ.get("APEX_SECRET_BLOCKPI_KEY")
    rpc = Rpc("https://base.blockpi.network/v1/rpc/" + key) if key else Rpc()
    rows, summary = analyze(race, ledger, load_journal(journal_path), rpc, route_pools, int(a.delta_ms * 1_000_000))
    with open(a.out, "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
