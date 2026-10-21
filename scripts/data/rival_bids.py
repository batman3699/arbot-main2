#!/usr/bin/env python3
"""rival_bids.py -- what rivals pay per gas for the gaps we price, and the
file the runtime bids against (Task 8.5 R31, re-measured in R33).

The runtime bids, for a trade of net n (before the tip) and gas g, the b that
maximises P_same * F(b) * n - b * g over bidding nothing: F the share of these
bids below b, P_same the share of gaps contested in our own flashblock, the
tip paid win or lose (apex_runtime::econ::RivalBids::bid; `best_bid` here is
the same rule). A first rule, F(b) * (n - b * g), charged the tip only on a
win; R31's holdout showed it overbid.

R33: who we race, and when we land. R31 took as the rival the first later
transaction that swapped through every pool of our route, and timed our send
from the ticket's close. Neither held for the canary's sends through Base's
sequencer (R32.1): for all six there was no such transaction within four
blocks, yet all six reverted; and a sent ticket closes after its outcome,
0.5-1 s after the send. So now, for every gap whose ticket passed our own
simulation:

  - the killer: the first transaction after the trigger's flashblock (the
    state we priced) that left our trade unable to pay. The first hop asks
    for exactly its quote and an Aerodrome v2 hop is asked for exactly its
    quote (apex_runtime::live::calls::lean_hop), so any net move against
    either kills; a v3 second hop pays while the move costs less than the
    margin (its minimum is the loan plus 1 wei; the 30 bps slippage bound is
    far below it). Judged on each transaction's end state, from the pools'
    Swap (v3) and Sync (Aerodrome v2) events.
  - where we land: the first flashblock to arrive at least d after our send,
    the send `tier2` after the proposal, both measured on our own sends
    through the sequencer (and d also on R32's probe): one landing per
    measured d, so a gap counts once per delay.
  - the class: the killer in an earlier flashblock than ours is a gap lost at
    any bid ("behind"); in ours, it is outbid or not ("same", the killer's
    effective priority fee is the rival bid); later or never, won at any bid
    ("ahead"). Base orders a flashblock's transactions by priority fee.

F is fitted on the "same" landings' rival bids only. The holdout fits on the
earlier gaps and scores the later ones: kept profit over every landing, a
lost one paying its tip on the revert. Each of our own sends is checked
against the model: the landing it really had, the bid it really made.

Gaps whose ticket's net was over 0.05 ETH are left out: on a two-hop WETH
cycle that is a mispriced state (one was 49 ETH), not a gap.

Runs on the host, against its var/ directories, with public endpoints only:
the canary's BlockPI key is the canary's.

Usage (on the host, from /opt/apex/scripts/data):
  python3 rival_bids.py --out /root/rival-bids.base.json --rows /root/r33-gaps.json \
      --current /opt/apex/ops/rival-bids.base.json
"""
import argparse
import calendar
import glob
import itertools
import json
import os
import statistics
import sys
import time
import urllib.request
from collections import namedtuple
from fractions import Fraction

SIM_PASSED = ("RiskRejected", "SignerUnavailable", "dispatched", "Sent", "Landed")
PHANTOM_WEI = 5 * 10**16
GAS = 350_000          # the lean executor's two-hop trade (R29)
CANARY_TIP = 0.2       # R28's share, in the canary run's nets until R31

SWAP_V3 = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"       # Uniswap v3, Slipstream
SWAP_PANCAKE = "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83"  # PancakeSwap v3
SYNC_V2 = "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"       # Aerodrome v2


def _ns(iso):
    return calendar.timegm(time.strptime(iso, "%Y-%m-%dT%H:%M:%SZ")) * 10**9


R31_AT_NS = _ns("2026-10-20T16:48:01Z")         # R31 live on the host: the bid replaced R28's 20%
SEQUENCER_SINCE_NS = _ns("2026-10-21T04:14:58Z")  # R32.1 live: the canary sends through the sequencer
PROBE_DELAYS_MS = (70, 254, 182, 136, 145)        # R32's apex send-probe, sequencer route, send to flashblock
LOG_BLOCKS = (-3, 2)                              # around the trigger: our pools' state before it, and after

Hop = namedtuple("Hop", "pool zero_for_one fee_ppm")


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


