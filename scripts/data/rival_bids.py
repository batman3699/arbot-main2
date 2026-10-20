#!/usr/bin/env python3
"""rival_bids.py -- what rivals pay per gas for the gaps we price, and the
file the runtime bids against (Task 8.5 R31).

For every gap in the race ledgers whose ticket passed our own simulation
(R25's race ledger, the runs' journals) and that a later transaction took
through every pool of our route (race_analysis.rival), this reads the rival's
effective priority fee per gas: its receipt's effectiveGasPrice over its
block's base fee. Base orders a flashblock's transactions by priority fee, so
against a rival in our flashblock the higher bid wins.

The runtime bids, for a trade of net n (before the tip) and gas g, the b that
maximises P_same * F(b) * n - b * g over bidding nothing: F the share of these
bids below b, P_same the share of gaps contested in our own flashblock, the
tip paid win or lose (apex_runtime::econ::RivalBids::bid; `best_bid` here is
the same rule). A first rule, F(b) * (n - b * g), charged the tip only on a
win; the holdout showed it overbid. The
holdout fits F on the earlier gaps and scores the later ones: kept profit
over every gap, where a gap our transaction would reach in an earlier
flashblock than the rival's is won at any bid, one in the same flashblock is
won by outbidding, one in a later flashblock is lost, and a lost one pays its
tip on the revert.

Gaps whose ticket's net was over 0.05 ETH are left out: on a two-hop WETH
cycle that is a mispriced state (one was 49 ETH), not a gap. A ticket's net is
after R28's 20% tip on the canary run, and before any tip on the shadow runs.

Runs on the host, against its var/ directories, with public endpoints only:
the canary's BlockPI key is the canary's.

Usage (on the host, from /opt/apex/scripts/data):
  python3 rival_bids.py --out /root/rival-bids.base.json
"""
import argparse
import bisect
import glob
import json
import os
import sys
import time
import urllib.request

SIM_PASSED = ("RiskRejected", "SignerUnavailable", "dispatched", "Sent", "Landed")
PHANTOM_WEI = 5 * 10**16
GAS = 350_000          # the lean executor's two-hop trade (R29)
DELTA_NS = 7_500_000   # half the co-located sequencer RTT (R25)
CANARY_TIP = 0.2       # R28's share, in the canary run's nets


def best_bid(sorted_bids, net, gas, same, total):
    """The runtime's rule (apex_runtime::econ::RivalBids::bid): over bidding
    nothing, b earns P_same * F(b) * net - b * gas, the tip paid win or lose;
    the measured bid + 1 wei that earns most, or 0. Integer, as the runtime:
    below * same * net - n * total * b * gas. A tie keeps the lower bid."""
    if net <= 0 or gas <= 0:
        return 0
    net = int(net)
    n = len(sorted_bids)
    best_b, best, below, i = 0, 0, 0, 0
    while i < n:
        v = sorted_bids[i]
        while i < n and sorted_bids[i] == v:
            i += 1
            below += 1
        b = v + 1
        value = below * same * net - n * total * b * gas
        if value > best:
            best_b, best = b, value
    return best_b


def effective_priority(effective_gas_price, base_fee):
    return max(0, effective_gas_price - base_fee)


def counted(rows):
    """Gaps our simulation passed, with a believable net."""
    return [r for r in rows if any(k in r["outcome"] for k in SIM_PASSED)
            and r["net_ev_wei"] is not None and 0 < r["net_ev_wei"] <= PHANTOM_WEI]


def split(rows, fit_share):
    rows = sorted(rows, key=lambda r: r["trigger_block"])
    k = int(len(rows) * fit_share)
    return rows[:k], rows[k:]


def pre_tip(r):
    """A ticket's net before any tip: the canary run's nets are after R28's."""
    return r["net_ev_wei"] / (1 - CANARY_TIP) if r["run"] == "apex" else r["net_ev_wei"]


def score(rows, bid_of, gas):
    """Profit kept over `rows` bidding `bid_of(row)` per gas, and gaps won."""
    kept, won = 0.0, 0
    for r in rows:
        b = bid_of(r)
        tip = b * gas
        if r["class"] == "ahead" or (r["class"] == "same" and b > r["rival_prio_wei_per_gas"]):
            kept += pre_tip(r) - tip
            won += 1
        else:
            kept -= tip
    return kept, won


# ---- the host run ---------------------------------------------------------

class Rpc:
    """Public endpoints only, rotated, with retries."""
    EPS = ["https://base.drpc.org", "https://base.gateway.tenderly.co", "https://1rpc.io/base"]

    def __init__(self):
        self.i, self.next = 0, 0.0

    def call(self, method, params):
        err = None
        for n in range(10):
            wait = self.next - time.monotonic()
            if wait > 0:
                time.sleep(wait)
            self.next = time.monotonic() + 0.15
            body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
            try:
                req = urllib.request.Request(self.EPS[self.i % len(self.EPS)], body,
                                             {"Content-Type": "application/json", "User-Agent": "apex-rival-bids/1"})
                r = json.load(urllib.request.urlopen(req, timeout=30))
                if "result" in r:
                    return r["result"]
                err = str(r.get("error"))[:80]
            except Exception as e:  # the endpoint is never printed
                err = type(e).__name__
            self.i += 1
            time.sleep(min(2 ** n * 0.5, 10))
        raise RuntimeError(f"{method}: {err}")


