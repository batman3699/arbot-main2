#!/usr/bin/env bash
# What `apex shadow` is doing: whether it is running, its latest five-minute
# report in plain terms, and its most recent warnings.
#
#   scripts/shadow-status.sh               # once
#   watch -n 60 scripts/shadow-status.sh   # refreshed every minute
#
# Reads var/apex/ only: the pid file, the JSON-lines report and the log. Prints
# nothing those files do not already hold, and they hold no secret. On the
# co-located host (R25, ops/host/README.md) there is no pid file or log file:
# the process is systemd's apex-shadow unit, or apex-canary while it runs
# (R28), and its log is the journal.
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

# R28: the unit is the canary's while it runs, else the shadow's (the two
# conflict, so at most one is active).
def unit():
    try:
        live = subprocess.run(["systemctl", "is-active", "--quiet", "apex-canary"]).returncode == 0
    except OSError:
        live = False
    return "apex-canary" if live else "apex-shadow"

def unit_pid():
    try:
        out = subprocess.run(["systemctl", "show", "-p", "MainPID", "--value", unit()],
                             capture_output=True, text=True).stdout.strip()
        return int(out) or None
    except (OSError, ValueError):
        return None

pid = None
try:
    pid = int(open("var/apex/shadow.pid").read().strip())
except (OSError, ValueError):
    pid = unit_pid()
try:
    print(f"revision: {open('/opt/apex/REVISION').read().strip()}")
except OSError:
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
    building = f", Tier 2 simulates in {r['building']}" if r.get("building") else (", Tier 2 has no block yet" if "building" in r else "")
    print(f"report: head {r['head']}{building}, uptime {r['uptime_s'] // 3600}h{(r['uptime_s'] % 3600) // 60:02d}m, config {r['config'][:10]}")
    print(f"capture assurance: {'%.4f' % ca if ca is not None else 'undefined -- no ticket authorized yet'}")
    print()
    print("funnel")
    print(f"  events                      {f['events']:>10}   (one per flashblock's swaps, applied at once)")
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
    if "positions_in_place" in rl:  # R28.1: absent from a report before it
        print(f"  positions   {rl['positions_in_place']} mints/burns applied in place, {rl['positions_read_again']} read again")
    print(f"  reads       view {rf['view']}, twap {rf['twap']}, l1 {rf['l1']}, fees {rf.get('fees', 0)} failures")
    fb, ra, sg = r.get("flashblocks", {}), r.get("race", {}), r.get("signer", {})
    if fb:  # R25: absent from a report before the race ledger
        print(f"  flashblocks {fb['frames']} frames, {fb['sessions']} sessions, {fb['decode_failures']} undecoded, "
              f"{fb['incomplete_blocks']} incomplete blocks, {fb['ledger_failures']} ledger write failures")
        print(f"  race        {ra.get('written', 0)} lines, {ra.get('dropped', 0)} dropped")
    am = r.get("armed")
    if am:  # R26: absent from a report before armed-shadow
        e = am["engine"]
        print(f"  armed       {e['arms']} arms, {e['renewals']} renewals, {e['slots_armable']} armable slots, "
              f"{e['unarmable_pools']} unarmable pools, last pass {e['last_tick_us'] / 1000:.1f} ms, "
              f"{am['write_failures']} write failures")
        if e.get("passes"):  # R27.1: absent from a report before it
            n, slots = e["passes"], max(e["slots"], 1)
            print(f"  armed pass  {n} passes, {e['pass_us_total'] / n / 1000:.0f} ms each; of {e['slots']} slots, "
                  f"{100 * e['current_total'] / n / slots:.0f}% priced at the book as it stood, "
                  f"{e['waiting_total'] / n:.1f} left waiting a pass (now {e['slots_current']} and {e['stale_waiting']})")
        print(f"  moves       {am['move_checks']} swaps checked against the move model: {am['move_tick_mismatches']} wrong ticks, "
              f"{am['move_liquidity_changes']} liquidity changes (unapplied mints/burns)")
    ln = r.get("lenders")
    if ln:  # R27: absent from a report before the Morpho lender
        weth = lambda w: f"{int(w) / 1e18:,.2f} WETH" if w is not None else "unread"
        print(f"  lenders     balancer {weth(ln.get('balancer_wei'))}, "
              + (f"morpho {weth(ln['morpho_wei'])}" if ln.get("morpho_wei") is not None else "morpho not configured")
              + (f" at {ln['morpho_extra_gas']:,} gas over Balancer" if ln.get("morpho_extra_gas") else ""))
        print(f"  signer      nonce at boot {sg.get('boot_nonce')}" + ("" if not sg.get("nonce_moved") else
              "  -- SIGNER NONCE MOVED: revoke the trader key (ops/host/README.md)"))
    cn = r.get("canary")
    if cn:  # R28: present only in a canary's report
        t = cn["totals"]
        print(f"  canary      sent {t['sent']} landed {t['landed']} reverted {t['reverted_early'] + t['reverted_late']} "
              f"refused {t['refused']} net {int(t['net_wei']) / 1e18:+.6f} ETH lost {int(t['lost_wei']) / 1e18:.6f} ETH open {cn['open']}"
              + (f"  -- HALTED: {cn['halted']}" if cn.get("halted") else "")
              + ("  -- STOP FILE" if cn.get("stop_file") else ""))
    print(f"  feed        {fd['sessions']} sessions ({max(fd['sessions'] - 1, 0)} reconnects), {fd['dropped']} dropped, lossless {fd['fast_lane_lossless']}")
    rpc = r.get("rpc")
    if rpc:  # R28.1: absent from a report before it
        hours = max(r.get("uptime_s", 0), 1) / 3600
        mb = lambda b: b / 1e6
        total = sum(u["bytes_in"] + u["bytes_out"] for u in rpc)
        print(f"  http        {mb(total):,.1f} MB since boot, {mb(total) / hours:,.1f} MB/h; by method, most received first:")
        for u in rpc[:8]:
            print(f"    {u['method']:<26} {u['attempts'] / hours:>9,.0f}/h  in {mb(u['bytes_in']) / hours:>8,.1f} MB/h  "
                  f"out {mb(u['bytes_out']) / hours:>7,.1f} MB/h  faults {u['faults']:,}  {u['endpoint']}")
    q = r.get("queue")
    if q:  # R21: absent from a report before the queue
        print(f"  queue       {q['stale']} dropped as stale, {q['conflicts']} as conflicts, deepest {q['max_depth']}")
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
    try:  # the co-located host: the unit's journal
        out = subprocess.run(["journalctl", "-u", unit(), "--no-pager", "-o", "cat", "-n", "20000"],
                             capture_output=True, text=True).stdout
        plain = (re.sub(r"\x1b\[[0-9;]*m", "", l).rstrip() for l in out.splitlines())
        warn = [l for l in plain if " WARN " in l or " ERROR " in l]
    except OSError:
        warn = []
if warn:
    print()
    print(f"recent warnings ({len(warn)} in the log):")
    for l in warn[-5:]:
        print("  " + l[:220])
EOF
