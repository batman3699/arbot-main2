"""Rank a census's arbitrage: by venue, by what each venue would add to
ours, against the market-maker venues, and by pair (R23)."""
import argparse
import gzip
import itertools
import json
import statistics
from pathlib import Path

from arb_census.venues import MARKET_MAKERS, OURS

BLOCKS_PER_HOUR = 1800
# Venue sets are combined from this many venues outside ours: those with the
# most arbitrage value needing them.
UNLOCK_CANDIDATES = 12
DATA = Path(__file__).resolve().parents[3] / "data" / "census"


def value(a):
    """What the bot kept after costs, never more than its legs took from the
    pools (`est_usd`): more than that is someone else's flow counted as the
    bot's, since ETH moves without logs."""
    est = float(a.get("est_usd") or 0.0)
    if a.get("valued_by") == "banked":
        return min(float(a.get("banked_usd") or 0.0), est)
    return est


def _hour(a, lo):
    return (a["block"] - lo) // BLOCKS_PER_HOUR


def spike_hours(arbs, lo, hi):
    n = (hi - lo) // BLOCKS_PER_HOUR + 1
    totals = [0.0] * n
    for a in arbs:
        h = _hour(a, lo)
        if 0 <= h < n:
            totals[h] += value(a)
    med = statistics.median(totals) if totals else 0.0
    return {h for h, t in enumerate(totals) if med > 0 and t > 5 * med}


def _q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))] if xs else 0.0


def _row(arbs, spikes, lo):
    vals = [value(a) for a in arbs]
    spike = sum(value(a) for a in arbs if _hour(a, lo) in spikes)
    gas = sum(a.get("gas_usd") or 0.0 for a in arbs)
    return {"arbs": len(arbs), "gross": round(sum(vals), 2), "net": round(sum(vals) - gas, 2),
            "spike": round(spike, 2), "calm": round(sum(vals) - spike, 2),
            "p50": round(_q(vals, 0.5), 4), "p90": round(_q(vals, 0.9), 2),
            "senders": len({a.get("sender") for a in arbs if a.get("sender")})}


def venue_table(arbs, spikes, lo=0):
    by = {}
    for a in arbs:
        for v in a["venues"]:
            by.setdefault(v, []).append(a)
    return sorted(({"venue": v, **_row(x, spikes, lo)} for v, x in by.items()), key=lambda r: -r["gross"])


def unlock_table(arbs, spikes, hours, lo=0):
    """What adding venues to ours would make reachable: arbitrage needing
    exactly those venues beyond ours, alone and in combinations of up to 3,
    among the `UNLOCK_CANDIDATES` venues with the most value needing them."""
    need = {}
    for a in arbs:
        n = frozenset(a["venues"]) - OURS
        if n:
            need.setdefault(n, []).append(a)
    weight = {}
    for n, xs in need.items():
        for v in n:
            weight[v] = weight.get(v, 0.0) + sum(value(a) for a in xs)
    cands = sorted(sorted(weight, key=lambda v: -weight[v])[:UNLOCK_CANDIDATES])
    rows = []
    for k in (1, 2, 3):
        for combo in itertools.combinations(cands, k):
            got = [a for n, xs in need.items() if n <= set(combo) for a in xs]
            if got:
                rows.append({"add": list(combo), **_row(got, spikes, lo), "calm_per_day": 0.0})
    calm_hours = max(1, hours - len(spikes))
    for r in rows:
        r["calm_per_day"] = round(r["calm"] / calm_hours * 24, 2)
    return sorted(rows, key=lambda r: -r["gross"])


def market_maker_table(arbs):
    rows = []
    for v in sorted(MARKET_MAKERS):
        xs = [a for a in arbs if v in a["venues"]]
        by_sender = {}
        for a in xs:
            by_sender[a.get("sender")] = by_sender.get(a.get("sender"), 0.0) + value(a)
        gross = sum(value(a) for a in xs)
        top3 = sum(sorted(by_sender.values(), reverse=True)[:3])
        rows.append({"venue": v, "arbs": len(xs), "gross": round(gross, 2),
                     "top3_share": round(top3 / gross, 3) if gross else 0.0})
    return rows


