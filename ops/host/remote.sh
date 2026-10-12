#!/usr/bin/env bash
# The co-located shadow, from the operator's machine (Task 8.5 R25). Runs
# read-only commands on the host through SSM Run Command; nothing on the host
# changes. Needs AWS CLI v2 signed in: aws login --profile apex-us
#
#   ops/host/remote.sh               # status: scripts/shadow-status.sh on the host
#   ops/host/remote.sh log [N]       # the unit's last N journal lines (default 40)
#   ops/host/remote.sh warnings      # every WARN and ERROR line since the unit started
#   ops/host/remote.sh analyze       # the contest analyzer over the run so far (G1, G2)
#   ops/host/remote.sh armed         # armed-shadow's bid curve so far (R26)
#
# The host is the running instance tagged Name=apex-shadow. Override with
# APEX_AWS_PROFILE, APEX_AWS_REGION or APEX_HOST_INSTANCE.
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
# AWS CLI v2 pages its output through less in a terminal; this prints instead.
export AWS_PAGER=""
profile="${APEX_AWS_PROFILE:-apex-us}"
region="${APEX_AWS_REGION:-us-east-1}"
# Half the host's median round trip to the sequencer (2026-10-12 probe: 15 ms).
delta_ms="${APEX_DELTA_MS:-7.5}"
aws() { command aws --profile "$profile" --region "$region" "$@"; }

if ! aws sts get-caller-identity >/dev/null 2>&1; then
  echo "remote: not signed in to AWS; run: aws login --profile $profile" >&2
  exit 1
fi
instance="${APEX_HOST_INSTANCE:-$(aws ec2 describe-instances \
  --filters Name=tag:Name,Values=apex-shadow Name=instance-state-name,Values=running \
  --query 'Reservations[0].Instances[0].InstanceId' --output text)}"
[[ "$instance" == i-* ]] || { echo "remote: no running instance tagged apex-shadow in $region" >&2; exit 1; }

strip='s/\x1b\[[0-9;]*m//g'
case "${1:-status}" in
  status) cmd='cd /opt/apex && scripts/shadow-status.sh 2>&1 | cut -c1-240' ;;
  log) n="${2:-40}"; [[ "$n" =~ ^[0-9]+$ ]] || { echo "remote: log takes a line count" >&2; exit 1; }
       cmd="journalctl -u apex-shadow -n $n --no-pager -o cat | sed '$strip' | cut -c1-240" ;;
  warnings) cmd="journalctl -u apex-shadow --no-pager -o cat | sed '$strip' | grep -E ' (WARN|ERROR) ' | cut -c1-240 | tail -60" ;;
  analyze) cmd="cd /opt/apex/scripts/data && APEX_SECRET_BLOCKPI_KEY=\"\$(sed -n 's/^APEX_SECRET_BLOCKPI_KEY=//p' /etc/apex/shadow.env)\" python3 race_analysis.py --dir /opt/apex/var/apex --delta-ms $delta_ms --out /root/race_rows.jsonl 2>&1 | tail -60" ;;
  armed) cmd='/opt/apex/bin/apex armed-eval --summary --dir /opt/apex/var/apex 2>&1 | tail -120' ;;
  *) sed -n '2,14p' "$0"; exit 1 ;;
esac

params="$(python3 -c 'import json,sys; print(json.dumps({"commands": [sys.argv[1]], "executionTimeout": ["900"]}))' "$cmd")"
id="$(aws ssm send-command --instance-ids "$instance" --document-name AWS-RunShellScript \
  --comment "apex remote ${1:-status}" --parameters "$params" --query Command.CommandId --output text)"
for _ in $(seq 1 300); do
  state="$(aws ssm get-command-invocation --command-id "$id" --instance-id "$instance" --query Status --output text 2>/dev/null || true)"
  case "$state" in Success|Failed|Cancelled|TimedOut) break ;; esac
  sleep 2
done
echo "[$instance $region: $state]"
aws ssm get-command-invocation --command-id "$id" --instance-id "$instance" \
  --query '[StandardOutputContent,StandardErrorContent]' --output text
