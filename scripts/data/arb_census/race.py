"""When the arbitrage on a venue is taken (Task 8.5 R34).

R33 measured, on the gaps the canary priced on its 52 pools, that 80% die in
the very next flashblock after their trigger, and that through Base's
sequencer we mostly land in the one after that. Before building a venue,
this asks the same of its arbitrage as the market takes it: for every
arbitrage in a window the host's flashblock ledger covers, the trigger behind
it and the flashblocks between them.

The trigger is inferred, counting back from the arbitrage: on one of its
pools, where the net flow against the arbitrage's leg there reached half of
what that leg moved; the latest such point over its pools. On a pool that
trades every block, the last opposite trade before an arbitrage is usually
noise; half its flow is the move it took back.

Then what our landings would have kept of each (R33's: each measured
sequencer-route delay, sent 52 ms after the proposal): all of it, landing in
an earlier flashblock; what is left after outbidding its priority fee,
landing in its flashblock; nothing later. Uniswap v4 with the venues we
trade, and our own venues as the control, where R33 measured the same thing
from our own gaps.

Runs on the host (the ledger is there), public endpoints only. A day's
result is written, and skipped on a rerun.

Usage (on the host, from a directory holding arb_census/ and race_analysis.py):
  python3 -m arb_census.race --days 2026-10-20,2026-10-21 --out /root/r34 \
      --meta /root/r34/meta.json --gaps /root/r33/gaps.json
"""
import argparse
import gzip
import json
import os
import statistics
import time
from collections import Counter, defaultdict, deque
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from arb_census import detect
from arb_census import venues as V
from arb_census.collect import Resolver, _prices, _usd
from arb_census.rpc import Rpc, RpcError

SHARE = 0.5            # the trigger: where half the flow the arbitrage took back had arrived
BACK_BLOCKS = 150      # five minutes of each pool's trades, to find it in
GAS = 350_000          # the lean executor's two-hop trade (R29)
STEP = 50              # blocks a getLogs (1rpc's limit)
VALUED_USD = 1.0       # arbitrages from here have their receipts read: the priority fee
OURS = frozenset({"uniswap_v3", "pancakeswap_v3", "slipstream_5e7b", "slipstream_f8f2", "aerodrome_v2"})
V4 = "uniswap_v4"
ENDPOINTS = ("https://base.gateway.tenderly.co", "https://base.drpc.org", "https://1rpc.io/base")
BATCHES = 2            # 1rpc answers a batch of 50; drpc refuses one (500) and tenderly rate-limits it


def flow(leg):
    """A two-token leg's pool side of its lower-address token; None otherwise."""
    if len(leg.deltas) != 2:
        return None
    t = min(leg.deltas)
    return t, leg.deltas[t]


def trigger(history, arb_legs, share=SHARE):
    """`history`: pool -> [(position, leg)], in chain order, before the
    arbitrage. The latest position by which, on one of its pools, the net
    flow against its leg there, counted back from it, reached `share` of what
    that leg moved; None when none did."""
    best = None
    for a in arb_legs:
        f = flow(a)
        if not f or not f[1]:
            continue
        d = f[1]
        need, acc = share * abs(d), 0
        for pos, h in reversed(history.get(a.pool, ())):
            hf = flow(h)
            if not hf:
                continue
            acc += -hf[1] if d > 0 else hf[1]
            if acc >= need:
                best = pos if best is None or pos > best else best
                break
    return best


def route_of(legs):
    """(the pool that took WETH, when exactly one did; every leg's pool, in log
    order; the tokens the trader gained, sorted). R36 keys routes on the first:
    a flash swap's callback can log its second leg before its first."""
    took = [l.pool for l in legs if l.deltas.get(V.WETH, 0) > 0]
    gain = sorted(k for k, x in detect.trader_change(legs).items() if x > 0)
    return (took[0] if len(took) == 1 else None), [l.pool for l in legs], gain


def block_offset(arb_block, trigger_block):
    """Blocks from the trigger's to the arbitrage's: the offset once Denim's
    200 ms blocks replace flashblocks, and there is no flashblock ledger."""
    return arb_block - trigger_block


def landing_distribution(gaps):
    """Where we land, flashblocks after the trigger's: R33's landings."""
    c = Counter(o for g in gaps for o in g["landing_offsets"] if o is not None)
    n = sum(c.values())
    return {o: k / n for o, k in sorted(c.items())}


