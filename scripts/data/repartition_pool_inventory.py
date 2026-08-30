#!/usr/bin/env python3
"""repartition_pool_inventory.py — rebuild pool inventories from chain state.

Companion to verify_pool_inventory.py. Where that script *detects* inventories
that disagree with chain, this one *repairs* them:

  * repartition — every pool is filed under the venue whose declared factory
                  equals the pool's own factory(), so scraped pools that landed
                  in the wrong venue move to the right one.
  * fee field   — rewritten from chain: tickSpacing() for Slipstream-style
                  venues (whose PoolRecord.fee holds tickSpacing), fee() for
                  real UniV3-style venues. Never one substituted for the other.
  * token order — token0/token1 rewritten from the pool's own token0()/token1(),
                  replacing an aggregator's base/quote ordering.

A pool is DROPPED when it is not a concentrated-liquidity pool at all, or when
its factory maps to no venue declared in ops/inputs.yaml — nothing can quote it,
so parking it in a file would only re-contaminate the next inventory that reads
that file.

Records keep every other field (created_block, hub_usd_liquidity, hub_symbol)
and their original key order. Each rewritten file is backed up to
`pools.jsonl.bak-<UTC timestamp>` first; /data/ is gitignored, so that backup is
the only way back.

Usage:
    python3 scripts/data/repartition_pool_inventory.py base uniswap_v3 pancakeswap_v3
    python3 scripts/data/repartition_pool_inventory.py base uniswap_v3 --apply
"""
import argparse
import json
import os
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import requests
import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from verify_pool_inventory import (  # noqa: E402
    CL_KINDS,
    DATA_ROOT,
    OPS_INPUTS,
    PROBES,
    SEL,
    TICK_SPACING_VENUES,
    multicall,
    resolve_rpc,
)


def uses_tick_spacing(venue_name: str) -> bool:
    """Same rule verify_pool_inventory.py applies, so the two agree by construction."""
    return any(t in venue_name for t in TICK_SPACING_VENUES)


def load_venues(chain: str):
    data = yaml.safe_load(OPS_INPUTS.read_text())
    for entry in data.get("chains") or []:
        if entry.get("chain_name") == chain:
            return entry.get("venues") or []
    return []


def read_inventory(chain: str, venue: str):
    path = DATA_ROOT / chain / venue / "pools.jsonl"
    if not path.exists():
        return [], None
    lines = [l for l in path.read_text().splitlines() if l.strip()]
    # Preserve each file's existing JSON spacing so a rewrite is not a whole-file
    # reformat on top of the real change.
    compact = bool(lines) and '": "' not in lines[0]
    return [json.loads(l) for l in lines], compact


def dump(rec: dict, compact: bool) -> str:
    return json.dumps(rec, separators=(",", ":")) if compact else json.dumps(rec)


def probe(pools, rpc):
    """-> {pool: {probe: hexdata-or-None}}; aborts on RPC failure rather than
    silently treating an unreachable node as a pool that failed a call."""
    session = requests.Session()
    flat = [(p, k) for p in pools for k in PROBES]
    out = {}
    for i in range(0, len(flat), 150):
        part = flat[i:i + 150]
        got = multicall(session, rpc, [(a, SEL[k]) for a, k in part])
        if got is None:
            print(f"FATAL: RPC never returned for chunk at {i}; aborting without writing",
                  file=sys.stderr)
            sys.exit(3)
        for (a, k), (ok, data) in zip(part, got):
            out.setdefault(a, {})[k] = data if ok and len(data) >= 66 else None
        print(f"  probed {min(i + 150, len(flat))}/{len(flat)} calls", end="\r", flush=True)
    print(" " * 60, end="\r")
    return out


