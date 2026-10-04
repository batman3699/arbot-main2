#!/usr/bin/env bash
# What `apex shadow` is doing: whether it is running, its latest five-minute
# report in plain terms, and its most recent warnings.
#
#   scripts/shadow-status.sh               # once
#   watch -n 60 scripts/shadow-status.sh   # refreshed every minute
#
# Reads var/apex/ only: the pid file, the JSON-lines report and the log. Prints
# nothing those files do not already hold, and they hold no secret.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'EOF'
import json, os, re, subprocess

def proc(pid):
    try:
        out = subprocess.run(["ps", "-p", str(pid), "-o", "comm=,etime=,rss="],
                             capture_output=True, text=True).stdout.split()
    except OSError:
        return None
    return out if len(out) == 3 and out[0] == "apex" else None

pid = None
try:
    pid = int(open("var/apex/shadow.pid").read().strip())
except (OSError, ValueError):
    pass
running = proc(pid) if pid else None
if running:
    print(f"apex shadow: RUNNING  pid {pid}, up {running[1]}, {int(running[2]) // 1024} MiB")
else:
    print("apex shadow: NOT RUNNING" + (f" (pid file says {pid})" if pid else " (no pid file)"))

last = None
try:
    with open("var/apex/shadow.report.jsonl") as f:
        for line in f:
            if line.strip():
                last = line
except OSError:
    pass
if not last:
    print("no report yet: the first is written five minutes after boot")
else:
    r = json.loads(last)
    f, s, b = r["funnel"], r["search"], r["book"]
    reached = sum(f["declined"].values()) + sum(f["closed"].values()) + f["suppressed"]
    ca = r["capture_assurance"]
    print(f"report: head {r['head']}, uptime {r['uptime_s'] // 3600}h{(r['uptime_s'] % 3600) // 60:02d}m, config {r['config'][:10]}")
    print(f"capture assurance: {'%.4f' % ca if ca is not None else 'undefined -- no ticket authorized yet'}")
    print()
    print("funnel")
    print(f"  events                      {f['events']:>10}")
    print(f"  skipped, book rebuilding    {f['skipped_unverified']:>10}")
    print(f"  skipped by Engine D         {s['skipped']:>10}   (below the $5k floor, or no template)")
    print(f"  priced, no profitable size  {s['declined']:>10}   (Engine C)")
    nm = r.get("near_miss")
    if nm and nm["measured"]:
        print(f"  closest to paying           {nm['best_bps']:>+10.2f}   bps, best net over 0.01-10 WETH, of {nm['measured']} priced routes")
        print(f"    pays {nm['pays']}, within 0.5 bp {nm['within_0_5']}, 1 bp {nm['within_1']}, 2 bp {nm['within_2']}, "
              f"5 bp {nm['within_5']}, 10 bp {nm['within_10']}, further {nm['beyond_10']}")
    print(f"  reached the plane           {reached:>10}")
    print(f"  null-dispatched (signed)    {r['null_dispatched']:>10}")
    if f["declined"]:
        print("  declined, by reason:")
        for k, v in sorted(f["declined"].items(), key=lambda kv: -kv[1]):
            print(f"    {v:>8}  {k}")
    if f["closed"]:
        print("  tickets closed, by outcome:")
        for k, v in sorted(f["closed"].items(), key=lambda kv: -kv[1]):
            print(f"    {v:>8}  {k}")
    print()
    rl, rf, c, cap, fd = r["reloads"], r["read_failures"], r["costs"], r["capacity"], r["feed"]
    print("health")
    venues = ", ".join(b.get("venues", [])) or "(not reported)"
    print(f"  book        {b['status']}, {b['held']}/{b['universe']} pools held, {b['admitted']} admitted, {s['resident']} cycles")
    print(f"  venues      {venues}")
    print(f"  reloads     {rl['full']} full, {rl['partial']} partial, {rl['failed']} failed; pools refused {rl['pools_refused']}, removed {rl['pools_removed']}")
    print(f"  reads       view {rf['view']}, twap {rf['twap']}, l1 {rf['l1']} failures")
    print(f"  feed        {fd['sessions']} sessions ({max(fd['sessions'] - 1, 0)} reconnects), {fd['dropped']} dropped, lossless {fd['fast_lane_lossless']}")
    print(f"  capacity    {cap['samples']} samples, {cap['blocks_held']} blocks held, largest window {cap['largest_window']:,} gas")
    if "route_other_wei" in c:
        print(f"  costs       base fee {c['base_fee_wei'] / 1e9:.4f} gwei; per route {c['route_other_wei'] / 1e18:.8f} ETH (L1 fee, failure) + its own gas")
    else:  # a report from before R12, when every route was one figure
        print(f"  costs       base fee {c['base_fee_wei'] / 1e9:.4f} gwei, two-hop route {c['route_cost_wei'] / 1e18:.8f} ETH")
    print(f"  settlement  asked {r['settlement_asked']} (should stay 0); misses written {r['misses_written']}")

try:
    with open("var/apex/shadow.log") as f:
        plain = (re.sub(r"\x1b\[[0-9;]*m", "", l).rstrip() for l in f)
        warn = [l for l in plain if " WARN " in l or " ERROR " in l]
except OSError:
    warn = []
if warn:
    print()
    print(f"recent warnings ({len(warn)} in the log):")
    for l in warn[-5:]:
        print("  " + l[:220])
EOF
