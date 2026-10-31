#!/usr/bin/env bash
# The switch at Base's Denim hardfork (Task 8.5 R41), run as root by
# apex-denim-switch.timer at 2026-11-05 17:59:00 UTC. See ops/host/ARMED.md
# "The Denim day" and docs/superpowers/specs/2026-10-31-denim-readiness-design.md §2.
#
# Every --every-s seconds, `apex denim-detect` reads the chain ID and the newest
# ten blocks from a public endpoint (the first of --rpc that answers). Denim is
# live when the chain is --chain-id, the ten are consecutive, every one carries
# timestampMs on the 200 ms lattice within its own second, each step is exactly
# 200 ms, and the oldest is at or after --fork-at. Then, once:
#   - systemd drop-ins point apex-canary and apex-armed-eval at the Denim
#     configs (a later deploy unpacks the tree, so the drop-ins, not the
#     configs, are what changes);
#   - daemon-reload, and restart apex-canary (the evaluator's timer takes its
#     drop-in at its next run);
#   - one JSON line in --record, with the ten blocks and the restart time.
# A round that sees an anomaly records it (once per kind); a not-yet round
# writes nothing. Past --give-up-at it records gave_up and changes nothing:
# if Base postpones the fork, re-arm the timer for the new date.
#
# --dry-run runs one round and prints what it would write; it writes nothing.
# It exits 0 on Denim and 1 otherwise, which is how gate S rehearses it:
#   ops/host/denim-switch.sh --dry-run --rpc https://sepolia.base.org \
#     --chain-id 84532 --fork-at 1792605600 --as-user ''
# A run that finds both drop-ins already written does nothing.
set -euo pipefail

rpcs=()
chain_id=8453
fork_at=1793901600     # base/base v1.5.1: Denim active on mainnet from this timestamp
give_up_at=1793908800  # 2026-11-05 20:00:00 UTC
every_s=2
dry_run=0
apex=/opt/apex/bin/apex
systemctl=systemctl
etc=/etc/systemd/system
record=/opt/apex/var/apex/denim-switch.jsonl
as_user=apex

usage() {
  echo "usage: denim-switch.sh [--rpc URL]... [--chain-id N] [--fork-at TS] [--give-up-at TS] [--every-s S] [--dry-run]" >&2
  echo "       [--apex PATH] [--systemctl PATH] [--etc DIR] [--record PATH] [--as-user USER]" >&2
  exit 2
}
while (($#)); do
  case "$1" in
    --rpc) rpcs+=("${2:?}"); shift 2 ;;
    --chain-id) chain_id="${2:?}"; shift 2 ;;
    --fork-at) fork_at="${2:?}"; shift 2 ;;
    --give-up-at) give_up_at="${2:?}"; shift 2 ;;
    --every-s) every_s="${2:?}"; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    --apex) apex="${2:?}"; shift 2 ;;
    --systemctl) systemctl="${2:?}"; shift 2 ;;
    --etc) etc="${2:?}"; shift 2 ;;
    --record) record="${2:?}"; shift 2 ;;
    --as-user) as_user="${2-}"; shift 2 ;;
    *) usage ;;
  esac
done
((${#rpcs[@]})) || rpcs=(https://mainnet.base.org https://base-rpc.publicnode.com)

canary_conf="$etc/apex-canary.service.d/denim.conf"
eval_conf="$etc/apex-armed-eval.service.d/denim.conf"
canary_exec='ExecStart=/opt/apex/bin/apex canary --config ops/canary.denim.yaml'
eval_exec='ExecStart=/opt/apex/bin/apex armed-eval --config ops/shadow.denim.yaml'

now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
drop_in() {
  printf '# R41: written by ops/host/denim-switch.sh at %s; remove at R41.1 (ops/host/ARMED.md).\n[Service]\nExecStart=\n%s\n' "$(now)" "$1"
}
written() { [[ -f "$1" ]] && grep -qxF "$2" "$1"; }
note() { ((dry_run)) || printf '%s\n' "$1" >>"$record"; }
write_drop_in() {
  mkdir -p "$(dirname "$1")"
  drop_in "$2" >"$1.tmp"
  mv "$1.tmp" "$1"
}
detect() {
  local args=(denim-detect --chain-id "$chain_id" --fork-at "$fork_at") r
  for r in "${rpcs[@]}"; do args+=(--rpc "$r"); done
  if [[ -n "$as_user" ]]; then runuser -u "$as_user" -- "$apex" "${args[@]}"; else "$apex" "${args[@]}"; fi
}

if written "$canary_conf" "$canary_exec" && written "$eval_conf" "$eval_exec"; then
  echo "denim-switch: already switched ($canary_conf, $eval_conf)"
  exit 0
fi

last_anomaly=
while :; do
  if (($(date +%s) >= give_up_at)); then
    note "{\"event\":\"gave_up\",\"at\":\"$(now)\"}"
    echo "denim-switch: gave up: Denim not detected by $give_up_at; nothing changed"
    exit 1
  fi
  if out=$(detect 2>/dev/null); then code=0; else code=$?; fi
  line=$(printf '%s\n' "$out" | tail -1)
  [[ "$line" == \{* ]] || line=null
  case "$code" in
    0)
      if ((dry_run)); then
        echo "denim-switch: Denim detected: $line"
        printf -- '--- would write %s:\n' "$canary_conf"; drop_in "$canary_exec"
        printf -- '--- would write %s:\n' "$eval_conf"; drop_in "$eval_exec"
        echo "denim-switch: would daemon-reload and would restart apex-canary"
        exit 0
      fi
      written "$canary_conf" "$canary_exec" || write_drop_in "$canary_conf" "$canary_exec"
      written "$eval_conf" "$eval_exec" || write_drop_in "$eval_conf" "$eval_exec"
      "$systemctl" daemon-reload
      "$systemctl" restart apex-canary
      note "{\"event\":\"switched\",\"at\":\"$(now)\",\"restarted_at\":\"$(now)\",\"detect\":$line}"
      echo "denim-switch: switched and restarted apex-canary: $line"
      exit 0
      ;;
    1) ;;
    *)
      # An anomaly (2) or no source answering (3): recorded once per kind.
      kind="$code:$(printf '%s' "$line" | sed -n 's/.*"reason":\("[^"]*"\).*/\1/p')"
      if [[ "$kind" != "$last_anomaly" ]]; then
        note "{\"event\":\"$([[ "$code" == 3 ]] && echo no_source || echo anomaly)\",\"at\":\"$(now)\",\"detect\":$line}"
        last_anomaly="$kind"
      fi
      ;;
  esac
  if ((dry_run)); then
    echo "denim-switch: not switched (detect exit $code): $line"
    exit 1
  fi
  sleep "$every_s"
done
