"""Collect a week of Base arbitrage, one day at a time (R23).

For each day: every decodable trade log, grouped by transaction; closure
tested on the decoded legs; receipts read for arbitrages worth $1 or more
and every arbitrage touching a market-maker venue, for what was banked and
the gas. Resumable: a day whose file exists is skipped.
"""
import argparse
import gzip
import json
import statistics
from pathlib import Path

from arb_census import detect
from arb_census import venues as V
from arb_census.rpc import Rpc, RpcError

REPO = Path(__file__).resolve().parents[3]
DATA = REPO / "data" / "census"
BLOCKS_PER_DAY = 43_200
RECEIPT_FLOOR_USD = 1.0
DEC = {V.WETH: 18, V.USDC: 6, "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": 6,
       "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": 8}
POSITION_MANAGER = "0x7c5f5a4bbd8fd63184577525326123b519429bdc"
MAVERICK_FACTORY = "0x0a7e848aca42d879ef06507fca0e7b33a0a63c1e"
FLUID_RESOLVER = "0x160ffc75904515f38c9b7ed488e1f5a43ce71eba"
# Metric v1's current factory (MetricOmmPoolFactory, Base from 2026-09-15):
# PoolCreated(poolAddress indexed, token0 indexed, token1 indexed, ...). Its
# legacy factory 0xe22f9fc0... and retired one 0x62291138... have no live pools.
METRIC_FACTORY = "0x2a53833cc95548cf52c7b159110e22d3a9018f32"
METRIC_FROM = 51_352_088
METRIC_CREATED = "0x4b36a0ddce54edb36597ee7d496df06c53fe875aba9d7257534a38d5177899aa"
# BlockPI keeps logs from this block only, and returns at most 5,000 blocks a
# query with an address. Hanji's books predate it, so they are read as they
# trade, through each book's own getConfig().
EARLIEST_LOGS = 50_500_000
SCAN_STEP = 5_000


def _addr(h):
    return "0x" + h[-40:].lower() if h and len(h) >= 42 else None


def _words(h):
    d = h[2:] if h else ""
    return [d[i:i + 64] for i in range(0, len(d), 64)]