def addr(word):
    return "0x" + word[2:66][-40:] if word else None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("chain")
    ap.add_argument("venues", nargs="+", help="inventories to repartition")
    ap.add_argument("--apply", action="store_true",
                    help="write the result (default is a dry run that writes nothing)")
    args = ap.parse_args()

    venues = load_venues(args.chain)
    if not venues:
        print(f"no venues for chain {args.chain} in {OPS_INPUTS}", file=sys.stderr)
        return 2
    by_name = {v["name"]: v for v in venues}
    by_factory = {(v.get("factory") or "").lower(): v for v in venues if v.get("factory")}

    for v in args.venues:
        if v not in by_name:
            print(f"venue {v} not declared on {args.chain}", file=sys.stderr)
            return 2

    # Load the union of the named inventories, remembering where each came from.
    origin, records, compact_of = {}, {}, {}
    for v in args.venues:
        recs, compact = read_inventory(args.chain, v)
        compact_of[v] = bool(compact)
        print(f"[{v}] loaded {len(recs)} records")
        for r in recs:
            a = r["pool"].lower()
            if a in records:
                print(f"    duplicate {a} (in {origin[a]} and {v}) — keeping first")
                continue
            r["pool"] = a
            records[a] = r
            origin[a] = v

    rpc = resolve_rpc()
    print(f"\nprobing {len(records)} pools against {rpc}")
    got = probe(list(records), rpc)

    assigned, dropped = {}, []
    for a, r in records.items():
        g = got[a]
        if g["slot0"] is None or g["tickSpacing"] is None:
            why = "Solidly/V2 pair" if g["getReserves"] else "not a CL pool"
            dropped.append((a, origin[a], why))
            continue
        fac = addr(g["factory"])
        if fac is None:
            dropped.append((a, origin[a], "factory() reverted"))
            continue
        venue = by_factory.get(fac)
        if venue is None:
            dropped.append((a, origin[a], f"factory {fac} maps to no declared venue"))
            continue
        if venue.get("kind") not in CL_KINDS:
            dropped.append((a, origin[a],
                            f"factory {fac} maps to {venue['name']} (kind={venue.get('kind')}, not CL)"))
            continue

        name = venue["name"]
        if uses_tick_spacing(name):
            fee = int(g["tickSpacing"][2:66], 16)
        elif g["fee"] is not None:
            fee = int(g["fee"][2:66], 16)
        else:
            dropped.append((a, origin[a], f"fee() reverted but {name} needs a fee tier"))
            continue

        r["token0"], r["token1"] = addr(g["token0"]), addr(g["token1"])
        r["fee"] = fee
        assigned.setdefault(name, []).append((r, origin[a]))

    # ---- report ----
    print(f"\n{'=' * 72}\nRESULT\n{'=' * 72}")
    targets = sorted(set(assigned) | set(args.venues))
    for name in targets:
        rows = assigned.get(name, [])
        moved_in = [o for _, o in rows if o != name]
        kept = len(rows) - len(moved_in)
        note = ""
        if moved_in:
            src = {}
            for o in moved_in:
                src[o] = src.get(o, 0) + 1
            note = "  (+" + ", ".join(f"{n} from {o}" for o, n in sorted(src.items())) + ")"
        star = "" if name in args.venues else "   <-- NOT a named inventory; would be rewritten too"
        print(f"[{name}] {len(rows)} records: {kept} kept{note}{star}")
    print(f"\ndropped: {len(dropped)}")
    by_reason = {}
    for a, o, why in dropped:
        by_reason.setdefault(why, []).append((a, o))
    for why, items in sorted(by_reason.items(), key=lambda kv: -len(kv[1])):
        print(f"  {len(items):3}  {why}")
        for a, o in items[:4]:
            print(f"         {a}  (from {o})")
        if len(items) > 4:
            print(f"         ... and {len(items) - 4} more")

    if not args.apply:
        print("\nDRY RUN — nothing written. Re-run with --apply.")
        return 0

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    print()
    for name in targets:
        rows = assigned.get(name, [])
        if not rows:
            print(f"[{name}] refusing to write an empty inventory — left untouched")
            continue
        path = DATA_ROOT / args.chain / name / "pools.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists():
            bak = path.with_name(f"pools.jsonl.bak-{stamp}")
            bak.write_bytes(path.read_bytes())
            print(f"[{name}] backed up -> {bak.name}")
        # Inventories are ordered by hub liquidity, richest first; keep that.
        rows.sort(key=lambda t: t[0].get("hub_usd_liquidity") or 0, reverse=True)
        compact = compact_of.get(name, False)
        tmp = path.with_suffix(".jsonl.tmp")
        tmp.write_text("".join(dump(r, compact) + "\n" for r, _ in rows))
        os.replace(tmp, path)
        print(f"[{name}] wrote {len(rows)} records")
    return 0


if __name__ == "__main__":
    sys.exit(main())