def captured(value_usd, cost_usd, prio_wei, offset, landing, eth_usd, gas=GAS):
    """What our landings keep of an arbitrage worth `value_usd`, taken
    `offset` flashblocks after its trigger at `prio_wei` per gas: landing
    earlier, all of it less our gas; in its flashblock, what outbidding it
    leaves, if anything; later, nothing. None without a trigger."""
    if offset is None:
        return None
    net = value_usd - cost_usd
    if net <= 0:
        return 0.0
    kept = 0.0
    for o, p in landing.items():
        if o < offset:
            kept += p * net
        elif o == offset:
            kept += p * max(0.0, net - (prio_wei + 1) * gas / 1e18 * eth_usd)
    return kept


def by_offset(arbs):
    """The share of value taken 0, 1, 2, 3+ flashblocks after its trigger, or with none found."""
    out = {"0": 0.0, "1": 0.0, "2": 0.0, "3+": 0.0, "none": 0.0}
    total = sum(a["usd"] for a in arbs)
    for a in arbs:
        o = a["offset"]
        out["none" if o is None else str(o) if o < 3 else "3+"] += a["usd"]
    return {k: v / total for k, v in out.items()} if total else out


# ---- the host run ---------------------------------------------------------

def transient(message):
    """A failure another endpoint, or a later try, may not have: the
    transport's, or a limit's. An error the chain gave (a revert) is the same
    everywhere."""
    m = message.lower()
    return m.startswith("transport failed") or any(w in m for w in ("rate", "limit", "timeout", "too many", "exceed"))