def counted(rows, sent=()):
    """Gaps our simulation passed, with a believable net. A ticket we `sent`
    passed it, though a revert on chain closes it as failed/Reverted, the
    class a Tier 2 failure gets."""
    return [r for r in rows if (any(k in r["outcome"] for k in SIM_PASSED) or r.get("ticket") in sent)
            and r["net_ev_wei"] is not None and 0 < r["net_ev_wei"] <= PHANTOM_WEI]


def split(rows, fit_share):
    rows = sorted(rows, key=lambda r: r["trigger_block"])
    k = int(len(rows) * fit_share)
    return rows[:k], rows[k:]


def pre_tip_net(net, run, at_ns):
    """A ticket's net before any tip: the canary's lost R28's 20% until R31;
    R31's bid is a fraction of a percent of it, and is left in."""
    return net * 5 // 4 if run == "apex" and at_ns < R31_AT_NS else net


# ---- the killer -----------------------------------------------------------

def _word(data, i):
    return int(data[2 + 64 * i: 2 + 64 * (i + 1)], 16)


def _pos(log):
    return int(log["blockNumber"], 16), int(log["transactionIndex"], 16)


def pool_state(log):
    """A pool's price after `log`: ("v3", sqrtPriceX96), ("v2", (reserve0, reserve1)), or None."""
    topic = log["topics"][0].lower() if log["topics"] else None
    if topic in (SWAP_V3, SWAP_PANCAKE):
        return "v3", _word(log["data"], 2)
    if topic == SYNC_V2:
        return "v2", (_word(log["data"], 0), _word(log["data"], 1))
    return None


def hop_output(state, zero_for_one, x, fee_ppm):
    """What the hop pays for x: a v3 pool at its price (the move is what is
    judged), an Aerodrome v2 pool exactly; x None is the v2 marginal price."""
    kind, s = state
    if kind == "v3":
        p = Fraction(s * s, 2 ** 192)
        return x * p if zero_for_one else x / p
    r_in, r_out = s if zero_for_one else s[::-1]
    if x is None:
        return Fraction(r_out * (10**6 - fee_ppm), r_in * 10**6) if r_in else Fraction(0)
    xin = Fraction(x * (10**6 - fee_ppm), 10**6)
    return r_out * xin / (r_in + xin)


def first_kill(logs, hops, amount_in, margin_wei, end):
    """The first transaction after `end` -- the trigger flashblock's last
    (block, tx index), the state we priced -- after which our trade could not
    pay: (block, tx index, hash); None when none in `logs` did; "no_reference"
    when one of our pools moved from a state `logs` does not show."""
    pools = [h.pool.lower() for h in hops]
    ordered = sorted((l for l in logs if l["address"].lower() in pools and pool_state(l)),
                     key=lambda l: (*_pos(l), int(l["logIndex"], 16)))
    ref = {}
    for l in ordered:
        if _pos(l) <= end:
            ref[l["address"].lower()] = pool_state(l)
    x = [amount_in]
    for prev, h in zip(hops, hops[1:]):
        r = ref.get(prev.pool.lower())
        x.append(hop_output(r, prev.zero_for_one, x[-1], prev.fee_ppm) if r and x[-1] is not None else None)
    margin = Fraction(max(0, margin_wei), amount_in + max(0, margin_wei))
    cur = dict(ref)
    post = (l for l in ordered if _pos(l) > end)
    for at, group in itertools.groupby(post, key=_pos):
        group = list(group)
        moved = set()
        for l in group:
            cur[l["address"].lower()] = pool_state(l)
            moved.add(l["address"].lower())
        for i, h in enumerate(hops):
            a = h.pool.lower()
            if a not in moved:
                continue
            if a not in ref:
                return "no_reference"
            kind = cur[a][0]
            tol = margin if i > 0 and kind == "v3" else 0
            xi = x[i] if x[i] is not None or kind == "v2" else 1
            if hop_output(cur[a], h.zero_for_one, xi, h.fee_ppm) < hop_output(ref[a], h.zero_for_one, xi, h.fee_ppm) * (1 - tol):
                return at[0], at[1], group[0]["transactionHash"]
    return None


# ---- where we land, and the class -----------------------------------------