def pair_table(arbs, symbols, spikes, lo=0):
    by, venues = {}, {}
    for a in arbs:
        for p in {tuple(p) for p in a["pairs"]}:
            by.setdefault(p, []).append(a)
            venues.setdefault(p, set()).update(a["venues"])
    rows = []
    for p, xs in by.items():
        name = "/".join(symbols.get(t, t[:8]) for t in p)
        reconstructable = bool(venues[p] & (OURS | {"uniswap_v2", "aerodrome_v2", "uniswap_v4"}))
        rows.append({"pair": name, "tokens": list(p), **_row(xs, spikes, lo),
                     "venues_seen": len(venues[p]), "two_venues": len(venues[p]) >= 2,
                     "reconstructable_venue": reconstructable})
    return sorted(rows, key=lambda r: -r["gross"])


def _md_table(rows, cols):
    head = "| " + " | ".join(cols) + " |\n|" + "---|" * len(cols) + "\n"
    return head + "".join("| " + " | ".join(str(r.get(c, "")) for c in cols) + " |\n" for r in rows)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default=str(DATA))
    a = ap.parse_args()
    d = Path(a.dir)
    days = [json.load(gzip.open(p, "rt")) for p in sorted(d.glob("day-*.json.gz"))]
    if not days:
        raise SystemExit("no day files")
    lo, hi = min(x["lo"] for x in days), max(x["hi"] for x in days)
    arbs = [a for x in days for a in x["arbs"]]
    symbols = {}
    for x in days:
        symbols.update(x["symbols"])
    hours = (hi - lo) // BLOCKS_PER_HOUR + 1
    spikes = spike_hours(arbs, lo, hi)
    cov_path = d / "coverage.json"
    coverage = json.loads(cov_path.read_text()) if cov_path.exists() else {}
    out = {"lo": lo, "hi": hi, "hours": hours, "spike_hours": sorted(spikes), "arbs": len(arbs),
           "gross": round(sum(value(x) for x in arbs), 2),
           "venues": venue_table(arbs, spikes, lo), "unlock": unlock_table(arbs, spikes, hours, lo)[:25],
           "market_makers": market_maker_table(arbs), "pairs": pair_table(arbs, symbols, spikes, lo)[:60],
           "coverage": coverage}
    stem = d / f"report-{lo}-{hi}"
    stem.with_suffix(".json").write_text(json.dumps(out, indent=1))
    md = [f"# Base arbitrage census, blocks {lo}–{hi}\n",
          f"{len(arbs):,} arbitrages, ${out['gross']:,.0f} gross over {hours} hours; spike hours: {len(spikes)}.\n",
          "\n## Venues\n", _md_table(out["venues"], ["venue", "arbs", "gross", "net", "calm", "spike", "p50", "p90", "senders"]),
          "\n## What adding venues would make reachable (beyond ours)\n",
          _md_table(out["unlock"], ["add", "arbs", "gross", "spike", "calm_per_day", "p50", "p90"]),
          "\n## Against the market-maker venues\n", _md_table(out["market_makers"], ["venue", "arbs", "gross", "top3_share"]),
          "\n## Pairs\n", _md_table(out["pairs"], ["pair", "arbs", "gross", "calm", "spike", "p50", "p90", "senders", "venues_seen", "two_venues", "reconstructable_venue"]),
          "\n## Coverage\n", _md_table([{"venue": k, **v} for k, v in coverage.items()], ["venue", "checked", "passed", "counts"])]
    stem.with_suffix(".md").write_text("".join(md))
    print(stem.with_suffix(".md"))


if __name__ == "__main__":
    main()
