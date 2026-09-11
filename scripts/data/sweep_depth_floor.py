#!/usr/bin/env python3
"""
Sweep the depth floor over ONE event_census run, as a filter rather than one
run per floor.

WHY A FILTER. Run each floor separately and each faces a different window, and
the realised-profit differences that produces are window luck: a sequential
sweep (100k/50k/25k) gave non-monotonic dollars and a claim that 50k was "13x
better" than 100k that did not survive a third run. The $25k universe is a
superset of $50k which is a superset of $100k, so recording each pool's depth
on every row lets one run answer every floor under identical conditions.

WHAT IT MEASURES. Hit rate: of the swap INSTANCES priced, the share where any
cycle came back positive on gross. A frequency converges in ~100 instances;
realised profit is fat-tailed and never converges in a window this short.

MEASURED 2026-09-12 over 23 swap instances and 51 controls:

     floor     cycles  instances  hit rate
         0      1,292         23     17.4%
    10,000      1,292         23     17.4%
    25,000        972         23     17.4%
    50,000        852         23     17.4%
   100,000        792         20     20.0%
   250,000        160          4      0.0%
   CONTROL                    51      2.0%

Two conclusions. The floor is nearly irrelevant between $0 and $100k -- it
removes CYCLES (1,292 -> 792) but not INSTANCES, because the best cycle in an
instance was already a deep one, so shallow pools add candidates and not
opportunities. Prefer $100k: same hit rate, a third of the pools to scan.
$250k is too tight to leave anything. And swap 17-20% against control 2.0% is
the cleanest statement yet that event-triggering is what finds the edge.

Usage:  python3 scripts/data/sweep_depth_floor.py <event_census.jsonl>
"""

import json, sys
from collections import defaultdict
rows=[]
for l in open(sys.argv[1]):
    l=l.strip()
    if l:
        try: rows.append(json.loads(l))
        except: pass
def g(r):
    a=int(r['amount_in']); return (int(r['amount_out'])-a)/a*1e4
sw=[r for r in rows if r.get('trigger')=='swap' and r.get('instance')]
ct=[r for r in rows if r.get('trigger')=='control' and r.get('instance')]
def depth(r):
    d=[h.get('depth_usd') for h in r['route']]
    return min(x for x in d if x is not None) if all(x is not None for x in d) else None
def fee(r):
    return sum(h.get('fee_ppm') or 0 for h in r['route'])/100.0
print(f"rows {len(rows):,}   swap rows {len(sw):,}   control rows {len(ct):,}")
print(f"\n{'floor':>9}{'cycles':>9}{'instances':>11}{'HIT RATE':>10}{'p90 best':>10}{'best':>9}{'med fee bps':>13}")
for floor in (0,10_000,25_000,50_000,100_000,250_000,1_000_000):
    sub=[r for r in sw if (depth(r) or 0)>=floor]
    if not sub: continue
    byi=defaultdict(lambda:-1e9)
    for r in sub: byi[r['instance']]=max(byi[r['instance']], g(r))
    best=sorted(byi.values())
    hits=sum(1 for v in best if v>0)
    fees=sorted(fee(r) for r in sub)
    print(f"{floor:>9,}{len(sub):>9,}{len(best):>11}{100*hits/len(best):>9.1f}%"
          f"{best[int(.9*(len(best)-1))]:>10.2f}{best[-1]:>9.2f}{fees[len(fees)//2]:>13.2f}")
if ct:
    byi=defaultdict(lambda:-1e9)
    for r in ct: byi[r['instance']]=max(byi[r['instance']], g(r))
    b=sorted(byi.values()); h=sum(1 for v in b if v>0)
    print(f"\n  CONTROL   instances {len(b):<5} hit rate {100*h/len(b):5.1f}%   best {b[-1]:+.2f} bps")