class Resolver:
    """Pool keys to tokens and venue; venue pool sets discovered once."""

    def __init__(self, rpc, cache_path=None):
        self.rpc = rpc
        self.path = Path(cache_path) if cache_path else DATA / "meta.json"
        self.meta = json.loads(self.path.read_text()) if self.path.exists() else {}
        # A pool cached before its factory was named keeps the generic label
        # unless renamed here.
        for m in self.meta.values():
            if isinstance(m, list) and len(m) == 3 and m[2].startswith("factory:"):
                m[2] = V.VENUE_OF_FACTORY.get(m[2][len("factory:"):], m[2])
        self._sets = None

    def save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(json.dumps(self.meta))

    def _scan(self, address, topic, lo):
        head = int(self.rpc.call("eth_blockNumber", []), 16)
        out, b, step = [], max(lo, EARLIEST_LOGS), SCAN_STEP
        while b <= head:
            e = min(b + step - 1, head)
            try:
                out += self.rpc.call("eth_getLogs", [{"address": address, "topics": [topic], "fromBlock": hex(b), "toBlock": hex(e)}])
            except RpcError:
                if step > 500:
                    step //= 2
                    continue
                raise
            b = e + 1
        return out

    def venue_sets(self):
        """Maverick, Fluid, Metric and Hanji pools, discovered from their
        factories once and cached in the metadata."""
        if self._sets is not None:
            return self._sets
        if "_sets" in self.meta:
            self._sets = self.meta["_sets"]
            return self._sets
        sets = {"maverick_v2": {}, "fluid": {}, "metric": {}, "hanji": {}}
        n = int(self.rpc.call("eth_call", [{"to": MAVERICK_FACTORY, "data": "0xf525cb68"}, "latest"]), 16)
        for start in range(0, n, 200):
            r = self.rpc.call("eth_call", [{"to": MAVERICK_FACTORY, "data": "0xb4b9d1f1" + format(start, "064x") + format(min(start + 200, n), "064x")}, "latest"])
            w = _words(r)
            pools = [_addr(x) for x in w[2:2 + int(w[1], 16)]]
            toks = self.rpc.batch([c for p in pools for c in (("eth_call", [{"to": p, "data": "0x0fc63d10"}, "latest"]), ("eth_call", [{"to": p, "data": "0x5f64b55b"}, "latest"]))])
            for i, p in enumerate(pools):
                a, b = _addr(toks[2 * i]), _addr(toks[2 * i + 1])
                if a and b:
                    sets["maverick_v2"][p] = [a, b, "maverick_v2"]
        r = self.rpc.call("eth_call", [{"to": FLUID_RESOLVER, "data": "0xd88ff1f4"}, "latest"])
        w = _words(r)
        count = int(w[1], 16)
        for i in range(count):
            p, t0, t1 = _addr(w[2 + 4 * i]), _addr(w[3 + 4 * i]), _addr(w[4 + 4 * i])
            sets["fluid"][p] = [t0, t1, "fluid"]
        for log in self._scan(METRIC_FACTORY, METRIC_CREATED, METRIC_FROM):
            t = log["topics"]
            if len(t) == 4:
                sets["metric"][_addr(t[1])] = [_addr(t[2]), _addr(t[3]), "metric"]
        self.meta["_sets"] = self._sets = sets
        self.save()
        return sets

    def meta_for_logs(self, logs):
        sets = self.venue_sets()
        need_pool, need_v4, need_curve, need_hanji = set(), set(), set(), set()
        for log in logs:
            if not log["topics"]:
                continue
            t0, key = log["topics"][0], V.pool_key(log)
            if key is None or key in self.meta:
                continue
            if t0 == V.MAVERICK_V2:
                if key in sets["maverick_v2"]:
                    self.meta[key] = sets["maverick_v2"][key]
            elif t0 == V.FLUID:
                if key in sets["fluid"]:
                    self.meta[key] = sets["fluid"][key]
            elif t0 == V.METRIC:
                if key in sets["metric"]:
                    self.meta[key] = sets["metric"][key]
            elif t0 == V.HANJI_ORDER:
                need_hanji.add(key)
            elif t0 == V.UNI_V4:
                need_v4.add(key)
            elif t0 in (V.CURVE_I128, V.CURVE_U256):
                need_curve.add(key)
            elif t0 in (V.UNI_V3, V.PANCAKE_V3, V.ALGEBRA_INTEGRAL, V.UNI_V2, V.AERO_V2):
                need_pool.add(key)
        pools = sorted(need_pool)
        res = self.rpc.batch([c for p in pools for c in (
            ("eth_call", [{"to": p, "data": "0x0dfe1681"}, "latest"]),
            ("eth_call", [{"to": p, "data": "0xd21220a7"}, "latest"]),
            ("eth_call", [{"to": p, "data": "0xc45a0155"}, "latest"]))])
        for i, p in enumerate(pools):
            t0, t1, f = _addr(res[3 * i]), _addr(res[3 * i + 1]), _addr(res[3 * i + 2])
            if t0 and t1:
                self.meta[p] = [t0, t1, V.VENUE_OF_FACTORY.get(f, f"factory:{f}" if f else "nofactory")]
        ids = sorted(need_v4)
        res = self.rpc.batch([("eth_call", [{"to": POSITION_MANAGER, "data": "0x86b6be7d" + i[2:52] + "0" * 14}, "latest"]) for i in ids])
        for i, r in zip(ids, res):
            w = _words(r)
            if len(w) >= 5 and (int(w[0], 16) or int(w[1], 16)):
                self.meta[i] = [_addr(w[0]), _addr(w[1]), "uniswap_v4"]
        books = sorted(need_hanji)
        cfgs = self.rpc.batch([("eth_call", [{"to": b, "data": "0xc3f909d4"}, "latest"]) for b in books])
        for b, r in zip(books, cfgs):
            w = _words(r)
            # getConfig(): scaling x, scaling y, token x, token y, ...
            if len(w) >= 4 and int(w[0], 16) and int(w[1], 16):
                self.meta[b] = {"x": _addr(w[2]), "y": _addr(w[3]), "sx": int(w[0], 16), "sy": int(w[1], 16), "venue": "hanji"}
        for p in sorted(need_curve):
            coins = []
            for sel in ("0xc6610657", "0x23746eb8"):
                for k in range(8):
                    try:
                        c = _addr(self.rpc.call("eth_call", [{"to": p, "data": sel + format(k, "064x")}, "latest"]))
                    except RpcError:
                        break
                    if not c or c == V.ZERO:
                        break
                    coins.append(c)
                if coins:
                    break
            if len(coins) >= 2:
                self.meta[p] = {"coins": coins, "venue": "curve"}
        self.save()
        return {k: self.meta[k] for k in {V.pool_key(l) for l in logs if l["topics"]} if k in self.meta}


def _prices(rpc, legs):
    """USD prices from the day's own trades: WETH from WETH/USDC, others
    from their trades against WETH or USDC, three observations at least."""
    obs = {}
    for leg in legs:
        if len(leg.deltas) != 2:
            continue
        (ta, xa), (tb, xb) = leg.deltas.items()
        if xa and xb:
            obs.setdefault(ta, []).append((tb, abs(xa), abs(xb)))
            obs.setdefault(tb, []).append((ta, abs(xb), abs(xa)))
    usd = {V.USDC: 1.0, "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": 1.0}
    weth = [b / 1e6 / (a / 1e18) for (o, a, b) in obs.get(V.WETH, []) if o == V.USDC]
    usd[V.WETH] = statistics.median(weth) if weth else 0.0
    others = [t for t in obs if t not in usd]
    decs = rpc.batch([("eth_call", [{"to": t, "data": "0x313ce567"}, "latest"]) for t in others])
    dec = dict(DEC)
    for t, h in zip(others, decs):
        try:
            n = int(h, 16)
            if 0 < n <= 36:
                dec[t] = n
        except (TypeError, ValueError):
            pass
    for t in others:
        if t not in dec:
            continue
        xs = [ao / 10 ** dec[o] * usd[o] / (ax / 10 ** dec[t]) for (o, ax, ao) in obs[t]
              if o in (V.WETH, V.USDC) and usd.get(o) and o in dec]
        if len(xs) >= 3:
            usd[t] = statistics.median(xs)
    return usd, dec