class Public:
    """The census client over public endpoints: calls in turn across them,
    the next one tried on a transient failure; a batch as single calls, six
    at a time (from AWS, 1rpc answers a batch with nothing and drpc refuses
    one)."""

    def __init__(self, rate=4.0):
        self.rpcs, self.i = [Rpc(url=u, max_per_second=rate) for u in ENDPOINTS], 0

    def call(self, method, params):
        err = None
        for k in range(3 * len(self.rpcs)):
            j = (self.i + k) % len(self.rpcs)
            try:
                out = self.rpcs[j].call(method, params)
                self.i = (j + 1) % len(self.rpcs)
                return out
            except RpcError as e:  # the endpoint is never printed
                if not transient(str(e)):
                    raise
                err = e
                time.sleep(min(2 ** (k // len(self.rpcs)), 8))
        raise err

    def batch(self, calls, size=50):
        def one(c):
            try:
                return self.call(*c)
            except RpcError:
                return None
        with ThreadPoolExecutor(6) as ex:
            return list(ex.map(one, calls))


class Resolving(Resolver):
    """The census's resolver, asking once about a pool it could not resolve."""

    def __init__(self, rpc, cache_path):
        super().__init__(rpc, cache_path)
        self.failed = set()

    def meta_for_logs(self, logs):
        logs = [l for l in logs if V.pool_key(l) not in self.failed]
        out = super().meta_for_logs(logs)
        self.failed |= {k for k in map(V.pool_key, logs) if k is not None and k not in self.meta}
        return out


def category(venues):
    v = set(venues)
    if V4 in v:
        return "v4_with_ours" if v <= OURS | {V4} else "v4_with_others"
    return "ours" if v <= OURS else "others"


def race_day(rpc, resolver, ledger, lo, hi, say=print):
    """Every arbitrage in blocks lo..hi, with its trigger and their flashblocks."""
    save, resolver.save = resolver.save, lambda: None   # 6 MB a write: once, at the end
    try:
        return _race_day(rpc, resolver, ledger, lo, hi, say)
    finally:
        resolver.save = save
        save()


def _decoded(entries, meta):
    """A pool's history as (position, leg), each raw log decoded once."""
    for e in entries:
        if e[2] is None:
            e[2] = V.decode(e[1], meta) or False
    return [(e[0], e[2]) for e in entries if e[2]]


def _race_day(rpc, resolver, ledger, lo, hi, say):
    history, arbs, legs_kept = defaultdict(deque), [], []
    b, step = lo, STEP
    while b <= hi:
        e = min(b + step - 1, hi)
        try:
            logs = [l for l in rpc.call("eth_getLogs", [{"fromBlock": hex(b), "toBlock": hex(e), "topics": [list(V.ALL_TOPICS)]}]) if l["topics"]]
        except RpcError:
            if step > 5:
                step //= 2
                continue
            raise
        by_tx = {}
        for l in logs:
            pos = (int(l["blockNumber"], 16), int(l["transactionIndex"], 16), int(l["logIndex"], 16))
            by_tx.setdefault((pos[0], pos[1], l["transactionHash"]), []).append((pos, l))
        # Only a transaction of two trades or more can be an arbitrage: its
        # pools are resolved; a lone trade waits in the history, raw, and is
        # decoded if an arbitrage on its pool comes.
        meta = resolver.meta_for_logs([l for items in by_tx.values() if len(items) >= 2 for _, l in items])
        for (blk, txi, tx), items in sorted(by_tx.items()):
            if len(items) >= 2:
                legs = [x for x in (V.decode(l, meta) for _, l in items) if x]
                legs_kept.extend(legs)
                if len(legs) >= 2 and detect.is_arbitrage(legs):
                    venues = sorted({x.venue for x in legs})
                    past = {g.pool: _decoded(history.get(g.pool, ()), resolver.meta) for g in legs}
                    weth_in, pools, gain = route_of(legs)
                    arbs.append({"tx": tx, "block": blk, "txi": txi, "venues": venues, "category": category(venues),
                                 "hops": len(legs), "change": {k: x for k, x in detect.trader_change(legs).items() if x > 0},
                                 "pools": pools, "weth_in": weth_in, "gain": gain,
                                 "trigger": trigger(past, legs)})
            for pos, l in items:
                k = V.pool_key(l)
                if k is not None:
                    history[k].append([pos, l, None])
        for p in list(history):
            q = history[p]
            while q and q[0][0][0] < e - BACK_BLOCKS:
                q.popleft()
            if not q:
                del history[p]
        if (b - lo) % 5_000 < step:
            say(f"  {time.strftime('%H:%M:%S')} {b}: {len(arbs)} arbitrages so far", flush=True)
        b, step = e + 1, min(step * 2, STEP)
    usd, dec = _prices(rpc, legs_kept)
    del legs_kept
    # After Denim there is no flashblock ledger: a 200 ms block is the unit.
    unledgered = not any(lo <= blk <= hi for blk in ledger.by_block)
    for a in arbs:
        a["usd"], _ = _usd(a.pop("change"), usd, dec)
        f = ledger.locate(a["block"], a["txi"])
        t = a["trigger"]
        tf = ledger.locate(t[0], t[1]) if t else None
        a["fb"] = [f["block"], f["index"]] if f else None
        a["offset"] = ledger.offset((tf["block"], tf["index"]), (f["block"], f["index"])) if f and tf else None
        if unledgered and t and a["offset"] is None:
            a["offset"] = block_offset(a["block"], t[0])
        a["timing"] = "no_trigger" if not t else "unseen" if a["offset"] is None else "ok"
    want = [a for a in arbs if a["usd"] >= VALUED_USD and a["category"] in ("v4_with_ours", "ours")]
    blocks = sorted({a["block"] for a in want})
    heads = rpc.batch([("eth_getBlockByNumber", [hex(x), False]) for x in blocks])
    base = {x: int(h["baseFeePerGas"], 16) for x, h in zip(blocks, heads) if h}
    for a, r in zip(want, rpc.batch([("eth_getTransactionReceipt", [a["tx"]]) for a in want])):
        if r and a["block"] in base:
            a["prio"] = max(0, int(r["effectiveGasPrice"], 16) - base[a["block"]])
            a["base_fee"], a["gas"] = base[a["block"]], int(r["gasUsed"], 16)
            a["sender"], a["to"] = r["from"].lower(), (r.get("to") or "").lower()
    return {"lo": lo, "hi": hi, "eth_usd": usd.get(V.WETH, 0.0), "unresolved_pools": len(getattr(resolver, "failed", ())),
            "base_fee_median": statistics.median(base.values()) if base else None, "arbs": arbs}


def day_range(var, day):
    lo = hi = None
    for line in open(os.path.join(var, f"flashblocks-{day}.jsonl")):
        b = json.loads(line)["block"]
        lo, hi = (b, b) if lo is None else (min(lo, b), max(hi, b))
    return lo, hi - 1   # the next day's file starts at its last block


def day_ledger(var, lo, hi):
    import race_analysis as RA
    lines = []
    for p in sorted(Path(var).glob("flashblocks-*.jsonl")):
        with open(p) as f:
            first = json.loads(f.readline())["block"]
            if first > hi + 1:
                continue
            f.seek(0)
            for line in f:
                x = json.loads(line)
                if lo - BACK_BLOCKS <= x["block"] <= hi + 1:
                    lines.append(x)
    return RA.Ledger(lines)


def summarize(days, landing):
    out = {"landing": {str(k): round(v, 4) for k, v in landing.items()}, "days": [], "categories": {}}
    cats = defaultdict(list)
    for d in days:
        eth, base = d["eth_usd"], d["base_fee_median"] or 0
        for a in d["arbs"]:
            cost = (a.get("base_fee") or base) * GAS / 1e18 * eth
            a["kept_usd"] = captured(a["usd"], cost, a.get("prio", 0), a["offset"], landing, eth)
            cats[a["category"]].append(a)
        out["days"].append({"lo": d["lo"], "hi": d["hi"], "eth_usd": round(eth, 2), "arbs": len(d["arbs"]),
                            "usd": round(sum(a["usd"] for a in d["arbs"]), 2)})
    for c, arbs in sorted(cats.items()):
        big = [a for a in arbs if a["usd"] >= VALUED_USD]
        timed = [a for a in arbs if a["timing"] == "ok"]
        prios = sorted(a["prio"] for a in big if "prio" in a and a["offset"] in (1, 2))
        q = (lambda p: prios[min(len(prios) - 1, int(len(prios) * p))] / 1e6) if prios else (lambda p: None)
        out["categories"][c] = {
            "arbs": len(arbs), "usd": round(sum(a["usd"] for a in arbs), 2),
            "arbs_1usd": len(big), "usd_1usd": round(sum(a["usd"] for a in big), 2),
            "timing": dict(Counter(a["timing"] for a in arbs)),
            "value_by_offset": {k: round(v, 3) for k, v in by_offset(timed).items()},
            "value_by_offset_1usd": {k: round(v, 3) for k, v in by_offset([a for a in big if a["timing"] == "ok"]).items()},
            "kept_usd": round(sum(a["kept_usd"] for a in timed), 2),
            "timed_usd": round(sum(a["usd"] for a in timed), 2),
            "rival_prio_mwei_at_1_2": {"n": len(prios), "p25": q(.25), "p50": q(.5), "p75": q(.75), "p90": q(.9)},
            "top": [{k: a.get(k) for k in ("tx", "usd", "offset", "venues", "hops", "prio", "kept_usd")}
                    for a in sorted(arbs, key=lambda a: -a["usd"])[:8]],
        }
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", required=True, help="comma-separated UTC days the ledger covers, e.g. 2026-10-20,2026-10-21")
    ap.add_argument("--var", default="/opt/apex/var/apex")
    ap.add_argument("--out", required=True)
    ap.add_argument("--meta", required=True, help="the census's pool metadata cache (data/census/meta.json)")
    ap.add_argument("--gaps", required=True, help="R33's measured gaps (rival_bids.py --rows), for our landings")
    ap.add_argument("--rate", type=float, default=6.0, help="requests a second, each endpoint")
    a = ap.parse_args()
    rpc = Public(a.rate)
    resolver = Resolving(rpc, a.meta)
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    days = []
    for day in a.days.split(","):
        path = out / f"race-{day}.json.gz"
        if not path.exists():
            lo, hi = day_range(a.var, day)
            print(f"{day}: blocks {lo}..{hi}", flush=True)
            d = race_day(rpc, resolver, day_ledger(a.var, lo, hi), lo, hi)
            with gzip.open(path, "wt") as f:
                json.dump(d, f)
            print(f"{day}: {len(d['arbs'])} arbitrages", flush=True)
        days.append(json.load(gzip.open(path, "rt")))
    gaps, _ = json.load(open(a.gaps))
    s = summarize(days, landing_distribution(gaps))
    (out / "summary.json").write_text(json.dumps(s, indent=1))
    print(json.dumps(s, indent=1))


if __name__ == "__main__":
    main()