def landing_offsets(ledger, trigger_fb, send_ns, delays_ms):
    """Per delay d, flashblocks from the trigger's to the first to arrive at
    or after send + d; None past the ledger."""
    return [ledger.offset(trigger_fb, ledger.would_land(send_ns, int(d * 1_000_000), 0)) for d in delays_ms]


def send_delays(sends, probe_ms=PROBE_DELAYS_MS):
    """From our sends: the median proposal-to-send (ms), and each send's
    send-to-landing-flashblock (ms), the probe's after them."""
    tier2 = int(round(statistics.median((s["sent_ns"] - s["proposed_ns"]) / 1e6 for s in sends)))
    return tier2, [(s["landed_arrival_ns"] - s["sent_ns"]) // 1_000_000 for s in sends] + list(probe_ms)


def classify(killer_offset, our_offset):
    if killer_offset is None or killer_offset > our_offset:
        return "ahead"
    return "same" if killer_offset == our_offset else "behind"


def pairs(gaps):
    """One row per (gap, landing): the gap with its class there."""
    return [dict(g, landing_offset=o, **{"class": classify(g["killer_offset"], o)})
            for g in gaps for o in g["landing_offsets"] if o is not None]


def fit(rows):
    """The bids the runtime reads (the rivals in our own flashblock), and P_same's terms."""
    same = [r for r in rows if r["class"] == "same"]
    return sorted(r["rival_prio_wei_per_gas"] for r in same), len(same), len(rows)


def score(rows, bid_of, gas):
    """Profit kept over `rows` bidding `bid_of(row)` per gas, and landings won."""
    kept, won = 0, 0
    for r in rows:
        tip = bid_of(r) * gas
        if r["class"] == "ahead" or (r["class"] == "same" and bid_of(r) > r["rival_prio_wei_per_gas"]):
            kept += r["pre_tip_net_wei"] - tip
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


def _int(v):
    return int(v, 16) if isinstance(v, str) and v.startswith("0x") else int(v)


def _addr(t):
    return int(t["address"] if isinstance(t, dict) else t, 16)


def tickets(path):
    """ticket id -> (expected_net_ev, exact input), and route hash -> its hops."""
    nets, routes = {}, {}
    for line in open(path):
        if '"Admitted"' not in line:
            continue
        tk = json.loads(line)["Admitted"]["ticket"]
        nets[tk["ticket_id"]] = (_int(tk["expected_net_ev"]), _int(tk["exact_input"]))
        rc = tk["route_commitment"]
        routes[rc["route_hash"]] = [Hop(h["pool"]["address"].lower(), _addr(h["token_in"]) < _addr(h["token_out"]), h["fee_ppm"])
                                    for h in rc["hops"]]
    return nets, routes


def sent_tickets(d):
    """The tickets the canary sent from run directory `d`."""
    p = os.path.join(d, "canary.jsonl")
    if not os.path.exists(p):
        return set()
    return {r["ticket"] for r in map(json.loads, open(p)) if r.get("kind") == "sent"}


def load(d, journal):
    """A run's sim-passed gaps with a believable net, and the ledger around them."""
    import race_analysis as RA
    nets, routes = tickets(journal)
    sent = sent_tickets(d)
    race = [json.loads(l) for f in sorted(glob.glob(os.path.join(d, "race-*.jsonl"))) for l in open(f) if l.strip()]
    race = [l for l in race if l.get("ticket") in nets and l["route_hash"] in routes and l["trigger"].get("tx")
            and counted([{"outcome": json.dumps(l["outcome"]), "net_ev_wei": nets[l["ticket"]][0], "ticket": l["ticket"]}], sent)]
    ledger = RA.load_ledger(sorted(glob.glob(os.path.join(d, "flashblocks-*.jsonl"))), {l["trigger"]["block"] for l in race})
    return nets, routes, race, ledger


class Chain:
    """The reads, cached: our pools' logs around a trigger, receipts, base fees."""

    def __init__(self, rpc):
        self.rpc, self.logs_, self.base_ = rpc, {}, {}

    def logs(self, pools, block):
        key = (tuple(sorted(pools)), block)
        if key not in self.logs_:
            self.logs_[key] = self.rpc.call("eth_getLogs", [{
                "address": list(key[0]), "topics": [[SWAP_V3, SWAP_PANCAKE, SYNC_V2]],
                "fromBlock": hex(block + LOG_BLOCKS[0]), "toBlock": hex(block + LOG_BLOCKS[1])}])
        return self.logs_[key]

    def receipt(self, tx):
        return self.rpc.call("eth_getTransactionReceipt", [tx])

    def priority(self, tx):
        rc = self.receipt(tx)
        b = int(rc["blockNumber"], 16)
        if b not in self.base_:
            self.base_[b] = int(self.rpc.call("eth_getBlockByNumber", [hex(b), False])["baseFeePerGas"], 16)
        return effective_priority(int(rc["effectiveGasPrice"], 16), self.base_[b])


def trigger_fb(chain, ledger, line, logs):
    """The trigger's flashblock, and its last transaction: the state we priced."""
    tx = line["trigger"]["tx"].lower()
    at = next((_pos(l) for l in logs if l["transactionHash"].lower() == tx), None)
    if at is None:
        rc = chain.receipt(tx)
        at = (int(rc["blockNumber"], 16), int(rc["transactionIndex"], 16))
    k = ledger.locate(*at)
    if k is None:
        return None, None
    return (k["block"], k["index"]), (k["block"], k["first_tx"] + k["tx_count"] - 1)


def our_sends(d, race, ledger, chain):
    """The canary's sends through the sequencer, timed: the proposal, the
    send, and the flashblock that carried it."""
    by_ticket = {l["ticket"]: l for l in race}
    out = []
    for line in open(os.path.join(d, "canary.jsonl")):
        s = json.loads(line)
        if s.get("kind") != "sent" or s["at_ms"] * 1_000_000 < SEQUENCER_SINCE_NS:
            continue
        rc = chain.receipt(s["tx"])
        r = by_ticket.get(s["ticket"])
        if not rc or not r:
            continue
        f = ledger.locate(int(rc["blockNumber"], 16), int(rc["transactionIndex"], 16))
        if f:
            out.append({"ticket": s["ticket"], "proposed_ns": r["proposed_ns"], "sent_ns": s["at_ms"] * 1_000_000,
                        "landed_arrival_ns": f["arrived_at_ns"], "landed_fb": (f["block"], f["index"]),
                        "bid": int(s["max_priority_fee_per_gas"]), "reverted": rc["status"] != "0x1"})
    return out


def measure(var, rpc):
    chain = Chain(rpc)
    dirs = [(os.path.join(var, "apex"), os.path.join(var, "apex", "canary.journal"))]
    dirs += [(d, os.path.join(d, "shadow.journal")) for d in sorted(glob.glob(os.path.join(var, "run-*")))]
    runs = [(d, *load(d, j)) for d, j in dirs if os.path.exists(j)]
    apex = next(r for r in runs if os.path.basename(r[0]) == "apex")
    sends = our_sends(apex[0], apex[3], apex[4], chain)
    tier2, delays = send_delays(sends)
    print(f"{len(sends)} sends through the sequencer: tier2 {tier2} ms; delays {sorted(delays)} ms", file=sys.stderr, flush=True)
    gaps, skipped, truth = [], {}, []
    for d, nets, routes, race, ledger in runs:
        run = os.path.basename(d)
        sent = {s["ticket"]: s for s in sends} if run == "apex" else {}
        for line in race:
            net, amount_in = nets[line["ticket"]]
            hops = routes[line["route_hash"]]
            logs = chain.logs([h.pool for h in hops], line["trigger"]["block"])
            kk, end = trigger_fb(chain, ledger, line, logs)
            kill = first_kill(logs, hops, amount_in, net, end) if kk else "trigger_unseen"
            if isinstance(kill, str):
                skipped[kill] = skipped.get(kill, 0) + 1
                continue
            killer_offset = prio = None
            if kill:
                kf = ledger.locate(kill[0], kill[1])
                if kf is None:
                    skipped["killer_unseen"] = skipped.get("killer_unseen", 0) + 1
                    continue
                killer_offset, prio = ledger.offset(kk, (kf["block"], kf["index"])), chain.priority(kill[2])
            g = {"run": run, "ticket": line["ticket"], "trigger_block": kk[0], "outcome": json.dumps(line["outcome"]),
                 "net_ev_wei": net, "pre_tip_net_wei": pre_tip_net(net, run, line["proposed_ns"]),
                 "killer_offset": killer_offset, "rival_prio_wei_per_gas": prio, "killer_tx": kill and kill[2],
                 "landing_offsets": landing_offsets(ledger, kk, line["proposed_ns"] + tier2 * 1_000_000, delays)}
            gaps.append(g)
            s = sent.get(line["ticket"])
            if s:
                o = ledger.offset(kk, s["landed_fb"])
                cls = classify(killer_offset, o)
                truth.append({"ticket": s["ticket"], "landed_offset": o, "modelled_offsets": g["landing_offsets"],
                              "killer_offset": killer_offset, "killer_prio": prio, "our_bid": s["bid"], "class": cls,
                              "model_says": "won" if cls == "ahead" or (cls == "same" and s["bid"] > prio) else "lost",
                              "chain_says": "reverted" if s["reverted"] else "landed"})
        print(f"{d}: {len(race)} gaps read; skipped so far {skipped}", file=sys.stderr, flush=True)
    return gaps, {"tier2_ms": tier2, "delays_ms": delays, "sends": len(sends), "skipped": skipped, "our_sends": truth}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--var", default="/opt/apex/var")
    ap.add_argument("--out", required=True)
    ap.add_argument("--fit-share", type=float, default=0.7)
    ap.add_argument("--rows", help="the measured gaps: read if it exists, else measured and written")
    ap.add_argument("--current", help="the bid file in use: scored on the same holdout")
    a = ap.parse_args()
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    if a.rows and os.path.exists(a.rows):
        gaps, landing = json.load(open(a.rows))
    else:
        gaps, landing = measure(a.var, Rpc())
        if a.rows:
            json.dump([gaps, landing], open(a.rows, "w"))
    # measure() keeps only the gaps `counted` keeps, and the ones we sent.
    rows = pairs(gaps)
    bids, _, _ = fit(rows)
    fit_gaps, test_gaps = split(gaps, a.fit_share)
    fit_bids, fit_same, fit_total = fit(pairs(fit_gaps))
    test = pairs(test_gaps)
    policies = {
        "measured_bid": lambda r: best_bid(fit_bids, r["pre_tip_net_wei"], GAS, fit_same, fit_total),
        "as_if_every_gap_contested": lambda r: best_bid(fit_bids, r["pre_tip_net_wei"], GAS, 1, 1),
        "flat_20pct": lambda r: int(0.2 * r["pre_tip_net_wei"] / GAS),
        "no_tip": lambda r: 0,
    }
    if a.current:
        cur = json.load(open(a.current))
        cb, cc = cur["bids_wei_per_gas"], cur["measured"]["classes"]
        cs, ct = cc.get("same", 0), sum(cc.get(k, 0) for k in ("same", "ahead", "behind"))
        policies["current_file"] = lambda r: best_bid(cb, r["pre_tip_net_wei"], GAS, cs, ct)
    held = {k: score(test, f, GAS) for k, f in policies.items()}
    perfect = sum(r["pre_tip_net_wei"] for r in test if r["class"] == "ahead") + sum(
        max(0, r["pre_tip_net_wei"] - (r["rival_prio_wei_per_gas"] + 1) * GAS) for r in test if r["class"] == "same")
    classes = {}
    for r in rows:
        classes[r["class"]] = classes.get(r["class"], 0) + 1
    out = {
        "measured": {
            "by": "scripts/data/rival_bids.py (R33)", "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "gaps": len(gaps), "landings": len(rows),
            "blocks": [gaps and min(g["trigger_block"] for g in gaps), gaps and max(g["trigger_block"] for g in gaps)],
            "runs": sorted({g["run"] for g in gaps}), "classes": classes,
            "landing": landing,
            "rule": "per trade, the bid b maximising P_same * F(b) * net - b * gas over these bids, P_same = classes.same / all,"
                    " F over the rivals in our own flashblock (apex_runtime::econ::RivalBids)",
        },
        "holdout": {
            "fit_gaps": len(fit_gaps), "test_gaps": len(test_gaps), "test_landings": len(test),
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