def _usd(amounts, usd, dec):
    total, unvalued = 0.0, False
    for t, x in amounts.items():
        if t in usd and t in dec:
            total += x / 10 ** dec[t] * usd[t]
        elif x > 0:
            unvalued = True
    return total, unvalued


def collect_day(rpc, resolver, lo, hi):
    logs_seen, by_tx = 0, {}
    b, step = lo, 50
    while b <= hi:
        e = min(b + step - 1, hi)
        try:
            logs = rpc.call("eth_getLogs", [{"fromBlock": hex(b), "toBlock": hex(e), "topics": [list(V.ALL_TOPICS)]}])
        except RpcError:
            if step > 5:
                step //= 2
                continue
            raise
        logs_seen += len(logs)
        chunk = {}
        for log in logs:
            chunk.setdefault(log["transactionHash"], []).append(log)
        keep = {t: v for t, v in chunk.items() if len(v) >= 2}
        if keep:
            meta = resolver.meta_for_logs([l for v in keep.values() for l in v])
            for t, v in keep.items():
                legs = [x for x in (V.decode(l, meta) for l in v) if x]
                if legs:
                    by_tx[t] = legs
        b = e + 1
        step = min(step * 2, 50)
    all_legs = [l for legs in by_tx.values() for l in legs]
    usd, dec = _prices(rpc, all_legs)
    arbs = []
    for t, legs in by_tx.items():
        if not detect.is_arbitrage(legs):
            continue
        change = {k: x for k, x in detect.trader_change(legs).items() if x > 0}
        est, _ = _usd(change, usd, dec)
        venues = sorted({l.venue for l in legs})
        arbs.append({"tx": t, "block": legs[0].block, "venues": venues, "pools": sorted({l.pool for l in legs}),
                     "pairs": sorted({tuple(sorted(l.deltas)) for l in legs if len(l.deltas) == 2}),
                     "hops": len(legs), "est_usd": est, "mm": bool(set(venues) & V.MARKET_MAKERS)})
    want = [a for a in arbs if a["est_usd"] >= RECEIPT_FLOOR_USD or a["mm"]]
    receipts = rpc.batch([("eth_getTransactionReceipt", [a["tx"]]) for a in want])
    for a, r in zip(want, receipts):
        if not r:
            continue
        kept = detect.banked(r, by_tx[a["tx"]])
        value, unvalued = _usd(kept, usd, dec)
        a["banked"] = {k: str(x) for k, x in kept.items()}
        a["banked_usd"] = value
        a["valued_by"] = "banked" if kept and not unvalued else "pool"
        a["gas_usd"] = detect.gas_wei(r) / 1e18 * usd[V.WETH]
        a["sender"], a["target"] = r["from"].lower(), (r.get("to") or "").lower()
    toks = sorted({t for a in arbs for p in a["pairs"] for t in p})
    syms = rpc.batch([("eth_call", [{"to": t, "data": "0x95d89b41"}, "latest"]) for t in toks])
    symbols = {t: _text(s) for t, s in zip(toks, syms)}
    return {"lo": lo, "hi": hi, "eth_usd": usd[V.WETH], "prices": usd, "decimals": dec, "symbols": symbols,
            "logs": logs_seen, "arbs": [dict(a, pairs=[list(p) for p in a["pairs"]]) for a in arbs]}


def _text(h):
    try:
        b = bytes.fromhex(h[2:])
        if len(b) >= 96:
            n = int.from_bytes(b[32:64], "big")
            return b[64:64 + n].decode("utf-8", "replace")
        return b.rstrip(b"\0").decode("utf-8", "replace")
    except (TypeError, ValueError, AttributeError):
        return "?"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=7)
    ap.add_argument("--end", type=int, help="last block (default: the head)")
    ap.add_argument("--out", default=str(DATA))
    ap.add_argument("--rate", type=float, default=10.0, help="requests a second")
    a = ap.parse_args()
    rpc = Rpc(max_per_second=a.rate)
    resolver = Resolver(rpc, Path(a.out) / "meta.json")
    end = a.end or int(rpc.call("eth_blockNumber", []), 16)
    for d in range(a.days, 0, -1):
        hi = end - (d - 1) * BLOCKS_PER_DAY
        lo = hi - BLOCKS_PER_DAY + 1
        path = Path(a.out) / f"day-{lo}-{hi}.json.gz"
        if path.exists():
            print(f"day {lo}..{hi}: done before", flush=True)
            continue
        day = collect_day(rpc, resolver, lo, hi)
        path.parent.mkdir(parents=True, exist_ok=True)
        with gzip.open(path, "wt") as f:
            json.dump(day, f)
        print(f"day {lo}..{hi}: {day['logs']} logs, {len(day['arbs'])} arbitrages", flush=True)


if __name__ == "__main__":
    main()