def tickets(path):
    """ticket id -> expected_net_ev wei, and route hash -> its pools."""
    nets, pools = {}, {}
    for line in open(path):
        r = json.loads(line)
        if "Admitted" in r:
            tk = r["Admitted"]["ticket"]
            nets[tk["ticket_id"]] = int(tk["expected_net_ev"])
            rc = tk["route_commitment"]
            pools[rc["route_hash"]] = [h["pool"]["address"].lower() for h in rc["hops"]]
    return nets, pools


def measure(dirs, rpc):
    import race_analysis as RA
    rows, base = [], {}
    for d, journal in dirs:
        if not os.path.exists(journal):
            continue
        nets, route_pools = tickets(journal)
        race = [json.loads(l) for f in sorted(glob.glob(os.path.join(d, "race-*.jsonl"))) for l in open(f) if l.strip()]
        race = [l for l in race if l["route_hash"] in route_pools and l["trigger"].get("tx")]
        if not race:
            continue
        ledger = RA.load_ledger(sorted(glob.glob(os.path.join(d, "flashblocks-*.jsonl"))), {l["trigger"]["block"] for l in race})
        contested, _ = RA.analyze(race, ledger, RA.load_journal(journal), rpc, route_pools, DELTA_NS)
        for c in contested:
            rc = rpc.call("eth_getTransactionReceipt", [c["rival_tx"]])
            b = int(rc["blockNumber"], 16)
            if b not in base:
                base[b] = int(rpc.call("eth_getBlockByNumber", [hex(b), False])["baseFeePerGas"], 16)
            ours = c["ours_after_tier2"].get(20) if c["ours_after_tier2"] else None
            ours = ours if ours is not None else c["ours_at_proposed"].get(20)
            ro = c["rival_offset"]
            cls = None if None in (ours, ro) else "ahead" if ours < ro else "same" if ours == ro else "behind"
            if cls is None:
                continue
            rows.append({"run": os.path.basename(d), "ticket": c["ticket"], "trigger_block": c["trigger_flashblock"][0],
                         "class": cls, "rival_prio_wei_per_gas": effective_priority(int(rc["effectiveGasPrice"], 16), base[b]),
                         "net_ev_wei": nets.get(c["ticket"]), "outcome": json.dumps(c["outcome"])})
        print(f"{d}: {len(contested)} contested", file=sys.stderr, flush=True)
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--var", default="/opt/apex/var")
    ap.add_argument("--out", required=True)
    ap.add_argument("--fit-share", type=float, default=0.7)
    ap.add_argument("--rows", help="the measured gaps: read if it exists, else measured and written")
    a = ap.parse_args()
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    if a.rows and os.path.exists(a.rows):
        rows = json.load(open(a.rows))
    else:
        dirs = [(os.path.join(a.var, "apex"), os.path.join(a.var, "apex", "canary.journal"))]
        dirs += [(d, os.path.join(d, "shadow.journal")) for d in sorted(glob.glob(os.path.join(a.var, "run-*")))]
        rows = measure(dirs, Rpc())
        if a.rows:
            json.dump(rows, open(a.rows, "w"))
    rows = counted(rows)
    bids = sorted(r["rival_prio_wei_per_gas"] for r in rows)
    fit, test = split(rows, a.fit_share)
    fit_bids = sorted(r["rival_prio_wei_per_gas"] for r in fit)
    fit_same = sum(1 for r in fit if r["class"] == "same")
    policies = {
        "measured_bid": lambda r: best_bid(fit_bids, pre_tip(r), GAS, fit_same, len(fit)),
        "as_if_every_gap_contested": lambda r: best_bid(fit_bids, pre_tip(r), GAS, 1, 1),
        "flat_20pct": lambda r: int(0.2 * pre_tip(r) / GAS),
        "no_tip": lambda r: 0,
    }
    held = {k: score(test, f, GAS) for k, f in policies.items()}
    perfect = sum(pre_tip(r) for r in test if r["class"] == "ahead") + sum(
        max(0, pre_tip(r) - (r["rival_prio_wei_per_gas"] + 1) * GAS) for r in test if r["class"] == "same")
    classes = {}
    for r in rows:
        classes[r["class"]] = classes.get(r["class"], 0) + 1
    out = {
        "measured": {
            "by": "scripts/data/rival_bids.py", "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "gaps": len(rows), "blocks": [rows and min(r["trigger_block"] for r in rows), rows and max(r["trigger_block"] for r in rows)],
            "runs": sorted({r["run"] for r in rows}), "classes": classes,
            "rule": "per trade, the bid b maximising P_same * F(b) * net - b * gas over these bids, P_same = classes.same / all (apex_runtime::econ::RivalBids)",
        },
        "holdout": {
            "fit_gaps": len(fit), "test_gaps": len(test),
            "kept_wei": {k: int(v[0]) for k, v in held.items()} | {"perfect_foresight": int(perfect)},
            "won": {k: v[1] for k, v in held.items()},
        },
        "bids_wei_per_gas": bids,
    }
    with open(a.out, "w") as f:
        json.dump(out, f, indent=1)
    print(json.dumps({k: out[k] for k in ("measured", "holdout")}, indent=1))


if __name__ == "__main__":
    main()
